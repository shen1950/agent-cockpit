//! 妙手（美团 CatPaw / CatDesk）反代上游：**进程内云端直连**。
//!
//! 旧方案（命名管道 `\\.\pipe\paw` + catpaw-bridge/bridge.js + 妙手桌面端必须开着）
//! 已整体废弃——那就是"每次反代都要先开妙手"的根源。2026-10-26 的逆向与真机实测结论
//! （对话全链路 200，扣积分、删会话都验过）：
//!
//!   1. 云端对话是**两台主机分工**：`https://nocode.cn/api/chat/*` 管建会话/提交消息/取消/删除，
//!      `https://ai.catpaw.meituan.com/api/agent/stream/connect` 管收 SSE 流。
//!      `catpaw-agent.meituan.com` 只代理 `/api/agent/*`（模型目录在它这儿，connect 会 500）。
//!   2. 鉴权只有一枚 accessToken（`~/.meituan-catpaw/auth.json`），无签名、无设备指纹。
//!      头按主机分两套：nocode.cn 要 `access-token` + Cookie；agent/stream 要 `X-Passport-Token`。
//!   3. chatId 本地生成（`desk-` + 16 位 base36）；conversationId 云端在 agent-stream
//!      响应里给。一条请求 = 一个全新云会话，答完即删（不留垃圾历史）。
//!
//! 桌面端唯一不可替代的时刻是**登录**：passport 不给 refresh_token，令牌过期只能在
//! 妙手窗口里重登一次（`open_login` 帮用户把窗口拉起来）。

use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::io::Read;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub const APPS: [&str; 1] = ["catpaw"];
/// 上游组里标记「这条上游是妙手云端直连」，`proxy.rs` 靠它进翻译分支
pub const BASE_MARK: &str = "catpaw://agent";
pub const CHAT_HOST: &str = "https://nocode.cn";
pub const STREAM_HOST: &str = "https://ai.catpaw.meituan.com";
pub const MAAS_HOST: &str = "https://catpaw-agent.meituan.com";
pub const CREDIT_HOST: &str = "https://catx.nocode.cn";
/// 妙手的家目录（product.json 的 dataFolderName）
pub const HOME_DIR: &str = ".meituan-catpaw";

const MODEL_TTL: Duration = Duration::from_secs(600);
const TOKEN_TTL: Duration = Duration::from_secs(60);
const ROUND_TIMEOUT: Duration = Duration::from_secs(600);

pub fn is_paw_base(base: &str) -> bool {
    base.trim().trim_end_matches('/') == BASE_MARK
}

pub fn home() -> PathBuf {
    PathBuf::from(std::env::var("USERPROFILE").unwrap_or_default()).join(HOME_DIR)
}

fn auth_path() -> PathBuf {
    home().join("auth.json")
}

pub fn auth_file_exists() -> bool {
    auth_path().exists()
}

/// 登录态：只读 accessToken，文件里其它字段一概不碰
fn token() -> String {
    std::fs::read_to_string(auth_path())
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| v["auth"]["accessToken"].as_str().map(|s| s.trim().to_string()))
        .unwrap_or_default()
}

/// 登录账户（面板账号表用）：文件在就报"登的是谁"，令牌有没有效另说
fn auth_account() -> Option<Value> {
    let txt = std::fs::read_to_string(auth_path()).ok()?;
    let v: Value = serde_json::from_str(&txt).ok()?;
    let tok = v["auth"]["accessToken"].as_str().unwrap_or("").trim();
    if tok.is_empty() {
        return None;
    }
    Some(json!({
        "uid": v["account"]["uid"].as_str().unwrap_or(""),
        "name": v["account"]["loginName"].as_str()
            .or_else(|| v["account"]["name"].as_str()).unwrap_or(""),
        "updated_ms": v["updatedAt"].as_u64().unwrap_or(0),
    }))
}

/// 拉起妙手窗口，只为登录：passport 不给 refresh_token，重登这步自动化不了
fn open_login() -> Value {
    let app = match crate::registry::app_def("catpaw") {
        Some(a) => a,
        None => return json!({ "ok": false, "msg": "注册表里没有妙手" }),
    };
    let exe = match crate::registry::resolve_exe(&app) {
        Some(e) => e,
        None => return json!({ "ok": false, "msg": "没找到妙手主程序（妙手.exe）" }),
    };
    let mut c = std::process::Command::new(&exe);
    c.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(0x0000_0008 | 0x0800_0000);
    }
    match c.spawn() {
        Ok(_) => json!({ "ok": true, "msg": "妙手已打开：在里面完成登录，再回来点「重新探测」" }),
        Err(e) => json!({ "ok": false, "msg": format!("启动妙手失败：{e}") }),
    }
}

// ───────────────────────── 令牌探活（不花积分） ─────────────────────────

static TOKEN_PROBE: Mutex<Option<(Instant, Value)>> = Mutex::new(None);

/// 用网关的余额接口验令牌：200 = 活着，顺带把余额带回来。结果缓存 60 秒。
fn token_check() -> Value {
    if let Ok(g) = TOKEN_PROBE.lock() {
        if let Some((at, ref v)) = *g {
            if at.elapsed() < TOKEN_TTL {
                return v.clone();
            }
        }
    }
    let tok = token();
    let v = if tok.is_empty() {
        json!({ "ok": false, "msg": "没读到登录态（~/.meituan-catpaw/auth.json），打开妙手登录一次" })
    } else {
        match ureq::get(&format!("{CREDIT_HOST}/api/gateway/credit/balance"))
            .timeout(Duration::from_secs(8))
            .set("X-Auth-Token", &tok)
            .call()
        {
            Ok(r) => {
                let j: Value = r.into_json().unwrap_or(json!({}));
                // 实测形状：{"code":0,"data":{"availableCredits":"1166.14",...}}
                let bal = j["data"]["availableCredits"]
                    .as_str()
                    .and_then(|s| s.parse::<f64>().ok())
                    .or_else(|| j["data"]["balance"].as_f64())
                    .or_else(|| j["balance"].as_f64());
                json!({ "ok": true, "balance": bal })
            }
            Err(ureq::Error::Status(401, _)) | Err(ureq::Error::Status(403, _)) => {
                json!({ "ok": false, "msg": "令牌已失效：打开妙手重新登录一次" })
            }
            Err(e) => json!({ "ok": Value::Null, "msg": format!("验不了令牌：{e}") }),
        }
    };
    if let Ok(mut g) = TOKEN_PROBE.lock() {
        *g = Some((Instant::now(), v.clone()));
    }
    v
}

fn clear_cache() {
    if let Ok(mut g) = TOKEN_PROBE.lock() {
        *g = None;
    }
    if let Ok(mut g) = MODEL_CACHE.lock() {
        *g = None;
    }
}

// ───────────────────────── 模型目录 ─────────────────────────

static MODEL_CACHE: Mutex<Option<(Instant, Vec<Value>)>> = Mutex::new(None);

/// 云端目录拉不到时的兜底（限时免费那条 + auto）
fn models_fallback() -> Vec<Value> {
    vec![
        json!({ "id": "LongCat-2.0", "name": "LongCat-2.0（限时免费）", "numeric": 77, "rate": 0.0, "free": true }),
        json!({ "id": "auto", "name": "Auto", "numeric": 0, "rate": Value::Null, "free": false }),
    ]
}

