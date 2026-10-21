//! 全局快捷键：键盘钩子（各平台的实现见 win/hook.rs、macos/hook.rs）把每个按键交给这里的状态机
//!
//! 不用 RegisterHotKey 之类的「按下即触发」机制：单修饰键热键（如 right alt）在
//! keydown 瞬间就会触发，导致 "right alt+space" 永远轮不到；长按的按键重复还会反复
//! 触发开始/停止。这里是一个状态机，单键热键在松开时才判定：
//!   - 按下→松开期间没按其他键 = 单击 → 切换普通录音 / 停止修订
//!   - 按住热键期间按下组合键（如 space）→ 开始修订录音
//! 热键的按下/松开事件全部拦截，系统看不到 Alt：不会弹窗口菜单，也不会卡键
//! （代价：该键原本的系统组合键不可用，属于有意为之）。
//! 键码统一用 Windows 虚拟键码，macOS 的钩子先把自己的键码换算过来（见 macos/keycode.rs）。

use crate::keys::{self, Hotkey};
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Mutex, OnceLock, RwLock};
use std::time::Duration;

#[derive(Clone, Copy, Debug)]
pub enum HotkeyEvent {
    /// 主热键单击
    Tap,
    Revise,
    ReviseStop,
    /// 翻译输入热键（开始 / 结束翻译录音）
    Translate,
    /// 取消本次任务（只在任务进行中触发）
    Cancel,
    /// 关闭屏幕底部的小窗（结果卡片、词条入库提示）：小窗不抢焦点、收不到按键，由钩子代为处理 Esc
    Dismiss,
}

/// 日文键盘的输入法切换键（かな、漢字、英数、カタカナひらがな、半角/全角等）：在日文输入法下
/// 往往只有「按下」没有「松开」事件。不把它们记为按住，也不能录成快捷键，
/// 否则会一直当作按着，录制快捷键永远等不到「松开」
const NO_KEYUP: &[u32] = &[0x15, 0x19, 0xF0, 0xF1, 0xF2, 0xF3, 0xF4, 0xF5];

/// 是否有任务在进行中：只有这时才拦截取消键，平时该键照常传给其他程序
static CANCEL_ARMED: AtomicBool = AtomicBool::new(false);

pub fn set_cancel_armed(armed: bool) {
    CANCEL_ARMED.store(armed, Ordering::SeqCst);
}

/// 是否有小窗开着：只有这时才拦截 Esc 用来关闭小窗
static DISMISS_ARMED: AtomicBool = AtomicBool::new(false);

pub fn set_dismiss_armed(armed: bool) {
    DISMISS_ARMED.store(armed, Ordering::SeqCst);
}

#[derive(Default)]
struct Bindings {
    main: Option<Hotkey>,
    /// 修订热键形如 "<单键主热键>+<键>" 时，记录那个组合键（按住主热键时按下即触发）
    revise_hold: Option<Vec<u32>>,
    revise: Option<Hotkey>,
    /// 与主热键相同时为 None（合并为单击逻辑）
    revise_stop: Option<Hotkey>,
    cancel: Option<Hotkey>,
    translate: Option<Hotkey>,
}

#[derive(Default)]
struct HookState {
    pressed: HashSet<u32>,
    tap_down: bool,
    tap_combo: bool,
    /// 被拦截了按下事件的组合热键触发键，松开事件也要拦截
    swallowed: HashSet<u32>,
}

struct Capture {
    /// 按下顺序（修饰键在前，最后一个是触发键）
    order: Vec<u32>,
    /// 录制期间钩子是否收到过任何按键（一个都没收到说明钩子没在工作）
    seen: bool,
    tx: Sender<Option<String>>,
}

/// 钩子是否收到过按键（第一次收到时记一条日志，便于从日志判断钩子是否在工作）
static RECEIVING: AtomicBool = AtomicBool::new(false);

struct Shared {
    bindings: RwLock<Bindings>,
    state: Mutex<HookState>,
    capture: Mutex<Option<Capture>>,
    tx: Sender<HotkeyEvent>,
}

static SHARED: OnceLock<Shared> = OnceLock::new();

/// 安装钩子（整个进程只调用一次）；事件通过 tx 发出
pub fn install(tx: Sender<HotkeyEvent>) {
    let _ = SHARED.set(Shared {
        bindings: RwLock::default(),
        state: Mutex::default(),
        capture: Mutex::default(),
        tx,
    });
    crate::platform::hook::install();
}

