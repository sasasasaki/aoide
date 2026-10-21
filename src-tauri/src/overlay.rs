//! 录音提示悬浮条：输入焦点所在屏幕底部居中的胶囊，录音时显示随音量跳动的波形，识别中显示波浪动画
//! 显示期间持续跟随焦点：切到另一块屏幕上的输入框，悬浮条也换到那块屏幕的同一位置
//!
//! 这里是与平台无关的部分：状态、淡入淡出、波形动画和绘制（tiny-skia）。
//! 窗口本身是各平台的原生窗口（win/overlay.rs、macos/overlay.rs）：不抢焦点、鼠标穿透，不影响打字和粘贴。

use crate::audio;
use std::sync::Mutex;
use std::time::Instant;
use tiny_skia::{FillRule, Paint, Path, PathBuilder, Pixmap, Transform};

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Mode {
    Hidden,
    /// 普通录音
    Recording,
    /// 语音修订录音
    Revising,
    /// 翻译输入录音
    Translating,
    /// 录音结束，转写中
    Processing,
    /// LLM 润色 / 修订中
    Polishing,
}

/// 胶囊的逻辑尺寸（按显示器缩放）
pub(crate) const WIDTH: f32 = 120.0;
pub(crate) const HEIGHT: f32 = 36.0;
/// 距工作区（任务栏上沿 / 程序坞上沿）的距离
pub(crate) const BOTTOM_GAP: f32 = 16.0;
const N_BARS: usize = 12;
pub(crate) const FRAME_MS: u32 = 16;
const FADE_PER_FRAME: f32 = 0.16;

/// 显示期间检查焦点所在屏幕的间隔
pub(crate) const FOLLOW_MS: u128 = 200;

static MODE: Mutex<Mode> = Mutex::new(Mode::Hidden);

/// 创建悬浮窗（整个进程调用一次）
pub fn init() {
    crate::platform::overlay::init();
}

/// 切换悬浮条状态（任意线程可调用）
pub fn set(mode: Mode) {
    *MODE.lock().unwrap() = mode;
    crate::platform::overlay::refresh();
}

/// 当前要求的状态
pub(crate) fn current() -> Mode {
    *MODE.lock().unwrap()
}

/// 动画状态：只在悬浮窗的绘制线程里访问
pub(crate) struct Anim {
    pub visible: bool,
    pub alpha: f32,
    /// 正在淡出时保留最后一个可见状态继续绘制
    shown_mode: Mode,
    /// 显示器缩放比例（胶囊按它放大绘制）
    pub scale: f32,
    bars: [f32; N_BARS],
    last_shift: Instant,
    start: Instant,
}

impl Anim {
    pub fn new() -> Self {
        Self {
            visible: false,
            alpha: 0.0,
            shown_mode: Mode::Hidden,
            scale: 1.0,
            bars: [0.0; N_BARS],
            last_shift: Instant::now(),
            start: Instant::now(),
        }
    }

    /// 状态改变时调用。返回 true 表示刚从隐藏变为显示：调用方定位窗口、开始逐帧绘制
    pub fn show(&mut self) -> bool {
        let mode = current();
        if mode == Mode::Hidden {
            return false; // 由逐帧动画淡出后隐藏
        }
        if mode != self.shown_mode && self.shown_mode != Mode::Processing {
            self.bars = [0.0; N_BARS];
        }
        self.shown_mode = mode;
        if self.visible {
            return false;
        }
        self.visible = true;
        self.alpha = 0.0;
        self.start = Instant::now();
        true
    }

    /// 每帧调用：淡入淡出、更新波形。返回 false 表示已经完全淡出：调用方隐藏窗口、停止逐帧绘制
    pub fn frame(&mut self) -> bool {
        if current() == Mode::Hidden {
            self.alpha -= FADE_PER_FRAME;
            if self.alpha <= 0.0 {
                self.alpha = 0.0;
                self.visible = false;
                self.shown_mode = Mode::Hidden;
                return false;
            }
        } else {
            self.alpha = (self.alpha + FADE_PER_FRAME).min(1.0);
        }
        self.update_bars();
        true
    }

    /// 录音时：每 60 ms 把当前音量推入波形（从右往左滚动）；转写中：波浪动画；润色中：高光扫过
    fn update_bars(&mut self) {
        match self.shown_mode {
            Mode::Recording | Mode::Revising | Mode::Translating => {
                if self.last_shift.elapsed().as_millis() >= 60 {
                    self.last_shift = Instant::now();
                    self.bars.rotate_left(1);
                    // 语音 RMS 通常在 0.01~0.2，开方压缩动态范围
                    let level = (audio::input_level() * 8.0).sqrt().min(1.0);
                    self.bars[N_BARS - 1] = level;
                }
            }
            Mode::Processing => {
                let t = self.start.elapsed().as_secs_f32();
                for (i, b) in self.bars.iter_mut().enumerate() {
                    let wave = (t * 6.0 - i as f32 * 0.55).sin().max(0.0);
                    *b = 0.15 + 0.45 * wave;
                }
            }
            Mode::Polishing => {
                // 一道高光从左往右扫过
                let t = self.start.elapsed().as_secs_f32();
                let span = N_BARS as f32 + 6.0;
                let pos = (t * 9.0) % span - 3.0;
                for (i, b) in self.bars.iter_mut().enumerate() {
                    let d = i as f32 - pos;
                    *b = 0.22 + 0.55 * (-d * d / 3.0).exp();
                }
            }
            Mode::Hidden => {}
        }
    }

