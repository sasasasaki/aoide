//! 主流程：热键 → 录音 → 转录 → 润色/修订 → 输出

use crate::audio::{Recorder, SAMPLE_RATE};
use crate::sound::{self, Cue};
use crate::config::Config;
use crate::hotkeys::{self, HotkeyEvent};
use crate::transcriber::Transcriber;
use crate::tray::{self, TrayState};
use crate::overlay::{self, Mode};
use crate::{learned_card, result_card};
use crate::mute;
use crate::i18n::tr;
use crate::{clipboard, focus, learn, llm, models, trf, vocab};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex, RwLock};
use std::thread;
use std::time::Duration;
use tauri::AppHandle;
use tauri_plugin_notification::NotificationExt;

#[derive(Default)]
struct RecState {
    recording: bool,
    recorder: Option<Arc<Recorder>>,
}

#[derive(Default)]
struct RevState {
    active: bool,
    recorder: Option<Arc<Recorder>>,
    selected: Option<String>,
    /// 修订跨两个线程（开始 / 停止），任务凭证存在这里交接
    task: Option<Task>,
}

/// 一次任务（录音 → 识别 → 输出）的凭证：存活期间取消键生效；
/// 取消会推进全局纪元，凭证据此判断自己是否已被取消
struct Task {
    ctl: Arc<Controller>,
    epoch: u64,
}

impl Task {
    fn canceled(&self) -> bool {
        self.ctl.epoch.load(Ordering::SeqCst) != self.epoch
    }
}

impl Drop for Task {
    fn drop(&mut self) {
        let left = self.ctl.in_flight.fetch_sub(1, Ordering::SeqCst) - 1;
        hotkeys::set_cancel_armed(left > 0);
    }
}

pub struct Controller {
    app: AppHandle,
    config: RwLock<Config>,
    transcriber: Transcriber,
    rec: Mutex<RecState>,
    rev: Mutex<RevState>,
    /// 每次取消加一
    epoch: AtomicU64,
    /// 进行中的任务数
    in_flight: AtomicUsize,
}

impl Controller {
    pub fn new(app: AppHandle, config: Config) -> Arc<Self> {
        Arc::new(Self {
            app,
            config: RwLock::new(config),
            transcriber: Transcriber::default(),
            rec: Mutex::default(),
            rev: Mutex::default(),
            epoch: AtomicU64::new(0),
            in_flight: AtomicUsize::new(0),
        })
    }

    fn begin_task(self: &Arc<Self>) -> Task {
        // 新的语音输入开始：结束对上一段文字的修改观察
        learn::stop_watching();
        self.in_flight.fetch_add(1, Ordering::SeqCst);
        hotkeys::set_cancel_armed(true);
        Task { ctl: self.clone(), epoch: self.epoch.load(Ordering::SeqCst) }
    }

    /// 取消键：中止录音并丢弃音频；已在识别 / 润色的任务结果作废，不再输出
    fn cancel(self: &Arc<Self>) {
        if self.in_flight.load(Ordering::SeqCst) == 0 {
            return;
        }
        self.epoch.fetch_add(1, Ordering::SeqCst);
        {
            let mut s = self.rec.lock().unwrap();
            if s.recording {
                if let Some(r) = &s.recorder {
                    r.stop();
                }
                s.recording = false;
            }
        }
        let rev_task = {
            let mut r = self.rev.lock().unwrap();
            if r.active {
                r.active = false;
                if let Some(rec) = r.recorder.take() {
                    rec.stop();
                }
                r.selected = None;
            }
            r.task.take()
        };
        drop(rev_task); // 修订录音中被取消时，停止修订的流程不会再走到，由这里结束任务
        mute::end();
        overlay::set(Mode::Hidden);
        self.set_tray(TrayState::Idle);
        sound::play(Cue::Cancel);
        log::info!("Task canceled");
    }

    pub fn config(&self) -> Config {
        self.config.read().unwrap().clone()
    }

    pub fn notify(&self, msg: &str) {
        let _ = self.app.notification().builder().title("Aoide").body(msg).show();
    }

    fn set_tray(&self, state: TrayState) {
        tray::set_state(&self.app, state);
    }

