mod audio;
mod clipboard;
mod config;
mod controller;
mod focus;
mod hotkeys;
mod i18n;
mod keys;
mod learn;
mod learned_card;
mod llm;
mod models;
mod notice;
mod overlay;
mod phonetic;
mod result_card;
mod sound;
mod transcriber;
mod tray;
mod updater;
mod vocab;

/// 平台相关的实现（键盘钩子、模拟按键、焦点检测、静音、原生窗口……）。两个平台提供相同的接口
#[cfg(windows)]
#[path = "win/mod.rs"]
mod platform;
#[cfg(target_os = "macos")]
#[path = "macos/mod.rs"]
mod platform;

use platform::{input, mute};

use config::Config;
use controller::Controller;
use serde::Serialize;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tauri::{AppHandle, Emitter, Manager, RunEvent, State, WebviewUrl, WebviewWindowBuilder};
use tauri_plugin_autostart::ManagerExt;

/// 开机自启时附带的命令行参数
const AUTOSTART_ARG: &str = "--autostart";

/// 打开（或激活已有的）主窗口
pub fn open_settings(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("settings") {
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
        return;
    }
    let built = WebviewWindowBuilder::new(app, "settings", WebviewUrl::App("index.html".into()))
        .title("Aoide")
        .inner_size(960.0, 720.0)
        .min_inner_size(560.0, 480.0)
        .center()
        .build();
    match built {
        Ok(w) => {
            platform::main_window_shown(app);
            let app = app.clone();
            w.on_window_event(move |e| match e {
                tauri::WindowEvent::Destroyed => platform::main_window_closed(&app),
                // 设置窗口在前台时其他程序的键盘钩子可能先拦下按键，重新安装让 Aoide 的钩子排到前面（见 win/hook.rs）
                tauri::WindowEvent::Focused(true) => log_focus_diagnosis(),
                _ => {}
            });
        }
        Err(e) => log::error!("Open settings failed: {e}"),
    }
}

