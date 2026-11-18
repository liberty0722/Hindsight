//! 聊天窗口的数据层:消息、模型预设、OpenAI-compatible 请求/响应。
//!
//! 请求体构造、响应解析、agent↔模型联动、错误处理都是纯函数,可确定性单测。
//! `send_once` 是唯一的网络调用(阻塞),用本地 mock server 做真实 HTTP 往返
//! 集成测试。GUI 必须把它放进独立系统线程(不能进 GPUI 异步 executor,否则
//! reqwest::blocking 的 Tokio runtime 嵌套会 panic)。

use crate::models::{AgentId, MessageKind, Role, TranscriptMessage};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// 一条聊天消息
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: ChatRole,
    pub content: String,
    /// 发送/接收时刻(epoch ms),仅用于界面显示
    pub timestamp: i64,
    /// 消息种类。Tool = CLI 会话里工具调用/命令输出的实时过程,渲染成等宽卡片,
    /// 且不会拼进 OpenAI-compatible 请求体。旧 JSON 无此字段时为 Normal。
    #[serde(default)]
    pub kind: ChatMessageKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChatMessageKind {
    #[default]
    Normal,
    Tool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChatRole {
    System,
    User,
    Assistant,
}

impl ChatRole {
    fn as_api_str(self) -> &'static str {
        match self {
            ChatRole::System => "system",
            ChatRole::User => "user",
            ChatRole::Assistant => "assistant",
        }
    }
}

/// 一个可选的 LLM 档位(模型名 + 提供方 + 默认 base URL)。
/// 内置预设与「用户自定义模型」共用同一结构:自定义项 `custom=true`,
/// 可带直接填写的 `api_key`;内置项用 `key_env` 读环境变量。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelPreset {
    /// 界面显示名
    pub label: String,
    /// 发给 API 的 model 字段
    pub id: String,
    /// OpenAI-compatible base URL(不含 /chat/completions)
    pub base_url: String,
    /// 环境变量名(读 API key),空串表示不需要 key
    pub key_env: String,
    /// 直接填写的 API key(自定义模型用,优先于 key_env)。旧 JSON 无此字段时为空。
    #[serde(default)]
    pub api_key: String,
    /// 是否用户自定义(可删除);内置预设为 false。旧 JSON 无此字段时为 false。
    #[serde(default)]
    pub custom: bool,
    /// 消耗速率/积分倍率(如 "0.51x"),来自网关 /v1/models 的 credit_rate。
    /// 空串表示未知(该来源不提供倍率),UI 显示占位符而非编造数值。
    #[serde(default)]
    pub credit_rate: String,
    /// 分组标签:"builtin" 内置 / "custom" 自定义 / "official" 接入内网官方 / "custom" 本地。
    /// 旧 JSON 为空按 custom 推断。
    #[serde(default)]
    pub group: String,
    /// 是否走 CodeBuddy CLI 会话通道(true:CLI 登录态+--resume 续会话;
    /// false:OpenAI-compatible HTTP 直连,用于用户自定义 endpoint/Ollama 兜底)。
    /// 旧持久化 JSON 无此字段时为 false,保持旧行为。
    #[serde(default)]
    pub via_cli: bool,
}

/// 内置的常用模型预设。OpenAI-compatible 是各家事实标准:OpenAI / DeepSeek /
/// 通义 / Moonshot / 本地 Ollama / vLLM 都能用同一套请求体。
/// 这些只是「开箱即用的默认值」,并非上限:用户可在设置里添加任意多个自定义模型
/// (`all_models` 把它们追加到内置项之后),下拉随之动态增长。
pub fn builtin_models() -> Vec<ModelPreset> {
    let mut v = vec![
        ModelPreset {
            label: "OpenAI · GPT-4o".into(),
            id: "gpt-4o".into(),
            base_url: "https://api.openai.com/v1".into(),
            key_env: "OPENAI_API_KEY".into(),
            ..Default::default()
        },
        ModelPreset {
            label: "OpenAI · GPT-4o mini".into(),
            id: "gpt-4o-mini".into(),
            base_url: "https://api.openai.com/v1".into(),
            key_env: "OPENAI_API_KEY".into(),
            ..Default::default()
        },
        ModelPreset {
            label: "DeepSeek · Chat".into(),
            id: "deepseek-chat".into(),
            base_url: "https://api.deepseek.com/v1".into(),
            key_env: "DEEPSEEK_API_KEY".into(),
            ..Default::default()
        },
        ModelPreset {
            label: "Anthropic · Claude (proxy)".into(),
            id: "claude-sonnet-4-5".into(),
            base_url: "https://api.anthropic.com/v1".into(),
            key_env: "ANTHROPIC_API_KEY".into(),
            ..Default::default()
        },
        ModelPreset {
            label: "Moonshot · Kimi".into(),
            id: "moonshot-v1-8k".into(),
            base_url: "https://api.moonshot.cn/v1".into(),
            key_env: "MOONSHOT_API_KEY".into(),
            ..Default::default()
        },
        ModelPreset {
            label: "Ollama · local".into(),
            id: "llama3".into(),
            base_url: "http://localhost:11434/v1".into(),
            key_env: String::new(),
            ..Default::default()
        },
        ModelPreset {
            label: "Custom".into(),
            id: String::new(),
            base_url: String::new(),
            key_env: String::new(),
            ..Default::default()
        },
    ];
    for m in &mut v {
        m.group = "builtin".into();
    }
    v
}

/// 把完整的 `/chat/completions` URL 规整成 base_url(去掉路径尾与斜杠)。
/// agent CLI 的 models.json 里每个模型带的是完整 url,发请求统一用 base_url。
pub fn base_url_from_completions(url: &str) -> String {
    let s = url.trim().trim_end_matches('/');
    let s = s.strip_suffix("/chat/completions").unwrap_or(s);
    s.trim_end_matches('/').to_string()
}

/// 把积分倍率数值格式化成界面文案:正数显示 "0.51x"/"1.39x"/"1x";
/// 0(网关里表示免费或浮动倍率)与负数/未知返回空串,UI 显示占位符 "—",
/// 绝不把免费/浮动误显示成 "0x",也不编造数值。
pub fn format_credit_rate(rate: f64) -> String {
    if rate <= 0.0 {
        return String::new();
    }
    let s = format!("{rate:.2}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    format!("{s}x")
}

/// 从 /v1/models 条目里宽松读取积分倍率(数值 credit_rate / rate,或 "x0.51" 字符串)。
fn credit_rate_of(m: &Value) -> String {
    for k in ["credit_rate", "creditRate", "rate", "credit"] {
        if let Some(v) = m.get(k) {
            if let Some(f) = v.as_f64() {
                return format_credit_rate(f);
            }
            if let Some(s) = v.as_str() {
                let t = s.trim().trim_start_matches(['x', 'X']);
                if let Ok(f) = t.parse::<f64>() {
                    return format_credit_rate(f);
                }
            }
        }
    }
    String::new()
}

/// 把 CLI 产品目录 `credits` 的数值格式化成界面文案。
/// 与网关 `/v1/models` 的 `format_credit_rate` 语义不同:那里的 0 含义模糊
/// (免费或浮动倍率),故隐藏;而目录里 `"x0.00 credits"` 是明确定价(免费),
/// 应按用户要求显示 `"0x"`。正数统一成 `"1.33x"`/`"1x"`;负数/无意义值视为未知。
/// 纯函数,可确定性单测。
pub fn format_catalog_credit_rate(rate: f64) -> String {
    if rate < 0.0 {
        return String::new();
    }
    if rate == 0.0 {
        return "0x".to_string();
    }
    format_credit_rate(rate)
}

/// 从 CodeBuddy 缓存的产品目录 JSON(解压后的文本)提取 `id -> 倍率文案`。
/// 目录里每个官方模型带 `credits: "x1.33 credits"` 这类字符串,这正是 CLI
/// 的 /model 界面显示倍率的数据源。解析宽松:去 `x`/`X` 前缀取首个数值,
/// 再走 `format_catalog_credit_rate` 统一成 "1.33x"(免费 "x0.00" 显示 "0x")。
/// 格式变了/缺字段一律返回空表,安全降级。
pub fn parse_codebuddy_catalog_credits(text: &str) -> std::collections::HashMap<String, String> {
    let mut out = std::collections::HashMap::new();
    let v: Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(_) => return out,
    };
    let arr = match v.get("models").and_then(Value::as_array) {
        Some(a) => a,
        None => return out,
    };
    for m in arr {
        let id = match m.get("id").and_then(Value::as_str) {
            Some(s) if !s.is_empty() => s.to_string(),
            _ => continue,
        };
        // catalog 字符串形如 "x1.33 credits":前缀 x 表示「倍」(=界面后缀 x),
        // 但 .parse::<f64>() 只吃纯数字,故取首个 token、去掉首/尾的 x/X
        // 得到数值,再由 format_catalog_credit_rate 按界面约定加回后缀 x。
        // 同时兼容:无前缀 "0.51 credits"、后缀 "1.33x"、以及数值型 credits。
        let num: Option<f64> = match m.get("credits") {
            Some(v) if v.is_number() => v.as_f64(),
            Some(v) => v.as_str().and_then(|s| {
                s.split_whitespace()
                    .next()
                    .unwrap_or("")
                    .trim_start_matches(['x', 'X'])
                    .trim_end_matches(['x', 'X'])
                    .parse::<f64>()
                    .ok()
            }),
            None => None,
        };
        let rate = match num {
            Some(f) => format_catalog_credit_rate(f),
            None => String::new(),
        };
        if !rate.is_empty() {
            out.insert(id, rate);
        }
    }
    out
}

/// best-effort 读 CodeBuddy CLI 缓存的产品目录倍率表。
/// CodeBuddy 把从 copilot.tencent.com 下发的 catalog 以 gzip+base64 存在
/// `~/.codebuddy/local_storage/*.info` 里(文件名哈希、可能多份、可能变格式)。
/// 逐份尝试:是 JSON 字符串、能 base64 解码、能 gzip 解压、且解出含 credits
/// 的模型目录就采纳;任何一步失败都跳过,绝不阻塞启动或抛错。
pub fn read_codebuddy_catalog_credits() -> std::collections::HashMap<String, String> {
    use base64::engine::general_purpose::STANDARD as BASE64;
    use base64::Engine as _;
    use flate2::read::GzDecoder;
    use std::io::Read as _;

    let Some(home) = dirs::home_dir() else {
        return std::collections::HashMap::new();
    };
    let dir = home.join(".codebuddy").join("local_storage");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return std::collections::HashMap::new();
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("info") {
            continue;
        }
        let Ok(raw) = std::fs::read_to_string(&path) else {
            continue;
        };
        // .info 内容可能是带外层引号的 JSON 字符串,也可能是裸文本
        let trimmed = raw.trim();
        let inner: String = if trimmed.starts_with('"') && trimmed.ends_with('"') {
            match serde_json::from_str::<String>(trimmed) {
                Ok(s) => s,
                Err(_) => trimmed[1..trimmed.len() - 1].to_string(),
            }
        } else {
            trimmed.to_string()
        };
        let Ok(bytes) = BASE64.decode(inner.as_bytes()) else {
            continue;
        };
        let mut decoder = GzDecoder::new(&bytes[..]);
        let mut decoded = String::new();
        if decoder.read_to_string(&mut decoded).is_err() {
            continue;
        }
        let table = parse_codebuddy_catalog_credits(&decoded);
        if !table.is_empty() {
            return table;
        }
    }
    std::collections::HashMap::new()
}