    fn busy(&self) -> bool {
        self.rec.lock().unwrap().recording || self.rev.lock().unwrap().active
    }

    /// 正在录音、识别或润色（自动更新据此推迟安装）
    pub fn is_busy(&self) -> bool {
        self.in_flight.load(Ordering::SeqCst) > 0 || self.busy()
    }

    /// 处理结束后收起悬浮条（期间又开始了新的录音则不动）
    fn finish_overlay(&self) {
        if !self.busy() {
            overlay::set(Mode::Hidden);
        }
    }

    /// 图标变绿 2 秒后恢复灰色（期间又开始录音则不动）
    fn flash_done(self: &Arc<Self>) {
        self.set_tray(TrayState::Done);
        let me = self.clone();
        thread::spawn(move || {
            thread::sleep(Duration::from_secs(2));
            if !me.busy() {
                me.set_tray(TrayState::Idle);
            }
        });
    }

    /// 安装键盘钩子、分发热键事件、后台预热模型和 UIA
    pub fn start(self: &Arc<Self>) {
        let (tx, rx) = mpsc::channel();
        hotkeys::install(tx);
        let cfg = self.config();
        hotkeys::update(&cfg.hotkey, &cfg.revise_hotkey, &cfg.revise_stop_hotkey, &cfg.cancel_hotkey);

        let me = self.clone();
        thread::spawn(move || {
            // 钩子回调必须尽快返回，耗时操作都丢到新线程
            for ev in rx {
                let me2 = me.clone();
                match ev {
                    HotkeyEvent::Tap => {
                        if me.rev.lock().unwrap().active {
                            thread::spawn(move || me2.stop_revision());
                        } else {
                            thread::spawn(move || me2.toggle_recording());
                        }
                    }
                    HotkeyEvent::Revise => {
                        thread::spawn(move || me2.start_revision());
                    }
                    HotkeyEvent::ReviseStop => {
                        if me.rev.lock().unwrap().active {
                            thread::spawn(move || me2.stop_revision());
                        }
                    }
                    HotkeyEvent::Cancel => {
                        thread::spawn(move || me2.cancel());
                    }
                    HotkeyEvent::Dismiss => {
                        result_card::close(&me.app);
                        learned_card::close(&me.app);
                    }
                }
            }
        });

        self.preload_model();
        thread::spawn(focus::warm_up);
    }

    pub fn model_downloaded(&self) -> bool {
        let cfg = self.config();
        models::is_downloaded(&cfg.model_dir(), &cfg.model)
    }

    pub fn preload_model(self: &Arc<Self>) {
        if !self.model_downloaded() {
            return;
        }
        let me = self.clone();
        thread::spawn(move || {
            if let Err(e) = me.transcriber.preload(&me.config()) {
                log::error!("{e}");
                me.notify(&e);
            }
        });
    }

    /// 设置页保存后调用：就地生效，无需重启
    pub fn apply_config(self: &Arc<Self>, new: Config) {
        let old = std::mem::replace(&mut *self.config.write().unwrap(), new.clone());
        hotkeys::update(&new.hotkey, &new.revise_hotkey, &new.revise_stop_hotkey, &new.cancel_hotkey);
        if (&old.model, &old.model_dir, &old.device) != (&new.model, &new.model_dir, &new.device) {
            self.transcriber.reset();
            self.preload_model();
        }
        if old.ui_language != new.ui_language {
            crate::i18n::set_language(&new.ui_language);
            tray::refresh_menu(&self.app);
        }
        tray::set_tooltip(&self.app, &new.hotkey);
        log::info!("Config reloaded");
    }

    /// 取出录音；完全没录到（如误触双击）时提示并返回 None
    fn take_audio(&self, rec: Option<&Recorder>, tag: &str) -> Option<Vec<f32>> {
        match rec.and_then(|r| r.audio()) {
            Some(a) => Some(a),
            None => {
                log::info!("{tag}: no audio captured");
                sound::play(Cue::Cancel);
                self.set_tray(TrayState::Idle);
                self.finish_overlay();
                self.notify(tr("没有录到声音", "No audio was recorded"));
                None
            }
        }
    }