#[derive(Serialize)]
struct ModelEntry {
    id: &'static str,
    engine: models::Engine,
    size_mb: u32,
    /// [中文, English, 日本語]
    note: [&'static str; 3],
    downloaded: bool,
}

fn model_entries(dir: &std::path::Path) -> Vec<ModelEntry> {
    models::MODELS
        .iter()
        .map(|m| ModelEntry {
            id: m.id,
            engine: m.engine,
            size_mb: m.size_mb,
            note: m.note,
            downloaded: models::is_downloaded(dir, m.id),
        })
        .collect()
}

fn dir_or_default(model_dir: &str) -> PathBuf {
    Config { model_dir: model_dir.to_string(), ..Config::default() }.model_dir()
}

/// 系统权限（macOS 需要用户授权；Windows 上总是齐全的）
#[derive(Serialize)]
pub(crate) struct Permissions {
    /// granted / denied / undetermined（还没问过）
    pub microphone: &'static str,
    pub accessibility: bool,
}

impl Permissions {
    fn all_granted(&self) -> bool {
        self.microphone == "granted" && self.accessibility
    }
}

#[tauri::command]
fn get_permissions() -> Permissions {
    platform::permissions::status()
}

/// 设置页上点了授权按钮：弹出系统对话框或打开系统设置的对应页面（麦克风对话框会等到用户回答）
#[tauri::command]
async fn request_permission(kind: String) {
    let _ = tauri::async_runtime::spawn_blocking(move || platform::permissions::request(&kind)).await;
}

#[derive(Serialize)]
struct SettingsPayload {
    config: Config,
    models: Vec<ModelEntry>,
    default_model_dir: String,
    log_dir: String,
    autostart: bool,
    version: String,
    /// windows / macos（设置页据此调整文字和按键名）
    platform: &'static str,
    /// 「跟随系统」时实际使用的语言
    system_language: &'static str,
    /// 各项设置的默认值（设置页「恢复默认」用）
    defaults: Config,
    /// 内置提示词（自定义为空时显示和使用）
    default_polish_prompt: &'static str,
    default_revise_prompt: &'static str,
    default_insert_prompt: &'static str,
}

#[tauri::command]
fn get_settings(app: AppHandle, ctl: State<Arc<Controller>>) -> SettingsPayload {
    let config = ctl.config();
    SettingsPayload {
        models: model_entries(&config.model_dir()),
        config,
        default_model_dir: config::default_model_dir().to_string_lossy().into(),
        log_dir: config::data_dir().to_string_lossy().into(),
        autostart: app.autolaunch().is_enabled().unwrap_or(false),
        version: app.package_info().version.to_string(),
        platform: std::env::consts::OS,
        system_language: i18n::system_language(),
        defaults: Config::default(),
        default_polish_prompt: llm::POLISH_PROMPT,
        default_revise_prompt: llm::REVISE_PROMPT,
        default_insert_prompt: llm::INSERT_PROMPT,
    }
}

#[tauri::command]
fn list_models(model_dir: String) -> Vec<ModelEntry> {
    model_entries(&dir_or_default(&model_dir))
}

#[tauri::command]
fn save_settings(
    app: AppHandle,
    ctl: State<Arc<Controller>>,
    config: Config,
    autostart: bool,
) -> Result<(), String> {
    for hk in [&config.hotkey, &config.revise_hotkey, &config.revise_stop_hotkey, &config.cancel_hotkey] {
        if keys::Hotkey::parse(hk).is_none() {
            return Err(trf!("无法识别的快捷键：{hk}", "Unrecognized hotkey: {hk}", "認識できないショートカットキーです: {hk}"));
        }
    }
    let cancel = keys::Hotkey::parse(&config.cancel_hotkey);
    if cancel == keys::Hotkey::parse(&config.hotkey) || cancel == keys::Hotkey::parse(&config.revise_hotkey) {
        return Err(i18n::tr("取消快捷键不能与语音输入 / 语音修订快捷键相同", "The cancel hotkey must differ from the dictation and voice edit hotkeys", "キャンセルのショートカットキーは、音声入力・音声修正のショートカットキーとは別のキーにしてください").into());
    }
    // 翻译输入快捷键可以留空（不启用）
    if !config.translate_hotkey.trim().is_empty() {
        let Some(translate) = keys::Hotkey::parse(&config.translate_hotkey) else {
            let hk = &config.translate_hotkey;
            return Err(trf!("无法识别的快捷键：{hk}", "Unrecognized hotkey: {hk}", "認識できないショートカットキーです: {hk}"));
        };
        if [&config.hotkey, &config.revise_hotkey, &config.cancel_hotkey].iter().any(|hk| keys::Hotkey::parse(hk).as_ref() == Some(&translate)) {
            return Err(i18n::tr("翻译输入快捷键不能与语音输入 / 语音修订 / 取消快捷键相同", "The translate hotkey must differ from the dictation, voice edit and cancel hotkeys", "翻訳入力のショートカットキーは、音声入力・音声修正・キャンセルのショートカットキーとは別のキーにしてください").into());
        }
    }
    if !config.polish_prompt.trim().is_empty() && !config.polish_prompt.contains("{text}") {
        return Err(i18n::tr("润色提示词必须包含 {text}（转写结果）", "The polish prompt must contain {text} (the transcript)", "整形プロンプトには {text}（文字起こし結果）を含めてください").into());
    }
    if !config.revise_prompt.trim().is_empty()
        && !(config.revise_prompt.contains("{original}") && config.revise_prompt.contains("{instruction}"))
    {
        return Err(i18n::tr(
            "修订提示词必须包含 {original}（选中的原文）和 {instruction}（修改意见）",
            "The edit prompt must contain {original} (the selected text) and {instruction} (your instruction)",
            "修正プロンプトには {original}（選択した元のテキスト）と {instruction}（修正の指示）を含めてください",
        )
        .into());
    }
    if !config.insert_prompt.trim().is_empty()
        && !["{before}", "{after}", "{text}"].iter().all(|p| config.insert_prompt.contains(p))
    {
        return Err(i18n::tr(
            "插入提示词必须包含 {before}、{after}（光标前后的文字）和 {text}（转写结果）",
            "The insert prompt must contain {before}, {after} (text around the cursor) and {text} (the transcript)",
            "挿入プロンプトには {before}・{after}（カーソル前後のテキスト）と {text}（文字起こし結果）を含めてください",
        )
        .into());
    }
    if models::find(&config.model).is_none() {
        return Err(trf!("未知模型：{}", "Unknown model: {}", "不明なモデルです: {}", config.model));
    }
    if config.min_duration >= config.max_duration {
        return Err(trf!(
            "最短录音（{} 秒）必须小于最长录音（{} 秒）",
            "Minimum recording length ({} s) must be shorter than the maximum ({} s)",
            "最短録音時間（{} 秒）は最長録音時間（{} 秒）より短くしてください",
            config.min_duration, config.max_duration
        ));
    }

    // 开机启动是系统状态（Windows 注册表 Run 项、macOS 登录项），不入配置文件
    let launcher = app.autolaunch();
    if launcher.is_enabled().unwrap_or(false) != autostart {
        let r = if autostart { launcher.enable() } else { launcher.disable() };
        r.map_err(|e| trf!("开机启动设置失败：{e}", "Failed to update start at login: {e}", "ログイン時の自動起動を設定できませんでした: {e}"))?;
    }

    config.save().map_err(|e| trf!("保存失败：{e}", "Failed to save: {e}", "保存できませんでした: {e}"))?;
    ctl.apply_config(config);
    Ok(())
}

/// 录制快捷键：期间所有按键都被拦截（不会触发录音），任意一个键松开时返回组合，Esc 取消。
/// 一个按键都没收到时返回错误（键盘钩子没在工作）
#[tauri::command]
async fn capture_hotkey() -> Result<Option<String>, String> {
    tauri::async_runtime::spawn_blocking(|| hotkeys::capture(Duration::from_secs(15)))
        .await
        .map_err(|e| e.to_string())?
}

#[derive(Default)]
struct Downloads {
    busy: AtomicBool,
    cancel: Arc<AtomicBool>,
}

#[derive(Clone, Serialize)]
struct DownloadProgress {
    id: String,
    done: u64,
    total: u64,
}

#[derive(Clone, Serialize)]
struct DownloadFinished {
    id: String,
    error: Option<String>,
}

#[tauri::command]
fn download_model(
    app: AppHandle,
    downloads: State<Downloads>,
    id: String,
    model_dir: String,
) -> Result<(), String> {
    if downloads.busy.swap(true, Ordering::SeqCst) {
        return Err(i18n::tr("已有下载任务在进行", "A download is already in progress", "ほかのダウンロードが進行中です").into());
    }
    downloads.cancel.store(false, Ordering::SeqCst);
    let cancel = downloads.cancel.clone();
    let dir = dir_or_default(&model_dir);
    std::thread::spawn(move || {
        let result = models::download(&id, &dir, &cancel, |done, total| {
            let _ = app.emit("download-progress", DownloadProgress { id: id.clone(), done, total });
        });
        if let Err(e) = &result {
            log::error!("Download {id} failed: {e}");
        }
        app.state::<Downloads>().busy.store(false, Ordering::SeqCst);
        if result.is_ok() {
            app.state::<Arc<Controller>>().preload_model();
        }
        let _ = app.emit("download-finished", DownloadFinished { id, error: result.err() });
    });
    Ok(())
}

#[tauri::command]
fn cancel_download(downloads: State<Downloads>) {
    downloads.cancel.store(true, Ordering::SeqCst);
}

/// 当前界面语言（zh / en / ja），结果卡片等小窗用
#[tauri::command]
fn get_ui_language() -> &'static str {
    i18n::current()
}