/// 解析 agent CLI 的 `models.json`(`{"models":[{id,name,url,apiKey,...}]}`)。
/// 每条变成一个可直接选用的档位:base_url 由 url 规整、apiKey 直接带入、
/// 归到「自定义模型」组。倍率若条目自带则带上,否则留空(网关目录会补齐)。
pub fn parse_agent_models_json(text: &str, _source: &str) -> Vec<ModelPreset> {
    let v: Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let arr = match v.get("models").and_then(Value::as_array) {
        Some(a) => a,
        None => return Vec::new(),
    };
    let mut out = Vec::new();
    for m in arr {
        let id = m
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        if id.is_empty() || id == "default" {
            continue;
        }
        let url = m.get("url").and_then(Value::as_str).unwrap_or("");
        let base_url = base_url_from_completions(url);
        if base_url.is_empty() {
            continue;
        }
        let name = m
            .get("display_name")
            .or_else(|| m.get("name"))
            .and_then(Value::as_str)
            .unwrap_or(&id)
            .trim()
            .to_string();
        let api_key = m
            .get("apiKey")
            .or_else(|| m.get("api_key"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        // 不再把来源前缀塞进 label,避免把本地模型误看成官方
        out.push(ModelPreset {
            label: name,
            id,
            base_url,
            api_key,
            custom: true,
            group: "custom".into(),
            credit_rate: credit_rate_of(m),
            ..Default::default()
        });
    }
    out
}

/// 解析 OpenAI-compatible `/v1/models` 响应。既支持裸 `{"data":[{"id":...}]}`,
/// 也读取网关返回的 `display_name` / `credit_rate` 元数据(用于分组与倍率显示)。
pub fn parse_openai_models_response(
    text: &str,
    base_url: &str,
    api_key: &str,
    source: &str,
) -> Vec<ModelPreset> {
    let v: Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let arr = match v.get("data").and_then(Value::as_array) {
        Some(a) => a,
        None => return Vec::new(),
    };
    let base = base_url.trim().trim_end_matches('/').to_string();
    let mut out = Vec::new();
    for m in arr {
        let id = m
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        if id.is_empty() {
            continue;
        }
        let name = m
            .get("display_name")
            .or_else(|| m.get("name"))
            .and_then(Value::as_str)
            .unwrap_or(&id)
            .trim()
            .to_string();
        let label = if source.is_empty() {
            name
        } else {
            format!("{source} · {name}")
        };
        out.push(ModelPreset {
            label,
            id,
            base_url: base.clone(),
            api_key: api_key.to_string(),
            custom: true,
            group: "custom".into(),
            credit_rate: credit_rate_of(m),
            ..Default::default()
        });
    }
    out
}

/// 用网关目录(带 display_name/credit_rate)按 id 富化已发现的档位:
/// 补上倍率、把 label 换成更干净的显示名。未匹配到的保持原样(倍率留空)。
/// 纯函数,便于单测;网关不在线时目录为空,仅仅是没倍率,不影响可用性。
pub fn enrich_with_catalog(models: &mut [ModelPreset], catalog: &[ModelPreset]) {
    for m in models.iter_mut() {
        if let Some(c) = catalog.iter().find(|c| c.id == m.id) {
            if !c.credit_rate.is_empty() {
                m.credit_rate = c.credit_rate.clone();
            }
            if !c.label.is_empty() {
                m.label = c.label.clone();
            }
        }
    }
}

/// 新会话默认选中的档位下标:优先首个走 CLI 会话的模型(开箱即用),
/// 没有 CLI 模型时退回 0(保持旧行为)。纯函数,可确定性单测。
pub fn default_model_ix(models: &[ModelPreset]) -> usize {
    models.iter().position(|m| m.via_cli).unwrap_or(0)
}

/// 合并三段档位(内置 / 自动发现 / 用户自定义),按 `(id, base_url)` 去重,
/// 先到先得(内置优先)。返回的下拉列表下标稳定地作为会话的 model_ix 依据。
pub fn merge_models(
    builtins: &[ModelPreset],
    discovered: &[ModelPreset],
    custom: &[ModelPreset],
) -> Vec<ModelPreset> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for m in builtins.iter().chain(discovered).chain(custom) {
        let key = (m.id.clone(), m.base_url.clone());
        if seen.insert(key) {
            out.push(m.clone());
        }
    }
    out
}

/// 删除某档位后修正会话的 model_ix:被删项之后的左移一位;正好选中被删项
/// 则回落到第 0 项(内置档,始终存在)。配合删模型时对所有会话批量调用。
pub fn reindex_model_ix(current: usize, removed: usize) -> usize {
    if current > removed {
        current - 1
    } else if current == removed {
        0
    } else {
        current
    }
}

/// 本机 agent CLI 配置目录里可能存在的 `models.json`(来源标签, 相对 home 路径)。
/// 格式不符的文件会被 `parse_agent_models_json` 安全跳过,不会误收。
const AGENT_MODEL_FILES: &[(&str, &str)] = &[
    ("CodeBuddy", ".codebuddy/models.json"),
    ("Claude Code", ".claude/models.json"),
    ("Codex", ".codex/models.json"),
    ("Gemini", ".gemini/models.json"),
];

/// 阻塞:扫描本机各 agent CLI 家目录下的 `models.json`,汇总成可直接选用的档位,
/// 并尝试用其中的网关 url/key 短超时拉一次带 credit_rate 的目录补上倍率与干净
/// 显示名。纯本地回环、超时 1.5s,网关不在线则静默降级(仅无倍率),不抛错。
pub fn discover_agent_cli_models() -> Vec<ModelPreset> {
    let Some(home) = dirs::home_dir() else {
        return Vec::new();
    };
    let mut discovered = Vec::new();
    let mut gateway: Option<(String, String)> = None;
    for (source, rel) in AGENT_MODEL_FILES {
        let path = home.join(rel);
        if let Ok(text) = std::fs::read_to_string(&path) {
            // 取第一条有效条目的 url/key 作为网关目录来源
            if gateway.is_none() {
                if let Ok(v) = serde_json::from_str::<Value>(&text) {
                    if let Some(first) = v.get("models").and_then(Value::as_array).and_then(|a| {
                        a.iter()
                            .find(|m| m.get("id").and_then(Value::as_str) != Some("default"))
                    }) {
                        let url = first.get("url").and_then(Value::as_str).unwrap_or("");
                        let key = first
                            .get("apiKey")
                            .or_else(|| first.get("api_key"))
                            .and_then(Value::as_str)
                            .unwrap_or("");
                        gateway = Some((base_url_from_completions(url), key.to_string()));
                    }
                }
            }
            discovered.extend(parse_agent_models_json(&text, source));
        }
    }
    let mut models = merge_models(&[], &discovered, &[]);
    if let Some((base, key)) = gateway {
        if !base.is_empty() {
            if let Ok(catalog) = fetch_gateway_catalog(&base, &key) {
                enrich_with_catalog(&mut models, &catalog);
            }
        }
    }
    models
}

/// 阻塞:拉网关 `/v1/models` 目录(带 credit_rate/display_name),与 send_once 一样
/// 用 reqwest::blocking,调用方必须放独立系统线程。短超时,失败快速返回。
pub fn fetch_gateway_catalog(base_url: &str, api_key: &str) -> Result<Vec<ModelPreset>, String> {
    validate_base_url(base_url).map_err(|e| format!("invalid base url: {e:?}"))?;
    let base = base_url.trim().trim_end_matches('/');
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_millis(1500))
        .build()
        .map_err(|e| format!("build client: {e}"))?;
    let mut req = client.get(format!("{base}/models"));
    if !api_key.is_empty() {
        req = req.bearer_auth(api_key);
    }
    let resp = req.send().map_err(|e| format!("request failed: {e}"))?;
    let status = resp.status().as_u16();
    let text = resp.text().map_err(|e| format!("read body: {e}"))?;
    if !(200..300).contains(&status) {
        return Err(api_error(status, &text));
    }
    // source 传空:目录项只用于按 id 富化,不直接进下拉
    Ok(parse_openai_models_response(&text, base, api_key, ""))
}

/// 从 `codebuddy --help` 文本里解析 `--model` 支持列表(即 `/model` 显示的那份)。
/// 括号内逗号分隔的 id:无前缀 = 接入内网的官方模型(group="official");
/// `custom-local:` 前缀 = 本地自定义模型(group="custom",label 去掉前缀更干净)。
/// 这就是 CLI 本身的分类,不再需要硬编码公网预设来凑数。纯函数,可确定性单测。
pub fn parse_cli_model_list(help: &str) -> Vec<ModelPreset> {
    let start = match help.find("Currently supported:") {
        Some(i) => i,
        None => return Vec::new(),
    };
    let rest = &help[start..];
    let open = match rest.find('(') {
        Some(i) => i,
        None => return Vec::new(),
    };
    let close = match rest[open + 1..].find(')') {
        Some(i) => open + 1 + i,
        None => return Vec::new(),
    };
    let inner = &rest[open + 1..close];
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for raw in inner.split(',') {
        let id = raw.trim().to_string();
        if id.is_empty() || !seen.insert(id.clone()) {
            continue;
        }
        let (label, group) = if let Some(name) = id.strip_prefix("custom-local:") {
            (name.to_string(), "custom")
        } else {
            (id.clone(), "official")
        };
        out.push(ModelPreset {
            label,
            id,
            group: group.into(),
            // CLI 通道的模型不需要 base_url/api_key,由 CLI 自身登录态处理
            custom: group == "custom",
            via_cli: true,
            ..Default::default()
        });
    }
    out
}