    // === 普通录音 ===
    fn toggle_recording(self: &Arc<Self>) {
        let (rec, task) = {
            let mut s = self.rec.lock().unwrap();
            if s.recording {
                sound::play(Cue::Stop);
                if let Some(r) = &s.recorder {
                    r.stop();
                }
                s.recording = false;
                overlay::set(Mode::Processing);
                mute::end();
                return; // 转录由正在等待录音结束的那个线程处理
            }
            s.recording = true;
            self.set_tray(TrayState::Recording);
            overlay::set(Mode::Recording);
            // 开始音播完才开录：提示音不会被录进去
            sound::play_sync(Cue::Start);
            let r = Recorder::start();
            s.recorder = Some(r.clone());
            (r, self.begin_task())
        };

        // 锁已释放，等待录音结束（第二次按键可以进入上面的分支停止）
        let cfg = self.config();
        if cfg.mute_while_recording {
            mute::begin(); // 在开始提示音之后静音，提示音本身也不受影响
        }
        if !rec.wait(Duration::from_secs(cfg.max_duration.into())) && !task.canceled() {
            log::info!("Max duration reached");
            rec.stop();
            rec.wait(Duration::from_secs(1));
            let mut s = self.rec.lock().unwrap();
            if s.recorder.as_ref().is_some_and(|r| Arc::ptr_eq(r, &rec)) {
                s.recording = false;
            }
            drop(s);
            sound::play(Cue::Stop);
            overlay::set(Mode::Processing);
            mute::end();
        }

        if task.canceled() {
            return;
        }
        let Some(audio) = self.take_audio(Some(&rec), "Recording") else {
            return;
        };
        let seconds = audio.len() as f32 / SAMPLE_RATE as f32;
        let result = self.transcriber.transcribe(&audio, &cfg);
        if task.canceled() {
            return;
        }
        match result {
            Ok(text) if text.trim().is_empty() => {
                log::info!("Empty transcription");
                self.set_tray(TrayState::Idle);
            }
            Ok(text) => {
                log::info!("Transcribed: {text}");
                let (text, glossary) = self.apply_vocab(text, &cfg);
                let text = if cfg.translate_to.trim().is_empty() || cfg.polish_api.trim().is_empty() {
                    match self.caret_context(&cfg) {
                        Some(ctx) => self.insert_in_context(text, seconds, &ctx, glossary.as_deref(), &cfg),
                        None => self.maybe_polish(text, seconds, glossary.as_deref(), &cfg),
                    }
                } else {
                    self.translate(text, glossary.as_deref(), &cfg)
                };
                if task.canceled() {
                    return;
                }
                if self.output_text(&text) && cfg.learn_from_edits {
                    learn::watch(self.app.clone(), text, cfg.clone());
                }
                self.flash_done();
            }
            Err(e) => {
                log::error!("Transcribe error: {e}");
                self.notify(&trf!("错误：{e}", "Error: {e}"));
                self.set_tray(TrayState::Idle);
            }
        }
        self.finish_overlay();
    }

    /// 词库：高置信度的误识别写法直接替换；其余与识别结果相关的词条整理成给 LLM 的说明
    fn apply_vocab(&self, text: String, cfg: &Config) -> (String, Option<String>) {
        if !cfg.vocab_enabled {
            return (text, None);
        }
        let (text, applied) = vocab::apply_confident(&text);
        for (wrong, right) in &applied {
            log::info!("Vocabulary: {wrong} -> {right}");
        }
        let glossary = vocab::glossary(&vocab::relevant(&text));
        if glossary.is_some() {
            log::info!("Vocabulary: related entries found, passing them to the LLM");
        }
        (text, glossary)
    }

    /// 文字较短、不值得整段润色时，若有相关词条，只按词库修正误识别的词
    fn correct_terms(&self, text: String, glossary: Option<&str>, cfg: &Config) -> String {
        let Some(glossary) = glossary.filter(|_| !cfg.polish_api.trim().is_empty()) else {
            return text;
        };
        overlay::set(Mode::Polishing);
        match llm::correct_terms(&text, glossary, cfg) {
            Ok(t) if !t.is_empty() => {
                if t != text {
                    log::info!("Terms corrected: {t}");
                }
                t
            }
            Ok(_) => text,
            Err(e) => {
                log::warn!("Term correction skipped: {e}");
                text
            }
        }
    }