    /// 按 scale 绘制胶囊（预乘 alpha 的 RGBA，不含整体淡入淡出的 alpha）
    pub fn draw(&self) -> Option<Pixmap> {
        let s = self.scale;
        let (w, h) = ((WIDTH * s).round(), (HEIGHT * s).round());
        let mut pm = Pixmap::new(w as u32, h as u32)?;
        let id = Transform::identity();

        let polishing = self.shown_mode == Mode::Polishing;
        let t = self.start.elapsed().as_secs_f32();
        let (dot, bar) = match self.shown_mode {
            Mode::Revising => ([167, 139, 250], [221, 214, 254]),
            Mode::Translating => ([45, 212, 191], [204, 251, 241]),
            Mode::Processing => ([96, 205, 255], [186, 230, 253]),
            Mode::Polishing => ([251, 191, 36], [253, 230, 138]),
            _ => ([255, 77, 79], [255, 255, 255]),
        };

        // 胶囊底 + 细描边（润色中描边换成琥珀色）
        let pill = rounded_rect(0.5, 0.5, w - 1.0, h - 1.0, h / 2.0)?;
        pm.fill_path(&pill, &paint(24, 24, 27, 235), FillRule::Winding, id, None);
        let border = if polishing { paint(dot[0], dot[1], dot[2], 120) } else { paint(255, 255, 255, 40) };
        pm.stroke_path(&pill, &border, &tiny_skia::Stroke { width: 1.0 * s.max(1.0), ..Default::default() }, id, None);

        // 左侧状态标记：录音时呼吸闪烁的圆点；转写中常亮圆点；润色中换成星芒 ✦
        let (cx, cy) = (18.0 * s, h / 2.0);
        if polishing {
            let pulse = 0.75 + 0.25 * (t * 5.0).sin();
            let mark = sparkle(cx, cy, 6.5 * s * pulse, 1.6 * s)?;
            pm.fill_path(&mark, &paint(dot[0], dot[1], dot[2], 255), FillRule::Winding, id, None);
        } else {
            let pulse = if self.shown_mode == Mode::Processing { 1.0 } else { 0.65 + 0.35 * (t * 4.0).sin().abs() };
            let mark = PathBuilder::from_circle(cx, cy, 4.5 * s)?;
            pm.fill_path(&mark, &paint(dot[0], dot[1], dot[2], (255.0 * pulse) as u8), FillRule::Winding, id, None);
        }

        // 波形条：中间的条更高，看起来更自然
        let (left, right) = (32.0 * s, w - 16.0 * s);
        let bar_w = 3.0 * s;
        let step = (right - left - bar_w) / (N_BARS - 1) as f32;
        let (min_h, max_h) = (3.0 * s, h - 14.0 * s);
        for (i, &value) in self.bars.iter().enumerate() {
            let profile = 0.55 + 0.45 * (std::f32::consts::PI * (i as f32 + 0.5) / N_BARS as f32).sin();
            let bh = min_h + (max_h - min_h) * value * profile;
            let x = left + step * i as f32;
            // 润色中：被高光扫到的条更亮
            let alpha = if polishing { (110.0 + 145.0 * ((value - 0.22) / 0.55).clamp(0.0, 1.0)) as u8 } else { 230 };
            if let Some(path) = rounded_rect(x, (h - bh) / 2.0, bar_w, bh, bar_w / 2.0) {
                pm.fill_path(&path, &paint(bar[0], bar[1], bar[2], alpha), FillRule::Winding, id, None);
            }
        }
        Some(pm)
    }
}

fn rounded_rect(x: f32, y: f32, w: f32, h: f32, r: f32) -> Option<Path> {
    let r = r.min(w / 2.0).min(h / 2.0);
    let k = r * 0.5523; // 三次贝塞尔近似四分之一圆
    let mut pb = PathBuilder::new();
    pb.move_to(x + r, y);
    pb.line_to(x + w - r, y);
    pb.cubic_to(x + w - r + k, y, x + w, y + r - k, x + w, y + r);
    pb.line_to(x + w, y + h - r);
    pb.cubic_to(x + w, y + h - r + k, x + w - r + k, y + h, x + w - r, y + h);
    pb.line_to(x + r, y + h);
    pb.cubic_to(x + r - k, y + h, x, y + h - r + k, x, y + h - r);
    pb.line_to(x, y + r);
    pb.cubic_to(x, y + r - k, x + r - k, y, x + r, y);
    pb.close();
    pb.finish()
}

