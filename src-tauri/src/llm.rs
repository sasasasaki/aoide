//! LLM 润色 / 修订：调用 OpenAI 格式 API

use crate::config::Config;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

/// 内置润色提示词（设置页可自定义）
pub const POLISH_PROMPT: &str = r#"你是文字润色助手。用户通过语音输入了一段文字，可能有停顿、语气词（嗯、啊、那个）、重复、断句。

请润色为连贯、流畅、有逻辑的文字（保持原文所用的语言，不要翻译），遵守以下规则：
1. 去除"嗯""啊""那个""然后然后"等语气词和重复
2. 把断裂的半句连成完整句子
3. 保持原意和语气，不要添加用户没说的内容
4. 不要改变人称和时态
5. 排版：
   - 如果内容包含多个并列的论点/事项/步骤，把每个论点独立成行，行首用"1. ""2. ""3. "编号，让结构一目了然
   - 如果只是围绕单一主题的一段话，就保持为通顺的一段，不要强行拆分
6. 只输出润色后的文本，不要加任何解释、引号或前缀

原文：{text}

润色后："#;

/// 内置修订提示词（设置页可自定义）
pub const REVISE_PROMPT: &str = r#"你是文字修改助手。用户选中了一段文本，并口述了修改意见。请根据用户的修改意见，对原文进行修改。

规则：
1. 严格按照用户的修改意见处理（增、删、改）
2. 保持原文未被提及部分的风格和语气
3. 排版：如果修改后的内容包含多个并列的论点/事项/步骤，把每个论点独立成行，行首用"1. ""2. ""3. "编号；若是单一主题的一段话则保持成段，不要强行拆分
4. 只输出修改后的完整文本，不要加任何解释、引号或前缀

原文：
{original}

修改意见：
{instruction}

修改后："#;

/// 内置插入提示词：光标前后已有文字时，只生成要插入光标处的文字（设置页可自定义）
pub const INSERT_PROMPT: &str = r#"你是文字输入助手。用户正在一个输入框里编辑文本，光标前后已经有内容。用户在光标处通过语音口述了一段话，可能有停顿、语气词（嗯、啊、那个）、重复、断句。

请把口述内容整理成可以直接插入到光标处的文字，遵守以下规则：
1. 只输出要插入到光标处的文字，绝对不要重复、改写或补全光标前后已有的内容
2. 去除语气词和重复，把断裂的半句连成完整的话
3. 让插入的文字与前后文自然衔接：语言、语气、人称、标点与前后文保持一致；光标在句子中间时，不要用句首格式开头，也不要擅自结束句子
4. 保持口述的原意，不要添加用户没说的内容
5. 不用考虑开头和结尾的空格，程序会自动处理
6. 只输出要插入的文字本身，不要加任何解释、引号或前缀

光标前的文字：
<<<{before}>>>

光标后的文字：
<<<{after}>>>

用户口述的内容：
{text}

要插入的文字："#;

/// 翻译提示词：设置了输出语言时代替润色 / 插入，把口述内容整理并翻译成目标语言
const TRANSLATE_PROMPT: &str = r#"你是翻译助手。用户通过语音输入了一段话，可能有停顿、语气词（嗯、啊、那个）、重复、断句。

请把它翻译成{lang}，遵守以下规则：
1. 先理解原意：去掉语气词和重复，把断裂的半句连成完整的话
2. 译文要自然地道，像以{lang}为母语的人直接写的；保持原文的语气和礼貌程度
3. 保持原意，不要添加用户没说的内容
4. 人名、产品名等专有名词用{lang}的通行写法，没有通行写法的保留原文
5. 如果原文已经是{lang}，只整理，不翻译
6. 只输出译文，不要加任何解释、引号或前缀

原文：{text}

译文："#;

/// 词库纠正提示词：识别结果较短、不值得整段润色时，只按词库修正误识别的词
const CORRECT_PROMPT: &str = r#"下面是一段语音识别结果。请只参考用户词库，修正其中被误识别的词；其他内容（包括标点、语气、措辞）一字不改。没有需要修正的就原样输出。只输出结果本身，不要加任何解释、引号或前缀。

