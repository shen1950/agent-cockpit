// Trae CN 的反代上游：把「账号档案里的登录态」变成 OpenAI 兼容上游。
//
// 协议底账（全部本机实测）：`memory/reference-trae-reverse-proxy.md`。
// 三条决定代码形状的事实：
//
//   1. 对话端点认 `X-User-Real-IP` = **本机出口 IP**。缺了、填回环、填假 IP 一律回
//      SSE `event:error` `code 4011`，消息写的是"exceeded the rate limit"——**是假话**。
//      所以这个头每笔请求都要现算；收到 4011 要先重算 IP 再重试一次，绝不能按限流去冷却/换号。
//   2. `4001` 是「通道×模型配对不存在」，属**请求内容错**，换号与冷却都无用。
//   3. 凭据解密与 SQLite（`state.vscdb` 里的模型目录）都交给 agenthub，与其余家一致。
//      这里只做调用与缓存，**不碰 refreshToken**（一次性，用一次就把 IDE 踢下线）。
use crate::registry::{base_dir, load_instances, now_secs};
use serde_json::{json, Map, Value};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::time::{Duration, Instant};

#[cfg(windows)]
use std::os::windows::process::CommandExt;
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// 驾驶舱程序 id（只有一个：TRAE CN 的登录态整机一份，多开不共存）
pub const APPS: [&str; 1] = ["traework-cn"];
pub const AGENT_HOST: &str = "https://trae-api-cn.mchost.guru";
pub const CHAT_PATH: &str = "/api/agent/v3/llm_utils_chat";
pub const APP_ID: &str = "6eefa01c-1036-4c7e-9ca5-d891f63bfcd8";
/// 上游组里标记「这条上游是 Trae」的 base_url，`proxy.rs` 靠它进翻译分支
pub const BASE_MARK: &str = "trae://agent";
/// 请求体必填的 int64 版本码。20260924 取自真 IDE 会话元数据（product.json 的 20250325 也能过，
/// 两个都不是闸门，取新的那个）
pub const APP_VERSION_CODE: i64 = 20260924;
/// 上游声明的输出上限，超过会被打成 4xx
const MAX_OUT: i64 = 64_000;

pub fn is_trae_base(base: &str) -> bool {
    base.trim().trim_end_matches('/') == BASE_MARK
}

pub fn app_display(app_id: &str) -> String {
    crate::registry::all_apps()
        .into_iter()
        .find(|a| a.id == app_id)
        .map(|a| a.name)
        .unwrap_or_else(|| app_id.to_string())
}

// ───────────────────────── 调 agenthub ─────────────────────────

fn agenthub_json(extra: &[&str]) -> Result<Value, String> {
    // agenthub 是 .cmd 包装，CreateProcess 直接执行不了，必须经 cmd.exe 转一手
    let mut c = Command::new("cmd.exe");
    let mut args: Vec<&str> = vec!["/d", "/c", "agenthub", "trae"];
    args.extend_from_slice(extra);
    c.args(&args);
    #[cfg(windows)]
    c.creation_flags(CREATE_NO_WINDOW);
    let out = c
        .output()
        .map_err(|e| format!("调不到 agenthub（{e}）：本机需要先装 agent-memory-suite"))?;
    if !out.status.success() {
        let s = String::from_utf8_lossy(&out.stderr);
        return Err(format!(
            "agenthub trae 失败：{}",
            s.trim().chars().take(180).collect::<String>()
        ));
    }
    serde_json::from_slice(&out.stdout).map_err(|e| format!("agenthub 输出不是 JSON：{e}"))
}

// ───────────────────────── 凭据 ─────────────────────────

#[derive(Clone)]
pub struct Cred {
    pub token: String,
    pub user_id: String,
    pub expires_at: String,
    pub hours_left: Option<f64>,
    pub who: Value,
    pub source: String,
}

/// key = 档案目录 + storage.json 的 mtime。IDE 自己刷新令牌会改 mtime，
/// 这里跟着失效重读，不需要我们碰 refreshToken。
static CRED_CACHE: Mutex<Option<(String, Result<Cred, String>)>> = Mutex::new(None);

fn storage_path(dir: &Path) -> PathBuf {
    dir.join("User").join("globalStorage").join("storage.json")
}

fn cache_key(dir: &Path) -> String {
    let m = std::fs::metadata(storage_path(dir))
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format!("{}|{}", dir.display(), m)
}

pub fn cred_for(dir: &Path) -> Result<Cred, String> {
    let key = cache_key(dir);
    if key.ends_with("|0") {
        return Err(format!(
            "{} 里没有 User/globalStorage/storage.json（这个号还没在这台机器上登录过）",
            dir.display()
        ));
    }
    if let Ok(g) = CRED_CACHE.lock() {
        if let Some((k, v)) = g.as_ref() {
            if *k == key {
                return v.clone();
            }
        }
    }
    let got = agenthub_json(&["--token", "--profile", &dir.display().to_string()])
        .and_then(|v| {
            if v["ok"].as_bool() != Some(true) {
                return Err(v["error"].as_str().unwrap_or("agenthub 没返回 token").to_string());
            }
            let token = v["token"].as_str().unwrap_or("").to_string();
            if token.is_empty() {
                return Err("agenthub 回的 token 是空的".into());
            }
            Ok(Cred {
                token,
                user_id: v["user_id"].as_str().unwrap_or("").to_string(),
                expires_at: v["expires_at"].as_str().unwrap_or("").to_string(),
                hours_left: v["hours_left"].as_f64(),
                who: v["who"].clone(),
                source: v["from"].as_str().unwrap_or("").to_string(),
            })
        });
    if let Ok(mut g) = CRED_CACHE.lock() {
        *g = Some((key, got.clone()));
    }
    got
}

pub fn invalidate_cred() {
    if let Ok(mut g) = CRED_CACHE.lock() {
        *g = None;
    }
}

// ───────────────────────── 出口 IP ─────────────────────────

static IP_CACHE: Mutex<Option<(String, Instant)>> = Mutex::new(None);

/// 本机出口 IP。服务端拿它和连接的来源做核对，**必须是真的那个**。
/// 用 UDP connect 读 `local_addr`（不发包），失败回落主机名解析。
pub fn egress_ip_fresh() -> Option<String> {
    use std::net::UdpSocket;
    if let Ok(s) = UdpSocket::bind("0.0.0.0:0") {
        if s.connect("8.8.8.8:80").is_ok() {
            if let Ok(a) = s.local_addr() {
                let ip = a.ip().to_string();
                if !ip.is_empty() && ip != "0.0.0.0" {
                    return Some(ip);
                }
            }
        }
    }
    let host = std::env::var("COMPUTERNAME").unwrap_or_default();
    use std::net::ToSocketAddrs;
    if !host.is_empty() {
        if let Ok(mut it) = (host.as_str(), 0u16).to_socket_addrs() {
            if let Some(a) = it.next() {
                return Some(a.ip().to_string());
            }
        }
    }
    None
}

