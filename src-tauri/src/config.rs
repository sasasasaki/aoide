//! 配置：默认值、读写、数据目录

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Config {
    /// 界面语言：auto（跟随系统）/ zh / en
    pub ui_language: String,
    /// 开始/停止录音（单击触发）
    pub hotkey: String,
    /// 开始语音修订；写成「主快捷键+某键」表示按住主快捷键再按该键
    pub revise_hotkey: String,
    /// 结束语音修订；与主快捷键相同时合并为单击逻辑
    pub revise_stop_hotkey: String,
    /// 取消本次任务（录音或识别中）；只在任务进行中拦截该键
    pub cancel_hotkey: String,
    /// whisper 模型 id，见 models::MODELS
    pub model: String,
    /// 模型目录；留空用默认目录
    pub model_dir: String,
    /// 识别语言；auto 为自动检测
    pub language: String,
    /// 单次录音最长秒数
    pub max_duration: u32,
    /// 短于此秒数的录音用静音补齐到该长度再识别
    pub min_duration: u32,
    /// gpu / cpu（仅 whisper 模型；SenseVoice 固定用 CPU）
    pub device: String,
    /// 录音期间静音其他程序的声音，结束后恢复
    pub mute_while_recording: bool,
    /// 启用 LLM 润色（未填 API Base 时自动跳过）
    pub polish: bool,
    /// OpenAI 格式 API base，如 https://api.openai.com/v1
    pub polish_api: String,
    pub polish_api_key: String,
    pub polish_model: String,
    /// 通过 API 参数（reasoning_effort 等）关闭推理模型的思考过程，降低延迟
    pub polish_no_think: bool,
    /// 短于此字符数（去空格）跳过润色直接输出
    pub polish_min_chars: usize,
    /// 录音短于此秒数跳过润色直接输出（润色主要用来整理大段口述，短句不值得花 LLM 的时间）
    pub polish_min_seconds: u32,
    /// 输出语言：留空按原语言输出；填 en / ja / ko / zh / zh-Hant 等时，用 LLM 把口述内容翻译成该语言再输出
    pub translate_to: String,
    /// 光标前后已有文字时，用 LLM 把口述内容接进光标处
    pub insert_smart: bool,
    /// 录音短于此秒数时不走 LLM 插入，直接粘贴转写结果
    pub insert_min_seconds: u32,
    /// 转写结果短于此字符数（去空格）时不走 LLM 插入，直接粘贴
    pub insert_min_chars: usize,
    /// 用词库纠正识别结果：高置信度的误识别直接替换，其余相关词条交给 LLM
    pub vocab_enabled: bool,
    /// 用户手动修改识别结果时，自动把纠正的词存入词库
    pub learn_from_edits: bool,
    /// 定期检查新版本（发现后通知，由用户一键更新）
    pub auto_check_updates: bool,
    /// 自定义润色提示词，{text} 为转写结果；留空使用内置提示词
    pub polish_prompt: String,
    /// 自定义修订提示词，{original} 为选中原文、{instruction} 为修改意见；留空使用内置提示词
    pub revise_prompt: String,
    /// 自定义插入提示词（光标前后已有文字时使用），{before} / {after} 为光标前后文字、{text} 为转写结果；留空使用内置
    pub insert_prompt: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            ui_language: "auto".into(),
            hotkey: "right alt".into(),
            revise_hotkey: "right alt+space".into(),
            revise_stop_hotkey: "right alt".into(),
            cancel_hotkey: "esc".into(),
            model: "sense-voice".into(),
            model_dir: String::new(),
            language: "zh".into(),
            max_duration: 60,
            min_duration: 3,
            device: "gpu".into(),
            mute_while_recording: true,
            polish: true,
            polish_api: String::new(),
            polish_api_key: String::new(),
            polish_model: String::new(),
            polish_no_think: true,
            polish_min_chars: 12,
            polish_min_seconds: 10,
            translate_to: String::new(),
            insert_smart: true,
            insert_min_seconds: 10,
            insert_min_chars: 12,
            vocab_enabled: true,
            learn_from_edits: true,
            auto_check_updates: true,
            polish_prompt: String::new(),
            revise_prompt: String::new(),
            insert_prompt: String::new(),
        }
    }
}

fn env_dir(var: &str) -> PathBuf {
    std::env::var_os(var)
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
}

/// 配置和日志：%APPDATA%\Aoide
pub fn data_dir() -> PathBuf {
    env_dir("APPDATA").join("Aoide")
}

/// 日志：%APPDATA%\Aoide\logs
pub fn log_dir() -> PathBuf {
    data_dir().join("logs")
}

/// 启动时准备日志目录：把旧版本放在数据目录根下的日志搬进来，并把进程的工作目录设到这里。
/// 有些第三方组件会往「当前目录」写文件（比如注入到所有 Vulkan 程序里的游戏叠加层），
/// 工作目录默认是启动位置（安装目录、桌面……），设到日志目录后这些文件也只会落在这里
pub fn prepare_log_dir() {
    let dir = log_dir();
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    if let Ok(entries) = std::fs::read_dir(data_dir()) {
        for e in entries.flatten() {
            let path = e.path();
            if path.is_file() && path.extension().is_some_and(|x| x == "log") {
                let _ = std::fs::rename(&path, dir.join(e.file_name()));
            }
        }
    }
    let _ = std::env::set_current_dir(&dir);
}

/// 默认模型目录：%LOCALAPPDATA%\Aoide\models（模型体积大，不放漫游目录）
pub fn default_model_dir() -> PathBuf {
    env_dir("LOCALAPPDATA").join("Aoide").join("models")
}

fn config_file() -> PathBuf {
    data_dir().join("config.json")
}

impl Config {
    pub fn load() -> Self {
        let mut cfg: Config = std::fs::read_to_string(config_file())
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        // 旧版（Python）用 cuda 表示 GPU
        if cfg.device != "cpu" {
            cfg.device = "gpu".into();
        }
        cfg
    }

    pub fn save(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(data_dir())?;
        let json = serde_json::to_string_pretty(self).expect("config is serializable");
        std::fs::write(config_file(), json)
    }

    pub fn model_dir(&self) -> PathBuf {
        if self.model_dir.trim().is_empty() {
            default_model_dir()
        } else {
            PathBuf::from(self.model_dir.trim())
        }
    }

    pub fn use_gpu(&self) -> bool {
        self.device != "cpu"
    }
}
