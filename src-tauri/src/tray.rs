//! 托盘图标：灰 = 空闲，红 = 录音中，绿 = 输出完成

use tauri::image::Image;
use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem, Submenu};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use crate::i18n::tr;
use tauri::{AppHandle, Emitter, Manager};
use std::sync::Arc;
use crate::controller::Controller;

const TRAY_ID: &str = "main";

#[derive(Clone, Copy)]
pub enum TrayState {
    Idle,
    Recording,
    Done,
}

/// 应用图标；状态用右下角的圆点角标表示（红 = 录音中，绿 = 完成）
fn mic_icon(state: TrayState) -> Image<'static> {
    let base = Image::from_bytes(include_bytes!("../icons/64x64.png")).expect("bundled tray icon");
    let (w, h) = (base.width() as usize, base.height() as usize);
    let mut rgba = base.rgba().to_vec();
    let badge: Option<[u8; 3]> = match state {
        TrayState::Idle => None,
        TrayState::Recording => Some([235, 50, 50]),
        TrayState::Done => Some([30, 190, 90]),
    };
    if let Some(color) = badge {
        // 白边圆点，边缘按覆盖率做抗锯齿
        let (cx, cy, r) = (w as f32 * 0.76, h as f32 * 0.76, w as f32 * 0.24);
        for y in 0..h {
            for x in 0..w {
                let d = ((x as f32 + 0.5 - cx).powi(2) + (y as f32 + 0.5 - cy).powi(2)).sqrt();
                let outer = (r + 0.5 - d).clamp(0.0, 1.0);
                if outer == 0.0 {
                    continue;
                }
                let inner = (r - 3.0 + 0.5 - d).clamp(0.0, 1.0);
                let i = (y * w + x) * 4;
                for c in 0..3 {
                    let fill = 255.0 * (1.0 - inner) + color[c] as f32 * inner; // 白边 → 彩色内圆
                    rgba[i + c] = (rgba[i + c] as f32 * (1.0 - outer) + fill * outer) as u8;
                }
                rgba[i + 3] = (rgba[i + 3] as f32 * (1.0 - outer) + 255.0 * outer) as u8;
            }
        }
    }
    Image::new_owned(rgba, w as u32, h as u32)
}

pub fn tooltip(hotkey: &str) -> String {
    crate::trf!("Aoide — 按 {hotkey} 开始/停止", "Aoide — press {hotkey} to start/stop")
}

/// 托盘「翻译成」子菜单的选项：输出语言代码与显示名（空 = 不翻译）
const TRANSLATE_TARGETS: &[(&str, &str)] = &[("en", "English"), ("ja", "日本語"), ("ko", "한국어"), ("zh", "简体中文"), ("zh-Hant", "繁體中文")];

fn translate_menu(app: &AppHandle) -> tauri::Result<Submenu<tauri::Wry>> {
    let current = app.try_state::<Arc<Controller>>().map(|c| c.config().translate_to).unwrap_or_default();
    let off = CheckMenuItem::with_id(app, "translate:", tr("不翻译", "Off"), true, current.is_empty(), None::<&str>)?;
    let menu = Submenu::with_id_and_items(app, "translate", tr("翻译成", "Translate to"), true, &[&off])?;
    for (code, name) in TRANSLATE_TARGETS {
        menu.append(&CheckMenuItem::with_id(app, format!("translate:{code}"), *name, true, current == *code, None::<&str>)?)?;
    }
    Ok(menu)
}

/// 从托盘切换输出语言：存盘、生效，并通知设置页更新显示
fn set_translate(app: &AppHandle, code: &str) {
    let ctl = app.state::<Arc<Controller>>();
    let mut cfg = ctl.config();
    cfg.translate_to = code.to_string();
    if let Err(e) = cfg.save() {
        ctl.notify(&crate::trf!("保存失败：{e}", "Failed to save: {e}"));
        return;
    }
    ctl.apply_config(cfg);
    refresh_menu(app);
    let _ = app.emit("translate-changed", code);
}

fn build_menu(app: &AppHandle) -> tauri::Result<Menu<tauri::Wry>> {
    let open = MenuItem::with_id(app, "settings", tr("打开 Aoide", "Open Aoide"), true, None::<&str>)?;
    let translate = translate_menu(app)?;
    let quit = MenuItem::with_id(app, "quit", tr("退出", "Quit"), true, None::<&str>)?;
    let separator = PredefinedMenuItem::separator(app)?;
    // 有新版本时，菜单最上面加一项「更新到 x.y.z」
    let update = crate::updater::available(app)
        .map(|u| MenuItem::with_id(app, "update", crate::trf!("更新到 {}", "Update to {}", u.version), true, None::<&str>))
        .transpose()?;
    match &update {
        Some(update) => Menu::with_items(app, &[update, &open, &translate, &separator, &quit]),
        None => Menu::with_items(app, &[&open, &translate, &separator, &quit]),
    }
}

/// 切换界面语言、发现新版本后重建菜单
pub fn refresh_menu(app: &AppHandle) {
    if let (Some(tray), Ok(menu)) = (app.tray_by_id(TRAY_ID), build_menu(app)) {
        let _ = tray.set_menu(Some(menu));
        log::info!("Tray menu language: {}", tr("中文", "English"));
    }
}

pub fn create(app: &AppHandle, hotkey: &str) -> tauri::Result<()> {
    let menu = build_menu(app)?;

    TrayIconBuilder::with_id(TRAY_ID)
        .icon(mic_icon(TrayState::Idle))
        .tooltip(tooltip(hotkey))
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id.as_ref() {
            "settings" => crate::open_settings(app),
            "update" => {
                let app = app.clone();
                tauri::async_runtime::spawn(async move {
                    if let Err(e) = crate::updater::install(&app).await {
                        use tauri::Manager;
                        app.state::<std::sync::Arc<crate::controller::Controller>>()
                            .notify(&crate::trf!("更新失败：{e}", "Update failed: {e}"));
                    }
                });
            }
            "quit" => {
                crate::mute::end(); // 退出前恢复被静音的程序
                log::info!("=== Aoide Ended ===");
                app.exit(0);
            }
            id => {
                if let Some(code) = id.strip_prefix("translate:") {
                    set_translate(app, code);
                }
            }
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = event {
                crate::open_settings(tray.app_handle());
            }
        })
        .build(app)?;
    Ok(())
}

pub fn set_state(app: &AppHandle, state: TrayState) {
    if let Some(tray) = app.tray_by_id(TRAY_ID) {
        let _ = tray.set_icon(Some(mic_icon(state)));
    }
}

pub fn set_tooltip(app: &AppHandle, hotkey: &str) {
    if let Some(tray) = app.tray_by_id(TRAY_ID) {
        let _ = tray.set_tooltip(Some(tooltip(hotkey)));
    }
}