/// 带 60 秒缓存；4011 时用 `egress_ip_fresh` 强制重算。
pub fn egress_ip() -> Option<String> {
    if let Ok(g) = IP_CACHE.lock() {
        if let Some((ip, at)) = g.as_ref() {
            if at.elapsed() < Duration::from_secs(60) {
                return Some(ip.clone());
            }
        }
    }
    let ip = egress_ip_fresh();
    if let (Some(ip), Ok(mut g)) = (ip.clone(), IP_CACHE.lock()) {
        *g = Some((ip, Instant::now()));
    }
    ip
}

pub fn invalidate_ip() {
    if let Ok(mut g) = IP_CACHE.lock() {
        *g = None;
    }
}

// ───────────────────────── 模型目录 ─────────────────────────

/// 各通道优先级（同名模型跨通道时按这个顺序挑，调用方也可以用 `channel/model` 显式指定）
pub const CHANNEL_ORDER: [&str; 9] = [
    "solo_agent",
    "solo_work_lite",
    "solo_agent_remote",
    "solo_work_remote",
    "solo_agent_lite",
    "solo_coder",
    "solo_design_lite",
    "solo_design_remote",
    "agent",
];

static MODEL_CACHE: Mutex<Option<(String, Value, i64)>> = Mutex::new(None);

/// 读 `state.vscdb` 里的模型目录。返回 (channels, source_mtime_ms)。
/// 缓存 60 秒——agenthub 起一次 python 要一两秒，不能每笔请求都调。
pub fn models_for(dir: &Path) -> Result<(Value, i64), String> {
    let key = dir.display().to_string();
    if let Ok(g) = MODEL_CACHE.lock() {
        if let Some((k, ch, mt)) = g.as_ref() {
            if *k == key {
                return Ok((ch.clone(), *mt));
            }
        }
    }
    let v = agenthub_json(&["--models", "--profile", &key])?;
    if v["ok"].as_bool() != Some(true) {
        return Err(v["error"].as_str().unwrap_or("读不到模型目录").to_string());
    }
    let ch = v["channels"].clone();
    let mt = v["source_mtime"].as_i64().unwrap_or(0);
    if let Ok(mut g) = MODEL_CACHE.lock() {
        *g = Some((key, ch.clone(), mt));
    }
    Ok((ch, mt))
}

/// 模型 → 通道。显式 `channel/model` 优先；否则按 `CHANNEL_ORDER` 找第一个含它的通道。
pub fn channel_for(channels: &Value, model: &str) -> Option<String> {
    if let Some((ch, m)) = model.split_once('/') {
        if channels.get(ch).and_then(|v| v.as_array()).is_some() {
            return Some(format!("{}|{}", ch, m));
        }
    }
    for ch in CHANNEL_ORDER {
        if let Some(arr) = channels.get(ch).and_then(|v| v.as_array()) {
            if arr.iter().any(|m| m["config_name"].as_str() == Some(model)) {
                return Some(ch.to_string());
            }
        }
    }
    None
}

/// 全部可选模型（按 CHANNEL_ORDER 去重），给 `/v1/models`
pub fn model_list(channels: &Value) -> Vec<Value> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for ch in CHANNEL_ORDER {
        let Some(arr) = channels.get(ch).and_then(|v| v.as_array()) else {
            continue;
        };
        for m in arr {
            let name = m["config_name"].as_str().unwrap_or("");
            if name.is_empty() || !seen.insert(name.to_string()) {
                continue;
            }
            out.push(json!({
                "id": name,
                "object": "model",
                "owned_by": "trae",
                "channel": ch,
                "context_window": m["context_window"].clone(),
                "max_tokens": m["max_tokens"].clone(),
                "thinking": m["thinking"].clone(),
                "multimodal": m["multimodal"].clone(),
            }));
        }
    }
    out
}

// ───────────────────────── 错误分类 ─────────────────────────

/// 三维分类：**账号 / 模型 / 请求内容**。这是好几个外部项目踩出来的同一条教训——
/// 把内容级错误判成账号故障，会让整池白休息。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    None,
    /// 客户端身份没过（4011 / 401 / 1001）。**不是限流**：重算出口 IP 再试一次。
    Identity,
    /// 请求内容错（4001 通道×模型不存在、400）。不重试、不换号、不冷却。
    BadRequest,
    /// 积分耗尽（4008）。账号级长冷却。
    Quota,
    /// 权益不足（1005）。账号级冷却。
    Plan,
    /// 模型下架/不存在（404）。该模型短冷却。
    ModelDead,
    /// 上游 5xx。
    Server,
}

impl Kind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Kind::None => "ok",
            Kind::Identity => "identity",
            Kind::BadRequest => "bad_request",
            Kind::Quota => "quota",
            Kind::Plan => "plan",
            Kind::ModelDead => "model_dead",
            Kind::Server => "server",
        }
    }
    /// 只有身份类错误值得「重算 IP 后重试一次」
    pub fn retry_with_new_ip(&self) -> bool {
        *self == Kind::Identity
    }
    /// 会拖累整个账号的错误——**`BadRequest` 永远不在这里**
    pub fn blames_account(&self) -> bool {
        matches!(self, Kind::Quota | Kind::Plan)
    }
}

pub fn classify(code: i64, msg: &str) -> Kind {
    let m = msg.to_lowercase();
    match code {
        0 => Kind::None,
        4001 => Kind::BadRequest,
        4008 => Kind::Quota,
        1005 => Kind::Plan,
        4011 | 401 | 1001 => Kind::Identity,
        404 => Kind::ModelDead,
        c if c >= 500 => Kind::Server,
        c if c >= 400 => Kind::BadRequest,
        _ => {
            // 有些错误只在 message 里带码
            if m.contains("4011") || m.contains("not able to authenticate") {
                Kind::Identity
            } else if m.contains("exceeded the quota") || m.contains("4008") {
                Kind::Quota
            } else {
                Kind::Server
            }
        }
    }
}

// ───────────────────────── 请求翻译 ─────────────────────────