fn paint(r: u8, g: u8, b: u8, a: u8) -> Paint<'static> {
    let mut p = Paint::default();
    p.set_color_rgba8(r, g, b, a);
    p.anti_alias = true;
    p
}

/// 四角星芒（外半径 r，内半径 inner）
fn sparkle(cx: f32, cy: f32, r: f32, inner: f32) -> Option<Path> {
    let mut pb = PathBuilder::new();
    for k in 0..8 {
        let angle = std::f32::consts::FRAC_PI_4 * k as f32 - std::f32::consts::FRAC_PI_2;
        let radius = if k % 2 == 0 { r } else { inner };
        let (x, y) = (cx + radius * angle.cos(), cy + radius * angle.sin());
        if k == 0 {
            pb.move_to(x, y);
        } else {
            pb.line_to(x, y);
        }
    }
    pb.close();
    pb.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn anim(mode: Mode, bars: [f32; N_BARS], start: Instant) -> Anim {
        Anim { visible: true, alpha: 1.0, shown_mode: mode, scale: 2.0, bars, last_shift: Instant::now(), start }
    }

    /// README 配图：用与程序相同的绘制代码把各状态的胶囊输出成 PNG（2 倍分辨率），不涉及真实屏幕。
    /// 由 scripts/readme-images.ps1 调用：VT_IMG_DIR=… cargo test --lib export_capsule_images -- --ignored
    #[test]
    #[ignore]
    fn export_capsule_images() {
        let dir = std::path::PathBuf::from(std::env::var("VT_IMG_DIR").unwrap_or_else(|_| "target/readme".into()));
        std::fs::create_dir_all(&dir).unwrap();
        // 一段像说话的波形
        let speech = [0.18, 0.4, 0.72, 0.95, 0.6, 0.38, 0.82, 1.0, 0.58, 0.3, 0.52, 0.22];
        let at = |secs: f32| Instant::now() - Duration::from_secs_f32(secs);
        for (name, mode, t) in [
            ("recording", Mode::Recording, 0.4),
            ("revising", Mode::Revising, 0.4),
            ("translating", Mode::Translating, 0.4),
            ("transcribing", Mode::Processing, 0.35),
            ("polishing", Mode::Polishing, 0.6),
        ] {
            let mut v = anim(mode, speech, at(t));
            v.update_bars();
            if matches!(mode, Mode::Recording | Mode::Revising | Mode::Translating) {
                v.bars = speech; // 录音态的波形来自麦克风，这里用固定样例
            }
            let path = dir.join(format!("capsule-{name}.png"));
            v.draw().unwrap().save_png(&path).unwrap();
            println!("wrote {}", path.display());
        }
    }

    /// README 演示动图用：按 30 fps 导出各状态的逐帧动画（scripts/readme-demo.ps1 调用）
    /// 输出 {VT_IMG_DIR}/frames/{recording,revising,transcribing,polishing}-NNNN.png
    #[test]
    #[ignore]
    fn export_capsule_frames() {
        const FPS: f32 = 30.0;
        let dir = std::path::PathBuf::from(std::env::var("VT_IMG_DIR").unwrap_or_else(|_| "target/readme".into())).join("frames");
        std::fs::create_dir_all(&dir).unwrap();
        // 模拟说话的音量：音节起伏 + 句间停顿 + 确定性的抖动
        let level = |t: f32| {
            let syllable = 0.5 + 0.5 * (t * 2.0 * std::f32::consts::PI * 4.2).sin();
            let phrase = ((t * 0.9).sin() * 0.5 + 0.5).powf(0.6);
            let jitter = ((t * 1000.0).sin() * 43758.547).fract().abs();
            (0.15 + 0.85 * syllable * phrase * (0.6 + 0.4 * jitter)).min(1.0)
        };
        for (name, mode, secs) in [
            ("recording", Mode::Recording, 8.0),
            ("revising", Mode::Revising, 6.0),
            ("transcribing", Mode::Processing, 2.0),
            ("polishing", Mode::Polishing, 3.0),
        ] {
            let now = Instant::now();
            for i in 0..(secs * FPS) as usize {
                let t = i as f32 / FPS;
                let mut v = anim(mode, [0.0; N_BARS], now - Duration::from_secs_f32(t));
                if matches!(mode, Mode::Recording | Mode::Revising) {
                    // 与 update_bars 相同：每 60 ms 推入一格音量，从右往左滚动
                    let shifts = (t * 1000.0 / 60.0) as i32;
                    for (k, b) in v.bars.iter_mut().enumerate() {
                        let n = shifts - (N_BARS - 1 - k) as i32;
                        *b = if n > 0 { level(n as f32 * 0.06) } else { 0.0 };
                    }
                } else {
                    v.update_bars();
                }
                v.draw().unwrap().save_png(dir.join(format!("{name}-{i:04}.png"))).unwrap();
            }
            println!("wrote {name} ({} frames)", (secs * FPS) as usize);
        }
    }
}