/// 云端模型目录（`/api/agent/maas/model-types`，不需要桌面端在跑）。
/// 服务端给 modelTypeName（真名）+ modelTypeId（数字，云端直连只当展示）。
pub fn models() -> Vec<Value> {
    if let Ok(g) = MODEL_CACHE.lock() {
        if let Some((at, ref v)) = *g {
            if at.elapsed() < MODEL_TTL && !v.is_empty() {
                return v.clone();
            }
        }
    }
    let tok = token();
    let mut got: Vec<Value> = Vec::new();
    if !tok.is_empty() {
        let body = json!({ "tenant": "CatDesk", "scene": "CATX_APP", "env": "EXTERNAL" });
        if let Ok(r) = ureq::post(&format!("{MAAS_HOST}/api/agent/maas/model-types"))
            .timeout(Duration::from_secs(10))
            .set("Content-Type", "application/json")
            .set("X-Passport-Token", &tok)
            .set("client-id", "catdesk")
            .send_string(&body.to_string())
        {
            let j: Value = r.into_json().unwrap_or(json!({}));
            let arr = j["data"]
                .as_array()
                .or_else(|| j["result"].as_array())
                .or_else(|| j["modelTypes"].as_array())
                .cloned()
                .unwrap_or_default();
            for m in arr {
                let ei = &m["extendedInfo"];
                let rate = ei["rateMultiplier"].as_f64();
                let id = m["modelTypeName"]
                    .as_str()
                    .or_else(|| m["description"].as_str())
                    .unwrap_or("")
                    .trim()
                    .to_string();
                if id.is_empty() || m["modelTypeId"].as_i64().is_none() {
                    continue;
                }
                let tag = match rate {
                    Some(0.0) => match ei["marketingCopy"].as_str() {
                        Some(s) if !s.is_empty() => format!("（{s}）"),
                        _ => "（免费）".to_string(),
                    },
                    Some(r) => format!("（×{}）", r),
                    None => String::new(),
                };
                got.push(json!({
                    "id": id,
                    "name": format!("{}{}", m["description"].as_str().unwrap_or(&id), tag),
                    "numeric": m["modelTypeId"].clone(),
                    "rate": rate,
                    "free": rate == Some(0.0),
                    "vision": m["supportImage"].as_bool().unwrap_or(false),
                }));
            }
        }
    }
    if got.is_empty() {
        got = models_fallback();
    }
    if let Ok(mut g) = MODEL_CACHE.lock() {
        *g = Some((Instant::now(), got.clone()));
    }
    got
}

/// 面板「获取模型ID」用：id 一律带 `catpaw/` 前缀——`glm-5.3-flash`、`kimi-k3`
/// 这些每家都有，裸名在反代里会漂到别家去。
pub fn model_ids() -> Value {
    if token().is_empty() {
        return json!({ "ok": false, "msg": "没读到登录态：打开妙手登录一次" });
    }
    let ms = models();
    if ms.is_empty() {
        return json!({ "ok": false, "msg": "云端没回模型清单（令牌可能失效）" });
    }
    let tagged = crate::proxy::tag_owner("catpaw", &ms);
    json!({ "ok": true, "count": tagged.len(), "models": tagged })
}

/// 客户端填的模型名 → 云端 agent-stream 的 model 字段。
/// 前缀（catpaw/xxx）在这里剥掉。**云端只认数字 id 字符串**（2026-10-27 实测：
/// 裸名 `glm-5.3-flashx` 报「model 与 modelType 不能同时为空」，`"98"` 一次通过；
/// `LongCat-2.0` 是云端有特判的历史例外）。认不出来的名字原样交给云端，让它自己判。
pub fn model_for_stream(raw: &str, list: &[Value]) -> String {
    let s = raw.trim();
    let bare = s.rsplit_once('/').map(|(_, b)| b.trim()).unwrap_or(s);
    if bare.is_empty() || bare.eq_ignore_ascii_case("auto") {
        return "auto".to_string();
    }
    let low = bare.to_lowercase();
    for m in list {
        if m["id"].as_str().map(|i| i.to_lowercase()) == Some(low.clone()) {
            // 云端要数字 id；目录条目缺 numeric 时退回名字（LongCat 有特判不会坏）
            return match m["numeric"].as_i64() {
                Some(n) if n > 0 => n.to_string(),
                _ => m["id"].as_str().unwrap_or(bare).to_string(),
            };
        }
    }
    // 已经是纯数字的（模型 ID 表里复制出来的 numeric）直接用
    if bare.chars().all(|c| c.is_ascii_digit()) {
        return bare.to_string();
    }
    bare.to_string()
}

// ───────────────────────── 请求翻译（OpenAI → 云端） ─────────────────────────

/// OpenAI messages → 妙手单条 prompt。多轮按角色分段，别把角色信息丢掉
pub fn flatten(messages: &Value) -> String {
    let mut parts: Vec<String> = Vec::new();
    for m in messages.as_array().cloned().unwrap_or_default() {
        let text = match &m["content"] {
            Value::String(s) => s.clone(),
            Value::Array(arr) => arr
                .iter()
                .filter_map(|c| {
                    if let Some(s) = c.as_str() {
                        Some(s.to_string())
                    } else {
                        c["text"].as_str().map(|s| s.to_string())
                    }
                })
                .collect::<Vec<_>>()
                .join(""),
            _ => continue,
        };
        if text.trim().is_empty() {
            continue;
        }
        let role = m["role"].as_str().unwrap_or("user").to_lowercase();
        let tagged = match role.as_str() {
            "system" => format!("【系统设定】\n{text}"),
            "assistant" => format!("【助手】\n{text}"),
            "tool" => format!("【工具结果】\n{text}"),
            _ => {
                if parts.is_empty() {
                    text
                } else {
                    format!("【用户】\n{text}")
                }
            }
        };
        parts.push(tagged);
    }
    parts.join("\n\n")
}

/// Prompt-level compatibility for OpenAI client tools. The remote CatPaw agent
/// has its own tools; those are not client tools and must never be executed here.
/// The client receives ordinary OpenAI tool_calls and decides whether to run them.
#[derive(Debug)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

#[derive(Debug)]
pub enum ToolReply {
    Text(String),
    Calls(Vec<ToolCall>),
}

pub fn has_tool_history(messages: &Value) -> bool {
    messages.as_array().map_or(false, |msgs| msgs.iter().any(|m| {
        m["role"].as_str() == Some("tool") || m.get("tool_calls").is_some() || m.get("function_call").is_some()
    }))
}

/// Use this even without current tools when earlier turns contain tool calls.
pub fn structured_history(messages: &Value) -> Result<String, String> {
    let msgs = messages.as_array().ok_or("messages 必须是数组")?;
    for msg in msgs {
        if let Some(content) = msg["content"].as_array() {
            if content.iter().any(|c| matches!(c["type"].as_str(), Some("image_url" | "image" | "input_image"))) {
                return Err("妙手工具适配目前不支持图片输入".into());
            }
        }
    }
    Ok(format!("CLIENT CONVERSATION (JSON):\n{messages}"))
}

// A deliberately limited, fail-closed JSON Schema subset. Do not silently
// accept constraints that this adapter cannot validate.
fn validate_tool_schema(schema: &Value, value: &Value, depth: usize) -> Result<(), String> {
    if depth > 32 { return Err("工具参数 schema 嵌套过深".into()); }
    if schema == &json!(true) || schema.is_null() { return Ok(()); }
    if schema == &json!(false) { return Err("工具参数不符合 schema".into()); }
    let obj = schema.as_object().ok_or("工具参数 schema 必须是对象")?;
    for key in obj.keys() {
        if !matches!(key.as_str(), "type" | "properties" | "required" | "additionalProperties" | "items" | "enum" | "const" | "description" | "title" | "default" | "examples" | "$schema" | "$id" | "minLength" | "maxLength" | "minimum" | "maximum" | "minItems" | "maxItems") {
            return Err(format!("工具参数 schema 暂不支持约束 {key}"));
        }
    }
    if let Some(t) = obj.get("type") {
        let matches_type = |name: &str| match name {
            "object" => value.is_object(), "array" => value.is_array(),
            "string" => value.is_string(), "boolean" => value.is_boolean(),
            "null" => value.is_null(), "number" => value.is_number(),
            "integer" => value.as_f64().map_or(false, |x| x.fract() == 0.0),
            _ => false,
        };
        let ok = if let Some(name) = t.as_str() { matches_type(name) }
            else if let Some(types) = t.as_array() { types.iter().any(|x| x.as_str().map_or(false, matches_type)) }
            else { false };
        if !ok { return Err("工具参数类型不符合 schema".into()); }
    }
    if let Some(choices) = obj.get("enum") {
        if !choices.as_array().map_or(false, |xs| xs.contains(value)) { return Err("工具参数不在 enum 中".into()); }
    }
    if let Some(c) = obj.get("const") {
        if c != value { return Err("工具参数不符合 const".into()); }
    }
    if let Some(fields) = value.as_object() {
        if let Some(req) = obj.get("required") {
            let req = req.as_array().ok_or("schema required 不是数组")?;
            for name in req {
                let name = name.as_str().ok_or("schema required 名称无效")?;
                if !fields.contains_key(name) { return Err(format!("工具参数缺少必填字段：{name}")); }
            }
        }
        let props = obj.get("properties").and_then(Value::as_object);
        for (name, v) in fields {
            if let Some(s) = props.and_then(|p| p.get(name)) {
                validate_tool_schema(s, v, depth + 1)?;
            } else if let Some(additional) = obj.get("additionalProperties") {
                if additional == &json!(false) { return Err(format!("工具参数包含未知字段：{name}")); }
                if additional.is_object() { validate_tool_schema(additional, v, depth + 1)?; }
            }
        }
    }
    if let Some(items) = value.as_array() {
        if let Some(min) = obj.get("minItems").and_then(Value::as_u64) { if items.len() < min as usize { return Err("工具参数数组过短".into()); } }
        if let Some(max) = obj.get("maxItems").and_then(Value::as_u64) { if items.len() > max as usize { return Err("工具参数数组过长".into()); } }
        if let Some(s) = obj.get("items") { for v in items { validate_tool_schema(s, v, depth + 1)?; } }
    }
    if let Some(s) = value.as_str() {
        let len = s.chars().count();
        if let Some(min) = obj.get("minLength").and_then(Value::as_u64) { if len < min as usize { return Err("工具参数字符串过短".into()); } }
        if let Some(max) = obj.get("maxLength").and_then(Value::as_u64) { if len > max as usize { return Err("工具参数字符串过长".into()); } }
    }
    if let Some(n) = value.as_f64() {
        if let Some(min) = obj.get("minimum").and_then(Value::as_f64) { if n < min { return Err("工具参数小于下限".into()); } }
        if let Some(max) = obj.get("maximum").and_then(Value::as_f64) { if n > max { return Err("工具参数大于上限".into()); } }
    }
    Ok(())
}