fn content_to_blocks(v: &Value) -> Value {
    match v {
        Value::String(s) => json!([{ "type": "text", "text": s }]),
        other => other.clone(),
    }
}

fn msg_to_solo(msg: &Value) -> Value {
    let mut o = msg.as_object().cloned().unwrap_or_default();
    if o.get("content").is_some() {
        let c = content_to_blocks(&o["content"]);
        o.insert("content".into(), c);
    }
    // assistant 的 tool_calls: function → function_call
    if o.get("role").and_then(|r| r.as_str()) == Some("assistant") {
        if let Some(arr) = o.get("tool_calls").and_then(|v| v.as_array()).cloned() {
            let kept: Vec<Value> = arr
                .iter()
                .filter_map(|tc| {
                    let mut t = tc.as_object()?.clone();
                    if let Some(f) = t.remove("function") {
                        t.insert("function_call".into(), f);
                    }
                    let name = t["function_call"]["name"].as_str().unwrap_or("").trim().to_string();
                    if name.is_empty() {
                        return None;
                    }
                    Some(Value::Object(t))
                })
                .collect();
            if kept.is_empty() {
                o.remove("tool_calls");
            } else {
                o.insert("tool_calls".into(), Value::Array(kept));
            }
        }
    }
    Value::Object(o)
}

fn tools_to_solo(tools: &Value) -> Value {
    let Some(arr) = tools.as_array() else {
        return tools.clone();
    };
    Value::Array(
        arr.iter()
            .map(|t| {
                let mut t = t.clone();
                if let Some(p) = t.get_mut("function").and_then(|f| f.get_mut("parameters")) {
                    // SOLO 要 JSON **字符串**；转换之后再补会静默失败
                    if p.is_object() || p.is_array() {
                        *p = Value::String(serde_json::to_string(p).unwrap_or_default());
                    }
                }
                t
            })
            .collect(),
    )
}

/// OpenAI 请求 → SOLO 请求。只白名单转发已知字段，避免上游把陌生字段判成 4001。
pub fn translate_request(openai: &Value, channel: &str, model: &str) -> Value {
    let mut out = Map::new();
    if let Some(msgs) = openai.get("messages").and_then(|v| v.as_array()) {
        out.insert("messages".into(), Value::Array(msgs.iter().map(msg_to_solo).collect()));
    }
    for k in ["temperature", "top_p", "stop", "presence_penalty", "frequency_penalty"] {
        if let Some(v) = openai.get(k) {
            if !v.is_null() {
                out.insert(k.into(), v.clone());
            }
        }
    }
    if let Some(t) = openai.get("tools") {
        if t.as_array().map(|a| !a.is_empty()).unwrap_or(false) {
            out.insert("tools".into(), tools_to_solo(t));
            // tool_choice 归一化；"none" 要连带删 tools
            match openai.get("tool_choice") {
                Some(Value::String(s)) if s == "none" => {
                    out.remove("tools");
                }
                Some(Value::String(s)) => {
                    out.insert("tool_choice".into(), json!(s));
                }
                Some(Value::Object(o)) => {
                    let s = match o.get("type").and_then(|v| v.as_str()).unwrap_or("") {
                        "function" => o["function"]["name"].as_str().unwrap_or("auto").to_string(),
                        other => other.to_string(),
                    };
                    out.insert("tool_choice".into(), json!(s));
                }
                _ => {}
            }
        }
    }
    let mt = openai.get("max_tokens").and_then(|v| v.as_i64()).unwrap_or(0);
    let mt = if mt <= 0 { MAX_OUT } else { mt.min(MAX_OUT) };
    out.insert("max_tokens".into(), json!(mt));
    out.insert("model".into(), json!(model));
    out.insert("config_name".into(), json!(model));
    out.insert("function".into(), json!(channel));
    out.insert("stream".into(), json!(true));
    out.insert("app_version_code".into(), json!(APP_VERSION_CODE));
    Value::Object(out)
}

// ───────────────────────── 响应翻译（SOLO SSE → OpenAI SSE） ─────────────────────────

#[derive(Default, Clone)]
pub struct SseState {
    /// 收到过 `output`——此后绝不重放请求
    pub started: bool,
    /// `timing_cost` 里的上游模型名，观测用
    pub provider: String,
    pub usage: Option<Value>,
    pub finish: Option<String>,
    pub error: Option<(i64, String)>,
}

pub struct SoloTranslator<R: Read> {
    inner: R,
    raw: Vec<u8>,
    out: Vec<u8>,
    pos: usize,
    eof: bool,
    done_sent: bool,
    id: String,
    created: i64,
    label: String,
    pub state: std::sync::Arc<Mutex<SseState>>,
}

impl<R: Read> SoloTranslator<R> {
    pub fn new(inner: R, label: &str) -> Self {
        Self {
            inner,
            raw: Vec::new(),
            out: Vec::new(),
            pos: 0,
            eof: false,
            done_sent: false,
            id: format!("chatcmpl-trea-{}", now_secs()),
            created: now_secs() as i64,
            label: label.to_string(),
            state: std::sync::Arc::new(Mutex::new(SseState::default())),
        }
    }

    pub fn shared_state(&self) -> std::sync::Arc<Mutex<SseState>> {
        self.state.clone()
    }

    fn chunk(&self, delta: Value, finish: Option<&str>) -> String {
        let mut c = json!({
            "id": self.id, "object": "chat.completion.chunk", "created": self.created,
            "model": self.label,
            "choices": [{ "index": 0, "delta": delta, "finish_reason": finish }],
        });
        if let Ok(s) = self.state.lock() {
            if let Some(u) = &s.usage {
                c["usage"] = u.clone();
            }
        }
        format!("data: {}\n\n", c)
    }