识别结果：{text}

修正后："#;

/// 判断用户的一处修改是不是在纠正语音识别错误（读音相似度处于中间地带时才用，见 learn.rs）
const JUDGE_PROMPT: &str = r#"下面是语音识别的输出，以及用户手动修改后的版本。用户把「{wrong}」改成了「{right}」。
请判断这是在纠正语音识别错误（同音字、近音字、专有名词或口音导致的误识别），还是用户主动改写了内容（换了说法、改了意思）。
如果是纠正识别错误，只回答"是"；否则只回答"否"。

识别输出：{old}
修改后：{new}

回答："#;

/// 模型的上下文窗口（prompt + 思考 + 答案的总 token 上限）
const CONTEXT_TOKENS: usize = 8192;

struct Api<'a> {
    base: &'a str,
    key: &'a str,
    model: &'a str,
    no_think: bool,
}

struct HttpError {
    status: Option<u16>,
    message: String,
}

/// 关闭思考的 API 参数，按兼容性从多到少逐级尝试：
/// 0 = reasoning_effort + chat_template_kwargs + enable_thinking，1 = 只带 reasoning_effort，2 = 不带
/// - reasoning_effort: "none"：OpenAI、Gemini（OpenAI 兼容接口）等
/// - chat_template_kwargs.enable_thinking：vLLM / SGLang 部署的 Qwen3 等
/// - enable_thinking：阿里云百炼等
/// 严格的服务端遇到不认识的参数会返回 400，此时降一级重试
const NO_THINK_LEVELS: usize = 3;

/// 每个「API 地址 + 模型」最终能用的级别，避免每次都重试
static NO_THINK_LEVEL: Mutex<Option<HashMap<String, usize>>> = Mutex::new(None);

fn apply_no_think(body: &mut Value, level: usize) {
    if level <= 1 {
        body["reasoning_effort"] = json!("none");
    }
    if level == 0 {
        body["chat_template_kwargs"] = json!({ "enable_thinking": false });
        body["enable_thinking"] = json!(false);
    }
}

impl<'a> Api<'a> {
    fn from(cfg: &'a Config) -> Result<Self, String> {
        let base = cfg.polish_api.trim().trim_end_matches('/');
        if base.is_empty() {
            return Err(crate::i18n::tr("未配置 LLM API（在设置中填写 API 地址）", "LLM API is not configured (set the API base in Settings)").into());
        }
        Ok(Self { base, key: cfg.polish_api_key.trim(), model: cfg.polish_model.trim(), no_think: cfg.polish_no_think })
    }