#[derive(Debug)]
pub struct ToolAdapter {
    tools: Value,
    names: Vec<String>,
    required: bool,
    selected: Option<String>,
    parallel: bool,
    marker: String,
}

impl ToolAdapter {
    /// None means no tool negotiation is requested. This is not native model
    /// function calling: success depends on the remote model following the prompt.
    pub fn from_request(request: &Value) -> Result<Option<Self>, String> {
        let choice = &request["tool_choice"];
        if choice.as_str() == Some("none") {
            return Ok(None);
        }
        let required = choice.as_str() == Some("required");
        let selected = if choice.is_object() {
            if choice["type"].as_str() != Some("function") {
                return Err("不支持的 tool_choice 类型".into());
            }
            Some(choice["function"]["name"].as_str().filter(|s| !s.is_empty())
                .ok_or("tool_choice 缺少函数名称")?.to_string())
        } else if choice.is_null() || matches!(choice.as_str(), Some("auto" | "required")) {
            None
        } else {
            return Err("不支持的 tool_choice".into());
        };
        let tools = match request["tools"].as_array() {
            Some(tools) if !tools.is_empty() => tools,
            _ if required || selected.is_some() => return Err("tool_choice 要求工具，但请求没有 tools".into()),
            _ => return Ok(None),
        };
        let mut names = Vec::new();
        let parallel = match request.get("parallel_tool_calls") {
            None | Some(Value::Null) => true,
            Some(Value::Bool(value)) => *value,
            Some(_) => return Err("parallel_tool_calls 必须是布尔值".into()),
        };
        for tool in tools {
            if tool["type"].as_str() != Some("function") {
                return Err("仅支持 function 类型的客户端工具".into());
            }
            let name = tool["function"]["name"].as_str().filter(|s| !s.is_empty())
                .ok_or("工具缺少函数名称")?;
            if names.iter().any(|n| n == name) {
                return Err(format!("工具名称重复：{name}"));
            }
            names.push(name.to_string());
        }
        if let Some(name) = selected.as_ref() {
            if !names.contains(name) {
                return Err(format!("tool_choice 指定的工具不存在：{name}"));
            }
        }
        static SERIAL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let nonce = SERIAL.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(Some(Self {
            tools: Value::Array(tools.clone()), names, required, selected, parallel,
            marker: format!("{}_{}", chat_id(), nonce),
        }))
    }

    /// Keep all messages, including assistant tool_calls and tool_call_id results.
    /// Images are rejected instead of silently discarding them.
    pub fn prompt(&self, messages: &Value) -> Result<String, String> {
        let history = structured_history(messages)?;
        let requirement = match self.selected.as_ref() {
            Some(n) => format!("You must call the function named {}.", json!(n)),
            None if self.required => "You must call at least one listed function.".to_string(),
            None => "If no function is needed, respond normally without the markers.".to_string(),
        };
        let parallel_rule = if self.parallel { "" } else { "Return at most one tool call." };
        Ok(format!(
            "You are answering for a client which executes the tools locally. You cannot execute them yourself. \
             Use the tools only by returning a function call to the client. Preserve arguments exactly. \
             When a function is needed, output ONLY the following envelope with valid JSON between the markers, \
             without Markdown or any other text: <{marker}>{{\"tool_calls\":[{{\"name\":\"function_name\",\"arguments\":{{}}}}]}}</{marker}>. \
             Each name must be one of the listed functions, and arguments must be a JSON object matching its parameters. \
             Do not claim the client executed a tool until a later tool message provides the result. {requirement} {parallel_rule}\n\n\
             CLIENT TOOL DEFINITIONS (JSON):\n{tools}\n\n{history}",
            marker = self.marker, tools = self.tools, history = history,
        ))
    }

    pub fn parse(&self, text: &str) -> Result<ToolReply, String> {
        let content = text.trim();
        let begin = format!("<{}>", self.marker);
        let end = format!("</{}>", self.marker);
        let payload = content.strip_prefix(&begin).and_then(|s| s.strip_suffix(&end));
        let Some(payload) = payload else {
            if self.required || self.selected.is_some() || content.contains(&begin) || content.contains(&end) {
                return Err("妙手未按约定返回完整工具调用格式".into());
            }
            return Ok(ToolReply::Text(text.to_string()));
        };
        let data: Value = serde_json::from_str(payload.trim()).map_err(|_| "工具调用不是有效 JSON")?;
        let obj = data.as_object().ok_or("工具调用必须是 JSON 对象")?;
        if obj.len() != 1 || !obj.contains_key("tool_calls") {
            return Err("工具调用格式包含未知字段".into());
        }
        let calls = data["tool_calls"].as_array().filter(|a| !a.is_empty())
            .ok_or("工具调用列表为空或格式无效")?;
        if !self.parallel && calls.len() > 1 {
            return Err("客户端禁止并行工具调用，妙手返回了多个调用".into());
        }
        let mut result = Vec::new();
        for (i, call) in calls.iter().enumerate() {
            let object = call.as_object().ok_or("工具调用必须是对象")?;
            if object.len() != 2 || !object.contains_key("name") || !object.contains_key("arguments") {
                return Err("工具调用包含未知字段或缺少字段".into());
            }
            let name = call["name"].as_str().ok_or("工具调用缺少名称")?;
            if !self.names.iter().any(|n| n == name) || self.selected.as_deref().map_or(false, |n| n != name) {
                return Err(format!("工具调用名称不在允许范围：{name}"));
            }
            if !call["arguments"].is_object() {
                return Err("工具参数必须是 JSON 对象".into());
            }
            let schema = self.tools.as_array().and_then(|tools| tools.iter().find(|t| t["function"]["name"].as_str() == Some(name)))
                .and_then(|t| t["function"].get("parameters"));
            if let Some(schema) = schema { validate_tool_schema(schema, &call["arguments"], 0)?; }
            result.push(ToolCall {
                id: format!("call_{}_{}", self.marker.replace('-', "_"), i),
                name: name.to_string(),
                arguments: call["arguments"].to_string(),
            });
        }
        Ok(ToolReply::Calls(result))
    }
}