/// 按配置更新快捷键；返回无法解析的快捷键文本
pub fn update(hotkey: &str, revise: &str, revise_stop: &str, cancel: &str, translate: &str) -> Vec<String> {
    let mut bad = Vec::new();
    let mut parse = |s: &str| {
        let hk = Hotkey::parse(s);
        if hk.is_none() && !s.trim().is_empty() {
            bad.push(s.to_string());
        }
        hk
    };
    let main = parse(hotkey);
    let revise = parse(revise);
    let revise_stop = parse(revise_stop).filter(|h| Some(h) != main.as_ref());
    let cancel = parse(cancel);
    let translate = parse(translate).filter(|h| Some(h) != main.as_ref());

    let mut b = Bindings { main, revise, revise_stop, cancel, translate, revise_hold: None };
    if let (Some(main), Some(rev)) = (&b.main, &b.revise) {
        if main.is_single() && rev.parts.len() == 2 && rev.parts[0] == main.parts[0] {
            b.revise_hold = Some(rev.parts[1].clone());
        }
    }
    match (&b.main, &b.revise_hold) {
        (Some(m), Some(_)) => log::info!("Hotkeys: tap='{}', revise='{}' (hold)", m.text, b.revise.as_ref().unwrap().text),
        (Some(m), None) => log::info!("Hotkeys: tap='{}', revise={:?}", m.text, b.revise.as_ref().map(|h| &h.text)),
        _ => log::warn!("Hotkeys: no main hotkey"),
    }
    if let Some(t) = &b.translate {
        log::info!("Hotkeys: translate='{}'", t.text);
    }
    if let Some(shared) = SHARED.get() {
        *shared.bindings.write().unwrap() = b;
        *shared.state.lock().unwrap() = HookState::default();
    }
    bad
}

/// 录制一个快捷键（设置页用）：期间拦截所有按键，任意一个键松开时返回组合；Esc 取消（返回 Ok(None)）。
/// 超时且一个按键都没收到时返回 Err：键盘钩子没在工作（常见原因是被安全软件拦截）
pub fn capture(timeout: Duration) -> Result<Option<String>, String> {
    let Some(shared) = SHARED.get() else { return Ok(None) };
    let (tx, rx) = mpsc::channel();
    *shared.capture.lock().unwrap() = Some(Capture { order: Vec::new(), seen: false, tx });
    let result = rx.recv_timeout(timeout).ok();
    let seen = shared.capture.lock().unwrap().take().is_some_and(|c| c.seen);
    match result {
        Some(r) => Ok(r),
        None if !seen => {
            log::warn!("Hotkey capture timed out without receiving any key events (keyboard hook not working?)");
            Err(crate::platform::hook::no_events_hint().into())
        }
        None => Ok(None),
    }
}

/// 钩子收到一个按键（Aoide 自己模拟的按键已由钩子排除）；返回 true 表示拦截该事件
pub(crate) fn on_key(vk: u32, down: bool) -> bool {
    if !RECEIVING.swap(true, Ordering::Relaxed) {
        // 写日志可能较慢，放到别的线程，钩子回调要尽快返回
        std::thread::spawn(|| log::info!("Keyboard hook is receiving key events"));
    }
    SHARED.get().is_some_and(|shared| handle(shared, vk, down))
}

/// Raw Input 补上的、钩子漏掉的按键（见 win/hook.rs）。Raw Input 不能拦截按键，所以不需要返回值
#[cfg(windows)]
pub(crate) fn on_raw_key(vk: u32, down: bool) {
    if let Some(shared) = SHARED.get() {
        handle(shared, vk, down);
    }
}

/// 正被按住的单键主热键（只有它的按下事件被拦截）。macOS 拦截了修饰键的事件，系统仍会把它记为按住，
/// 钩子据此从放行的按键上去掉它的标志位：按住右 Option 时按的其他键不会变成 Option 组合
#[cfg(target_os = "macos")]
pub(crate) fn held_main_key() -> Vec<u32> {
    let Some(shared) = SHARED.get() else { return Vec::new() };
    let st = shared.state.lock().unwrap();
    match &shared.bindings.read().unwrap().main {
        Some(main) if main.is_single() && st.tap_down => {
            main.trigger().iter().copied().filter(|vk| st.pressed.contains(vk)).collect()
        }
        _ => Vec::new(),
    }
}