/// 阻塞:运行 `codebuddy --help`,返回 stdout。调用方放独立系统线程,短超时。
pub fn read_cli_model_help() -> Result<String, String> {
    let out = std::process::Command::new("codebuddy")
        .arg("--help")
        .output()
        .map_err(|e| format!("failed to run codebuddy --help: {e}"))?;
    if !out.status.success() {
        return Err(format!("codebuddy --help exited {}", out.status));
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

/// 把倍率表按 id 回填到模型列表里(只填空值,已有倍率不被覆盖)。纯函数,便于单测。
pub fn apply_cli_credits(
    models: &mut [ModelPreset],
    credits: &std::collections::HashMap<String, String>,
) {
    for m in models.iter_mut() {
        if m.credit_rate.is_empty() {
            if let Some(rate) = credits.get(&m.id) {
                m.credit_rate = rate.clone();
            }
        }
    }
}

/// 阻塞:发现 CLI 模型列表(即 /model 列表),分成 official/custom 两组。
/// CLI 不在或未装时返回错误,由调用方决定是否降级到旧 HTTP 预设。
/// 官方模型的倍率来自 CLI 自己缓存的产品目录(credits 字段),按 id 回填;
/// 缓存缺失/格式变化/本地自定义项拿不到倍率时保持空串,UI 显示占位符「—」,
/// 绝不编造数值。best-effort,失败不影响模型列表本身。
pub fn fetch_cli_models() -> Result<Vec<ModelPreset>, String> {
    let help = read_cli_model_help()?;
    let mut models = parse_cli_model_list(&help);
    if models.is_empty() {
        return Err("no models parsed from codebuddy --help".to_string());
    }
    let credits = read_codebuddy_catalog_credits();
    if !credits.is_empty() {
        apply_cli_credits(&mut models, &credits);
    }
    Ok(models)
}

/// 拼出一次 CLI 会话调用的命令行参数(不含可执行文件名)。纯对话版:
/// 默认 `--tools ""` 禁用内置工具,只做对话。要看命令输出请用
/// `build_cli_args_with_tools(..., true)`。
pub fn build_cli_args(model_id: &str, resume: Option<&str>, prompt: &str) -> Vec<String> {
    build_cli_args_with_tools(model_id, resume, prompt, false)
}

/// 拼 CLI 参数。`allow_tools=true` 时:加 `-y`(bypass 权限确认,非交互模式下
/// 工具才真能执行)且不加 `--tools ""` 限制,让 CLI 可调用 Bash/Read 等工具,
/// 其调用与输出会以 stream-json 事件回传并在聊天窗口实时显示。
/// 注意:这会真实执行工具(读写文件/跑命令),属有副作用操作,由 GUI 开关控制。
pub fn build_cli_args_with_tools(
    model_id: &str,
    resume: Option<&str>,
    prompt: &str,
    allow_tools: bool,
) -> Vec<String> {
    let mut a = vec![
        "-p".to_string(),
        "--output-format".to_string(),
        "stream-json".to_string(),
    ];
    if allow_tools {
        a.push("-y".to_string());
    } else {
        a.push("--tools".to_string());
        a.push(String::new());
    }
    a.push("--model".to_string());
    a.push(model_id.to_string());
    if let Some(sid) = resume {
        if !sid.is_empty() {
            a.push("--resume".to_string());
            a.push(sid.to_string());
        }
    }
    a.push(prompt.to_string());
    a
}

/// CLI 会话流式事件:由 `parse_cli_line` 从 `stream-json` 的每行解析出来,
/// GUI 逐条追加到聊天窗口,实现「命令输出实时显示」。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CliEvent {
    /// CLI 会话 id(用于 --resume 续多轮),init/result 行携带
    Session(String),
    /// 模型思考片段(可选展示)
    Thinking(String),
    /// 工具调用(如 Bash 命令):名称 + 命令/参数文本
    ToolUse { name: String, input: String },
    /// 工具执行结果:命令输出(stdout/stderr/exit code)或错误
    ToolResult { output: String, is_error: bool },
    /// 助手正文片段
    Text(String),
    /// 本轮最终结果文本
    Done { reply: String },
}

fn emit_content_events(arr: &Value, tag: &str, out: &mut Vec<CliEvent>) {
    let Some(items) = arr.as_array() else { return };
    for c in items {
        match c.get("type").and_then(Value::as_str) {
            Some("text") => {
                let t = c.get("text").and_then(Value::as_str).unwrap_or("");
                if !t.is_empty() {
                    out.push(CliEvent::Text(t.to_string()));
                }
            }
            Some("thinking") => {
                let t = c.get("thinking").and_then(Value::as_str).unwrap_or("");
                if !t.is_empty() {
                    out.push(CliEvent::Thinking(t.to_string()));
                }
            }
            Some("tool_use") => {
                let name = c
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("tool")
                    .to_string();
                let input = c
                    .get("input")
                    .map(|i| match i {
                        Value::Object(_) => i
                            .get("command")
                            .and_then(Value::as_str)
                            .map(str::to_string)
                            .unwrap_or_else(|| i.to_string()),
                        _ => i.to_string(),
                    })
                    .unwrap_or_default();
                out.push(CliEvent::ToolUse { name, input });
            }
            Some("tool_result") => {
                let is_error =
                    tag == "user" && c.get("is_error").and_then(Value::as_bool).unwrap_or(false);
                let output = c
                    .get("content")
                    .map(|cc| match cc {
                        Value::Array(a) => a
                            .iter()
                            .filter_map(|x| x.get("text").and_then(Value::as_str))
                            .collect::<Vec<_>>()
                            .join("\n"),
                        Value::String(s) => s.clone(),
                        _ => cc.to_string(),
                    })
                    .unwrap_or_default();
                if !output.is_empty() {
                    out.push(CliEvent::ToolResult { output, is_error });
                }
            }
            _ => {}
        }
    }
}

/// 解析一行 `codebuddy -p --output-format stream-json` 输出成实时事件。
/// 覆盖探测到的全部 content 形态:assistant 的 text/thinking/tool_use、
/// user 的 tool_result、result 终止行,以及任意行的 session_id。
/// 纯函数,可确定性单测;非 JSON 或无关行返回空 vec。
pub fn parse_cli_line(line: &str) -> Vec<CliEvent> {
    let line = line.trim();
    if !line.starts_with('{') {
        return Vec::new();
    }
    let v: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let mut out = Vec::new();
    if let Some(s) = v.get("session_id").and_then(Value::as_str) {
        if !s.is_empty() {
            out.push(CliEvent::Session(s.to_string()));
        }
    }
    match v.get("type").and_then(Value::as_str) {
        Some("assistant") | Some("user") => {
            if let Some(c) = v.get("message").and_then(|m| m.get("content")) {
                let tag = v.get("type").and_then(Value::as_str).unwrap_or("");
                emit_content_events(c, tag, &mut out);
            }
        }
        Some("result") => {
            let reply = v
                .get("result")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            out.push(CliEvent::Done { reply });
        }
        _ => {}
    }
    out
}

/// 解析 `codebuddy -p --output-format stream-json` 的输出:逐行取 JSON,
/// 返回 (最终回复文本, CLI session_id)。优先用 `type=result` 的 result 字段;
/// 没有 result 行时用最后一条 assistant 文本兜底。session_id 取 system/init
/// 或任意行携带的字段。纯函数,可确定性单测。
pub fn parse_cli_stream(text: &str) -> Result<(String, Option<String>), String> {
    let mut session_id: Option<String> = None;
    let mut result_text: Option<String> = None;
    let mut last_assistant: Option<String> = None;
    for line in text.lines() {
        let line = line.trim();
        if !line.starts_with('{') {
            continue;
        }
        let v: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if session_id.is_none() {
            if let Some(s) = v.get("session_id").and_then(Value::as_str) {
                session_id = Some(s.to_string());
            }
        }
        match v.get("type").and_then(Value::as_str) {
            Some("result") => {
                if let Some(r) = v.get("result").and_then(Value::as_str) {
                    result_text = Some(r.to_string());
                }
            }
            Some("assistant") => {
                if let Some(content) = v.get("message").and_then(|m| m.get("content")) {
                    if let Some(arr) = content.as_array() {
                        let mut buf = String::new();
                        for c in arr {
                            if c.get("type").and_then(Value::as_str) == Some("text") {
                                if let Some(t) = c.get("text").and_then(Value::as_str) {
                                    buf.push_str(t);
                                }
                            }
                        }
                        if !buf.is_empty() {
                            last_assistant = Some(buf);
                        }
                    }
                }
            }
            _ => {}
        }
    }
    let reply = result_text
        .or(last_assistant)
        .ok_or_else(|| "no assistant reply in CLI output".to_string())?;
    Ok((reply, session_id))
}

/// 阻塞:跑一次 CodeBuddy CLI 会话,返回 (回复文本, CLI session_id)。
/// 供测试/一次性调用;GUI 实时显示请用 `stream_cli_session`。
pub fn run_cli_session(
    model_id: &str,
    resume: Option<&str>,
    prompt: &str,
) -> Result<(String, Option<String>), String> {
    let args = build_cli_args(model_id, resume, prompt);
    let out = std::process::Command::new("codebuddy")
        .args(&args)
        .output()
        .map_err(|e| format!("failed to run codebuddy: {e}"))?;
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(format!(
            "codebuddy exited {}: {}",
            out.status,
            stderr.trim().chars().take(300).collect::<String>()
        ));
    }
    parse_cli_stream(&stdout)
}

/// 流式:跑一次 CodeBuddy CLI 会话,把每行 `stream-json` 解析成 `CliEvent`
/// 经 `std::sync::mpsc::Sender` 实时送出,供 GUI 追加到聊天窗口。
/// 调用方必须放在独立系统线程里(进程会阻塞数秒);tx 在读取期间持续可用。
/// `allow_tools=true` 时带 `-y` 放行工具(命令/文件操作会真实执行,有副作用)。
pub fn stream_cli_session(
    model_id: &str,
    resume: Option<&str>,
    prompt: &str,
    allow_tools: bool,
    tx: std::sync::mpsc::Sender<CliEvent>,
) {
    let args = build_cli_args_with_tools(model_id, resume, prompt, allow_tools);
    let child = std::process::Command::new("codebuddy")
        .args(&args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn();
    let mut child = match child {
        Ok(c) => c,
        Err(e) => {
            let _ = tx.send(CliEvent::ToolResult {
                output: format!("failed to run codebuddy: {e}"),
                is_error: true,
            });
            return;
        }
    };
    if let Some(stdout) = child.stdout.take() {
        use std::io::BufRead;
        let reader = std::io::BufReader::new(stdout);
        for line in reader.lines() {
            let line = match line {
                Ok(l) => l,
                Err(_) => break,
            };
            for ev in parse_cli_line(&line) {
                if tx.send(ev).is_err() {
                    // 接收方已关闭(会话被清空/关闭):尽力杀掉子进程后退出
                    let _ = child.kill();
                    return;
                }
            }
        }
    }
    let status = child.wait();
    match status {
        Ok(s) if !s.success() => {
            let _ = tx.send(CliEvent::ToolResult {
                output: format!("codebuddy exited {s}"),
                is_error: true,
            });
        }
        Err(e) => {
            let _ = tx.send(CliEvent::ToolResult {
                output: format!("codebuddy wait failed: {e}"),
                is_error: true,
            });
        }
        _ => {}
    }
}

/// 阻塞:对 OpenAI-compatible 网关 GET `{base_url}/models`,返回发现的档位。
/// 和 `send_once` 一样用 `reqwest::blocking`,调用方必须放独立系统线程,
/// 不能进 GPUI 异步 executor。短超时,网关不在线时快速失败。
pub fn fetch_openai_models(base_url: &str, api_key: &str) -> Result<Vec<ModelPreset>, String> {
    validate_base_url(base_url).map_err(|e| format!("invalid base url: {e:?}"))?;
    let base = base_url.trim().trim_end_matches('/');
    let url = format!("{base}/models");
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(8))
        .build()
        .map_err(|e| format!("build client: {e}"))?;
    let mut req = client.get(&url);
    if !api_key.is_empty() {
        req = req.bearer_auth(api_key);
    }
    let resp = req.send().map_err(|e| format!("request failed: {e}"))?;
    let status = resp.status().as_u16();
    let text = resp.text().map_err(|e| format!("read body: {e}"))?;
    if !(200..300).contains(&status) {
        return Err(api_error(status, &text));
    }
    Ok(parse_openai_models_response(
        &text, base, api_key, "Gateway",
    ))
}

/// 用户在「连接设置」里填写的自定义覆盖值。空串表示不覆盖、回落到所选预设。
/// 这样 Custom 档(空预设)也能用,任意内置预设也能临时改 endpoint / 模型 / key。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectionOverride {
    /// 自定义 base URL(不含 /chat/completions)
    pub base_url: String,
    /// 自定义 model id
    pub model: String,
    /// 自定义 API key(优先于环境变量)
    pub api_key: String,
}

/// 把预设 + 自定义覆盖解析成实际发送用的 (model, base_url, api_key)。
/// 规则:覆盖值非空则覆盖对应字段;否则回落到预设。key 优先级为
/// 自定义 key > 预设 key_env 环境变量 > 空(本地端点不需要)。
/// `env` 以闭包注入便于确定性单测;公开版 `resolve_connection` 用真实环境变量。
pub fn resolve_connection_with_env<F>(
    preset: &ModelPreset,
    ov: &ConnectionOverride,
    env: F,
) -> (String, String, String)
where
    F: Fn(&str) -> Option<String>,
{
    let model = if ov.model.trim().is_empty() {
        preset.id.clone()
    } else {
        ov.model.trim().to_string()
    };
    let base_url = if ov.base_url.trim().is_empty() {
        preset.base_url.clone()
    } else {
        ov.base_url.trim().to_string()
    };
    let key = if !ov.api_key.is_empty() {
        ov.api_key.clone()
    } else if !preset.api_key.is_empty() {
        // 档位自带的 key(自动发现的 agent CLI 模型 / 自定义模型),优先于环境变量
        preset.api_key.clone()
    } else if !preset.key_env.is_empty() {
        env(&preset.key_env).unwrap_or_default()
    } else {
        String::new()
    };
    (model, base_url, key)
}

/// `resolve_connection_with_env` 的真实环境版本(GUI 调用)。
pub fn resolve_connection(
    preset: &ModelPreset,
    ov: &ConnectionOverride,
) -> (String, String, String) {
    resolve_connection_with_env(preset, ov, |k| std::env::var(k).ok())
}