/// `desk-` + 16 位 base36（V5() 的原样复刻；chatId 是本地身份，云端照单全收）
fn chat_id() -> String {
    let mut s = String::with_capacity(20);
    s.push_str("desk-");
    let mut last = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64 ^ (d.as_secs() << 20))
        .unwrap_or(0x9E3779B97F4A7C15);
    for _ in 0..16 {
        last = last
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let idx = ((last >> 33) % 36) as usize;
        s.push(['0', '1', '2', '3', '4', '5', '6', '7', '8', '9', 'a', 'b', 'c',
                'd', 'e', 'f', 'g', 'h', 'i', 'j', 'k', 'l', 'm', 'n', 'o', 'p',
                'q', 'r', 's', 't', 'u', 'v', 'w', 'x', 'y', 'z'][idx]);
    }
    s
}

fn chat_headers(tok: &str) -> Vec<(&'static str, String)> {
    vec![
        ("access-token", tok.to_string()),
        ("Cookie", format!("X-Passport-Token={tok}")),
    ]
}

/// 云端报错统一形状；auth 类错误要指路「打开妙手重登」
#[derive(Debug)]
pub struct PawErr {
    pub kind: &'static str,
    pub msg: String,
}

impl PawErr {
    fn auth(msg: impl Into<String>) -> Self {
        PawErr { kind: "auth", msg: msg.into() }
    }
    fn upstream(msg: impl Into<String>) -> Self {
        PawErr { kind: "upstream", msg: msg.into() }
    }
}

/// 一条对话请求的云端链路前三步（create → agent-stream → stream/connect）。
/// 打开之后流还没读——流式路径把 `reader()` 包进 PawTranslator 直接 respond；
/// 非流式路径收完调 `collect_completion`。无论怎么收场，最后都要 `cleanup()`：
/// 云端那一轮要停掉、会话要删掉，不留尾巴烧积分。
pub struct PawRound {
    tok: String,
    chat: String,
    stream_msg: String,
    resp: Option<ureq::Response>,
}

impl PawRound {
    pub fn open(prompt: &str, model: &str) -> Result<PawRound, PawErr> {
        let tok = token();
        if tok.is_empty() {
            return Err(PawErr::auth("没读到登录态（~/.meituan-catpaw/auth.json）：打开妙手登录一次"));
        }
        let chat = chat_id();
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let create = json!({
            "chatId": chat,
            "source": "catdesk",
            "prompt": prompt,
            "images": [],
            "files": [],
            "type": "COMMON",
            "techStackTemplateId": "default",
            "isAgent": true,
        });

        let post = |url: &str, body: &Value, extra: &[(&str, String)]| -> Result<Value, PawErr> {
            let mut call = ureq::post(url).timeout(ROUND_TIMEOUT);
            for (k, v) in chat_headers(&tok) {
                call = call.set(k, &v);
            }
            for (k, v) in extra {
                call = call.set(k, v);
            }
            call = call.set("Content-Type", "application/json");
            match call.send_string(&body.to_string()) {
                Ok(r) => {
                    let j: Value = r
                        .into_json()
                        .map_err(|e| PawErr::upstream(format!("响应不是 JSON：{e}")))?;
                    Ok(j)
                }
                Err(ureq::Error::Status(code, resp)) => {
                    let t = resp.into_string().unwrap_or_default();
                    let msg = serde_json::from_str::<Value>(&t)
                        .ok()
                        .and_then(|v| {
                            v["msg"].as_str().or_else(|| v["message"].as_str()).map(|s| s.to_string())
                        })
                        .unwrap_or_else(|| t.chars().take(200).collect());
                    if code == 401 || code == 403 {
                        Err(PawErr::auth(format!("云端拒了令牌（{code}）：{msg}。打开妙手重新登录一次")))
                    } else {
                        Err(PawErr::upstream(format!("云端 {code}：{msg}")))
                    }
                }
                Err(e) => Err(PawErr::upstream(format!("连不上妙手云端：{e}"))),
            }
        };

        // ① 建会话
        let r1 = post(&format!("{CHAT_HOST}/api/chat/create"), &create, &[])?;
        if r1["code"].as_i64().unwrap_or(-1) != 0 {
            return Err(PawErr::upstream(format!("建会话失败：{}", r1["msg"].as_str().unwrap_or("未知"))));
        }

        // ② 提交本轮消息（conversationId / streamMessageId 全在这条响应里）
        let now_iso = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        let agent = json!({
            "chatId": chat,
            "messages": [{
                "role": "user",
                "createTime": now_iso,
                "content": [{ "type": "text", "text": prompt }],
            }],
            "files": [],
            "urls": [],
            "model": model,
            "manualToolName": "",
            "toolMessageId": "",
            "isAgent": true,
            "currentPath": "/",
            "techStackTemplateId": "default",
            "sourcePlatform": "web",
            "isImportProject": false,
            "frontCreateTime": now_ms,
        });
        let r2 = post(&format!("{CHAT_HOST}/api/chat/agent-stream"), &agent, &[])?;
        if r2["code"].as_i64().unwrap_or(-1) != 0 {
            let _ = delete_chat(&tok, &chat);
            return Err(PawErr::upstream(format!(
                "提交消息失败：{}",
                r2["msg"].as_str().unwrap_or("未知")
            )));
        }
        let conv = r2["data"]["conversationId"].as_str().unwrap_or("").to_string();
        let stream_msg = r2["data"]["streamMessageId"].as_str().unwrap_or("").to_string();
        if conv.is_empty() {
            let _ = delete_chat(&tok, &chat);
            return Err(PawErr::upstream("云端没回 conversationId，收不了流"));
        }

        // ③ 连收流（SSE）。连接失败先取消云端那一轮，别让它空烧积分。
        let connect_body = json!({
            "timestamp": now_ms,
            "conversationId": conv,
            "messageIndex": 0,
            "incrementalOnly": true,
        });
        let mut call = ureq::post(&format!("{STREAM_HOST}/api/agent/stream/connect"))
            .timeout(ROUND_TIMEOUT);
        call = call
            .set("Accept", "text/event-stream")
            .set("Content-Type", "application/json")
            .set("X-Passport-Token", &tok)
            .set("Cookie", &format!("X-Passport-Token={tok}"));
        let resp = match call.send_string(&connect_body.to_string()) {
            Ok(r) => r,
            Err(ureq::Error::Status(code, resp)) => {
                let t = resp.into_string().unwrap_or_default();
                let _ = cancel_round(&tok, &stream_msg);
                let _ = delete_chat(&tok, &chat);
                return Err(if code == 401 || code == 403 {
                    PawErr::auth(format!("收流被拒（{code}）：打开妙手重新登录一次"))
                } else {
                    PawErr::upstream(format!(
                        "收流连接失败（{code}）：{}",
                        t.chars().take(160).collect::<String>()
                    ))
                });
            }
            Err(e) => {
                let _ = cancel_round(&tok, &stream_msg);
                let _ = delete_chat(&tok, &chat);
                return Err(PawErr::upstream(format!("连不上收流网关：{e}")));
            }
        };
        Ok(PawRound { tok, chat, stream_msg, resp: Some(resp) })
    }

    /// 收流的本体。只许取一次。
    pub fn reader(&mut self) -> impl Read + use<> {
        self.resp.take().expect("reader 只能取一次").into_reader()
    }

    /// 收场：停掉云端这一轮 + 删掉这个会话（都 best-effort，失败不影响结果）
    pub fn cleanup(&self) {
        let _ = cancel_round(&self.tok, &self.stream_msg);
        let _ = delete_chat(&self.tok, &self.chat);
    }
}

fn cancel_round(tok: &str, stream_msg: &str) -> Result<(), ()> {
    if stream_msg.is_empty() {
        return Ok(());
    }
    ureq::post(&format!("{CHAT_HOST}/api/chat/resumable-stream/cancel?messageId={stream_msg}"))
        .timeout(Duration::from_secs(8))
        .set("access-token", tok)
        .set("Cookie", &format!("X-Passport-Token={tok}"))
        .call()
        .map(|_| ())
        .map_err(|_| ())
}

fn delete_chat(tok: &str, chat: &str) -> Result<(), ()> {
    ureq::post(&format!("{CHAT_HOST}/api/chat/delete?chatId={chat}"))
        .timeout(Duration::from_secs(8))
        .set("access-token", tok)
        .set("Cookie", &format!("X-Passport-Token={tok}"))
        .call()
        .map(|_| ())
        .map_err(|_| ())
}

// ───────────────────────── 响应翻译（云端 SSE → OpenAI SSE） ─────────────────────────