/// 返回 true 表示拦截该事件
fn handle(shared: &Shared, vk: u32, down: bool) -> bool {
    if let Some(cap) = shared.capture.lock().unwrap().as_mut() {
        handle_capture(cap, vk, down);
        return true;
    }
    if NO_KEYUP.contains(&vk) {
        return false;
    }

    let mut st = shared.state.lock().unwrap();
    let repeat = down && st.pressed.contains(&vk);
    if down {
        st.pressed.insert(vk);
    } else {
        st.pressed.remove(&vk);
    }
    let b = shared.bindings.read().unwrap();
    let send = |ev| {
        let _ = shared.tx.send(ev);
    };

    if let Some(cancel) = &b.cancel {
        // 未在任务中时不拦截，但仍要放行之前被拦截了按下事件的松开事件
        if (CANCEL_ARMED.load(Ordering::SeqCst) || !down) && combo(cancel, vk, down, repeat, &mut st) {
            if down && !repeat {
                send(HotkeyEvent::Cancel);
            }
            return true;
        }
    }

    // 小窗开着时，单独按 Esc 关闭小窗（取消任务优先，见上）；按下被拦截的，松开也拦截
    if vk == keys::VK_ESCAPE {
        if !down {
            if st.swallowed.remove(&vk) {
                return true;
            }
        } else if DISMISS_ARMED.load(Ordering::SeqCst) && st.pressed.len() == 1 {
            st.swallowed.insert(vk);
            if !repeat {
                send(HotkeyEvent::Dismiss);
            }
            return true;
        }
    }

    if let Some(translate) = &b.translate {
        if combo(translate, vk, down, repeat, &mut st) {
            if down && !repeat {
                send(HotkeyEvent::Translate);
            }
            return true;
        }
    }

    if let Some(main) = &b.main {
        if main.is_single() {
            if main.trigger().contains(&vk) {
                if down {
                    if !st.tap_down {
                        st.tap_down = true;
                        st.tap_combo = false;
                    }
                } else {
                    let was_tap = st.tap_down && !st.tap_combo;
                    st.tap_down = false;
                    if was_tap {
                        send(HotkeyEvent::Tap);
                    }
                }
                return true;
            }
            if st.tap_down {
                if let Some(hold) = &b.revise_hold {
                    if hold.contains(&vk) {
                        if down {
                            st.tap_combo = true;
                            if !repeat {
                                send(HotkeyEvent::Revise);
                            }
                        }
                        return true; // 吞掉组合键，避免打进输入框
                    }
                }
                if down {
                    st.tap_combo = true; // 按住热键期间按了别的键，松开时不算单击
                }
            }
        } else if combo(main, vk, down, repeat, &mut st) {
            if down && !repeat {
                send(HotkeyEvent::Tap);
            }
            return true;
        }
    }
    if b.revise_hold.is_none() {
        if let Some(rev) = &b.revise {
            if combo(rev, vk, down, repeat, &mut st) {
                if down && !repeat {
                    send(HotkeyEvent::Revise);
                }
                return true;
            }
        }
    }
    if let Some(stop) = &b.revise_stop {
        if combo(stop, vk, down, repeat, &mut st) {
            if down && !repeat {
                send(HotkeyEvent::ReviseStop);
            }
            return true;
        }
    }
    false
}

/// 普通组合键：触发键按下时修饰键都按住即命中；命中的按下/松开都拦截
fn combo(hk: &Hotkey, vk: u32, down: bool, _repeat: bool, st: &mut HookState) -> bool {
    if !hk.trigger().contains(&vk) {
        return false;
    }
    if !down {
        return st.swallowed.remove(&vk);
    }
    let mods_held = hk
        .modifiers()
        .iter()
        .all(|m| m.iter().any(|k| st.pressed.contains(k)));
    if mods_held {
        st.swallowed.insert(vk);
    }
    mods_held
}

/// 录制：记下按下的键，其中任意一个松开时就录完（不等全部松开：有些键不发松开事件，见 NO_KEYUP）
fn handle_capture(cap: &mut Capture, vk: u32, down: bool) {
    cap.seen = true;
    if NO_KEYUP.contains(&vk) {
        return;
    }
    if down {
        if vk == keys::VK_ESCAPE && cap.order.is_empty() {
            let _ = cap.tx.send(None);
            return;
        }
        if !cap.order.contains(&vk) {
            cap.order.push(vk);
        }
    } else if cap.order.contains(&vk) {
        let n = cap.order.len();
        let text = cap
            .order
            .iter()
            .enumerate()
            .map(|(i, &k)| keys::name_of(k, i + 1 < n))
            .collect::<Vec<_>>()
            .join("+");
        let _ = cap.tx.send(Some(text));
        cap.order.clear();
    }
    // 录制开始前就按下的键（比如点「更改」用的回车）松开时不在 order 里，忽略
}

#[cfg(test)]
mod tests {
    use super::*;

    const RALT: u32 = 0xA5;
    const SPACE: u32 = 0x20;
    const ESC: u32 = 0x1B;
    const A: u32 = 0x41;