    /// 开启「按上下文插入」且已配置 LLM 时读取光标前后的文字；光标前后都没有内容时返回 None（按普通润色处理）
    fn caret_context(&self, cfg: &Config) -> Option<focus::CaretContext> {
        if !cfg.insert_smart || cfg.polish_api.trim().is_empty() {
            return None;
        }
        let ctx = focus::caret_context().filter(|c| !c.is_empty())?;
        log::info!(
            "Caret context: {} chars before, {} chars after",
            ctx.before.chars().count(),
            ctx.after.chars().count()
        );
        Some(ctx)
    }

    /// 光标在已有文字中间：让 LLM 根据前后文生成要插入的文字。
    /// 输出仍是在光标处粘贴，前后原文不会被改动；录音太短、文字太少或调用失败时直接插入转写结果
    fn insert_in_context(&self, text: String, seconds: f32, ctx: &focus::CaretContext, glossary: Option<&str>, cfg: &Config) -> String {
        let plain = text.chars().filter(|c| !c.is_whitespace()).count();
        if seconds < cfg.insert_min_seconds as f32 || plain < cfg.insert_min_chars {
            log::info!(
                "Context insertion skipped (short: {seconds:.1}s, {plain} chars; need {}s, {} chars)",
                cfg.insert_min_seconds,
                cfg.insert_min_chars
            );
            // 不走 LLM 插入（命中词库时仍按词库修正），西文单词之间的空格按前后文补上
            let text = self.correct_terms(text, glossary, cfg);
            return llm::fit_spacing(text.trim(), &ctx.before, &ctx.after);
        }
        overlay::set(Mode::Polishing);
        match llm::insert_text(&text, &ctx.before, &ctx.after, glossary, cfg) {
            Ok(fragment) => {
                log::info!("Insert in context: {fragment}");
                fragment
            }
            Err(e) => {
                log::warn!("Context insertion skipped: {e}");
                text
            }
        }
    }

    /// LLM 润色（如果启用且已配置 API）；录音太短或文字太少都不值得润色，此时只按词库修正（有相关词条时）
    fn maybe_polish(&self, text: String, seconds: f32, glossary: Option<&str>, cfg: &Config) -> String {
        if !cfg.polish || cfg.polish_api.trim().is_empty() {
            return self.correct_terms(text, glossary, cfg);
        }
        if seconds < cfg.polish_min_seconds as f32 {
            log::info!("Polish skipped (short recording: {seconds:.1}s < {}s)", cfg.polish_min_seconds);
            return self.correct_terms(text, glossary, cfg);
        }
        let plain = text.chars().filter(|c| !c.is_whitespace()).count();
        if plain < cfg.polish_min_chars {
            log::info!("Polish skipped (short text: {plain} < {} chars)", cfg.polish_min_chars);
            return self.correct_terms(text, glossary, cfg);
        }
        overlay::set(Mode::Polishing);
        match llm::polish_text(&text, glossary, cfg) {
            Ok(p) if !p.is_empty() && p != text => {
                log::info!("Polished: {p}");
                p
            }
            Ok(_) => text,
            Err(e) => {
                log::warn!("Polish skipped: {e}");
                text
            }
        }
    }

    /// 设置了输出语言：不论长短都翻译（短句也要换成目标语言），失败时输出原文
    fn translate(&self, text: String, glossary: Option<&str>, cfg: &Config) -> String {
        overlay::set(Mode::Polishing);
        match llm::translate_text(&text, glossary, cfg) {
            Ok(t) if !t.is_empty() => {
                log::info!("Translated ({}): {t}", cfg.translate_to);
                t
            }
            Ok(_) => text,
            Err(e) => {
                log::warn!("Translation skipped: {e}");
                text
            }
        }
    }