    fn post(&self, path: &str, payload: &Value) -> Result<Value, HttpError> {
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(120))
            .build()
            .map_err(|e| HttpError { status: None, message: e.to_string() })?;
        let mut req = client.post(format!("{}{path}", self.base)).json(payload);
        if !self.key.is_empty() {
            req = req.bearer_auth(self.key);
        }
        let resp = req.send().map_err(|e| HttpError { status: None, message: e.to_string() })?;
        let status = resp.status();
        if !status.is_success() {
            let body: String = resp.text().unwrap_or_default().chars().take(300).collect();
            return Err(HttpError { status: Some(status.as_u16()), message: format!("HTTP {status}: {body}") });
        }
        resp.json::<Value>().map_err(|e| HttpError { status: None, message: e.to_string() })
    }

    /// 标准 chat 接口；no_think 时附带关闭思考的参数（见 NO_THINK_LEVELS）。
    /// max_tokens 取上下文减去 prompt 估算后的全部余量（思考 token 也计入其中）；
    /// 中文按 1 token/字 保守估算 prompt 占用
    fn call(&self, prompt: &str) -> Result<String, String> {
        self.call_with(None, prompt)
    }

    /// system：附加的说明（如用户词库），作为 system 消息发送，不影响用户自定义的提示词
    fn call_with(&self, system: Option<&str>, prompt: &str) -> Result<String, String> {
        let used = prompt.chars().count() + system.map_or(0, |s| s.chars().count());
        let max_tokens = CONTEXT_TOKENS.saturating_sub(used + 100).max(512);
        let mut messages = Vec::new();
        if let Some(system) = system {
            messages.push(json!({"role": "system", "content": system}));
        }
        messages.push(json!({"role": "user", "content": prompt}));
        let cache_key = format!("{}|{}", self.base, self.model);
        let mut level = if self.no_think {
            NO_THINK_LEVEL.lock().unwrap().as_ref().and_then(|m| m.get(&cache_key).copied()).unwrap_or(0)
        } else {
            NO_THINK_LEVELS - 1
        };
        let result = loop {
            let mut body = json!({
                "model": self.model,
                "messages": messages,
                "temperature": 0.3, "max_tokens": max_tokens, "stream": false,
            });
            apply_no_think(&mut body, level);
            match self.post("/chat/completions", &body) {
                Ok(v) => {
                    if self.no_think {
                        NO_THINK_LEVEL.lock().unwrap().get_or_insert_with(HashMap::new).insert(cache_key, level);
                    }
                    break v;
                }
                Err(e) if matches!(e.status, Some(400 | 422)) && level + 1 < NO_THINK_LEVELS => {
                    log::info!("Thinking-off parameters rejected (level {level}), retrying with fewer: {}", e.message);
                    level += 1;
                }
                Err(e) => return Err(e.message),
            }
        };
        let choice = &result["choices"][0];
        let content = strip_think(choice["message"]["content"].as_str().unwrap_or(""));
        if content.is_empty() {
            return Err(crate::trf!(
                "LLM 返回为空（finish_reason={}，可能思考耗尽了 token 额度）",
                "The LLM returned nothing (finish_reason={}; thinking may have used up the token budget)",
                choice["finish_reason"]
            ));
        }
        Ok(content)
    }
}

/// 有些服务端把思考过程内联在 content 里
fn strip_think(s: &str) -> String {
    let mut out = s.to_string();
    while let (Some(a), Some(b)) = (out.find("<think>"), out.find("</think>")) {
        if b < a {
            break;
        }
        out.replace_range(a..b + "</think>".len(), "");
    }
    out.trim().to_string()
}

/// 自定义提示词为空时用内置的
fn prompt_or_default<'a>(custom: &'a str, default: &'a str) -> &'a str {
    if custom.trim().is_empty() {
        default
    } else {
        custom
    }
}

/// 根据光标前后文，把口述内容整理成要插入光标处的文字；调用失败返回 Err，由调用方决定回退。
/// glossary：相关的用户词库（见 vocab::glossary）
pub fn insert_text(text: &str, before: &str, after: &str, glossary: Option<&str>, cfg: &Config) -> Result<String, String> {
    let prompt = prompt_or_default(&cfg.insert_prompt, INSERT_PROMPT)
        .replace("{before}", before)
        .replace("{after}", after)
        .replace("{text}", text);
    let out = Api::from(cfg)?.call_with(glossary, &prompt)?;
    let out = strip_context_echo(&out, before, after);
    if out.is_empty() {
        return Err("empty insertion".into());
    }
    Ok(fit_spacing(&out, before, after))
}

/// 模型偶尔会把光标前后的文字也抄进输出：去掉开头与光标前文字末尾重合、结尾与光标后文字开头重合的部分。
/// 至少重合 6 个字符才算，避免误删正常内容
fn strip_context_echo(out: &str, before: &str, after: &str) -> String {
    const MIN: usize = 6;
    let mut s: &str = out.trim();
    let b = before.trim_end();
    // 光标前的文字整段出现在开头（模型把全文都输出了）
    if b.chars().count() >= MIN && s.starts_with(b) {
        s = &s[b.len()..];
    } else {
        let tail_starts: Vec<usize> = b.char_indices().map(|(i, _)| i).collect();
        for &i in &tail_starts {
            let tail = &b[i..];
            if tail.chars().count() < MIN {
                break;
            }
            if s.starts_with(tail) {
                s = &s[tail.len()..];
                break;
            }
        }
    }
    let a = after.trim_start();
    if a.chars().count() >= MIN && s.ends_with(a) {
        s = &s[..s.len() - a.len()];
    } else {
        let head_ends: Vec<usize> = a.char_indices().map(|(i, c)| i + c.len_utf8()).rev().collect();
        for &j in &head_ends {
            let head = &a[..j];
            if head.chars().count() < MIN {
                break;
            }
            if s.ends_with(head) {
                s = &s[..s.len() - head.len()];
                break;
            }
        }
    }
    s.trim().to_string()
}