    /// 录制快捷键：任意一个键松开就录完；日文输入法切换键没有松开事件，不能卡住录制
    #[test]
    fn captures_hotkeys() {
        const HANKAKU: u32 = 0xF3; // 半角/全角（只有按下）
        const NONCONVERT: u32 = 0x1D; // 無変換
        let run = |events: &[(u32, bool)]| {
            let (tx, rx) = mpsc::channel();
            let mut cap = Capture { order: Vec::new(), seen: false, tx };
            for &(vk, down) in events {
                handle_capture(&mut cap, vk, down);
            }
            rx.try_iter().collect::<Vec<_>>()
        };
        // macOS 上叫 right option
        let ralt = format!("right {}", keys::ALT);
        assert_eq!(run(&[(RALT, true), (RALT, false)]), [Some(ralt.clone())]);
        // 组合键：先松开哪个都一样
        assert_eq!(run(&[(RALT, true), (SPACE, true), (SPACE, false), (RALT, false)]), [Some(format!("{ralt}+space"))]);
        assert_eq!(run(&[(RALT, true), (SPACE, true), (RALT, false), (SPACE, false)]), [Some(format!("{ralt}+space"))]);
        // 先按了半角/全角（没有松开事件）：不影响接下来的录制
        assert_eq!(run(&[(HANKAKU, true), (NONCONVERT, true), (NONCONVERT, false)]), [Some("nonconvert".to_string())]);
        // 录制前按下的键（点「更改」用的回车）的松开事件不算
        assert_eq!(run(&[(0x0D, false), (A, true), (A, false)]), [Some("a".to_string())]);
        // 一开始就按 Esc：取消
        assert_eq!(run(&[(ESC, true)]), [None]);
        assert_eq!(Hotkey::parse("vk0x1d").unwrap().trigger(), [NONCONVERT]);
    }

    #[test]
    fn state_machine() {
        let (tx, rx) = mpsc::channel();
        let _ = SHARED.set(Shared {
            bindings: RwLock::default(),
            state: Mutex::default(),
            capture: Mutex::default(),
            tx,
        });
        update("right alt", "right alt+space", "right alt", "esc", "vk0xff");
        let sh = SHARED.get().unwrap();
        let events = || rx.try_iter().map(|e| format!("{e:?}")).collect::<Vec<_>>();

        // 单击右 Alt：按下、松开都被拦截，松开时触发
        assert!(handle(sh, RALT, true));
        assert!(events().is_empty());
        assert!(handle(sh, RALT, false));
        assert_eq!(events(), ["Tap"]);

        // 长按的重复按下只算一次
        handle(sh, RALT, true);
        handle(sh, RALT, true);
        handle(sh, RALT, false);
        assert_eq!(events(), ["Tap"]);

        // 按住右 Alt 再按空格：触发修订，空格被吞掉，松开右 Alt 不算单击
        handle(sh, RALT, true);
        assert!(handle(sh, SPACE, true));
        assert!(handle(sh, SPACE, false));
        handle(sh, RALT, false);
        assert_eq!(events(), ["Revise"]);

        // 翻译键：按下即触发并拦截，长按的重复按下只算一次，松开也拦截
        assert!(handle(sh, 0xFF, true));
        assert!(handle(sh, 0xFF, true));
        assert!(handle(sh, 0xFF, false));
        assert_eq!(events(), ["Translate"]);

        // 按住右 Alt 期间按了别的键：放行该键，松开不算单击
        handle(sh, RALT, true);
        assert!(!handle(sh, A, true));
        assert!(!handle(sh, A, false));
        handle(sh, RALT, false);
        assert!(events().is_empty());

        // 没有任务时 Esc 照常放行
        set_cancel_armed(false);
        assert!(!handle(sh, ESC, true));
        assert!(!handle(sh, ESC, false));
        assert!(events().is_empty());

        // 任务进行中：拦截 Esc 并触发取消；任务结束后松开事件也要一并拦截
        set_cancel_armed(true);
        assert!(handle(sh, ESC, true));
        set_cancel_armed(false);
        assert!(handle(sh, ESC, false));
        assert_eq!(events(), ["Cancel"]);

        // 小窗开着：Esc 关闭小窗，按下和松开都拦截；任务进行中时取消优先
        set_dismiss_armed(true);
        assert!(handle(sh, ESC, true));
        assert!(handle(sh, ESC, false));
        assert_eq!(events(), ["Dismiss"]);
        set_cancel_armed(true);
        assert!(handle(sh, ESC, true));
        assert!(handle(sh, ESC, false));
        assert_eq!(events(), ["Cancel"]);
        set_cancel_armed(false);
        // 和别的键一起按（如 Shift+Esc）不算
        assert!(!handle(sh, A, true));
        assert!(!handle(sh, ESC, true));
        assert!(!handle(sh, ESC, false));
        assert!(!handle(sh, A, false));
        // 半角/全角只有按下事件：不算按住，Esc 关闭小窗不受影响
        set_dismiss_armed(true);
        assert!(!handle(sh, 0xF3, true));
        assert!(handle(sh, ESC, true));
        assert!(handle(sh, ESC, false));
        assert_eq!(events(), ["Dismiss"]);
        set_dismiss_armed(false);
        assert!(!handle(sh, ESC, true));
        assert!(!handle(sh, ESC, false));
        assert!(events().is_empty());
    }
}