#[derive(Default, Clone)]
pub struct SseState {
    pub started: bool,
    /// True only on an explicit cloud completed event, never on EOF or cancel.
    pub completed: bool,
    /// 已收的正文与思考（messageId 级增量已并入）
    pub text: String,
    pub reasoning: String,
    pub usage: Value,
    pub finish: Option<String>,
    /// (kind, msg)：kind ∈ auth / upstream
    pub error: Option<(&'static str, String)>,
}

/// 云端 SSE → OpenAI SSE 的边读边翻。帧只有三种要管：
///   `{"headlessRemoteAgentResp":{"message":{...}}}`  助手消息（全量反复推，按 messageId 去重算增量）
///   `{"headlessRemoteAgentResp":{"roundInfo":[...]}}` 计费（creditCost）
///   `{"statusUpdate":"completed"}`                    终局
pub struct PawTranslator<R: Read> {
    inner: R,
    raw: Vec<u8>,
    out: Vec<u8>,
    pos: usize,
    eof: bool,
    done_sent: bool,
    id: String,
    created: i64,
    label: String,
    acc: HashMap<String, (String, String)>,
    state: std::sync::Arc<Mutex<SseState>>,
}

impl<R: Read> PawTranslator<R> {
    pub fn new(inner: R, label: &str) -> Self {
        Self {
            inner,
            raw: Vec::new(),
            out: Vec::new(),
            pos: 0,
            eof: false,
            done_sent: false,
            id: format!("chatcmpl-paw-{}", crate::registry::now_secs()),
            created: crate::registry::now_secs() as i64,
            label: label.to_string(),
            acc: HashMap::new(),
            state: std::sync::Arc::new(Mutex::new(SseState::default())),
        }
    }

    pub fn shared(&self) -> std::sync::Arc<Mutex<SseState>> {
        self.state.clone()
    }

    fn chunk(&self, delta: Value, finish: Option<&str>) -> String {
        let mut c = json!({
            "id": self.id, "object": "chat.completion.chunk", "created": self.created,
            "model": self.label,
            "choices": [{ "index": 0, "delta": delta, "finish_reason": finish }],
        });
        if let Ok(s) = self.state.lock() {
            if !s.usage.is_null() {
                c["usage"] = s.usage.clone();
            }
        }
        format!("data: {}\n\n", c)
    }

    fn handle_block(&mut self, block: &str) {
        if self.done_sent { return; }
        let mut data = String::new();
        for line in block.lines() {
            if let Some(d) = line.strip_prefix("data:") {
                if !data.is_empty() {
                    data.push('\n');
                }
                data.push_str(d.trim());
            }
        }
        if data.is_empty() || data == "[DONE]" {
            return;
        }
        let Ok(j) = serde_json::from_str::<Value>(&data) else { return };

        // 终局帧：completed=正常收场；其余都是非正常，按错误收
        if let Some(st) = j["statusUpdate"].as_str() {
            let (finish, err) = match st {
                "completed" => (Some("stop"), None),
                "canceled" | "cancel" | "user_stopped" => (Some("stop"), None),
                other => (
                    Some("stop"),
                    Some(("upstream", format!("妙手云端回合异常收场：{other}"))),
                ),
            };
            if let Ok(mut s) = self.state.lock() {
                if st == "completed" { s.completed = true; }
                s.finish = finish.map(|f| f.to_string());
                if let Some(e) = err {
                    if s.error.is_none() {
                        s.error = Some(e);
                    }
                }
            }
            if let Some(f) = finish {
                let text = self.chunk(json!({}), Some(f));
                self.out.extend_from_slice(text.as_bytes());
            }
            self.out.extend_from_slice(b"data: [DONE]\n\n");
            self.done_sent = true;
            return;
        }

        let Some(h) = j["headlessRemoteAgentResp"].as_object() else {
            // 不认识的帧不产生任何 chunk（和 Trae 的 metadata 一个待遇）
            if let Some(e) = j["error"].as_str() {
                if let Ok(mut s) = self.state.lock() {
                    if s.error.is_none() {
                        s.error = Some(("upstream", e.to_string()));
                    }
                }
            }
            return;
        };
        if h.contains_key("historyMessages") || h.contains_key("currentStatus") {
            return; // 历史回显 / 状态心跳：不是「已产出内容」
        }
        if let Some(ri) = h.get("roundInfo").and_then(|v| v.as_array()) {
            for r in ri {
                if let Some(c) = r["creditCost"].as_str() {
                    if let Ok(mut s) = self.state.lock() {
                        if let Ok(n) = c.parse::<f64>() {
                            s.usage = json!({
                                "credit": n,
                                "prompt_tokens": 0,
                                "completion_tokens": 0,
                                "total_tokens": 0,
                            });
                        }
                    }
                }
            }
        }
        let Some(msg) = h.get("message").and_then(|v| v.as_object()) else { return };
        let mid = msg["messageId"].as_str().unwrap_or("").to_string();
        let mut delta_c = String::new();
        let mut delta_r = String::new();
        if let Some(content) = msg["content"].as_array() {
            for item in content {
                if item["type"].as_str() != Some("text") {
                    continue; // tool_use / tool_result 是 agent 运行时的内部动作，不给客户端
                }
                let piece = item["text"].as_str().unwrap_or("");
                let think = item["reasoningContent"].as_str().unwrap_or("");
                let (mut acc_c, mut acc_r) = self.acc.get(&mid).cloned().unwrap_or_default();
                // 云端对同一条消息会反复推全量/增量文本，按「新片是旧文的延长」判增量
                if !piece.is_empty() {
                    if piece.starts_with(acc_c.as_str()) {
                        delta_c.push_str(&piece[acc_c.len()..]);
                    } else {
                        delta_c.push_str(piece);
                    }
                    acc_c = if piece.starts_with(acc_c.as_str()) { piece.to_string() } else { acc_c + piece };
                }
                if !think.is_empty() {
                    if think.starts_with(acc_r.as_str()) {
                        delta_r.push_str(&think[acc_r.len()..]);
                    } else {
                        delta_r.push_str(think);
                    }
                    acc_r = if think.starts_with(acc_r.as_str()) { think.to_string() } else { acc_r + think };
                }
                self.acc.insert(mid.clone(), (acc_c, acc_r));
            }
        }
        if delta_c.is_empty() && delta_r.is_empty() {
            return;
        }
        if let Ok(mut s) = self.state.lock() {
            s.started = true;
            s.text.push_str(&delta_c);
            s.reasoning.push_str(&delta_r);
        }
        let mut d = Map::new();
        if !delta_c.is_empty() {
            d.insert("content".into(), json!(delta_c));
        }
        if !delta_r.is_empty() {
            d.insert("reasoning_content".into(), json!(delta_r));
        }
        let text = self.chunk(Value::Object(d), None);
        self.out.extend_from_slice(text.as_bytes());
    }

    fn pump(&mut self) {
        loop {
            let sep = find_sep(&self.raw);
            let Some((i, len)) = sep else { break };
            let block: Vec<u8> = self.raw.drain(..i + len).collect();
            let text = String::from_utf8_lossy(&block[..i]).to_string();
            self.handle_block(&text);
        }
    }
}

fn find_sep(buf: &[u8]) -> Option<(usize, usize)> {
    let mut best: Option<(usize, usize)> = None;
    for pat in [b"\r\n\r\n".as_slice(), b"\n\n".as_slice()] {
        if let Some(p) = buf.windows(pat.len()).position(|w| w == pat) {
            if best.map(|(b, _)| p < b).unwrap_or(true) {
                best = Some((p, pat.len()));
            }
        }
    }
    best
}

impl<R: Read> Read for PawTranslator<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        loop {
            if self.pos < self.out.len() {
                let n = (self.out.len() - self.pos).min(buf.len());
                buf[..n].copy_from_slice(&self.out[self.pos..self.pos + n]);
                self.pos += n;
                if self.pos == self.out.len() {
                    self.out.clear();
                    self.pos = 0;
                }
                return Ok(n);
            }
            if self.done_sent { return Ok(0); }
            if self.eof {
                if !self.done_sent {
                    if let Ok(mut s) = self.state.lock() {
                        if s.finish.is_none() {
                            s.finish = Some("stop".into());
                        }
                    }
                    self.out.extend_from_slice(b"data: [DONE]\n\n");
                    self.done_sent = true;
                    continue;
                }
                return Ok(0);
            }
            let mut tmp = [0u8; 8192];
            let n = match self.inner.read(&mut tmp) {
                Ok(n) => n,
                Err(e) => {
                    if let Ok(mut s) = self.state.lock() {
                        s.error = Some(("upstream", format!("读取妙手响应失败：{e}")));
                    }
                    return Err(e);
                }
            };
            if n == 0 {
                self.eof = true;
                if !self.raw.is_empty() {
                    let tail: Vec<u8> = std::mem::take(&mut self.raw);
                    let text = String::from_utf8_lossy(&tail).to_string();
                    self.handle_block(&text);
                }
                continue;
            }
            self.raw.extend_from_slice(&tmp[..n]);
            self.pump();
        }
    }
}