/// 结果卡片：读取当前内容
#[tauri::command]
fn get_result(state: State<result_card::ResultState>) -> result_card::CardContent {
    state.0.lock().unwrap().clone()
}

/// 结果卡片：复制识别结果
#[tauri::command]
fn copy_result(state: State<result_card::ResultState>) -> bool {
    let text = state.0.lock().unwrap().text.clone();
    clipboard::set_text(&text)
}

#[tauri::command]
fn close_result(app: AppHandle) {
    result_card::close(&app);
}

/// 提示小窗：读取当前消息
#[tauri::command]
fn get_notice(app: AppHandle) -> String {
    notice::message(&app)
}

#[tauri::command]
fn close_notice(app: AppHandle) {
    notice::close(&app)
}

/// 设置窗口第一次获得焦点时，记下接收键盘输入的进程和权限级别（排查键盘钩子漏掉按键的问题，见 win/hook.rs）。
/// 等一会儿再记：刚获得焦点时，键盘焦点还没落到页面（WebView2 的进程）上
fn log_focus_diagnosis() {
    static LOGGED: AtomicBool = AtomicBool::new(false);
    if !LOGGED.swap(true, Ordering::SeqCst) {
        std::thread::spawn(|| {
            std::thread::sleep(Duration::from_millis(800));
            log::info!("Settings window focused. {}", platform::hook::diagnose());
        });
    }
}