/// 从一条用户消息生成会话标签标题:取首行、去首尾空白、按字符数截断到 24,
/// 超出加省略号。全空白返回空串(GUI 用「新对话」兜底)。
pub fn derive_title(text: &str) -> String {
    let first = text.lines().next().unwrap_or("").trim();
    let chars: Vec<char> = first.chars().collect();
    if chars.is_empty() {
        return String::new();
    }
    if chars.len() > 24 {
        let mut s: String = chars[..24].iter().collect();
        s.push('…');
        s
    } else {
        first.to_string()
    }
}

/// 把只读历史转录映射成可续聊的自建会话消息。
/// 只保留正文(MessageKind::Text)且非空文本;Meta / CompactSummary 等
/// 与工具过程、thinking 由 ToolCallView/thinking 字段另存,不进聊天上下文。
/// Role::User/Assistant/System 与 ChatRole 一一对应,timestamp 缺省 0
/// (仅用于界面显示,不参与请求)。纯函数,可确定性单测。
pub fn transcript_to_chat_messages(transcript: &[TranscriptMessage]) -> Vec<ChatMessage> {
    transcript
        .iter()
        .filter(|m| m.kind == MessageKind::Text && !m.text.trim().is_empty())
        .map(|m| {
            let role = match m.role {
                Role::User => ChatRole::User,
                Role::Assistant => ChatRole::Assistant,
                Role::System => ChatRole::System,
            };
            ChatMessage {
                role,
                content: m.text.clone(),
                timestamp: m.timestamp.unwrap_or(0),
                kind: ChatMessageKind::Normal,
            }
        })
        .collect()
}

/// 一个聊天会话的数据(不含 GPUI 运行时字段,故可序列化、可确定性单测)。
/// 多会话标签的真相全在这里;GUI 只为滚动条另存一组平行 `ScrollHandle`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatSessionData {
    pub id: u64,
    /// 由首条消息生成;空串表示「新对话」
    pub title: String,
    pub messages: Vec<ChatMessage>,
    pub agent: AgentId,
    /// `builtin_models()` 的下标
    pub model_ix: usize,
    /// 切走标签时存下的输入框草稿
    pub draft: String,
    /// 丢弃在途回复用;清空/关闭时自增
    pub generation: u64,
    pub sending: bool,
    pub error: Option<String>,
    /// CodeBuddy CLI 会话 id(走 CLI 会话通道时用于 --resume 续多轮)。
    /// 旧持久化 JSON 无此字段时反序列化为 None;HTTP 直连会话保持 None。
    #[serde(default)]
    pub cli_session_id: Option<String>,
    /// 若本会话是从某条只读历史会话「派生」出来的续聊窗口,记下那条历史的 key。
    /// 原历史/磁盘数据全程只读,续聊只发生在这个 Hindsight 自建副本里;同一条
    /// 历史再次「在聊天窗口打开」时按此去重、切回已有副本。旧 JSON 无此字段为 None。
    #[serde(default)]
    pub source_key: Option<String>,
}

impl ChatSessionData {
    pub fn new(id: u64, agent: AgentId, model_ix: usize) -> Self {
        Self {
            id,
            title: String::new(),
            messages: Vec::new(),
            agent,
            model_ix,
            draft: String::new(),
            generation: 0,
            sending: false,
            error: None,
            cli_session_id: None,
            source_key: None,
        }
    }
}

/// 关闭会话的结果,告诉 GUI 要不要同步删掉对应滚动条。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseResult {
    /// 真的移除了一个会话,值是它在 Vec 里的原下标
    Removed(usize),
    /// 只剩最后一个:改为清空重置,不增删
    Cleared,
}

/// 多会话集合的纯状态机(可脱离 GUI 单测)。至少保持 1 个会话。
/// 可序列化:GUI 把它整体落盘,重启后恢复标签、消息、草稿、模型。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSet {
    pub sessions: Vec<ChatSessionData>,
    pub active_id: u64,
    pub next_id: u64,
}

/// 单个 Workbench 允许的最大会话标签数,防止 UI 与内存无限膨胀。
pub const MAX_SESSIONS: usize = 24;

impl SessionSet {
    pub fn new(agent: AgentId, model_ix: usize) -> Self {
        Self {
            sessions: vec![ChatSessionData::new(0, agent, model_ix)],
            active_id: 0,
            next_id: 1,
        }
    }

    pub fn index_of(&self, id: u64) -> Option<usize> {
        self.sessions.iter().position(|s| s.id == id)
    }

    pub fn active_index(&self) -> usize {
        self.index_of(self.active_id).unwrap_or(0)
    }

    pub fn active(&self) -> &ChatSessionData {
        &self.sessions[self.active_index()]
    }

    pub fn active_mut(&mut self) -> &mut ChatSessionData {
        let i = self.active_index();
        &mut self.sessions[i]
    }

    pub fn get_mut(&mut self, id: u64) -> Option<&mut ChatSessionData> {
        let i = self.index_of(id)?;
        Some(&mut self.sessions[i])
    }

    /// 新建:沿用当前会话的 agent/model,空消息,切为 active。
    /// 返回 (新 id, 新下标),供 GUI 同步 push 一个滚动条。
    pub fn add(&mut self) -> (u64, usize) {
        let (agent, model_ix) = (self.active().agent, self.active().model_ix);
        let id = self.next_id;
        self.next_id += 1;
        self.sessions
            .push(ChatSessionData::new(id, agent, model_ix));
        self.active_id = id;
        (id, self.sessions.len() - 1)
    }

    /// 找到第一个「空会话」(title 空且没有任何消息)的 id。
    /// 点「+」新建时优先复用它,避免连点堆出一排同名空白标签。
    /// 纯函数,可确定性单测。
    pub fn find_empty_session(&self) -> Option<u64> {
        self.sessions
            .iter()
            .find(|s| s.title.is_empty() && s.messages.is_empty())
            .map(|s| s.id)
    }

    /// 从一条只读历史会话派生/复用一个「续聊窗口」(自建会话)。
    /// 同一条历史(source_key 相同)已派生过就切到它、返回 (id, false),不重复堆标签;
    /// 否则新建一个带标题、导入的转录消息、agent 与来源标记的会话,返回 (id, true)。
    /// 原历史/磁盘全程只读,续聊只发生在这个 Hindsight 自建副本里。
    /// 纯状态机,可确定性单测。
    pub fn open_from_source(
        &mut self,
        source_key: &str,
        title: String,
        messages: Vec<ChatMessage>,
        agent: AgentId,
    ) -> (u64, bool) {
        if let Some(existing) = self
            .sessions
            .iter()
            .find(|s| s.source_key.as_deref() == Some(source_key))
        {
            let id = existing.id;
            self.active_id = id;
            return (id, false);
        }
        let model_ix = self.active().model_ix;
        let id = self.next_id;
        self.next_id += 1;
        self.sessions.push(ChatSessionData {
            id,
            title,
            messages,
            agent,
            model_ix,
            draft: String::new(),
            generation: 0,
            sending: false,
            error: None,
            cli_session_id: None,
            source_key: Some(source_key.to_string()),
        });
        self.active_id = id;
        (id, true)
    }

    /// 切换 active;目标不存在或已是当前则返回 false(调用方据此跳过输入框同步)。
    pub fn switch(&mut self, id: u64) -> bool {
        if self.index_of(id).is_none() || self.active_id == id {
            return false;
        }
        self.active_id = id;
        true
    }

    /// 关闭会话。只剩一个时改为清空重置(不清空 id 体系);否则移除,并在
    /// 关掉 active 时把 active 调到相邻会话。被移除者 generation 自增以丢弃在途回复。
    pub fn close(&mut self, id: u64) -> CloseResult {
        let Some(idx) = self.index_of(id) else {
            return CloseResult::Cleared;
        };
        if self.sessions.len() == 1 {
            let s = &mut self.sessions[0];
            s.generation = s.generation.wrapping_add(1);
            s.messages.clear();
            s.error = None;
            s.sending = false;
            s.title.clear();
            s.draft.clear();
            return CloseResult::Cleared;
        }
        self.sessions[idx].generation = self.sessions[idx].generation.wrapping_add(1);
        self.sessions.remove(idx);
        if self.active_id == id {
            let new_ix = idx.min(self.sessions.len() - 1);
            self.active_id = self.sessions[new_ix].id;
        }
        CloseResult::Removed(idx)
    }

    /// 从持久化文件恢复后修正不变量,防止脏数据卡死 UI 或越界:
    /// 至少 1 个会话;active_id 指向现存会话;next_id 大于所有现存 id;
    /// 清空所有 sending/error(在途请求不可能跨重启存活,否则按钮永久 disabled);
    /// model_ix 收敛到 [0, model_count);会话数截断到 MAX_SESSIONS。
    /// GUI 恢复后据此重建与 sessions 等长的平行滚动条数组。
    pub fn sanitize(&mut self, model_count: usize) {
        if self.sessions.is_empty() {
            self.sessions
                .push(ChatSessionData::new(0, AgentId::ClaudeCode, 0));
        }
        if self.sessions.len() > MAX_SESSIONS {
            self.sessions.truncate(MAX_SESSIONS);
        }
        let max_id = self.sessions.iter().map(|s| s.id).max().unwrap_or(0);
        self.next_id = self.next_id.max(max_id.saturating_add(1));
        for s in &mut self.sessions {
            s.sending = false;
            s.error = None;
            s.model_ix = if model_count == 0 {
                0
            } else {
                s.model_ix.min(model_count - 1)
            };
        }
        if self.index_of(self.active_id).is_none() {
            self.active_id = self.sessions[0].id;
        }
    }
}

/// agent 角色提示词:选不同 agent 时,系统提示据此变化(轻量"关联",不做工具调用)
pub fn agent_system_prompt(agent: AgentId) -> String {
    let name = agent.display_name();
    format!(
        "You are acting as the {name} coding agent inside Hindsight, a session-review desktop app. \
         Answer concisely and helpfully. The user is reviewing past agent sessions and may ask you \
         to explain code, summarize a change, or suggest next steps."
    )
}

/// 把界面上的消息序列(不含 system)拼成发给 API 的 messages,前面插入系统提示。
pub fn build_request_body(model: &str, agent: AgentId, history: &[ChatMessage]) -> Value {
    let mut msgs: Vec<Value> = Vec::with_capacity(history.len() + 1);
    msgs.push(json!({
        "role": "system",
        "content": agent_system_prompt(agent),
    }));
    for m in history {
        if m.kind != ChatMessageKind::Normal {
            continue; // 工具过程只在 GUI 展示,不进 LLM 上下文
        }
        msgs.push(json!({
            "role": m.role.as_api_str(),
            "content": m.content,
        }));
    }
    json!({
        "model": model,
        "messages": msgs,
        "stream": false,
    })
}

/// 自定义 base URL 的校验错误(稳定标识,便于 GUI 按类型本地化文案)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BaseUrlError {
    Empty,
    Parse,
    UnsupportedScheme,
    MissingHost,
}

/// 严格校验自定义 endpoint:必须是合法 URL、scheme 为 http/https、且带 host。
/// 防止把 Bearer API key 发给畸形地址或 file:// 等非网络目标。
pub fn validate_base_url(base_url: &str) -> Result<(), BaseUrlError> {
    let b = base_url.trim();
    if b.is_empty() {
        return Err(BaseUrlError::Empty);
    }
    let url = reqwest::Url::parse(b).map_err(|_| BaseUrlError::Parse)?;
    match url.scheme() {
        "http" | "https" => {}
        _ => return Err(BaseUrlError::UnsupportedScheme),
    }
    if url.host_str().is_none() {
        return Err(BaseUrlError::MissingHost);
    }
    Ok(())
}

/// 拼出完整的 chat/completions URL(base_url 末尾的 / 容忍)
pub fn completions_url(base_url: &str) -> Option<String> {
    let b = base_url.trim();
    if b.is_empty() {
        return None;
    }
    Some(format!("{}/chat/completions", b.trim_end_matches('/')))
}