/// 流式响应收完之后把 OpenAI chunks 翻成一条非流式 `chat.completion`
pub fn collect_completion<R: Read>(tr: &mut PawTranslator<R>, label: &str) -> Value {
    let mut buf = Vec::new();
    let _ = tr.read_to_end(&mut buf);
    let s = tr.shared().lock().map(|g| g.clone()).unwrap_or_default();
    let mut msg = json!({ "role": "assistant", "content": s.text });
    if !s.reasoning.is_empty() {
        msg["reasoning_content"] = json!(s.reasoning);
    }
    json!({
        "id": format!("chatcmpl-paw-{}", crate::registry::now_secs()),
        "object": "chat.completion",
        "created": crate::registry::now_secs() as i64,
        "model": label,
        "choices": [{ "index": 0, "message": msg, "finish_reason": s.finish.unwrap_or_else(|| "stop".into()) }],
        "usage": s.usage,
    })
}

// ───────────────────────── 接线 / 视图 / 一键 ─────────────────────────

/// 桥有没有被挂进某个上游组，以及那条链路上能用的接入密钥
fn wiring() -> (bool, String) {
    let cfg = crate::registry::load_config();
    let pools = cfg["proxy"]["pools"].as_array().cloned().unwrap_or_default();
    let mut hit: Vec<String> = Vec::new();
    for p in &pools {
        let members = p["members"].as_array().cloned().unwrap_or_default();
        let mine = members
            .iter()
            .any(|m| m["src"].as_str().unwrap_or("").starts_with("bridge:catpaw/"));
        if !mine {
            continue;
        }
        if p["enabled"].as_bool().unwrap_or(true) {
            if let Some(id) = p["id"].as_str() {
                hit.push(id.to_string());
            }
        }
    }
    if hit.is_empty() {
        return (false, String::new());
    }
    let dist = cfg["proxy"]["dist"].as_array().cloned().unwrap_or_default();
    for d in dist {
        if !d["enabled"].as_bool().unwrap_or(true) {
            continue;
        }
        let ok = d["pools"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .iter()
            .any(|x| x.as_str().map(|s| hit.iter().any(|h| h == s)).unwrap_or(false));
        if ok {
            return (true, d["key"].as_str().unwrap_or("").to_string());
        }
    }
    (true, String::new())
}

pub fn reveal() -> String {
    wiring().1
}

fn dist_view() -> Value {
    let cfg = crate::registry::load_config();
    for d in cfg["proxy"]["dist"].as_array().cloned().unwrap_or_default() {
        if d["id"].as_str().unwrap_or("") == "dk_pawbridge" {
            let key = d["key"].as_str().unwrap_or("");
            let masked = if key.len() > 8 {
                format!("{}…{}", &key[..5], &key[key.len() - 4..])
            } else {
                String::new()
            };
            return json!({
                "id": "dk_pawbridge",
                "name": d["name"].as_str().unwrap_or(""),
                "masked": masked,
                "calls": d["calls"].as_u64().unwrap_or(0),
            });
        }
    }
    Value::Null
}

/// 给界面看的总览。纯读本地文件 + 一次缓存的余额探测，不发对话请求。
pub fn view() -> Value {
    let acct = auth_account();
    let chk = token_check();
    let (in_pool, key) = wiring();
    let token_ok = chk["ok"].as_bool();

    // 妙手云直连的"可用"只有两关：令牌在、令牌活着。桌面端跑不跑无关。
    let (state, text) = if acct.is_none() {
        ("need_login", "登录态读不到：打开妙手登录一次")
    } else if token_ok == Some(false) {
        ("need_login", chk["msg"].as_str().unwrap_or("令牌已失效，打开妙手重新登录"))
    } else if token_ok.is_none() {
        ("unknown", chk["msg"].as_str().unwrap_or("令牌状态未知（网络？）"))
    } else {
        ("ok", "可用（云端直连，妙手不用开）")
    };

    let age_h = acct
        .as_ref()
        .and_then(|a| a["updated_ms"].as_u64())
        .filter(|m| *m > 0)
        .map(|m| crate::registry::now_secs().saturating_sub(m / 1000) / 3600);

    json!({
        "app": "catpaw",
        "base": BASE_MARK,
        "direct": true,
        "token_ok": token_ok,
        "token_msg": chk["msg"].as_str().unwrap_or(""),
        "balance": chk["balance"].clone(),
        "account": acct,
        "token_age_h": age_h,
        "ready": state == "ok",
        "state": state,
        "text": text,
        "models": models(),
        "in_pool": in_pool,
        "key": key,
        "dist": dist_view(),
        "accounts": accounts(state == "ok", acct.as_ref(), age_h),
    })
}

pub fn probe() -> Value {
    clear_cache();
    view()
}

fn accounts(ready: bool, acct: Option<&Value>, age_h: Option<u64>) -> Vec<Value> {
    let calls = dist_view()["calls"].as_u64().unwrap_or(0);
    let Some(a) = acct else {
        return Vec::new();
    };
    let name = a["name"].as_str().unwrap_or("").to_string();
    let uid = a["uid"].as_str().unwrap_or("");
    vec![json!({
        "name": if name.is_empty() { "（没读到昵称）".to_string() } else { name },
        "region": "国内",
        "state": if ready { "ok" } else { "off" },
        "text": if ready { "可用" } else { "登录失效" },
        "token_mask": if uid.is_empty() { String::new() } else { format!("uid {uid}") },
        "token_age_h": age_h,
        "calls": calls,
    })]
}

/// 一键：验令牌（不花积分）→ 接线（缺才建）→ 起反代。
/// 不再有"拉起妙手、开通道、起桥"——云端直连后这些环节整个消失了。
pub fn one_click(proxy: &std::sync::Arc<crate::proxy::Proxy>) -> Value {
    let mut steps: Vec<String> = Vec::new();

    let tok = token();
    if tok.is_empty() {
        let r = open_login();
        return json!({ "ok": false, "msg": r["msg"].clone(), "steps": steps });
    }
    let chk = token_check();
    if chk["ok"].as_bool() == Some(false) {
        return json!({ "ok": false, "msg": chk["msg"].as_str().unwrap_or("令牌已失效"), "steps": steps });
    }
    steps.push("登录态可用".into());

    let acct = auth_account();
    let who = acct
        .as_ref()
        .and_then(|a| a["name"].as_str())
        .unwrap_or("古零")
        .to_string();
    let (_pid, key, wire_steps) = crate::gateway::ensure_wiring(
        "gp_pawbridge",
        "妙手 上游组",
        &format!("bridge:catpaw/{who}"),
        "dk_pawbridge",
        "妙手 接入",
    );
    steps.extend(wire_steps);
    if !key.is_empty() {
        steps.push("接入密钥已就绪".into());
    }

    if !proxy.is_running() {
        let port = crate::qproxy::proxy_port();
        proxy.set_config(Some("https://api.openai.com"), None);
        let mut cfg = crate::registry::load_config();
        if !cfg["proxy"].is_object() {
            cfg["proxy"] = json!({});
        }
        cfg["proxy"]["port"] = json!(port);
        crate::registry::save_config(&cfg);
        let (o, m) = proxy.start(port);
        if !o {
            return json!({ "ok": false, "msg": format!("反代起不来：{m}"), "steps": steps });
        }
        steps.push(format!("反代已启动 127.0.0.1:{port}"));
    } else {
        steps.push("反代已在运行".into());
    }
    clear_cache();
    let v = view();
    json!({
        "ok": v["ready"].as_bool().unwrap_or(false),
        "msg": if v["ready"].as_bool().unwrap_or(false) {
            format!("反代已可用：{}", v["text"].as_str().unwrap_or(""))
        } else {
            v["text"].as_str().unwrap_or("还差一步").to_string()
        },
        "steps": steps,
        "state": v["state"].clone(),
    })
}

/// 拿云端发一次极小请求（真花积分，实测一轮 ~1.34），确认整条链路真的能出字
pub fn test(model: &str) -> Value {
    if token().is_empty() {
        return json!({ "ok": false, "error": "没读到登录态：打开妙手登录一次" });
    }
    let want = if model.trim().is_empty() { "LongCat-2.0" } else { model.trim() };
    let list = models();
    let used = model_for_stream(want, &list);
    let prompt = "只回复两个字：可用";
    let t0 = Instant::now();
    let mut round = match PawRound::open(prompt, &used) {
        Ok(r) => r,
        Err(e) => return json!({ "ok": false, "error": e.msg, "kind": e.kind }),
    };
    let display = want.rsplit_once('/').map(|(_, b)| b).unwrap_or(want);
    let mut tr = PawTranslator::new(round.reader(), display);
    let out = collect_completion(&mut tr, display);
    round.cleanup();
    let s = tr.shared().lock().map(|g| g.clone()).unwrap_or_default();
    if let Some((kind, msg)) = &s.error {
        return json!({ "ok": false, "error": msg, "kind": kind });
    }
    let text = s.text.clone();
    let ok = text.to_lowercase().contains("可用");
    json!({
        "ok": ok,
        "text": text,
        "ms": t0.elapsed().as_millis() as u64,
        "model_used": out["model"].as_str().unwrap_or(display),
        "usage": s.usage,
        "error": if ok { "" } else { "云端回了别的内容" },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn adapter_request(choice: Value) -> Value {
        json!({"tools":[{"type":"function","function":{"name":"read_file","parameters":{
            "type":"object","properties":{"path":{"type":"string","minLength":1}},
            "required":["path"],"additionalProperties":false
        }}}],"tool_choice":choice})
    }

    #[test]
    fn tool_adapter_preserves_history_and_emits_validated_calls() {
        let adapter = ToolAdapter::from_request(&adapter_request(json!("auto"))).unwrap().unwrap();
        let messages = json!([
            {"role":"assistant","content":null,"tool_calls":[{"id":"call_old","type":"function","function":{"name":"read_file","arguments":"{\"path\":\"a.txt\"}"}}]},
            {"role":"tool","tool_call_id":"call_old","content":"file data"},
            {"role":"user","content":"next"}
        ]);
        let prompt = adapter.prompt(&messages).unwrap();
        assert!(prompt.contains("call_old"));
        assert!(prompt.contains("a.txt"));
        assert!(prompt.contains("file data"));
        assert!(has_tool_history(&messages));
        let answer = format!("<{}>{{\"tool_calls\":[{{\"name\":\"read_file\",\"arguments\":{{\"path\":\"b.txt\"}}}}]}}</{}>", adapter.marker, adapter.marker);
        match adapter.parse(&answer).unwrap() {
            ToolReply::Calls(calls) => {
                assert_eq!(calls.len(), 1);
                assert_eq!(calls[0].name, "read_file");
                assert_eq!(serde_json::from_str::<Value>(&calls[0].arguments).unwrap()["path"], "b.txt");
                assert!(calls[0].id.starts_with("call_"));
            }
            _ => panic!("expected tool call"),
        }
        assert!(matches!(adapter.parse("normal response").unwrap(), ToolReply::Text(_)));
    }

    #[test]
    fn tool_adapter_rejects_bad_calls_and_respects_choice() {
        let adapter = ToolAdapter::from_request(&adapter_request(json!("required"))).unwrap().unwrap();
        assert!(adapter.parse("normal response").is_err());
        for body in [
            r#"{"tool_calls":[{"name":"unknown","arguments":{"path":"a"}}]}"#,
            r#"{"tool_calls":[{"name":"read_file","arguments":"{}"}]}"#,
            r#"{"tool_calls":[{"name":"read_file","arguments":{}}]}"#,
            r#"{"tool_calls":[{"name":"read_file","arguments":{"path":"a","extra":1}}]}"#,
            r#"{"tool_calls":[]}"#,
        ] {
            let answer = format!("<{}>{}</{}>", adapter.marker, body, adapter.marker);
            assert!(adapter.parse(&answer).is_err(), "accepted {body}");
        }
        let chosen = ToolAdapter::from_request(&adapter_request(json!({"type":"function","function":{"name":"read_file"}}))).unwrap().unwrap();
        assert!(chosen.parse("no call").is_err());
        assert!(ToolAdapter::from_request(&adapter_request(json!("none"))).unwrap().is_none());
        assert!(ToolAdapter::from_request(&adapter_request(json!({"type":"function","function":{"name":"other"}}))).is_err());
        assert!(adapter.prompt(&json!([{"role":"user","content":[{"type":"image_url","image_url":{"url":"x"}}]}])).is_err());
        assert!(validate_tool_schema(&json!({"type":"string","pattern":"x"}), &json!("x"), 0).is_err());
        let history = json!([{"role":"assistant","tool_calls":[{"id":"call_7","function":{"name":"read_file","arguments":"{}"}}]}, {"role":"tool","tool_call_id":"call_7","content":"result"}]);
        assert!(structured_history(&history).unwrap().contains("call_7"));
        let mut serial_request = adapter_request(json!("auto"));
        serial_request["parallel_tool_calls"] = json!(false);
        let serial = ToolAdapter::from_request(&serial_request).unwrap().unwrap();
        let body = r#"{"tool_calls":[{"name":"read_file","arguments":{"path":"a"}},{"name":"read_file","arguments":{"path":"b"}}]}"#;
        let answer = format!("<{}>{}</{}>", serial.marker, body, serial.marker);
        assert!(serial.parse(&answer).is_err());
        assert!(serial.prompt(&json!([])).unwrap().contains("at most one tool call"));
        serial_request["parallel_tool_calls"] = json!("false");
        assert!(ToolAdapter::from_request(&serial_request).is_err());
    }

    #[test]
    fn base_mark_is_recognised() {
        assert!(is_paw_base(BASE_MARK));
        assert!(is_paw_base("catpaw://agent/"));
        assert!(!is_paw_base("http://127.0.0.1:8789"));
    }

    #[test]
    fn flatten_keeps_roles_and_skips_empty() {
        let msgs = json!([
            {"role":"system","content":"你是助手"},
            {"role":"user","content":"你好"},
            {"role":"assistant","content":[{"type":"text","text":"在"}]},
            {"role":"user","content":""}
        ]);
        let p = flatten(&msgs);
        assert!(p.starts_with("【系统设定】"));
        assert!(p.contains("【助手】"));
        assert!(p.ends_with("【用户】\n你好") || p.contains("【用户】"));
    }

    #[test]
    fn chat_id_has_desk_prefix_and_base36() {
        let id = chat_id();
        assert!(id.starts_with("desk-"));
        assert_eq!(id.len(), 21);
        assert!(id[5..].chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()));
    }

    #[test]
    fn model_for_stream_maps_to_numeric_id() {
        let list = vec![
            json!({"id": "LongCat-2.0", "numeric": 77}),
            json!({"id": "kimi-k3", "numeric": 12}),
            json!({"id": "glm-5.3-flashx", "numeric": 98}),
        ];
        // 云端只认数字 id 字符串（"98"），名字一律映射过去
        assert_eq!(model_for_stream("catpaw/LongCat-2.0", &list), "77");
        assert_eq!(model_for_stream("catpaw/glm-5.3-flashx", &list), "98");
        assert_eq!(model_for_stream("glm-5.3-flashx", &list), "98");
        // 纯数字直接用（模型 ID 表里复制出来的 numeric）
        assert_eq!(model_for_stream("98", &list), "98");
        assert_eq!(model_for_stream("auto", &list), "auto");
        assert_eq!(model_for_stream("", &list), "auto");
        // 认不出的原样透传，让云端自己判
        assert_eq!(model_for_stream("mystery-model", &list), "mystery-model");
    }

    #[test]
    fn sse_translation_handles_full_and_incremental() {
        // 云端同一条消息先推全量"你好"，再推全量"你好呀"——第二帧的增量只能是"呀"
        let raw = concat!(
            "data: {\"headlessRemoteAgentResp\":{\"isHistoryMessage\":true,\"historyMessages\":[]}}\n\n",
            "data: {\"headlessRemoteAgentResp\":{\"currentStatus\":\"running\"}}\n\n",
            "data: {\"headlessRemoteAgentResp\":{\"message\":{\"type\":\"assistant\",\"messageId\":\"m1\",\"content\":[{\"type\":\"text\",\"text\":\"你好\"}],\"finished\":false}}}\n\n",
            "data: {\"headlessRemoteAgentResp\":{\"message\":{\"type\":\"assistant\",\"messageId\":\"m1\",\"content\":[{\"type\":\"text\",\"text\":\"你好呀\"}],\"finished\":true}}}\n\n",
            "data: {\"headlessRemoteAgentResp\":{\"roundInfo\":[{\"round\":1,\"creditCost\":\"1.34000000\"}]}}\n\n",
            "data: {\"statusUpdate\":\"completed\"}\n\n"
        );
        let mut t = PawTranslator::new(std::io::Cursor::new(raw.as_bytes().to_vec()), "LongCat-2.0");
        let st = t.shared();
        let mut out = String::new();
        t.read_to_string(&mut out).unwrap();
        assert!(out.contains("你好"));
        assert!(out.contains("呀"), "第二帧要只推增量：{out}");
        assert!(out.contains("\"finish_reason\":\"stop\""));
        assert!(out.contains("data: [DONE]"));
        assert!(!out.contains("historyMessages"), "历史回显不许产生 chunk");
        let s = st.lock().unwrap();
        assert_eq!(s.text, "你好呀");
        assert_eq!(s.usage["credit"], 1.34);
        assert!(s.started);
        assert!(s.completed);
    }

    #[test]
    fn sse_error_status_terminates_with_error() {
        let raw = "data: {\"statusUpdate\":\"error\"}\n\ndata: {\"statusUpdate\":\"completed\"}\n\n";
        let mut t = PawTranslator::new(std::io::Cursor::new(raw.as_bytes().to_vec()), "LongCat-2.0");
        let st = t.shared();
        let mut out = String::new();
        t.read_to_string(&mut out).unwrap();
        assert!(out.contains("data: [DONE]"));
        let s = st.lock().unwrap();
        assert!(s.error.is_some(), "异常终局要落 error");
    }

    #[test]
    fn sse_eof_and_cancel_do_not_mark_completed() {
        for raw in ["", "data: {\"statusUpdate\":\"canceled\"}\n\n", "data: {\"statusUpdate\":\"canceled\"}\n\ndata: {\"statusUpdate\":\"completed\"}\n\n"] {
            let mut t = PawTranslator::new(std::io::Cursor::new(raw.as_bytes().to_vec()), "test");
            let st = t.shared();
            t.read_to_end(&mut Vec::new()).unwrap();
            assert!(!st.lock().unwrap().completed);
        }
    }

    #[test]
    fn sse_read_failure_is_recorded() {
        struct FailedReader;
        impl std::io::Read for FailedReader {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "connection lost"))
            }
        }
        let mut t = PawTranslator::new(FailedReader, "test");
        let st = t.shared();
        assert!(t.read_to_end(&mut Vec::new()).is_err());
        let s = st.lock().unwrap();
        assert!(!s.completed);
        assert!(s.error.is_some());
    }

    #[test]
    fn separator_handles_crlf() {
        assert_eq!(find_sep(b"data:1\r\n\r\n"), Some((6, 4)));
        assert_eq!(find_sep(b"data:1\n\n"), Some((6, 2)));
        assert_eq!(find_sep(b"data:1"), None);
    }

    /// 真机云端直连：一轮完整对话（建会话 → 提交 → 收流 → 清理）。
    /// 会真花积分（实测一轮 ~1.34）。
    /// `cargo test live_cloud_roundtrip -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn live_cloud_roundtrip() {
        let mut round = PawRound::open("只回复两个字：可用", "LongCat-2.0")
            .expect("云端链路没打通");
        let mut tr = PawTranslator::new(round.reader(), "LongCat-2.0");
        let out = collect_completion(&mut tr, "LongCat-2.0");
        round.cleanup();
        println!("{}", serde_json::to_string_pretty(&out).unwrap());
        assert_eq!(out["object"], "chat.completion");
        assert!(
            out["choices"][0]["message"]["content"]
                .as_str()
                .unwrap_or("")
                .contains("可用"),
            "真模型没回预期内容"
        );
    }

    /// 真机端到端：一键反代 → 用接入密钥打一条真实补全（流式 + 非流式）。
    /// 会临时改写 config.json（结束原样还原），并真的花掉几个积分。
    /// `cargo test real_paw_oneclick_serves_a_completion -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn real_paw_oneclick_serves_a_completion() {
        struct CfgGuard(Option<String>, PathBuf);
        impl Drop for CfgGuard {
            fn drop(&mut self) {
                match &self.0 {
                    Some(s) => {
                        let _ = std::fs::write(&self.1, s);
                    }
                    None => {
                        let _ = std::fs::remove_file(&self.1);
                    }
                }
            }
        }
        let cfg = crate::registry::config_file();
        let _guard = CfgGuard(std::fs::read_to_string(&cfg).ok(), cfg);

        let p = std::sync::Arc::new(crate::proxy::Proxy::shared());
        let port = 18999u16;
        let (ok, msg) = p.start(port);
        assert!(ok, "{msg}");

        let r = one_click(&p);
        println!("one_click: {}", serde_json::to_string_pretty(&r).unwrap());
        assert!(r["ok"].as_bool().unwrap_or(false), "一键没成功：{r}");
        let key = reveal();
        assert!(key.starts_with("ac-"), "接入密钥不对：{key}");

        // 非流式：要一条 chat.completion
        let body = json!({
            "model": "catpaw/LongCat-2.0",
            "messages": [{ "role": "user", "content": "只回复两个字：可用" }],
            "stream": false,
        })
        .to_string();
        let res = ureq::post(&format!("http://127.0.0.1:{port}/v1/chat/completions"))
            .timeout(Duration::from_secs(300))
            .set("Content-Type", "application/json")
            .set("Authorization", &format!("Bearer {key}"))
            .send_string(&body);
        match res {
            Ok(x) => {
                assert_eq!(x.status(), 200);
                let v: Value = x.into_json().unwrap_or(json!({}));
                println!("非流式：{}", serde_json::to_string_pretty(&v).unwrap());
                assert_eq!(v["object"], "chat.completion");
                assert!(v["choices"][0]["message"]["content"]
                    .as_str()
                    .unwrap_or("")
                    .contains("可用"));
            }
            Err(ureq::Error::Status(c, x)) => {
                let s = x.into_string().unwrap_or_default();
                panic!("反代返回 {c}：{}", s.chars().take(400).collect::<String>());
            }
            Err(e) => panic!("反代请求失败：{e}"),
        }

        // 流式：要 OpenAI SSE，且以 [DONE] 收尾
        let body = json!({
            "model": "catpaw/LongCat-2.0",
            "messages": [{ "role": "user", "content": "只回复两个字：可用" }],
            "stream": true,
        })
        .to_string();
        let res = ureq::post(&format!("http://127.0.0.1:{port}/v1/chat/completions"))
            .timeout(Duration::from_secs(300))
            .set("Content-Type", "application/json")
            .set("Authorization", &format!("Bearer {key}"))
            .send_string(&body);
        let mut stream = String::new();
        match res {
            Ok(x) => {
                assert_eq!(x.status(), 200);
                let _ = std::io::Read::read_to_string(&mut x.into_reader(), &mut stream);
            }
            Err(e) => panic!("流式请求失败：{e}"),
        }
        assert!(
            stream.contains("chat.completion.chunk"),
            "不是 OpenAI 流：{}",
            &stream[..stream.len().min(300)]
        );
        assert!(stream.contains("data: [DONE]"));

        p.stop();
    }
}