/// 有小窗（结果卡片、词条入库提示、提示小窗）开着时，让键盘钩子用 Esc 关闭它们
pub(crate) fn refresh_dismiss_key() {
    hotkeys::set_dismiss_armed(result_card::is_open() || learned_card::is_open() || notice::is_open());
}

/// 用默认浏览器打开网址（只允许 https）
#[tauri::command]
fn open_url(url: String) {
    if url.starts_with("https://") {
        let _ = platform::open(url);
    }
}

/// 用默认浏览器打开随安装包附带的第三方许可声明（scripts/third-party-licenses.ps1 生成）
#[tauri::command]
fn open_licenses(app: AppHandle) -> Result<(), String> {
    let path = app
        .path()
        .resolve("THIRD_PARTY_LICENSES.html", tauri::path::BaseDirectory::Resource)
        .map_err(|e| e.to_string())?;
    if !path.is_file() {
        return Err(trf!("找不到第三方许可文件：{}", "Third-party license file not found: {}", "サードパーティライセンスのファイルが見つかりません: {}", path.display()));
    }
    platform::open(path).map_err(|e| e.to_string())
}

/// 词库：全部词条
#[tauri::command]
fn get_vocab() -> Vec<vocab::Entry> {
    vocab::all()
}

/// 词库：手动添加（正确写法已存在时合并误识别写法）
#[tauri::command]
fn add_vocab(term: String, variants: Vec<String>) -> Result<(), String> {
    vocab::add_manual(&term, &variants)
}

#[tauri::command]
fn remove_vocab(id: u64) {
    vocab::remove(id)
}

#[tauri::command]
fn remove_vocab_variant(id: u64, text: String) {
    vocab::remove_variant(id, &text)
}

#[tauri::command]
fn set_vocab_enabled(id: u64, enabled: bool) {
    vocab::set_enabled(id, enabled)
}

/// 词条入库提示：读取当前列出的词条
#[tauri::command]
fn get_learned(app: AppHandle) -> Vec<learned_card::Item> {
    learned_card::items(&app)
}

/// 词条入库提示：撤销
#[tauri::command]
fn undo_learned(app: AppHandle) {
    learned_card::undo(&app)
}

#[tauri::command]
fn close_learned(app: AppHandle) {
    learned_card::close(&app)
}

/// 手动检查更新：返回新版本信息，已是最新时返回 None
#[tauri::command]
async fn check_update(app: AppHandle) -> Result<Option<updater::UpdateInfo>, String> {
    updater::check(&app, false).await
}

/// 已知的新版本（设置页打开时显示）
#[tauri::command]
fn get_update(app: AppHandle) -> Option<updater::UpdateInfo> {
    updater::available(&app)
}

/// 一键更新：下载、校验、安装并重启
#[tauri::command]
async fn install_update(app: AppHandle) -> Result<(), String> {
    updater::install(&app).await
}