/// 解析 OpenAI-compatible 的响应,取出助手文本。
/// 兼容 `choices[0].message.content` 为字符串或数组(部分代理返回分块)、
/// content 为 null、以及旧式 completion 的 `choices[0].text`。
pub fn parse_response(body: &Value) -> Result<String, String> {
    let choices = body
        .get("choices")
        .and_then(Value::as_array)
        .ok_or_else(|| "response has no choices".to_string())?;
    let first = choices
        .first()
        .ok_or_else(|| "response choices is empty".to_string())?;
    // 现代 chat 格式:choices[0].message.content
    if let Some(content) = first.pointer("/message/content") {
        match content {
            Value::String(s) => return Ok(s.clone()),
            Value::Array(parts) => {
                let mut out = String::new();
                for p in parts {
                    if let Some(t) = p.get("text").and_then(Value::as_str) {
                        out.push_str(t);
                    } else if let Some(t) = p.as_str() {
                        out.push_str(t);
                    }
                }
                if out.is_empty() {
                    return Err("message.content array has no text".to_string());
                }
                return Ok(out);
            }
            _ => {}
        }
    }
    // 旧 completion 格式 / content 为 null 时回落到 choices[0].text
    if let Some(t) = first.get("text").and_then(Value::as_str) {
        return Ok(t.to_string());
    }
    Err("response has no assistant text".to_string())
}

/// 真正发一次 OpenAI-compatible 请求(阻塞),返回解析后的助手正文或错误文案。
/// 用 `reqwest::blocking`,调用方必须放在独立系统线程里(不能进 GPUI 异步
/// executor,否则 runtime 嵌套 panic)。抽出到 core 是为了能用本地 mock server
/// 做真实 HTTP 往返的集成测试。
pub fn send_once(url: &str, api_key: &str, body_json: &str) -> Result<String, String> {
    // 纵深防御:即使 GUI 漏校验,也不把 key 发给非 http(s)/畸形地址。
    let parsed_url = reqwest::Url::parse(url).map_err(|e| format!("invalid url: {e}"))?;
    if !matches!(parsed_url.scheme(), "http" | "https") {
        return Err(format!("unsupported url scheme: {}", parsed_url.scheme()));
    }
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .build()
        .map_err(|e| format!("build client: {e}"))?;
    let mut req = client.post(url).header("Content-Type", "application/json");
    if !api_key.is_empty() {
        req = req.bearer_auth(api_key);
    }
    let resp = req
        .body(body_json.to_string())
        .send()
        .map_err(|e| format!("request failed: {e}"))?;
    let status = resp.status().as_u16();
    let text = resp.text().map_err(|e| format!("read body: {e}"))?;
    if !(200..300).contains(&status) {
        return Err(api_error(status, &text));
    }
    let value: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| format!("bad json: {e}"))?;
    parse_response(&value)
}