    /// 聚焦在输入框时粘贴输出（粘完恢复原剪贴板），返回 true；无处可粘贴时弹出结果卡片，由用户决定复制
    fn output_text(&self, text: &str) -> bool {
        thread::sleep(Duration::from_millis(200));
        if focus::can_type_here() && clipboard::paste_text(text) {
            return true;
        }
        log::info!("No input focused, showing result card: {text}");
        result_card::show(&self.app, tr("没有检测到输入框，识别结果如下", "No input box detected. Here is the transcript"), text);
        false
    }

    // === 修订模式：选中文本 + 口述修改意见 ===
    fn start_revision(self: &Arc<Self>) {
        {
            let mut r = self.rev.lock().unwrap();
            if r.active || self.rec.lock().unwrap().recording {
                return;
            }
            // 先占位：抓取选中文本要几百毫秒，期间的停止键单击应视为「取消修订」而非「开始普通录音」
            r.active = true;
            r.task = Some(self.begin_task());
        }

        let selected = clipboard::get_selected_text();
        {
            let mut r = self.rev.lock().unwrap();
            if !r.active {
                return; // 抓取期间被停止键取消
            }
            if selected.is_empty() {
                r.active = false;
                r.task = None;
            } else {
                sound::play_sync(Cue::ReviseStart);
                r.selected = Some(selected.clone());
                r.recorder = Some(Recorder::start());
            }
        }
        if selected.is_empty() {
            self.notify(tr("请先选中要修改的文本", "Select the text you want to edit first"));
            return;
        }
        self.set_tray(TrayState::Recording);
        overlay::set(Mode::Revising);
        if self.config().mute_while_recording {
            mute::begin();
        }
        log::info!("Revision recording started: {} chars selected", selected.chars().count());
    }

    /// 停止修订录音 → 转录 → LLM 修改 → 替换选中
    fn stop_revision(self: &Arc<Self>) {
        let (rec, selected, task) = {
            let mut r = self.rev.lock().unwrap();
            if !r.active {
                return;
            }
            r.active = false;
            (r.recorder.take(), r.selected.take().unwrap_or_default(), r.task.take())
        };
        let canceled = || task.as_ref().is_some_and(Task::canceled);
        if let Some(r) = &rec {
            r.stop();
        }
        sound::play(Cue::Stop);
        overlay::set(Mode::Processing);
        mute::end();

        let cfg = self.config();
        let Some(audio) = self.take_audio(rec.as_deref(), "Revision recording") else {
            return;
        };
        let result = (|| -> Result<(), String> {
            let instruction = self.transcriber.transcribe(&audio, &cfg)?;
            if instruction.trim().is_empty() || canceled() {
                return Ok(());
            }
            log::info!("Revision instruction: {instruction}");
            let (instruction, _) = if cfg.vocab_enabled { vocab::apply_confident(&instruction) } else { (instruction, vec![]) };
            let glossary = cfg.vocab_enabled.then(|| vocab::glossary(&vocab::relevant(&format!("{selected}\n{instruction}")))).flatten();
            overlay::set(Mode::Polishing);
            let revised = llm::revise_text(&selected, &instruction, glossary.as_deref(), &cfg)?;
            if canceled() {
                return Ok(());
            }
            if revised.is_empty() || revised == selected {
                self.notify(tr("未检测到修改意见", "No edit instruction detected"));
                return Ok(());
            }
            thread::sleep(Duration::from_millis(100));
            if focus::can_type_here() {
                let pasted = clipboard::paste_text(&revised); // 粘贴替换选中，之后恢复原剪贴板
                log::info!("Revised: {revised}");
                self.notify(tr("已修改", "Text updated"));
                if pasted && cfg.learn_from_edits {
                    learn::watch(self.app.clone(), revised.clone(), cfg.clone());
                }
            } else {
                // 选区不可编辑（如网页正文），把修改结果放进卡片，由用户决定复制到哪
                log::info!("Revised (result card): {revised}");
                result_card::show(&self.app, tr("选中的内容不可编辑，修改结果如下", "The selection is not editable. Here is the edited text"), &revised);
            }
            Ok(())
        })();
        if canceled() {
            return;
        }
        if let Err(e) = result {
            log::error!("Revision error: {e}");
            self.notify(&trf!("修订失败：{e}", "Edit failed: {e}"));
        }
        self.finish_overlay();
        self.flash_done();
    }
}