#[tauri::command]
fn open_folder(path: String) {
    let path = dir_or_default(&path);
    let _ = std::fs::create_dir_all(&path);
    let _ = platform::open(path);
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    use tauri_plugin_log::{RotationStrategy, Target, TargetKind, TimezoneStrategy};

    platform::prepare();
    config::prepare_log_dir();

    let app = tauri::Builder::default()
        // 必须最先注册：重复启动时激活已运行实例的设置窗口
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            // 仅调试版：再次启动时带 --demo-result 弹出结果卡片，用于测试
            #[cfg(debug_assertions)]
            if _args.iter().any(|a| a == "--demo-result") {
                result_card::show(app, i18n::tr("没有检测到输入框，识别结果如下", "No input box detected. Here is the transcript", "入力欄が見つからないため、認識結果をここに表示します"), "这是一段测试文字：今天下午三点的会议改到明天上午十点，请大家提前准备好季度汇报材料。");
                return;
            }
            // 仅调试版：带 --demo-learned 时模拟学到一个词条，弹出入库提示。
            // 延迟一下再弹：等这次启动带出来的临时控制台关掉，小窗记下的「之前的前台窗口」才是真实的
            #[cfg(debug_assertions)]
            if _args.iter().any(|a| a == "--demo-learned") {
                let app = app.clone();
                std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_millis(1500));
                    let c = learn::Correction { wrong: "思维到图".into(), right: "思维导图".into(), score: 1.0 };
                    if let Some(change) = vocab::learn(&c.wrong, &c.right) {
                        learned_card::show(&app, vec![(c, change)]);
                    }
                });
                return;
            }
            open_settings(app)
        }))
        .plugin(
            tauri_plugin_log::Builder::new()
                .clear_targets()
                .target(Target::new(TargetKind::Folder {
                    path: config::log_dir(),
                    file_name: Some("app".into()),
                }))
                .target(Target::new(TargetKind::Stdout))
                .level(log::LevelFilter::Info)
                .max_file_size(1_000_000)
                .rotation_strategy(RotationStrategy::KeepOne)
                .timezone_strategy(TimezoneStrategy::UseLocal)
                .build(),
        )
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            Some(vec![AUTOSTART_ARG]),
        ))
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .manage(updater::UpdaterState::default())
        .manage(Downloads::default())
        .manage(result_card::ResultState::default())
        .manage(learned_card::LearnedState::default())
        .manage(notice::NoticeState::default())
        .invoke_handler(tauri::generate_handler![
            get_settings,
            list_models,
            save_settings,
            capture_hotkey,
            download_model,
            cancel_download,
            open_folder,
            open_licenses,
            open_url,
            get_result,
            get_ui_language,
            copy_result,
            close_result,
            get_vocab,
            add_vocab,
            remove_vocab,
            remove_vocab_variant,
            set_vocab_enabled,
            get_learned,
            undo_learned,
            close_learned,
            get_notice,
            close_notice,
            get_permissions,
            request_permission,
            check_update,
            get_update,
            install_update,
        ])
        .setup(|app| {
            log::info!("=== Aoide {} Started ===", app.package_info().version);
            log::info!("WebView version: {}", tauri::webview_version().unwrap_or_default());
            // whisper.cpp 默认把每个 token 的调试信息打到 stderr，这里统一屏蔽
            whisper_rs::install_logging_hooks();
            platform::setup(app);
            overlay::init();
            // 上次被强退时可能还有程序处于静音
            mute::recover();
            let cfg = Config::load();
            i18n::set_language(&cfg.ui_language);
            let ctl = Controller::new(app.handle().clone(), cfg.clone());
            app.manage(ctl.clone());
            tray::create(app.handle(), &cfg.hotkey)?;
            ctl.start();
            updater::start(app.handle().clone());
            // 用户手动启动时打开主窗口；开机自启时只在托盘运行（除非还没下载模型、缺少系统权限）
            let autostarted = std::env::args().any(|a| a == AUTOSTART_ARG);
            if !autostarted || !ctl.model_downloaded() || !platform::permissions::status().all_granted() {
                // macOS：首次打开时先请求麦克风权限，用户回答后再打开窗口，免得窗口盖住系统的对话框
                #[cfg(target_os = "macos")]
                {
                    let app = app.handle().clone();
                    std::thread::spawn(move || {
                        platform::permissions::request_microphone();
                        open_settings(&app);
                    });
                }
                #[cfg(not(target_os = "macos"))]
                open_settings(app.handle());
            }
            if !ctl.model_downloaded() {
                ctl.notify(i18n::tr("请先下载语音识别模型", "Download a speech recognition model first", "先に音声認識モデルをダウンロードしてください"));
            }
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building Aoide");

    app.run(|_app, event| match event {
        // 关闭设置窗口后继续在托盘运行；只有托盘菜单「退出」才真正退出
        RunEvent::ExitRequested { api, code: None, .. } => api.prevent_exit(),
        // macOS：程序已在运行时再次打开它（访达、启动台、程序坞）
        #[cfg(target_os = "macos")]
        RunEvent::Reopen { .. } => open_settings(_app),
        _ => {}
    });
}