    /// 处理一个 SSE 块（`event:` + `data:` 若干行），把 OpenAI chunk 追加到 out
    fn handle_block(&mut self, block: &str) {
        let mut event = "";
        let mut data = String::new();
        for line in block.lines() {
            if let Some(e) = line.strip_prefix("event:") {
                event = e.trim();
            } else if let Some(d) = line.strip_prefix("data:") {
                if !data.is_empty() {
                    data.push('\n');
                }
                data.push_str(d.trim());
            }
        }
        if event.is_empty() && data.is_empty() {
            return;
        }
        let Ok(j) = serde_json::from_str::<Value>(&data) else {
            return;
        };
        match event {
            "output" => {
                let mut delta = Map::new();
                if let Some(s) = j["response"].as_str() {
                    if !s.is_empty() {
                        delta.insert("content".into(), json!(s));
                    }
                }
                if let Some(s) = j["reasoning_content"].as_str() {
                    if !s.is_empty() {
                        delta.insert("reasoning_content".into(), json!(s));
                    }
                }
                if let Some(arr) = j["tool_calls"].as_array() {
                    let calls: Vec<Value> = arr
                        .iter()
                        .enumerate()
                        .map(|(i, tc)| {
                            let mut t = tc.clone();
                            let fv = t.get("function").cloned();
                            if t.get("function_call").is_none() {
                                if let Some(fv) = fv {
                                    t["function_call"] = fv;
                                }
                            }
                            if let Some(o) = t.as_object_mut() {
                                o.remove("function");
                            }
                            if let Some(fc) = t.get_mut("function_call").and_then(|v| v.as_object_mut()) {
                                fc.remove("namespace");
                                fc.remove("partial_arguments");
                            }
                            json!({
                                "index": i,
                                "id": t["id"].as_str().unwrap_or(""),
                                "type": "function",
                                "function": t["function_call"].clone(),
                            })
                        })
                        .collect();
                    if !calls.is_empty() {
                        delta.insert("tool_calls".into(), Value::Array(calls));
                        if delta.get("content").is_none() {
                            delta.insert("content".into(), Value::Null);
                        }
                    }
                }
                if let Ok(mut s) = self.state.lock() {
                    s.started = true;
                }
                if !delta.is_empty() {
                    let text = self.chunk(Value::Object(delta), None);
                    self.out.extend_from_slice(text.as_bytes());
                }
            }
            "timing_cost" => {
                if let Some(p) = j["provider_model_name"].as_str() {
                    if let Ok(mut s) = self.state.lock() {
                        s.provider = p.to_string();
                    }
                }
            }
            "token_usage" => {
                if let Ok(mut s) = self.state.lock() {
                    s.usage = Some(json!({
                        "prompt_tokens": j["prompt_tokens"].as_i64().unwrap_or(0),
                        "completion_tokens": j["completion_tokens"].as_i64().unwrap_or(0),
                        "total_tokens": j["total_tokens"].as_i64().unwrap_or(0),
                    }));
                }
            }
            "done" => {
                let f = j["finish_reason"].as_str().unwrap_or("stop").to_string();
                if let Ok(mut s) = self.state.lock() {
                    s.finish = Some(f.clone());
                }
                let text = self.chunk(json!({}), Some(&f));
                self.out.extend_from_slice(text.as_bytes());
                self.out.extend_from_slice(b"data: [DONE]\n\n");
                self.done_sent = true;
            }
            "error" => {
                let code = j["code"].as_i64().unwrap_or(-1);
                let msg = j["message"].as_str().unwrap_or("").to_string();
                if let Ok(mut s) = self.state.lock() {
                    s.error = Some((code, msg.clone()));
                }
                let text = format!(
                    "data: {}\n\n",
                    json!({ "error": { "code": code, "message": msg, "type": classify(code, &msg).as_str() } })
                );
                self.out.extend_from_slice(text.as_bytes());
                self.out.extend_from_slice(b"data: [DONE]\n\n");
                self.done_sent = true;
            }
            // metadata 不算「已产出内容」，也不翻译
            _ => {}
        }
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

impl<R: Read> Read for SoloTranslator<R> {
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
            let n = self.inner.read(&mut tmp)?;
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

// ───────────────────────── 账号与观测 ─────────────────────────

/// 只读缓存：界面 4 秒一轮的轮询走这条，**绝不在这里起 python**
pub fn cred_cached(dir: &Path) -> Option<Result<Cred, String>> {
    let key = cache_key(dir);
    CRED_CACHE
        .lock()
        .ok()
        .and_then(|g| g.as_ref().filter(|(k, _)| *k == key).map(|(_, v)| v.clone()))
}

/// 只读缓存，同上
pub fn models_cached(dir: &Path) -> Option<(Value, i64)> {
    let key = dir.display().to_string();
    MODEL_CACHE
        .lock()
        .ok()
        .and_then(|g| g.as_ref().filter(|(k, _, _)| *k == key).map(|(_, c, m)| (c.clone(), *m)))
}

pub struct Acct {
    pub app: String,
    pub app_name: String,
    pub name: String,
    pub data_dir: PathBuf,
    pub cred: Result<Cred, String>,
}

/// 只做目录发现，不起外部进程
pub fn accounts_raw() -> Vec<(String, String, String, PathBuf)> {
    let mut out = Vec::new();
    for app in APPS {
        let disp = app_display(app);
        for m in load_instances(app) {
            let name = m["name"].as_str().unwrap_or("").to_string();
            if name.is_empty() {
                continue;
            }
            let dd = m["data_dir"].as_str().unwrap_or("").trim().to_string();
            let dir = if dd.is_empty() {
                match crate::registry::profile_dir(app) {
                    Some(p) => p,
                    None => continue,
                }
            } else {
                PathBuf::from(dd)
            };
            out.push((app.to_string(), disp.clone(), name, dir));
        }
    }
    // 一个实例都没有时，至少把默认档案当一行报出来，界面不至于空着
    if out.is_empty() {
        if let Some(dir) = crate::registry::profile_dir("traework-cn") {
            out.push((
                "traework-cn".into(),
                app_display("traework-cn"),
                "默认档案".into(),
                dir,
            ));
        }
    }
    out
}

pub fn accounts() -> Vec<Acct> {
    accounts_raw()
        .into_iter()
        .map(|(app, app_name, name, data_dir)| {
            let cred = cred_for(&data_dir);
            Acct {
                app,
                app_name,
                name,
                data_dir,
                cred,
            }
        })
        .collect()
}

pub fn token_for(app_id: &str, name: &str) -> Option<String> {
    accounts()
        .into_iter()
        .find(|a| a.app == app_id && a.name == name)
        .and_then(|a| a.cred.ok())
        .map(|c| c.token)
}

// ───────────────────────── 请求日志（内存环 + 落盘计数） ─────────────────────────

static LOGS: Mutex<Vec<Value>> = Mutex::new(Vec::new());
const LOG_CAP: usize = 200;

pub fn note(mut entry: Value) {
    if let Ok(mut g) = LOGS.lock() {
        entry["at"] = json!(now_secs());
        g.push(entry);
        if g.len() > LOG_CAP {
            let drop = g.len() - LOG_CAP;
            g.drain(..drop);
        }
    }
}

fn store_path() -> PathBuf {
    base_dir().join("traeproxy.json")
}

fn load_store() -> Value {
    std::fs::read_to_string(store_path())
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .filter(|v| v.is_object())
        .unwrap_or_else(|| json!({ "counters": {} }))
}

fn save_store(v: &Value) {
    let _ = std::fs::create_dir_all(base_dir());
    let tmp = store_path().with_extension("json.tmp");
    if std::fs::write(&tmp, serde_json::to_string_pretty(v).unwrap_or_default()).is_ok() {
        let _ = std::fs::rename(&tmp, store_path());
    }
}

/// 单个模型/错误类别的累计计数，重启不丢
pub fn bump(key: &str) {
    let mut s = load_store();
    let n = s["counters"][key].as_i64().unwrap_or(0) + 1;
    s["counters"][key] = json!(n);
    s["updated"] = json!(now_secs());
    save_store(&s);
}

pub fn counters() -> Value {
    load_store()["counters"].clone()
}

/// 错误类别 → 给调用方的 HTTP 状态（非流式路径用；流式路径在 SSE 里带 type）
pub fn http_status(k: Kind) -> u16 {
    match k {
        Kind::None => 200,
        Kind::Identity => 401,
        Kind::BadRequest => 400,
        Kind::Quota => 402,
        Kind::Plan => 403,
        Kind::ModelDead => 404,
        Kind::Server => 502,
    }
}

/// 显式刷新：起 agenthub 读令牌与目录、重算出口 IP。只给按钮/探针调，**不进 4 秒轮询**。
pub fn refresh() -> Value {
    for (_, _, _, dir) in accounts_raw() {
        let _ = cred_for(&dir);
    }
    if let Some(d) = crate::registry::profile_dir("traework-cn") {
        let _ = models_for(&d);
    }
    invalidate_ip();
    let _ = egress_ip();
    view()
}

/// 给界面看的总览。**纯读缓存，不发网络请求、不起外部进程**——
/// 界面默认 4 秒一轮，这里一旦起 python 就会每分钟卡一下。
pub fn view() -> Value {
    let accts: Vec<Value> = accounts_raw()
        .into_iter()
        .map(|(app, app_name, name, dir)| {
            let cached = cred_cached(&dir);
            let (ok, err, tail, exp, left, uid, src) = match &cached {
                Some(Ok(c)) => (
                    true,
                    String::new(),
                    if c.token.len() > 12 {
                        format!("…{}", &c.token[c.token.len() - 6..])
                    } else {
                        String::new()
                    },
                    c.expires_at.clone(),
                    c.hours_left,
                    c.user_id.clone(),
                    c.source.clone(),
                ),
                Some(Err(e)) => (false, e.clone(), String::new(), String::new(), None, String::new(), String::new()),
                None => (false, String::new(), String::new(), String::new(), None, String::new(), String::new()),
            };
            json!({
                "app": app, "app_name": app_name, "name": name,
                "data_dir": dir.display().to_string(),
                "ready": cached.is_some(),
                "ok": ok, "error": err, "token_tail": tail,
                "expires_at": exp, "hours_left": left, "user_id": uid, "auth_from": src,
            })
        })
        .collect();

    let dir = crate::registry::profile_dir("traework-cn");
    let cached = dir.as_ref().and_then(|d| models_cached(d));
    let (channels, mtime, models_ready) = match cached {
        Some((c, m)) => (c, m, true),
        None => (json!({}), 0, false),
    };
    let list = model_list(&channels);
    let logs = LOGS.lock().map(|g| g.clone()).unwrap_or_default();
    json!({
        "apps": APPS,
        "base_url": format!("http://127.0.0.1:{}/v1", crate::qproxy::proxy_port()),
        "accounts": accts,
        "egress_ip": egress_ip(),
        "models": list,
        "models_ready": models_ready,
        "model_source_mtime": mtime,
        "channel_counts": channels.as_object().map(|o| o.iter()
            .map(|(k, v)| (k.clone(), json!(v.as_array().map(|a| a.len()).unwrap_or(0))))
            .collect::<Map<String, Value>>()).unwrap_or_default(),
        "logs": logs,
        "counters": counters(),
        // 上游组与下发 Key 的现状（不建，只报）
        "pool": find_pool().map(|p| json!({
            "id": p["id"], "name": p["name"], "enabled": p["enabled"],
            "members": p["members"].as_array().map(|a| a.len()).unwrap_or(0),
        })),
        "dist": find_dist().map(|d| json!({
            "name": d["name"],
            "masked": crate::keys::mask(d["key"].as_str().unwrap_or("")),
            "pools": d["pools"],
        })),
    })
}

// ───────────────────────── 一键 ─────────────────────────

fn pool_name() -> &'static str {
    "Trae 上游组"
}
fn dist_name() -> &'static str {
    "Trae 接入"
}
fn find_pool() -> Option<Value> {
    crate::gateway::pools()
        .into_iter()
        .find(|p| p["name"].as_str() == Some(pool_name()))
}
fn find_dist() -> Option<Value> {
    crate::gateway::dist_keys()
        .into_iter()
        .find(|d| d["name"].as_str() == Some(dist_name()))
}

pub fn dist_reveal() -> String {
    find_dist()
        .and_then(|d| d["key"].as_str().map(|s| s.to_string()))
        .unwrap_or_default()
}

/// 一键：刷新 → （可选）探一发 → 起反代。
/// 上游组和接入密钥**不再代建**：归到哪个组、客户端用哪把 key，是用户自己的事，
/// 到「上游组」「接入密钥」两个表里手动选（历史教训：按名复用一旦名字对不上，
/// 每轮一键都会新建一个组+key，堆出一排同名残留）。
/// 探针默认开着，因为「账号令牌根本不能调」比「少建一个组」更值得先知道。
pub fn one_click(
    proxy: &std::sync::Arc<crate::proxy::Proxy>,
    probe_model: &str,
    skip_probe: bool,
) -> Value {
    refresh();
    let accts = accounts();
    if accts.is_empty() {
        return json!({ "ok": false, "msg": "没有 Trae 账号档案" });
    }
    let usable: Vec<_> = accts.iter().filter(|a| a.cred.is_ok()).collect();
    if usable.is_empty() {
        let why: Vec<String> = accts
            .iter()
            .map(|a| {
                format!(
                    "{}/{}：{}",
                    a.app_name,
                    a.name,
                    a.cred.as_ref().err().cloned().unwrap_or_default()
                )
            })
            .collect();
        return json!({ "ok": false, "msg": format!("令牌都读不到：{}", why.join("；")) });
    }

    let mut probe_note = String::new();
    if !skip_probe {
        let a = usable[0];
        let r = probe(&a.app, &a.name, probe_model);
        if r["ok"].as_bool() != Some(true) {
            return json!({
                "ok": false,
                "msg": format!(
                    "探针没过，先不接入：{}（kind={}）",
                    r["error"].as_str().or_else(|| r["message"].as_str()).unwrap_or("未知"),
                    r["kind"].as_str().unwrap_or("")
                ),
                "result": r,
            });
        }
        probe_note = format!(
            "探针通过（{}，{}ms）",
            r["provider"].as_str().unwrap_or(probe_model),
            r["elapsed_ms"].as_u64().unwrap_or(0)
        );
    }

    // 整个驾驶舱只有一个反代服务，端口与 Qoder 共用
    let port = crate::qproxy::proxy_port();
    let mut proxy_msg = "反代已在运行".to_string();
    if !proxy.is_running() {
        proxy.set_config(Some("https://api.openai.com"), None);
        let mut cfg = crate::registry::load_config();
        if !cfg["proxy"].is_object() {
            cfg["proxy"] = json!({});
        }
        cfg["proxy"]["port"] = json!(port);
        crate::registry::save_config(&cfg);
        let (o, m) = proxy.start(port);
        if !o {
            return json!({ "ok": false, "msg": format!("反代起不来：{m}") });
        }
        proxy_msg = m;
    }

    json!({
        "ok": true,
        "msg": format!(
            "{}；{}。账号还没动上游组和接入密钥：到「上游组」把账号勾进组，客户端用「接入密钥」表里的 key。",
            if probe_note.is_empty() { format!("{} 个账号可用", usable.len()) } else { probe_note },
            proxy_msg
        ),
        "base_url": format!("http://127.0.0.1:{port}/v1"),
        "models": model_list(&crate::registry::profile_dir("traework-cn")
            .and_then(|d| models_cached(&d)).map(|(c, _)| c).unwrap_or_else(|| json!({}))),
    })
}

/// 把翻译出来的 OpenAI SSE 收成一条非流式 `chat.completion`。
/// 调用方发 `stream:false` 时必须走这条——SSE 喂给非流式客户端会被判成坏响应。
pub fn collect_completion<R: Read>(tr: &mut SoloTranslator<R>, label: &str) -> Value {
    let mut buf = Vec::new();
    let _ = tr.read_to_end(&mut buf);
    let text = String::from_utf8_lossy(&buf);
    let (mut content, mut reasoning) = (String::new(), String::new());
    let mut finish = "stop".to_string();
    let mut usage = Value::Null;
    let mut calls: Vec<Value> = Vec::new();
    for line in text.lines() {
        let Some(d) = line.strip_prefix("data: ") else { continue };
        if d.trim() == "[DONE]" {
            continue;
        }
        let Ok(j) = serde_json::from_str::<Value>(d) else { continue };
        if let Some(u) = j.get("usage").filter(|u| !u.is_null()) {
            usage = u.clone();
        }
        let Some(ch) = j["choices"].get(0) else { continue };
        if let Some(s) = ch["delta"]["content"].as_str() {
            content.push_str(s);
        }
        if let Some(s) = ch["delta"]["reasoning_content"].as_str() {
            reasoning.push_str(s);
        }
        if let Some(f) = ch["finish_reason"].as_str() {
            finish = f.to_string();
        }
        if let Some(arr) = ch["delta"]["tool_calls"].as_array() {
            calls.extend(arr.iter().cloned());
        }
    }
    let s = tr.state.lock().map(|g| g.clone()).unwrap_or_default();
    let mut msg = json!({ "role": "assistant", "content": content });
    if !reasoning.is_empty() {
        msg["reasoning_content"] = json!(reasoning);
    }
    if !calls.is_empty() {
        msg["tool_calls"] = Value::Array(calls);
    }
    json!({
        "id": format!("chatcmpl-trea-{}", now_secs()),
        "object": "chat.completion",
        "created": now_secs() as i64,
        "model": if s.provider.is_empty() { label.to_string() } else { s.provider.clone() },
        "choices": [{ "index": 0, "message": msg, "finish_reason": finish }],
        "usage": usage,
    })
}

// ───────────────────────── 单发探针 ─────────────────────────

/// 一次真请求（花几个积分）。**只给手动按钮用，绝不进循环**——
/// 2026-10-19 就是自动化探针自己把限流窗口撞出来了。
pub fn probe(app_id: &str, name: &str, model: &str) -> Value {
    let Some(a) = accounts()
        .into_iter()
        .find(|a| a.app == app_id && (name.is_empty() || a.name == name))
    else {
        return json!({ "ok": false, "error": format!("找不到账号 {app_id}/{name}") });
    };
    let cred = match a.cred {
        Ok(c) => c,
        Err(e) => return json!({ "ok": false, "error": e }),
    };
    let (channels, _) = match models_for(&a.data_dir) {
        Ok(v) => v,
        Err(e) => return json!({ "ok": false, "error": e }),
    };
    let Some(pick) = channel_for(&channels, model) else {
        return json!({ "ok": false, "error": format!("目录里没有模型 {model}") });
    };
    let (channel, model_name) = match pick.split_once('|') {
        Some((c, m)) => (c.to_string(), m.to_string()),
        None => (pick, model.to_string()),
    };
    let body = translate_request(
        &json!({
            "messages": [{ "role": "user", "content": "只回复两个字：收到" }],
            "max_tokens": 32
        }),
        &channel,
        &model_name,
    );

    // 4011 = 身份/出口 IP 没过：强制重算一次再试，只重试一次
    let mut ip_used = String::new();
    for attempt in 0..2 {
        let ip = if attempt == 0 { egress_ip() } else { invalidate_ip(); egress_ip_fresh() };
        let Some(ip) = ip else {
            return json!({ "ok": false, "error": "取不到本机出口 IP" });
        };
        ip_used = ip.clone();
        let t0 = Instant::now();
        let mut call = ureq::post(&format!("{AGENT_HOST}{CHAT_PATH}"))
            .timeout(Duration::from_secs(120))
            .set("Authorization", &format!("Cloud-IDE-JWT {}", cred.token))
            .set("Content-Type", "application/json")
            .set("Accept", "text/event-stream")
            .set("X-App-Id", APP_ID)
            .set("X-User-Real-IP", &ip);
        match call.send(body.to_string().as_bytes()) {
            Ok(resp) => {
                let status = resp.status() as u16;
                let mut tr = SoloTranslator::new(resp.into_reader(), &model_name);
                let st = tr.shared_state();
                let mut out = Vec::new();
                let _ = tr.by_ref().take(64 * 1024).read_to_end(&mut out);
                let s = st.lock().map(|g| g.clone()).unwrap_or_default();
                if let Some((code, msg)) = &s.error {
                    let k = classify(*code, msg);
                    let retry = k.retry_with_new_ip() && attempt == 0;
                    if retry {
                        continue;
                    }
                    return json!({
                        "ok": false, "status": status, "code": code, "message": msg,
                        "kind": k.as_str(), "ip": ip_used,
                        "elapsed_ms": t0.elapsed().as_millis() as u64,
                    });
                }
                return json!({
                    "ok": true, "status": status, "kind": "ok",
                    "provider": s.provider, "finish": s.finish,
                    "usage": s.usage, "ip": ip_used,
                    "elapsed_ms": t0.elapsed().as_millis() as u64,
                    "preview": String::from_utf8_lossy(&out[..out.len().min(400)]).to_string(),
                });
            }
            Err(ureq::Error::Status(code, resp)) => {
                let mut txt = String::new();
                let _ = resp.into_reader().take(2000).read_to_string(&mut txt);
                let k = classify(code as i64, &txt);
                if k.retry_with_new_ip() && attempt == 0 {
                    continue;
                }
                return json!({
                    "ok": false, "status": code, "kind": k.as_str(),
                    "message": txt.chars().take(200).collect::<String>(), "ip": ip_used,
                    "elapsed_ms": t0.elapsed().as_millis() as u64,
                });
            }
            Err(e) => {
                return json!({ "ok": false, "status": 0, "kind": "server",
                               "message": e.to_string(), "ip": ip_used });
            }
        }
    }
    json!({ "ok": false, "error": "两次都没通过身份校验（出口 IP 可能不对）", "ip": ip_used })
}

// ───────────────────────── 单测 ─────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_4011_is_identity_not_rate_limit() {
        // 这是整套里最容易抄错的一条：vendored 把 4011 当限流→冷却→换号。
        let k = classify(4011, "We're sorry, your requests have exceeded the rate limit.");
        assert_eq!(k, Kind::Identity);
        assert!(k.retry_with_new_ip());
        assert!(!k.blames_account(), "4011 不许拖累整个账号");
    }

    #[test]
    fn code_4001_is_bad_request_and_never_blames_account() {
        let k = classify(4001, "the param is invalid");
        assert_eq!(k, Kind::BadRequest);
        assert!(!k.retry_with_new_ip());
        assert!(!k.blames_account(), "内容级错误换号无用，绝不能进冷却账本");
    }

    #[test]
    fn other_codes_map_to_expected_kinds() {
        assert_eq!(classify(4008, ""), Kind::Quota);
        assert_eq!(classify(1005, "plan limit"), Kind::Plan);
        assert_eq!(classify(404, ""), Kind::ModelDead);
        assert_eq!(classify(503, ""), Kind::Server);
        assert_eq!(classify(1001, "not able to authenticate"), Kind::Identity);
        assert_eq!(classify(0, ""), Kind::None);
        assert!(Kind::Quota.blames_account());
        assert!(Kind::Plan.blames_account());
        assert!(!Kind::ModelDead.blames_account());
    }

    #[test]
    fn translate_keeps_solo_shape() {
        let openai = json!({
            "model": "glm-5.3-flash",
            "messages": [
                {"role":"system","content":"你是助手"},
                {"role":"user","content":"只回复收到"},
                {"role":"assistant","tool_calls":[{"id":"c1","type":"function","function":{"name":"f","arguments":"{}"}}]}
            ],
            "max_tokens": 128,
            "temperature": 0.2,
            "unknown_field": "必须被丢掉"
        });
        let solo = translate_request(&openai, "solo_agent", "glm-5.3-flash");
        assert_eq!(solo["function"], "solo_agent");
        assert_eq!(solo["model"], "glm-5.3-flash");
        assert_eq!(solo["config_name"], "glm-5.3-flash");
        assert_eq!(solo["app_version_code"], APP_VERSION_CODE);
        assert_eq!(solo["stream"], true);
        assert_eq!(solo["max_tokens"], 128);
        assert_eq!(solo["temperature"], 0.2);
        assert!(solo.get("unknown_field").is_none(), "陌生字段不许透传");
        // content 字符串 → 文本块数组
        assert_eq!(solo["messages"][1]["content"][0]["type"], "text");
        assert_eq!(solo["messages"][1]["content"][0]["text"], "只回复收到");
        // assistant 的 function → function_call
        assert!(solo["messages"][2]["tool_calls"][0].get("function").is_none());
        assert_eq!(solo["messages"][2]["tool_calls"][0]["function_call"]["name"], "f");
    }

    #[test]
    fn tools_parameters_become_json_string() {
        let openai = json!({
            "messages": [{"role":"user","content":"x"}],
            "tools": [{"type":"function","function":{
                "name":"read","description":"读文件",
                "parameters":{"type":"object","properties":{"p":{"type":"string"}}}
            }}]
        });
        let solo = translate_request(&openai, "solo_agent", "glm-5.3-flash");
        let p = &solo["tools"][0]["function"]["parameters"];
        assert!(p.is_string(), "SOLO 要 JSON 字符串，不是对象");
        let parsed: Value = serde_json::from_str(p.as_str().unwrap()).unwrap();
        assert_eq!(parsed["type"], "object");
    }

    #[test]
    fn tool_choice_none_drops_tools() {
        let openai = json!({
            "messages": [{"role":"user","content":"x"}],
            "tools": [{"type":"function","function":{"name":"f","parameters":{"type":"object"}}}],
            "tool_choice": "none"
        });
        let solo = translate_request(&openai, "solo_agent", "m");
        assert!(solo.get("tools").is_none(), "none 要连带删掉 tools");
        assert!(solo.get("tool_choice").is_none());
    }

    #[test]
    fn sse_translation_skips_metadata_and_marks_started_on_output() {
        let raw = concat!(
            "event:metadata\ndata:{\"session_id\":\"s1\"}\n\n",
            "event:timing_cost\ndata:{\"provider_model_name\":\"glm-5.3-flash\"}\n\n",
            "event:output\ndata:{\"response\":\"收到\",\"reasoning_content\":null,\"tool_calls\":null}\n\n",
            "event:token_usage\ndata:{\"prompt_tokens\":10,\"completion_tokens\":2,\"total_tokens\":12}\n\n",
            "event:done\ndata:{\"finish_reason\":\"stop\"}\n\n"
        );
        let mut t = SoloTranslator::new(std::io::Cursor::new(raw.as_bytes().to_vec()), "glm-5.3-flash");
        let st = t.shared_state();
        let mut out = String::new();
        t.read_to_string(&mut out).unwrap();
        // metadata 不许产生任何 chunk（它不是「已产出内容」）
        assert!(!out.contains("\"session_id\""));
        assert!(out.contains("收到"));
        assert!(out.contains("\"finish_reason\":\"stop\""));
        assert!(out.contains("data: [DONE]"));
        let s = st.lock().unwrap();
        assert!(s.started, "收到 output 后必须标记已产出");
        assert_eq!(s.provider, "glm-5.3-flash");
        assert_eq!(s.usage.as_ref().unwrap()["total_tokens"], 12);
    }

    #[test]
    fn sse_error_maps_to_kind_and_terminates() {
        let raw = "event:error\ndata:{\"code\":4001,\"message\":\"the param is invalid\"}\n\n";
        let mut t = SoloTranslator::new(std::io::Cursor::new(raw.as_bytes().to_vec()), "m");
        let st = t.shared_state();
        let mut out = String::new();
        t.read_to_string(&mut out).unwrap();
        assert!(out.contains("\"type\":\"bad_request\""));
        assert!(out.contains("data: [DONE]"));
        let s = st.lock().unwrap();
        assert!(!s.started, "error 前没有 output，不算已产出（可以换号重发）");
        assert_eq!(s.error.as_ref().unwrap().0, 4001);
    }

    #[test]
    fn separator_handles_crlf() {
        assert_eq!(find_sep(b"event:x\r\ndata:{}\r\n\r\n"), Some((16, 4)));
        assert_eq!(find_sep(b"event:x\ndata:{}\n\n"), Some((15, 2)));
        assert_eq!(find_sep(b"event:x\ndata:{}"), None);
    }

    #[test]
    fn channel_lookup_prefers_priority_and_supports_explicit() {
        let ch = json!({
            "solo_agent": [{"config_name":"glm-5.3-flash"}],
            "solo_work_lite": [{"config_name":"glm-5.3-flash"},{"config_name":"glm-5.2"}],
            "solo_coder": [{"config_name":"glm-5.3"}],
        });
        assert_eq!(channel_for(&ch, "glm-5.3-flash").as_deref(), Some("solo_agent"));
        assert_eq!(channel_for(&ch, "glm-5.2").as_deref(), Some("solo_work_lite"));
        // 显式指定通道：返回 "通道|模型"，调用方拆开用
        assert_eq!(
            channel_for(&ch, "solo_coder/glm-5.3-flash").as_deref(),
            Some("solo_coder|glm-5.3-flash")
        );
        assert_eq!(channel_for(&ch, "nope"), None);
    }

    #[test]
    fn model_list_dedupes_in_priority_order() {
        let ch = json!({
            "solo_agent": [{"config_name":"a","context_window":1},{"config_name":"b"}],
            "solo_work_lite": [{"config_name":"a"},{"config_name":"c"}],
        });
        let l = model_list(&ch);
        let ids: Vec<&str> = l.iter().map(|m| m["id"].as_str().unwrap()).collect();
        assert_eq!(ids, vec!["a", "b", "c"]);
        assert_eq!(l[0]["channel"], "solo_agent");
    }

    #[test]
    fn base_mark_is_recognised() {
        assert!(is_trae_base(BASE_MARK));
        assert!(is_trae_base("trae://agent/"));
        assert!(!is_trae_base("https://trae-api-cn.mchost.guru"));
    }

    /// 真网络探针：**默认忽略，只能手动跑一次**。
    /// `cargo test live_probe -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn live_probe() {
        let r = probe("traework-cn", "", "glm-5.3-flash");
        println!("{}", serde_json::to_string_pretty(&r).unwrap());
        assert_eq!(r["ok"], true, "真模型没通：{r}");
    }

    /// 真机端到端：一键反代 → 用分发出的 Key 打一条真实补全（流式 + 非流式各一次）。
    /// 会临时改写 config.json（结束原样还原），并真的花掉几个积分。
    /// `cargo test real_oneclick_serves_a_completion -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn real_oneclick_serves_a_completion() {
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
        let port = 18998u16;
        let (ok, msg) = p.start(port);
        assert!(ok, "{msg}");

        let r = one_click(&p, "glm-5.3-flash", false);
        assert!(r["ok"].as_bool().unwrap_or(false), "一键没成功：{r}");
        let key = r["key"].as_str().unwrap_or("").to_string();
        assert!(key.starts_with("ac-"), "接入密钥 不对：{r}");

        // 非流式：要一条 chat.completion
        let body = json!({
            "model": "glm-5.3-flash",
            "messages": [{ "role": "user", "content": "只回复两个字：收到" }],
            "stream": false, "max_tokens": 32,
        })
        .to_string();
        let res = ureq::post(&format!("http://127.0.0.1:{port}/v1/chat/completions"))
            .timeout(Duration::from_secs(120))
            .set("Content-Type", "application/json")
            .set("Authorization", &format!("Bearer {key}"))
            .send_string(&body);
        let text = match res {
            Ok(x) => {
                assert_eq!(x.status(), 200);
                let mut s = String::new();
                let _ = std::io::Read::read_to_string(&mut x.into_reader(), &mut s);
                s
            }
            Err(ureq::Error::Status(c, x)) => {
                let mut s = String::new();
                let _ = std::io::Read::read_to_string(&mut x.into_reader(), &mut s);
                panic!("反代返回 {c}：{}", s.chars().take(400).collect::<String>());
            }
            Err(e) => panic!("反代请求失败：{e}"),
        };
        println!("非流式响应：{}", text.chars().take(500).collect::<String>());
        assert_eq!(
            serde_json::from_str::<Value>(&text).unwrap()["object"],
            "chat.completion"
        );

        // 流式：要 OpenAI SSE，且以 [DONE] 收尾
        let body = json!({
            "model": "glm-5.3-flash",
            "messages": [{ "role": "user", "content": "只回复两个字：收到" }],
            "stream": true, "max_tokens": 32,
        })
        .to_string();
        let res = ureq::post(&format!("http://127.0.0.1:{port}/v1/chat/completions"))
            .timeout(Duration::from_secs(120))
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
        assert!(stream.contains("chat.completion.chunk"), "不是 OpenAI 流：{}", &stream[..stream.len().min(300)]);
        assert!(stream.contains("data: [DONE]"));

        p.stop();
    }
}