/// 西文单词之间补空格：光标前紧挨字母 / 数字且插入内容以字母 / 数字开头时，前面补一个空格；结尾同理。
/// 中日韩文字不需要空格
pub fn fit_spacing(text: &str, before: &str, after: &str) -> String {
    let word = |c: Option<char>| c.is_some_and(|c| c.is_ascii_alphanumeric());
    let mut out = String::new();
    let before_end = before.chars().last();
    if word(before_end) && word(text.chars().next()) || matches!(before_end, Some(',' | '.' | ';' | ':' | '!' | '?')) && word(text.chars().next()) {
        out.push(' ');
    }
    out.push_str(text);
    if word(text.chars().last()) && word(after.chars().next()) {
        out.push(' ');
    }
    out
}

/// 润色转录文本；调用失败返回 Err，由调用方决定回退
pub fn polish_text(text: &str, glossary: Option<&str>, cfg: &Config) -> Result<String, String> {
    if text.trim().is_empty() {
        return Ok(text.to_string());
    }
    Api::from(cfg)?.call_with(glossary, &prompt_or_default(&cfg.polish_prompt, POLISH_PROMPT).replace("{text}", text))
}

/// 输出语言代码 → 写进提示词的语言名；不认识的代码原样使用（可以直接填语言名）
fn language_name(code: &str) -> &str {
    match code {
        "en" => "English",
        "ja" => "日本語",
        "ko" => "한국어",
        "zh" => "简体中文",
        "zh-Hant" => "繁體中文",
        other => other,
    }
}

/// 把转录文本翻译成 cfg.translate_to 指定的语言；调用失败返回 Err，由调用方决定回退
pub fn translate_text(text: &str, glossary: Option<&str>, cfg: &Config) -> Result<String, String> {
    if text.trim().is_empty() {
        return Ok(text.to_string());
    }
    let prompt = TRANSLATE_PROMPT.replace("{lang}", language_name(cfg.translate_to.trim())).replace("{text}", text);
    Api::from(cfg)?.call_with(glossary, &prompt)
}

/// 根据口述指令修改选中文本
pub fn revise_text(original: &str, instruction: &str, glossary: Option<&str>, cfg: &Config) -> Result<String, String> {
    if instruction.trim().is_empty() {
        return Ok(original.to_string());
    }
    let prompt = prompt_or_default(&cfg.revise_prompt, REVISE_PROMPT)
        .replace("{original}", original)
        .replace("{instruction}", instruction);
    Api::from(cfg)?.call_with(glossary, &prompt)
}

/// 只按词库修正误识别的词，其余一字不改；结果和原文长度相差太多时视为模型改写过头，返回原文
pub fn correct_terms(text: &str, glossary: &str, cfg: &Config) -> Result<String, String> {
    let out = Api::from(cfg)?.call_with(Some(glossary), &CORRECT_PROMPT.replace("{text}", text))?;
    let (a, b) = (text.chars().count() as f32, out.chars().count() as f32);
    if b < a * 0.5 || b > a * 1.5 + 8.0 {
        log::warn!("Term correction rejected (changed too much): {out}");
        return Ok(text.to_string());
    }
    Ok(out)
}