/// 把 API 返回的错误体转成可读文案(HTTP 非 2xx 时)
pub fn api_error(status: u16, body: &str) -> String {
    let parsed: Option<Value> = serde_json::from_str(body).ok();
    let msg = parsed
        .as_ref()
        .and_then(|v| v.pointer("/error/message"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| {
            let s = body.trim();
            if s.is_empty() {
                format!("HTTP {status}")
            } else {
                let cut: String = s.chars().take(200).collect();
                format!("HTTP {status}: {cut}")
            }
        });
    msg
}

/// 聊天状态机的一次输入:用户点发送时的动作结果(纯函数,便于测试)
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SendAction {
    /// 输入为空,忽略
    Empty,
    /// 没配好模型/base_url,返回提示文案
    Unconfigured(String),
    /// 可以发送,带上清洗后的文本
    Ready(String),
}

pub fn decide_send(input: &str, model: &str, base_url: &str) -> SendAction {
    let text = input.trim().to_string();
    if text.is_empty() {
        return SendAction::Empty;
    }
    if model.trim().is_empty() || base_url.trim().is_empty() {
        return SendAction::Unconfigured(
            "Set a model and API base URL in chat settings first.".to_string(),
        );
    }
    SendAction::Ready(text)
}

/// 左侧选中的 agent 决定聊天页默认 agent;聊天页手动切换后,反向给左侧的联动值。
/// None 表示左侧"全部",此时聊天页用一个兜底 agent。
pub fn linked_agent(sidebar: Option<AgentId>, fallback: AgentId) -> AgentId {
    sidebar.unwrap_or(fallback)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_models_are_well_formed() {
        let m = builtin_models();
        assert!(m.len() >= 6);
        assert_eq!(m.last().unwrap().label, "Custom");
        for p in &m {
            assert!(!p.label.is_empty());
            // 除 Custom 外都得有 id 与 url
            if p.label != "Custom" {
                assert!(!p.id.is_empty(), "{}", p.label);
                assert!(p.base_url.starts_with("http"), "{}", p.label);
            }
        }
    }

    #[test]
    fn request_body_has_system_and_history_in_order() {
        let h = vec![
            ChatMessage {
                role: ChatRole::User,
                content: "hi".into(),
                timestamp: 1,
                kind: ChatMessageKind::Normal,
            },
            ChatMessage {
                role: ChatRole::Assistant,
                content: "hello".into(),
                timestamp: 2,
                kind: ChatMessageKind::Normal,
            },
            ChatMessage {
                role: ChatRole::User,
                content: "explain".into(),
                timestamp: 3,
                kind: ChatMessageKind::Normal,
            },
        ];
        let body = build_request_body("gpt-4o", AgentId::Codex, &h);
        assert_eq!(body["model"], "gpt-4o");
        assert_eq!(body["stream"], false);
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 4);
        assert_eq!(msgs[0]["role"], "system");
        assert!(msgs[0]["content"].as_str().unwrap().contains("Codex"));
        assert_eq!(msgs[1]["role"], "user");
        assert_eq!(msgs[1]["content"], "hi");
        assert_eq!(msgs[2]["role"], "assistant");
        assert_eq!(msgs[3]["content"], "explain");
    }

    #[test]
    fn agent_prompt_varies_by_agent() {
        let a = agent_system_prompt(AgentId::ClaudeCode);
        let b = agent_system_prompt(AgentId::Codex);
        assert!(a.contains("Claude Code"));
        assert!(b.contains("Codex"));
        assert_ne!(a, b);
    }

    #[test]
    fn completions_url_normalizes_trailing_slash() {
        assert_eq!(
            completions_url("https://api.x.com/v1/"),
            Some("https://api.x.com/v1/chat/completions".to_string())
        );
        assert_eq!(completions_url(""), None);
        assert_eq!(
            completions_url("http://localhost:11434/v1"),
            Some("http://localhost:11434/v1/chat/completions".to_string())
        );
    }

    #[test]
    fn parse_standard_response() {
        let v = json!({
            "choices": [{"message": {"role":"assistant","content":"Done."}}]
        });
        assert_eq!(parse_response(&v).unwrap(), "Done.");
    }

    #[test]
    fn parse_array_content_response() {
        let v = json!({
            "choices": [{"message": {"content": [
                {"type":"text","text":"Part 1. "},
                {"type":"text","text":"Part 2."}
            ]}}]
        });
        assert_eq!(parse_response(&v).unwrap(), "Part 1. Part 2.");
    }

    #[test]
    fn parse_bad_responses_are_errors() {
        assert!(parse_response(&json!({})).is_err());
        assert!(parse_response(&json!({"choices":[]})).is_err());
        assert!(parse_response(&json!({"choices":[{"message":{}}]})).is_err());
    }

    #[test]
    fn api_error_extracts_nested_message() {
        let body = r#"{"error":{"message":"invalid api key"}}"#;
        assert_eq!(api_error(401, body), "invalid api key");
        assert_eq!(api_error(500, "gateway down"), "HTTP 500: gateway down");
        assert_eq!(api_error(502, ""), "HTTP 502");
    }

    #[test]
    fn decide_send_states() {
        assert_eq!(decide_send("  ", "gpt-4o", "https://x"), SendAction::Empty);
        assert_eq!(
            decide_send("hello", "", "https://x"),
            SendAction::Unconfigured(
                "Set a model and API base URL in chat settings first.".to_string()
            )
        );
        assert_eq!(
            decide_send("  hi  ", "gpt-4o", "https://x"),
            SendAction::Ready("hi".to_string())
        );
    }

    #[test]
    fn linked_agent_prefers_sidebar() {
        assert_eq!(
            linked_agent(Some(AgentId::Gemini), AgentId::Codex),
            AgentId::Gemini
        );
        assert_eq!(linked_agent(None, AgentId::Codex), AgentId::Codex);
    }

    // ---- 自定义连接覆盖解析 ----

    fn openai() -> ModelPreset {
        ModelPreset {
            label: "OpenAI".into(),
            id: "gpt-4o".into(),
            base_url: "https://api.openai.com/v1".into(),
            key_env: "OPENAI_API_KEY".into(),
            ..Default::default()
        }
    }

    #[test]
    fn resolve_connection_falls_back_to_preset() {
        let env = |k: &str| match k {
            "OPENAI_API_KEY" => Some("from-env".to_string()),
            _ => None,
        };
        let (m, u, k) = resolve_connection_with_env(&openai(), &ConnectionOverride::default(), env);
        assert_eq!(m, "gpt-4o");
        assert_eq!(u, "https://api.openai.com/v1");
        assert_eq!(k, "from-env");
    }

    #[test]
    fn resolve_connection_override_wins() {
        let ov = ConnectionOverride {
            base_url: " http://localhost:8080/v1 ".into(),
            model: " qwen2.5-72b ".into(),
            api_key: "sk-custom".into(),
        };
        let (m, u, k) = resolve_connection_with_env(&openai(), &ov, |_| Some("from-env".into()));
        assert_eq!(m, "qwen2.5-72b"); // trim 后覆盖
        assert_eq!(u, "http://localhost:8080/v1");
        assert_eq!(k, "sk-custom"); // 自定义 key 优先于环境变量
    }

    #[test]
    fn resolve_connection_custom_preset_with_blank_fields_is_unconfigured() {
        let custom = ModelPreset {
            label: "Custom".into(),
            id: String::new(),
            base_url: String::new(),
            key_env: String::new(),
            ..Default::default()
        };
        let (m, u, k) =
            resolve_connection_with_env(&custom, &ConnectionOverride::default(), |_| None);
        assert!(m.is_empty());
        assert!(u.is_empty());
        assert!(k.is_empty());
        // 空 model/url 会被 decide_send 判为未配置
        assert_eq!(
            decide_send("hi", &m, &u),
            SendAction::Unconfigured(
                "Set a model and API base URL in chat settings first.".to_string()
            )
        );
    }

    #[test]
    fn resolve_connection_custom_becomes_ready_when_filled() {
        let custom = ModelPreset {
            label: "Custom".into(),
            id: String::new(),
            base_url: String::new(),
            key_env: String::new(),
            ..Default::default()
        };
        let ov = ConnectionOverride {
            base_url: "https://llm-proxy.woa.com/v1".into(),
            model: "hunyuan-pro".into(),
            api_key: "token".into(),
        };
        let (m, u, k) = resolve_connection_with_env(&custom, &ov, |_| None);
        assert_eq!(m, "hunyuan-pro");
        assert_eq!(u, "https://llm-proxy.woa.com/v1");
        assert_eq!(k, "token");
        assert_eq!(
            decide_send("hi", &m, &u),
            SendAction::Ready("hi".to_string())
        );
    }

    // ---- 标题生成 ----

    #[test]
    fn derive_title_first_line_truncate_and_blank() {
        assert_eq!(derive_title(""), "");
        assert_eq!(derive_title("   \n\n"), "");
        assert_eq!(derive_title("  解释一下这段数据流  "), "解释一下这段数据流");
        assert_eq!(derive_title("第一行\n第二行"), "第一行");
        let long = "字".repeat(30);
        let t = derive_title(&long);
        assert_eq!(t.chars().count(), 25); // 24 + 省略号
        assert!(t.ends_with('…'));
    }

    // ---- 多会话状态机 ----

    #[test]
    fn session_set_always_keeps_one_and_derives_ids() {
        let mut set = SessionSet::new(AgentId::Codex, 2);
        assert_eq!(set.sessions.len(), 1);
        assert_eq!(set.active_id, 0);
        assert_eq!(set.active().model_ix, 2);
        assert_eq!(set.active().agent, AgentId::Codex);

        let (id1, ix1) = set.add();
        assert_eq!((id1, ix1), (1, 1));
        let (id2, ix2) = set.add();
        assert_eq!((id2, ix2), (2, 2));
        assert_eq!(set.active_id, 2);
        assert_eq!(set.sessions.len(), 3);
    }

    #[test]
    fn find_empty_session_reuses_existing_blank_instead_of_piling_up() {
        let mut set = SessionSet::new(AgentId::Codex, 0);
        // 初始唯一的空会话应被找到:点 + 不该再新建一个
        assert_eq!(set.find_empty_session(), Some(0));
        set.switch(set.find_empty_session().unwrap());
        assert_eq!(set.sessions.len(), 1); // 复用,不增长

        // 给当前会话发首条消息:title 与 messages 都非空 -> 不再是空会话
        set.active_mut().title = derive_title("第一个问题");
        set.active_mut().messages.push(ChatMessage {
            role: ChatRole::User,
            content: "第一个问题".into(),
            timestamp: 0,
            kind: ChatMessageKind::Normal,
        });
        assert_eq!(set.find_empty_session(), None);

        // 此时点 + 才真正新建一个空会话
        let (new_id, _) = set.add();
        assert_eq!(set.sessions.len(), 2);
        assert_eq!(set.find_empty_session(), Some(new_id));
        // 再点 + 仍复用这个空会话,会话数不再涨 -> 不会堆出一排同名空标签
        set.switch(set.find_empty_session().unwrap());
        assert_eq!(set.sessions.len(), 2);
    }

    fn transcript_msg(role: Role, kind: MessageKind, text: &str) -> TranscriptMessage {
        TranscriptMessage {
            seq: 0,
            role,
            kind,
            text: text.to_string(),
            truncated: false,
            tool_calls: Vec::new(),
            thinking: None,
            timestamp: Some(123),
            model: None,
            images: Vec::new(),
        }
    }

    #[test]
    fn transcript_to_chat_messages_keeps_only_text_and_maps_roles() {
        let transcript = vec![
            transcript_msg(Role::User, MessageKind::Text, "问一下"),
            transcript_msg(Role::Assistant, MessageKind::Text, "回答一下"),
            transcript_msg(Role::System, MessageKind::Text, "系统提示"),
            transcript_msg(Role::User, MessageKind::Meta, "元信息不进上下文"),
            transcript_msg(Role::Assistant, MessageKind::CompactSummary, "压缩摘要不进"),
            transcript_msg(Role::User, MessageKind::Text, "   "), // 空文本过滤
        ];
        let got = transcript_to_chat_messages(&transcript);
        assert_eq!(got.len(), 3);
        assert_eq!(got[0].role, ChatRole::User);
        assert_eq!(got[0].content, "问一下");
        assert_eq!(got[0].timestamp, 123);
        assert_eq!(got[0].kind, ChatMessageKind::Normal);
        assert_eq!(got[1].role, ChatRole::Assistant);
        assert_eq!(got[2].role, ChatRole::System);
    }

    #[test]
    fn open_from_source_derives_once_and_reuses_the_same_window() {
        let mut set = SessionSet::new(AgentId::Codebuddy, 0);
        let msgs =
            transcript_to_chat_messages(&[transcript_msg(Role::User, MessageKind::Text, "hi")]);
        // 首次:新建派生窗口,带 source_key / 标题 / 消息 / agent
        let (id1, is_new) = set.open_from_source(
            "codebuddy:proj:abc",
            "排查卡顿".into(),
            msgs.clone(),
            AgentId::Codebuddy,
        );
        assert!(is_new);
        assert_eq!(set.sessions.len(), 2);
        let s = set.get_mut(id1).unwrap();
        assert_eq!(s.source_key.as_deref(), Some("codebuddy:proj:abc"));
        assert_eq!(s.title, "排查卡顿");
        assert_eq!(s.messages, msgs);
        assert_eq!(s.agent, AgentId::Codebuddy);
        assert_eq!(set.active_id, id1);

        // 同一条历史再次打开:复用,会话数不增长,active 切回它
        let (id2, is_new2) = set.open_from_source(
            "codebuddy:proj:abc",
            "排查卡顿(重复)".into(),
            vec![],
            AgentId::Codex,
        );
        assert!(!is_new2);
        assert_eq!(id2, id1);
        assert_eq!(set.sessions.len(), 2);
        assert_eq!(set.active_id, id1);
        // 复用不覆盖已有内容
        assert_eq!(set.get_mut(id1).unwrap().messages, msgs);

        // 不同来源:另建一个窗口
        let (id3, is_new3) =
            set.open_from_source("codebuddy:proj:xyz", "另一条".into(), vec![], AgentId::With);
        assert!(is_new3);
        assert_ne!(id3, id1);
        assert_eq!(set.sessions.len(), 3);
    }

    #[test]
    fn session_set_switch_and_close_active_selects_neighbor() {
        let mut set = SessionSet::new(AgentId::Codex, 0);
        let (id1, _) = set.add();
        let (_id2, _) = set.add();
        assert!(set.switch(id1));
        assert_eq!(set.active_id, id1);
        assert!(!set.switch(id1)); // 已是当前
        assert!(!set.switch(999)); // 不存在
                                   // 关掉中间的 active(id1):移除后剩 [id0, id2],active 落到 idx.min(len-1)=1 -> id2
        assert_eq!(set.close(id1), CloseResult::Removed(1));
        assert_eq!(set.sessions.len(), 2);
        assert_eq!(set.active_id, _id2);
        // 关闭非 active 不改 active
        let cur = set.active_id;
        assert_eq!(set.close(0), CloseResult::Removed(0));
        assert_eq!(set.active_id, cur);
        assert_eq!(set.sessions.len(), 1);
    }

    #[test]
    fn session_set_close_last_clears_not_removes() {
        let mut set = SessionSet::new(AgentId::With, 0);
        set.active_mut().messages.push(ChatMessage {
            role: ChatRole::User,
            content: "hi".into(),
            timestamp: 1,
            kind: ChatMessageKind::Normal,
        });
        set.active_mut().title = "hi".into();
        let gen = set.active().generation;
        assert_eq!(set.close(0), CloseResult::Cleared);
        assert_eq!(set.sessions.len(), 1);
        assert!(set.active().messages.is_empty());
        assert!(set.active().title.is_empty());
        assert_eq!(set.active().generation, gen.wrapping_add(1)); // 在途回复应被丢弃
    }

    #[test]
    fn session_set_isolation_draft_and_messages() {
        let mut set = SessionSet::new(AgentId::Codex, 0);
        set.active_mut().messages.push(ChatMessage {
            role: ChatRole::User,
            content: "topic A".into(),
            timestamp: 1,
            kind: ChatMessageKind::Normal,
        });
        set.active_mut().draft = "draft A".into();
        set.active_mut().agent = AgentId::Codex;
        let a_id = set.active_id;
        let (b_id, _) = set.add();
        // 新会话继承 agent/model,但消息/草稿为空
        assert_eq!(set.active().agent, AgentId::Codex);
        assert!(set.active().messages.is_empty());
        assert!(set.active().draft.is_empty());
        set.active_mut().draft = "draft B".into();
        set.active_mut().model_ix = 3;
        // 切回 A,原消息/草稿仍在,且 model 不受 B 影响
        set.switch(a_id);
        assert_eq!(set.active().messages.len(), 1);
        assert_eq!(set.active().draft, "draft A");
        assert_eq!(set.active().model_ix, 0);
        // 切到 B,它的草稿/模型独立
        set.switch(b_id);
        assert_eq!(set.active().draft, "draft B");
        assert_eq!(set.active().model_ix, 3);
        assert!(set.active().messages.is_empty());
    }

    // ---- URL 安全校验 ----

    #[test]
    fn validate_base_url_accepts_http_https_with_host() {
        assert_eq!(validate_base_url("https://api.openai.com/v1"), Ok(()));
        assert_eq!(validate_base_url("  http://localhost:11434/v1  "), Ok(()));
        assert_eq!(validate_base_url("http://127.0.0.1:8080/v1"), Ok(()));
    }

    #[test]
    fn validate_base_url_rejects_bad_or_unsafe() {
        assert_eq!(validate_base_url(""), Err(BaseUrlError::Empty));
        assert_eq!(validate_base_url("   "), Err(BaseUrlError::Empty));
        assert_eq!(validate_base_url("not a url"), Err(BaseUrlError::Parse));
        assert_eq!(
            validate_base_url("ftp://x.com/v1"),
            Err(BaseUrlError::UnsupportedScheme)
        );
        assert_eq!(
            validate_base_url("file:///etc/passwd"),
            Err(BaseUrlError::UnsupportedScheme)
        );
    }

    // ---- 响应解析兼容 ----

    #[test]
    fn parse_null_content_falls_back_to_text() {
        let v = json!({"choices": [{"message": {"content": null}, "text": "legacy reply"}]});
        assert_eq!(parse_response(&v).unwrap(), "legacy reply");
    }

    #[test]
    fn parse_legacy_completion_text() {
        let v = json!({"choices": [{"text": "completion reply"}]});
        assert_eq!(parse_response(&v).unwrap(), "completion reply");
    }

    #[test]
    fn parse_array_with_non_text_parts_still_errors() {
        let v = json!({"choices": [{"message": {"content": [{"type":"image"}]}}]});
        assert!(parse_response(&v).is_err());
    }

    // ---- 恢复清洗 ----

    #[test]
    fn sanitize_repairs_dirty_restored_state() {
        // 模拟持久化里的脏数据:active 指向已删会话、sending 残留、越界 model、next_id 过小
        let mut set = SessionSet::new(AgentId::Codex, 0);
        let (_a, _) = set.add();
        let (b, _) = set.add();
        // 直接改第 0 个会话,不依赖 active(active_id 此刻故意设为不存在的值)
        set.sessions[0].sending = true;
        set.sessions[0].error = Some("stale".into());
        set.sessions[0].model_ix = 999;
        set.active_id = 12345;
        set.next_id = 1; // 比现存 id 小
        set.sanitize(7);
        assert_eq!(set.sessions.len(), 3);
        assert!(set.index_of(set.active_id).is_some());
        assert_eq!(set.active_id, set.sessions[0].id); // 不存在的 active 回落到首个
        assert!(set.next_id > b);
        for s in &set.sessions {
            assert!(!s.sending, "sending 必须清零");
            assert_eq!(s.error, None, "error 必须清空");
            assert!(s.model_ix <= 6, "model_ix 必须落在合法范围内");
        }
        // 只有被改成 999 的那个会话会收敛到上界 6;其余本就是 0
        assert_eq!(set.sessions[0].model_ix, 6);
    }

    // ---- 模型发现 / 合并 / 重索引 ----

    #[test]
    fn base_url_from_completions_strips_suffix() {
        assert_eq!(
            base_url_from_completions("http://127.0.0.1:7864/v1/chat/completions"),
            "http://127.0.0.1:7864/v1"
        );
        assert_eq!(
            base_url_from_completions("https://api.openai.com/v1/"),
            "https://api.openai.com/v1"
        );
    }

    #[test]
    fn parse_agent_models_json_builds_usable_presets() {
        let text = r#"{"models":[
            {"id":"default","name":"default","url":"http://127.0.0.1:7864/v1/chat/completions","apiKey":"k"},
            {"id":"deepseek-v4-pro","name":"deepseek-v4-pro","url":"http://127.0.0.1:7864/v1/chat/completions","apiKey":"sk-x"},
            {"id":"kimi-k3.1-pro","name":"kimi-k3.1-pro","url":"http://127.0.0.1:7864/v1/chat/completions","apiKey":"sk-x"}
        ]}"#;
        let got = parse_agent_models_json(text, "CodeBuddy");
        assert_eq!(got.len(), 2); // default 与缺字段项被跳过
        assert_eq!(got[0].id, "deepseek-v4-pro");
        assert_eq!(got[0].base_url, "http://127.0.0.1:7864/v1");
        assert_eq!(got[0].api_key, "sk-x");
        assert!(got[0].custom);
        assert_eq!(got[0].group, "custom");
        assert_eq!(got[0].label, "deepseek-v4-pro"); // 干净名,不带来源前缀
    }

    #[test]
    fn parse_agent_models_json_bad_input_is_empty() {
        assert!(parse_agent_models_json("not json", "X").is_empty());
        assert!(parse_agent_models_json("{}", "X").is_empty());
    }

    #[test]
    fn parse_openai_models_response_builds_presets() {
        let text = r#"{"object":"list","data":[{"id":"ttsw-codex-hy4std"},{"id":"ttsw-codex-kimi-k3.1-pro"}]}"#;
        let got = parse_openai_models_response(text, "http://127.0.0.1:15721/v1", "tok", "Gateway");
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].id, "ttsw-codex-hy4std");
        assert_eq!(got[0].base_url, "http://127.0.0.1:15721/v1");
        assert_eq!(got[0].api_key, "tok");
    }

    #[test]
    fn merge_models_dedups_keeps_builtin_first() {
        let builtin = openai();
        let mut dup = openai();
        dup.label = "dup".into();
        let custom = ModelPreset {
            label: "My model".into(),
            id: "my".into(),
            base_url: "http://localhost:1/v1".into(),
            custom: true,
            ..Default::default()
        };
        let all = merge_models(std::slice::from_ref(&builtin), &[dup], &[custom]);
        assert_eq!(all.len(), 2); // 内置 + 自定义;重复内置被去
        assert_eq!(all[0].label, builtin.label);
        assert_eq!(all[1].id, "my");
    }

    #[test]
    fn reindex_model_ix_after_remove() {
        assert_eq!(reindex_model_ix(0, 0), 0); // 选中被删项 -> 回落 0
        assert_eq!(reindex_model_ix(3, 0), 2); // 被删项之后左移
        assert_eq!(reindex_model_ix(3, 5), 3); // 被删项在后面,不变
    }

    #[test]
    fn resolve_connection_uses_preset_embedded_key() {
        let mut p = openai();
        p.api_key = "from-preset".into();
        p.key_env = "OPENAI_API_KEY".into();
        let ov = ConnectionOverride::default();
        // 档位自带 key 优先于 key_env 环境变量
        let (_m, _u, k) = resolve_connection_with_env(&p, &ov, |_| Some("from-env".into()));
        assert_eq!(k, "from-preset");
        // 但用户在设置里临时填的 key 仍最高优先
        let ov2 = ConnectionOverride {
            api_key: "user-key".into(),
            ..Default::default()
        };
        let (_m, _u, k) = resolve_connection_with_env(&p, &ov2, |_| Some("from-env".into()));
        assert_eq!(k, "user-key");
    }

    #[test]
    fn format_credit_rate_formats_and_hides_unknown() {
        assert_eq!(format_credit_rate(0.51), "0.51x");
        assert_eq!(format_credit_rate(1.39), "1.39x");
        assert_eq!(format_credit_rate(0.08), "0.08x");
        assert_eq!(format_credit_rate(1.0), "1x");
        assert_eq!(format_credit_rate(0.0), ""); // 免费/浮动不编造
        assert_eq!(format_credit_rate(-1.0), "");
    }

    #[test]
    fn format_catalog_credit_rate_shows_free_as_zero_x() {
        // CLI catalog 里的 0 是明确定价(免费),与网关「含义模糊故隐藏」不同
        assert_eq!(format_catalog_credit_rate(1.33), "1.33x");
        assert_eq!(format_catalog_credit_rate(1.0), "1x");
        assert_eq!(format_catalog_credit_rate(0.0), "0x"); // 免费也要显示
        assert_eq!(format_catalog_credit_rate(-1.0), ""); // 未知/错误值仍不编造
    }

    #[test]
    fn parse_codebuddy_catalog_credits_handles_free_and_variants() {
        let text = r#"{"models":[
            {"id":"claude-sonnet-5","credits":"x1.33 credits"},
            {"id":"hy4std","credits":"x0.00 credits"},
            {"id":"no-prefix","credits":"0.51 credits"},
            {"id":"suffix","credits":"2.17x"},
            {"id":"numeric","credits":0.97},
            {"id":"missing","name":"none"},
            {"id":"bad","credits":"free"}
        ]}"#;
        let got = parse_codebuddy_catalog_credits(text);
        assert_eq!(
            got.get("claude-sonnet-5").map(String::as_str),
            Some("1.33x")
        );
        assert_eq!(got.get("hy4std").map(String::as_str), Some("0x")); // 免费显示 0x
        assert_eq!(got.get("no-prefix").map(String::as_str), Some("0.51x"));
        assert_eq!(got.get("suffix").map(String::as_str), Some("2.17x"));
        assert_eq!(got.get("numeric").map(String::as_str), Some("0.97x"));
        assert!(!got.contains_key("missing"));
        assert!(!got.contains_key("bad"));
    }

    #[test]
    fn parse_codebuddy_catalog_credits_degrades_on_bad_input() {
        assert!(parse_codebuddy_catalog_credits("").is_empty());
        assert!(parse_codebuddy_catalog_credits("not json").is_empty());
        assert!(parse_codebuddy_catalog_credits(r#"{"data":[]}"#).is_empty());
        assert!(parse_codebuddy_catalog_credits(r#"{"models":[]}"#).is_empty());
    }

    #[test]
    fn apply_cli_credits_fills_official_including_free_and_keeps_existing() {
        use std::collections::HashMap;
        let mut models = vec![
            ModelPreset {
                id: "claude-sonnet-5".into(),
                ..Default::default()
            },
            ModelPreset {
                id: "hy4std".into(),
                ..Default::default()
            }, // 免费
            ModelPreset {
                id: "local".into(),
                credit_rate: "3x".into(),
                ..Default::default()
            }, // 已有值
            ModelPreset {
                id: "unknown".into(),
                ..Default::default()
            }, // 表内无
        ];
        let mut credits = HashMap::new();
        credits.insert("claude-sonnet-5".to_string(), "1.33x".to_string());
        credits.insert("hy4std".to_string(), "0x".to_string());
        credits.insert("local".to_string(), "9x".to_string());
        apply_cli_credits(&mut models, &credits);
        assert_eq!(models[0].credit_rate, "1.33x");
        assert_eq!(models[1].credit_rate, "0x"); // 免费也回填
        assert_eq!(models[2].credit_rate, "3x"); // 已有值不被覆盖
        assert_eq!(models[3].credit_rate, ""); // 未匹配保持空(UI 显示 —)
    }

    #[test]
    fn parse_gateway_catalog_reads_rate_and_display_name() {
        let text = r#"{"data":[
            {"id":"deepseek-v4-pro","display_name":"DeepSeek-V4-Pro","credit_rate":0.51,"owned_by":"poolmate"},
            {"id":"space-bunny","display_name":"Space-Bunny","credit_rate":0.08},
            {"id":"free-one","display_name":"Free","credit_rate":0}
        ]}"#;
        let got = parse_openai_models_response(text, "http://127.0.0.1:7864/v1", "k", "");
        assert_eq!(got.len(), 3);
        assert_eq!(got[0].label, "DeepSeek-V4-Pro");
        assert_eq!(got[0].credit_rate, "0.51x");
        assert_eq!(got[0].group, "custom");
        assert_eq!(got[1].credit_rate, "0.08x");
        assert_eq!(got[2].credit_rate, ""); // 免费不显示
    }

    #[test]
    fn enrich_with_catalog_adds_rate_and_clean_label() {
        let mut models = parse_agent_models_json(
            r#"{"models":[{"id":"deepseek-v4-pro","name":"deepseek-v4-pro","url":"http://127.0.0.1:7864/v1/chat/completions","apiKey":"k"}]}"#,
            "CodeBuddy",
        );
        assert!(models[0].credit_rate.is_empty());
        assert_eq!(models[0].label, "deepseek-v4-pro"); // 无前缀的干净名
        let catalog = parse_openai_models_response(
            r#"{"data":[{"id":"deepseek-v4-pro","display_name":"DeepSeek-V4-Pro","credit_rate":0.51}]}"#,
            "http://127.0.0.1:7864/v1",
            "k",
            "",
        );
        enrich_with_catalog(&mut models, &catalog);
        assert_eq!(models[0].credit_rate, "0.51x");
        assert_eq!(models[0].label, "DeepSeek-V4-Pro");
        assert_eq!(models[0].group, "custom");
    }

    #[test]
    fn discovered_cli_model_end_to_end_request_is_well_formed() {
        // 模拟 ~/.codebuddy/models.json 的一条 -> 并入下拉 -> 用户点选 ->
        // 解析连接 -> 构造请求体,验证整条链路带的 model/url/key 都对。
        let text = r#"{"models":[{"id":"minimax-m3-ioa","name":"minimax-m3-ioa","url":"http://127.0.0.1:7864/v1/chat/completions","apiKey":"sk-cli"}]}"#;
        let discovered = parse_agent_models_json(text, "CodeBuddy");
        let all = merge_models(&builtin_models(), &discovered, &[]);
        // 内置 7 项之后追加了自动发现项
        let ix = all
            .iter()
            .position(|m| m.id == "minimax-m3-ioa")
            .expect("discovered model must appear in dropdown");
        assert!(ix >= builtin_models().len());
        let chosen = &all[ix];
        let (model, base_url, key) =
            resolve_connection_with_env(chosen, &ConnectionOverride::default(), |_| None);
        assert_eq!(model, "minimax-m3-ioa");
        assert_eq!(base_url, "http://127.0.0.1:7864/v1");
        assert_eq!(key, "sk-cli"); // 来自 CLI 配置的 key,不需用户手填
                                   // 拼出的 URL 与请求体可用于 send_once
        let url = completions_url(&base_url).unwrap();
        assert_eq!(url, "http://127.0.0.1:7864/v1/chat/completions");
        let body = build_request_body(&model, AgentId::Codebuddy, &[]);
        assert_eq!(body["model"], "minimax-m3-ioa");
        assert_eq!(body["messages"][0]["role"], "system");
    }

    #[test]
    fn sanitize_empty_set_gets_one_session() {
        let mut set = SessionSet::new(AgentId::With, 0);
        set.sessions.clear();
        set.sanitize(7);
        assert_eq!(set.sessions.len(), 1);
        assert_eq!(set.active_id, set.sessions[0].id);
    }

    #[test]
    fn sanitize_truncates_over_limit() {
        let mut set = SessionSet::new(AgentId::With, 0);
        for _ in 0..(MAX_SESSIONS + 5) {
            set.add();
        }
        set.sanitize(7);
        assert_eq!(set.sessions.len(), MAX_SESSIONS);
    }

    #[test]
    fn session_set_serde_round_trip_with_sanitize() {
        // 模拟 GUI:构造 -> 填充 -> 落盘 JSON -> 读回 -> sanitize -> 不变量成立
        let mut set = SessionSet::new(AgentId::Codex, 0);
        set.active_mut().title = "话题 A".into();
        set.active_mut().draft = "没发完的草稿".into();
        set.active_mut().messages.push(ChatMessage {
            role: ChatRole::User,
            content: "hi".into(),
            timestamp: 1,
            kind: ChatMessageKind::Normal,
        });
        let (b, _) = set.add();
        set.active_mut().model_ix = 99; // 越界
        set.active_mut().sending = true; // 重启后不该卡死
        let json = serde_json::to_string(&set).unwrap();
        let mut restored: SessionSet = serde_json::from_str(&json).unwrap();
        restored.sanitize(7);
        assert_eq!(restored.sessions.len(), 2);
        assert_eq!(restored.sessions[0].title, "话题 A");
        assert_eq!(restored.sessions[0].draft, "没发完的草稿");
        assert_eq!(restored.sessions[0].messages.len(), 1);
        assert_eq!(restored.sessions[1].id, b);
        for s in &restored.sessions {
            assert!(!s.sending);
            assert_eq!(s.error, None);
            assert!(s.model_ix <= 6);
        }
        assert!(restored.index_of(restored.active_id).is_some());
    }

    #[test]
    fn sanitize_repairs_broken_active_from_json() {
        // 手改过的 JSON:active_id 指向不存在的 id、空 sessions
        let json = r#"{"sessions":[],"active_id":999,"next_id":0}"#;
        let mut set: SessionSet = serde_json::from_str(json).unwrap();
        set.sanitize(7);
        assert_eq!(set.sessions.len(), 1);
        assert!(set.index_of(set.active_id).is_some());
        assert!(set.next_id >= 1);
    }

    #[test]
    fn default_model_ix_prefers_first_cli_model() {
        let mut a = ModelPreset {
            id: "custom".into(),
            group: "connection".into(),
            ..Default::default()
        };
        a.via_cli = false;
        let mut b = ModelPreset {
            id: "claude-opus-5.5".into(),
            group: "official".into(),
            ..Default::default()
        };
        b.via_cli = true;
        let mut c = ModelPreset {
            id: "custom-local:x".into(),
            group: "custom".into(),
            ..Default::default()
        };
        c.via_cli = true;
        assert_eq!(default_model_ix(&[a.clone(), b.clone(), c.clone()]), 1);
        assert_eq!(default_model_ix(&[a.clone()]), 0); // 无 CLI 模型退回 0
        assert_eq!(default_model_ix(&[]), 0);
    }

    #[test]
    fn parse_cli_model_list_splits_official_and_local() {
        let help = r#"--model <model> Model for the current session. Please provide
the model ID. Currently supported: (claude-opus-5.5, gpt-5.6-terra, echo,
custom-local:default, custom-local:deepseek-v4-pro, custom-local:space-bunny)"#;
        let got = parse_cli_model_list(help);
        assert_eq!(got.len(), 6);
        let official: Vec<_> = got.iter().filter(|m| m.group == "official").collect();
        let local: Vec<_> = got.iter().filter(|m| m.group == "custom").collect();
        assert_eq!(official.len(), 3);
        assert_eq!(local.len(), 3);
        assert_eq!(official[0].id, "claude-opus-5.5");
        assert_eq!(official[0].label, "claude-opus-5.5");
        assert_eq!(local[0].id, "custom-local:default");
        assert_eq!(local[0].label, "default"); // 去掉前缀
        assert!(local.iter().all(|m| m.custom));
        assert!(official.iter().all(|m| !m.custom));
        // id 保留原样,用于 --model
        assert_eq!(got[3].id, "custom-local:default");
    }

    #[test]
    fn parse_cli_model_list_handles_empty_and_dedups() {
        assert!(parse_cli_model_list("no list here").is_empty());
        let help = "Currently supported: (echo, echo, custom-local:x)";
        assert_eq!(parse_cli_model_list(help).len(), 2);
    }

    #[test]
    fn build_cli_args_fresh_and_resume() {
        let fresh = build_cli_args("echo", None, "hello");
        assert_eq!(fresh[0], "-p");
        assert!(fresh.contains(&"--model".to_string()));
        assert!(fresh.contains(&"echo".to_string()));
        assert!(fresh.contains(&"hello".to_string()));
        assert!(!fresh.iter().any(|a| a == "--resume"));
        assert!(fresh.windows(2).any(|w| w[0] == "--tools" && w[1] == ""));
        let resume = build_cli_args("custom-local:space-bunny", Some("sid-123"), "hi");
        assert!(resume
            .windows(2)
            .any(|w| w[0] == "--resume" && w[1] == "sid-123"));
    }

    #[test]
    fn parse_cli_stream_extracts_result_and_session_id() {
        let text = concat!(
            r#"{"type":"system","subtype":"init","session_id":"S1","model":"echo"}"#,
            "\n",
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"po"}]}}"#,
            "\n",
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"ng"}]}}"#,
            "\n",
            r#"{"type":"result","result":"pong","session_id":"S1"}"#,
        );
        let (reply, sid) = parse_cli_stream(text).unwrap();
        assert_eq!(reply, "pong"); // result 优先于逐块 assistant
        assert_eq!(sid.as_deref(), Some("S1"));
    }

    #[test]
    fn parse_cli_stream_falls_back_to_assistant_text() {
        let text = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"hi"}]},"session_id":"S2"}"#;
        assert_eq!(
            parse_cli_stream(text).unwrap(),
            ("hi".into(), Some("S2".into()))
        );
        assert!(parse_cli_stream("not json at all").is_err());
    }

    #[test]
    fn chat_session_cli_id_is_backward_compatible() {
        // 旧持久化 JSON 没有 cli_session_id -> 反序列化为 None
        let old = r#"{"id":0,"title":"","messages":[],"agent":"claude-code","model_ix":0,"draft":"","generation":0,"sending":false,"error":null}"#;
        let s: ChatSessionData = serde_json::from_str(old).unwrap();
        assert_eq!(s.cli_session_id, None);
        // 新会话默认为 None
        assert_eq!(
            ChatSessionData::new(0, AgentId::ClaudeCode, 0).cli_session_id,
            None
        );
    }

    /// 真机集成:验证 core 的 CLI 会话驱动链路。依赖本机 codebuddy CLI 与登录态,
    /// 默认 #[ignore];用 `cargo test -p hindsight-core cli_roundtrip -- --ignored` 手动跑。
    #[test]
    #[ignore = "requires local codebuddy CLI (echo model)"]
    fn cli_roundtrip_discovers_and_chats() {
        let models = fetch_cli_models().expect("fetch_cli_models");
        let official = models.iter().filter(|m| m.group == "official").count();
        let local = models.iter().filter(|m| m.group == "custom").count();
        assert!(official > 0 && local > 0, "expected official+local groups");
        assert!(models.iter().all(|m| m.via_cli));

        let (r1, sid) = run_cli_session("echo", None, "say exactly: ROUND1").expect("first turn");
        assert_eq!(r1.trim(), "ROUND1");
        let sid = sid.expect("session id from first turn");
        let (r2, sid2) =
            run_cli_session("echo", Some(&sid), "say exactly: ROUND2").expect("resume");
        assert_eq!(r2.trim(), "ROUND2");
        assert_eq!(sid2.as_deref(), Some(sid.as_str())); // 同一会话续接
    }

    #[test]
    fn parse_cli_line_emits_all_event_kinds() {
        // tool_use(带命令)
        let tool_use = r#"{"type":"assistant","session_id":"S1","message":{"content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"echo OK","description":"run"}}]}}"#;
        assert_eq!(
            parse_cli_line(tool_use),
            vec![
                CliEvent::Session("S1".into()),
                CliEvent::ToolUse {
                    name: "Bash".into(),
                    input: "echo OK".into()
                },
            ]
        );
        // tool_result(带 stdout)+is_error
        let ok = r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":[{"type":"text","text":"Stdout: OK\nExit Code: 0"}],"is_error":false}]}}"#;
        assert_eq!(
            parse_cli_line(ok),
            vec![CliEvent::ToolResult {
                output: "Stdout: OK\nExit Code: 0".into(),
                is_error: false
            }]
        );
        let denied = r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":[{"type":"text","text":"Permission denied"}],"is_error":true}]}}"#;
        assert_eq!(
            parse_cli_line(denied),
            vec![CliEvent::ToolResult {
                output: "Permission denied".into(),
                is_error: true
            }]
        );
        // thinking + text
        let think = r#"{"type":"assistant","message":{"content":[{"type":"thinking","thinking":"let me check"},{"type":"text","text":"hello"}]}}"#;
        assert_eq!(
            parse_cli_line(think),
            vec![
                CliEvent::Thinking("let me check".into()),
                CliEvent::Text("hello".into())
            ]
        );
        // result 终止行
        let done = r#"{"type":"result","result":"pong","session_id":"S1"}"#;
        assert_eq!(
            parse_cli_line(done),
            vec![
                CliEvent::Session("S1".into()),
                CliEvent::Done {
                    reply: "pong".into()
                }
            ]
        );
        // 无关行/非 JSON
        assert!(parse_cli_line(r#"{"type":"system","subtype":"status"}"#).is_empty());
        assert!(parse_cli_line("not json").is_empty());
    }

    #[test]
    fn build_cli_args_with_tools_controls_permission() {
        let pure = build_cli_args("echo", None, "hi"); // 默认纯对话
        assert!(pure.windows(2).any(|w| w[0] == "--tools" && w[1] == ""));
        assert!(!pure.iter().any(|a| a == "-y"));
        let tool = build_cli_args_with_tools("echo", None, "hi", true);
        assert!(tool.iter().any(|a| a == "-y"));
        assert!(!tool.iter().any(|a| a == "--tools")); // 放行时不加 --tools 限制
    }

    #[test]
    fn build_request_body_skips_tool_messages() {
        let h = vec![
            ChatMessage {
                role: ChatRole::User,
                content: "hi".into(),
                timestamp: 1,
                kind: ChatMessageKind::Normal,
            },
            ChatMessage {
                role: ChatRole::Assistant,
                content: "Stdout: OK".into(),
                timestamp: 2,
                kind: ChatMessageKind::Tool,
            },
        ];
        let body = build_request_body("gpt-4o", AgentId::Codex, &h);
        let msgs = body["messages"].as_array().unwrap();
        // system + 1 条 Normal;Tool 过程不进 LLM 上下文
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[1]["content"], "hi");
    }

    #[test]
    fn chat_message_kind_is_backward_compatible() {
        // 旧持久化消息无 kind -> 反序列化为 Normal
        let old = r#"{"role":"user","content":"hi","timestamp":1}"#;
        let m: ChatMessage = serde_json::from_str(old).unwrap();
        assert_eq!(m.kind, ChatMessageKind::Normal);
    }

    /// 真机流式集成:验证 `stream_cli_session` 能把命令调用与命令输出作为
    /// 实时事件送回(聊天窗口据此显示)。用 echo 模型 + 无害 Bash echo 命令。
    /// 用 `cargo test -p hindsight-core cli_stream_shows_tool_output -- --ignored --nocapture` 手动跑。
    #[test]
    #[ignore = "requires local codebuddy CLI (echo + Bash tool)"]
    fn cli_stream_shows_tool_output() {
        let (tx, rx) = std::sync::mpsc::channel();
        let model = "echo".to_string();
        let prompt = "Use the Bash tool to run exactly: echo HS_STREAM_789. Then reply with just the echoed text.".to_string();
        let handle = std::thread::spawn(move || {
            stream_cli_session(&model, None, &prompt, true, tx);
        });
        let events: Vec<CliEvent> = rx.into_iter().collect();
        handle.join().unwrap();
        let has_session = events.iter().any(|e| matches!(e, CliEvent::Session(_)));
        let has_tool_use = events.iter().any(|e| matches!(e, CliEvent::ToolUse { .. }));
        let tool_output = events.iter().find_map(|e| match e {
            CliEvent::ToolResult { output, .. } => Some(output.clone()),
            _ => None,
        });
        let has_done = events.iter().any(|e| matches!(e, CliEvent::Done { .. }));
        println!(
            "events={} session={has_session} tool_use={has_tool_use} done={has_done}",
            events.len()
        );
        println!("tool_output={tool_output:?}");
        assert!(has_session && has_done, "应有 session 与 done 事件");
        assert!(has_tool_use, "应捕获到工具调用事件");
        let out = tool_output.expect("应捕获到命令输出事件");
        assert!(
            out.contains("HS_STREAM_789"),
            "命令输出应包含 echo 的文本: {out:?}"
        );
    }
}