/// 用户把 wrong 改成 right 是不是在纠正识别错误；old / new 为修改前后的整段文字
pub fn judge_correction(wrong: &str, right: &str, old: &str, new: &str, cfg: &Config) -> Result<bool, String> {
    let prompt = JUDGE_PROMPT.replace("{wrong}", wrong).replace("{right}", right).replace("{old}", old).replace("{new}", new);
    let answer = Api::from(cfg)?.call(&prompt)?;
    let answer = answer.trim().to_lowercase();
    Ok(answer.starts_with('是') || answer.starts_with("yes"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 手动运行（会用本机配置里的 LLM API 发几条示例请求）：
    /// cargo test --lib insert_with_real_llm -- --ignored --nocapture
    #[test]
    #[ignore]
    fn insert_with_real_llm() {
        let cfg = Config::load();
        let cases = [
            ("明天的会议安排如下：上午十点在三楼会议室，", "请大家准时参加。", "嗯那个下午两点还有一个产品评审会 就在二楼"),
            ("Hi team, I wanted to let you know that ", " so please plan accordingly.", "um the release is uh pushed to next friday"),
            ("我觉得这个方案", "，但是成本太高了。", "整体上挺不错的"),
            ("Shopping list: milk, eggs,", "", "and also some bread and butter"),
        ];
        for (before, after, spoken) in cases {
            let start = std::time::Instant::now();
            match insert_text(spoken, before, after, None, &cfg) {
                Ok(insert) => println!("[{:?}] insert={insert:?}\n    result={:?}", start.elapsed(), format!("{before}{insert}{after}")),
                Err(e) => println!("error: {e}"),
            }
        }
    }

    /// 手动运行（会用本机配置里的 LLM API）：按词库纠正、判断修改是否在纠正识别错误
    /// cargo test --lib vocab_with_real_llm -- --ignored --nocapture
    #[test]
    #[ignore]
    fn vocab_with_real_llm() {
        let cfg = Config::load();
        let glossary = "用户词库：语音识别经常把下面这些词识别错。如果文字里出现读音相近、明显是误识别的词，请改成这里的正确写法；不相关的词不要改动。\n\
            - 思维导图（曾被误识别为：思维到图）\n- Aoide（曾被误识别为：奥伊德）\n- 前端（曾被误识别为：前段）\n- Tauri\n";
        for text in ["我用思维到图整理了一下需求", "今天试了一下奥一德的语音输入", "前段时间我们讨论了前段的页面怎么改", "这个应用是用 tori 写的", "今天天气不错"] {
            let start = std::time::Instant::now();
            println!("[{:?}] {text}\n    -> {:?}", start.elapsed(), correct_terms(text, glossary, &cfg));
        }
        for (wrong, right, old, new) in [
            ("tori", "Tauri", "这个应用是用 tori 写的", "这个应用是用 Tauri 写的"),
            ("明天下午", "明天晚上", "我们明天下午去公司开会", "我们明天晚上去公司开会"),
            ("会议", "会面", "下周的会议改到周三", "下周的会面改到周三"),
        ] {
            println!("judge {wrong} -> {right}: {:?}", judge_correction(wrong, right, old, new, &cfg));
        }
    }

    #[test]
    fn strips_echoed_context() {
        // 模型把光标前的末尾、光标后的开头也抄了进来
        assert_eq!(strip_context_echo("今天下午三点开会，地点在三楼会议室", "通知：今天下午三点开会，", "请准时参加。"), "地点在三楼会议室");
        assert_eq!(strip_context_echo("地点在三楼会议室，请准时参加。", "开会，", "请准时参加。"), "地点在三楼会议室，");
        // 整段光标前文字都被输出
        assert_eq!(strip_context_echo("Dear team, the meeting moved to Friday", "Dear team,", ""), "the meeting moved to Friday");
        // 重合太短（< 6 字符）不删，避免误伤
        assert_eq!(strip_context_echo("会议改到周五", "会议", ""), "会议改到周五");
        assert_eq!(strip_context_echo("内容不变", "", ""), "内容不变");
    }

    #[test]
    fn fits_spacing_between_words() {
        assert_eq!(fit_spacing("big", "a", "deal"), " big ");
        assert_eq!(fit_spacing("meeting", "the ", ""), "meeting");
        assert_eq!(fit_spacing("Thanks", "Hello,", ""), " Thanks");
        assert_eq!(fit_spacing("地点在三楼", "开会，", "请准时"), "地点在三楼");
        assert_eq!(fit_spacing("API", "调用", "接口"), "API");
    }
}
