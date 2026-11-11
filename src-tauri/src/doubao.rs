//! 豆包（doubao.com）反代上游：**进程内云端直连**。
//!
//! 2026-11-10 本机实测结论（决定了这里为什么不需要浏览器、不需要桥进程）：
//!
//!   1. 对话接口是 `POST https://www.doubao.com/samantha/chat/completion?<公共query>`，
//!      **纯 cookie 鉴权，没有 a_bogus / msToken 签名**——字节系那套风控在这条链路上
//!      不存在（签到那三条 /alice/ug/* 也一样）。所以只要拿到会话 cookie 就能直连。
//!   2. 请求体只有一层：messages[0].content 是**字符串化的** `{"text": "..."}`，
//!      content_type=2001；conversation_id 传 "0" 让服务端新建会话，
//!      local_conversation_id / local_message_id 本地生成即可。
//!   3. 回 `text/event-stream`，每行 `data: {"event_data":"<json>","event_type":N}`：
//!        2002 会话建立（带 conversation_id）/ 2011 proto / 2001 正文增量 / 2003 结束。
//!      正文在 event_data.message.content 里，**又是一层 JSON**，取其中的 text；
//!      同一事件类型也会混进 `{"suggest": ...}` 这类非正文块，所以必须按"有没有 text"过滤。
//!   4. 公共 query 只要 aid/version_code/pkg_type 这一小撮，device_id 留空、
//!      web_id/tea_uuid 全省都能过（四种组合实测状态一致），不必去本机翻设备号。
//!
//! 凭据：cookies 库是 SQLite + os_crypt v10，Rust 这边没有 sqlite 依赖（全项目统一把
//! 凭据解析放 agenthub）。agenthub 的 doubao store 读到 cookie 后会写一份
//! `~/.agent-cockpit/doubao_auth.json`；这里只读那份。缓存不在或太旧就先调一次
//! `agenthub quota doubao --json` 让它刷新（顺带把"豆包在运行、Cookies 被独占"这种情况
//! 也交给它去退缓存，这里不重复一遍那套逻辑）。
//!
//! 边界：只做**纯文本**对话。豆包 web 没有 OpenAI 那套 tool_calls 协议，硬塞工具会把
//! 输出搅成一段自然语言里夹 JSON，所以这里明确不支持 tools（见到 tools 直接如实报错，
//! 不假装支持）。

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const APPS: [&str; 1] = ["doubao"];
/// 上游组里标记「这条上游是豆包直连」，`proxy.rs` 靠它进翻译分支
pub const BASE_MARK: &str = "doubao://chat";
pub const MAIN: &str = "https://www.doubao.com";

const ROUND_TIMEOUT: Duration = Duration::from_secs(300);
const CACHE_TTL: u64 = 7 * 86400; // 会话 cookie 按天算，7 天足够签到/反代，过期就重读

pub fn is_doubao_base(base: &str) -> bool {
    base.trim().trim_end_matches('/') == BASE_MARK
}

fn now_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

fn cache_path() -> PathBuf {
    PathBuf::from(std::env::var("USERPROFILE").unwrap_or_default())
        .join(".agent-cockpit")
        .join("doubao_auth.json")
}

/// 读 agenthub 落的那份缓存；过旧就当作没有
fn cached_cookie() -> Option<String> {
    let txt = std::fs::read_to_string(cache_path()).ok()?;
    let v: Value = serde_json::from_str(&txt).ok()?;
    let ck = v["cookie"].as_str()?.trim().to_string();
    if ck.is_empty() {
        return None;
    }
    let at = v["at"].as_f64().unwrap_or(0.0);
    if now_secs() - at > CACHE_TTL as f64 {
        return None;
    }
    Some(ck)
}

/// 让 agenthub 重读一次 cookies 库（它自己处理独占锁→缓存的回退）
fn refresh_cookie() {
    let _ = crate::agenthub::command(&["quota", "doubao", "--json"]).output();
}

/// 缓存里有没有可用凭据。**只看缓存不触发刷新**——列上游时调它，
/// 触发 agenthub 重读要 1~2 秒，会把「上游列表」拖慢。
pub fn auth_ready() -> bool {
    cached_cookie().is_some()
}

pub fn cookie() -> Result<String, String> {
    if let Some(c) = cached_cookie() {
        return Ok(c);
    }
    refresh_cookie();
    cached_cookie().ok_or_else(|| {
        "读不到豆包登录态：先在豆包里登录一次（凭据来自 Cookies 库，由 agenthub 解析）".to_string()
    })
}

// ───────────────────────────── 模型 ─────────────────────────────

/// 豆包 web 只暴露"一个会回答的模型"，能调的其实是**模式**（是否深度思考）。
/// 所以这里给的 id 是驾驶舱自己的口径，不假装是厂商模型名。
pub fn models() -> Vec<Value> {
    vec![
        json!({"id": "doubao", "name": "豆包", "numeric": 0, "free": true, "vision": false,
               "note": "默认模式（快速）"}),
        json!({"id": "doubao-think", "name": "豆包·深度思考", "numeric": 1, "free": true,
               "vision": false, "note": "打开 use_deep_think，回答更慢更细"}),
    ]
}

pub fn model_ids() -> Value {
    json!({
        "object": "list",
        "data": models().iter().map(|m| json!({
            "id": m["id"], "object": "model", "owned_by": "doubao",
            "name": m["name"], "free": m["free"], "vision": m["vision"],
        })).collect::<Vec<_>>()
    })
}

/// 客户端传什么都收敛到这两个 id 上（`doubao/xxx` 这类带前缀的也认）
pub fn model_for_stream(raw: &str, _list: &[Value]) -> String {
    let base = raw.rsplit_once('/').map(|(_, b)| b).unwrap_or(raw).trim();
    let low = base.to_ascii_lowercase();
    if low.contains("think") || low.contains("reason") || low.contains("深度") {
        "doubao-think".to_string()
    } else {
        "doubao".to_string()
    }
}

/// 界面「模型反代」页的状态段：ready 只看缓存（不触发 agenthub），
/// 拿去决定这张卡能不能被选进上游组。
pub fn view() -> Value {
    json!({
        "ready": auth_ready(),
        "models": model_ids()["data"],
        "cache": cache_path().to_string_lossy(),
    })
}

/// 「测试」按钮：真发一句最短的话，看能不能收正文。
/// 会消耗一条短消息的额度（豆包按额度计，不按 token，代价可忽略）。
pub fn test(model: &str) -> Value {
    let used = model_for_stream(model, &models());
    match DoubaoRound::open("只回复两个字：你好", &used) {
        Ok(mut r) => {
            let mut tr = DoubaoTranslator::new(r.reader(), "doubao");
            let out = collect_completion(&mut tr, "doubao");
            r.cleanup();
            let text = out["choices"][0]["message"]["content"]
                .as_str()
                .unwrap_or("")
                .trim()
                .to_string();
            if text.is_empty() {
                json!({
                    "ok": false,
                    "model": used,
                    "error": "豆包没回正文：登录态可能过期，或今天的额度已经用完"
                })
            } else {
                json!({
                    "ok": true,
                    "model": used,
                    "reply": text.chars().take(60).collect::<String>(),
                    "note": "豆包不返回 token 用量，这条实测只验通不通"
                })
            }
        }
        Err(e) => json!({ "ok": false, "model": used, "error": e }),
    }
}

// ───────────────────────────── 历史拼装 ─────────────────────────────

fn part_text(p: &Value) -> String {
    match p {
        Value::String(s) => s.clone(),
        Value::Object(o) => match o.get("type").and_then(|t| t.as_str()) {
            Some("text") | None => o.get("text").and_then(|t| t.as_str()).unwrap_or("").to_string(),
            Some(_) => String::new(), // 图片/音频：豆包这条链路不吃，静默丢弃比塞个占位符强
        },
        _ => String::new(),
    }
}

fn msg_text(m: &Value) -> String {
    match &m["content"] {
        Value::String(s) => s.clone(),
        Value::Array(a) => a.iter().map(part_text).collect::<Vec<_>>().join(""),
        _ => String::new(),
    }
}

/// 豆包 web 每次都是新会话（conversation_id="0"），所以多轮历史只能压成一段提示词。
/// 用一个稳定的分隔格式，让模型能看出谁说了什么。
pub fn flatten(messages: &Value) -> String {
    let Some(arr) = messages.as_array() else {
        return String::new();
    };
    let mut parts: Vec<String> = Vec::new();
    for m in arr {
        let role = m["role"].as_str().unwrap_or("user");
        let text = msg_text(m);
        if text.trim().is_empty() {
            continue;
        }
        let tag = match role {
            "system" | "developer" => "系统设定",
            "assistant" => "助手",
            "tool" => "工具结果",
            _ => "用户",
        };
        parts.push(format!("【{tag}】\n{text}"));
    }
    parts.join("\n\n")
}

pub fn wants_tools(req: &Value) -> bool {
    req["tools"].as_array().map(|a| !a.is_empty()).unwrap_or(false)
}

// ───────────────────────────── 一轮对话 ─────────────────────────────

pub struct DoubaoRound {
    resp: Option<ureq::Response>,
}

impl DoubaoRound {
    pub fn open(prompt: &str, model: &str) -> Result<DoubaoRound, String> {
        let ck = cookie()?;
        let think = model == "doubao-think";
        let ts = now_ms();
        let body = json!({
            "messages": [{
                "content": json!({ "text": prompt }).to_string(),
                "content_type": 2001,
                "attachments": [],
                "references": [],
            }],
            "conversation_id": "0",
            "local_conversation_id": format!("local_{ts}"),
            "local_message_id": format!("local_{ts}_0"),
            "completion_option": {
                "is_regen": false,
                "with_suggest": false,
                "need_create_conversation": true,
                "launch_stage": 1,
                "is_replace": false,
                "is_delete": false,
                "message_from": 0,
                "action_bar_skill_id": 0,
                "use_deep_think": think,
                "use_auto_cot": false,
            },
            "evaluate_option": { "evaluate_type": 0, "evaluate_content": "" },
            "ext": {},
        });
        let url = format!("{MAIN}/samantha/chat/completion?{}", common_query());
        let call = ureq::post(&url)
            .timeout(ROUND_TIMEOUT)
            .set("User-Agent", UA)
            .set("Accept", "text/event-stream")
            .set("Content-Type", "application/json")
            .set("Referer", &format!("{MAIN}/chat/"))
            .set("Cookie", &ck);
        let resp = match call.send_string(&body.to_string()) {
            Ok(r) => r,
            Err(ureq::Error::Status(code, r)) => {
                let t = r.into_string().unwrap_or_default();
                let msg = serde_json::from_str::<Value>(&t)
                    .ok()
                    .and_then(|v| {
                        v["message"]
                            .as_str()
                            .or_else(|| v["msg"].as_str())
                            .map(|s| s.to_string())
                    })
                    .unwrap_or_else(|| t.chars().take(200).collect());
                if code == 401 || code == 403 {
                    return Err(format!(
                        "豆包拒了会话（{code}）：{msg}。打开豆包重新登录一次，再让 agenthub 刷一次凭据"
                    ));
                }
                return Err(format!("豆包云端 {code}：{msg}"));
            }
            Err(e) => return Err(format!("连不上豆包云端：{e}")),
        };
        Ok(DoubaoRound { resp: Some(resp) })
    }

    /// 收流的本体。只许取一次。`use<>` = 不捕获 self 的生命周期，
    /// 这样 tr 能活过 round.cleanup()（同 pawbridge 的写法）。
    pub fn reader(&mut self) -> impl Read + use<> {
        self.resp.take().expect("reader 只能取一次").into_reader()
    }

    /// 豆包这边一条请求就是一轮，服务端自己收尾，没有"取消/删除会话"要补
    pub fn cleanup(&self) {}
}

const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
(KHTML, like Gecko) Chrome/147.0.0.0 Safari/537.36 Doubao/2.32.6";

/// 公共 query。**故意只留实测必需的那几个**：device_id 空、web_id/tea_uuid 省略都能过。
fn common_query() -> String {
    [
        ("version_code", "20800"),
        ("language", "zh"),
        ("device_platform", "web"),
        ("doubao_device_platform", "desktop"),
        ("aid", "497858"),
        ("real_aid", "497858"),
        ("pkg_type", "release_version"),
        ("device_id", ""),
        ("region", "CN"),
        ("sys_region", "CN"),
        ("samantha_web", "1"),
        ("web_platform", "desktop"),
        ("use-olympus-account", "1"),
    ]
    .iter()
    .map(|(k, v)| format!("{k}={v}"))
    .collect::<Vec<_>>()
    .join("&")
}

// ───────────────────────────── SSE → OpenAI ─────────────────────────────

#[derive(Default, Clone)]
pub struct DoubaoState {
    pub text: String,
    pub completed: bool,
    pub error: Option<(String, String)>,
}

pub struct DoubaoTranslator<R: Read> {
    inner: BufReader<R>,
    pending: Vec<u8>,
    pos: usize,
    eof: bool,
    done_sent: bool,
    id: String,
    created: i64,
    label: String,
    state: Arc<Mutex<DoubaoState>>,
    /// `Read` 实现用的出缓冲（tiny_http 的 Response 要 Read，不是 Iterator）
    out: Vec<u8>,
    out_pos: usize,
}

impl<R: Read> DoubaoTranslator<R> {
    pub fn new(inner: R, label: &str) -> Self {
        Self {
            inner: BufReader::new(inner),
            pending: Vec::new(),
            pos: 0,
            eof: false,
            done_sent: false,
            id: format!("chatcmpl-doubao-{}", now_ms()),
            created: now_secs() as i64,
            label: label.to_string(),
            state: Arc::new(Mutex::new(DoubaoState::default())),
            out: Vec::new(),
            out_pos: 0,
        }
    }

    pub fn shared(&self) -> Arc<Mutex<DoubaoState>> {
        self.state.clone()
    }

    fn chunk(&self, delta: Value, finish: Option<&str>) -> Vec<u8> {
        let c = json!({
            "id": self.id, "object": "chat.completion.chunk", "created": self.created,
            "model": self.label,
            "choices": [{ "index": 0, "delta": delta, "finish_reason": finish }],
        });
        format!("data: {c}\n\n").into_bytes()
    }

    /// 解析一行 `data: {...}`。返回要往下游吐的字节。
    fn feed_line(&mut self, line: &str) -> Vec<u8> {
        let Some(rest) = line.strip_prefix("data:") else {
            return Vec::new();
        };
        let rest = rest.trim();
        if rest.is_empty() {
            return Vec::new();
        }
        let Ok(ev) = serde_json::from_str::<Value>(rest) else {
            return Vec::new();
        };
        let et = ev["event_type"].as_i64().unwrap_or(0);
        let payload = match &ev["event_data"] {
            Value::String(s) => serde_json::from_str::<Value>(s).unwrap_or(Value::Null),
            v if v.is_object() => v.clone(),
            _ => Value::Null,
        };
        if et == 2003 {
            // 结束事件：正文在 message.content 里，但也可能只是收尾标记
            let piece = self.text_of(&payload);
            if let Ok(mut s) = self.state.lock() {
                s.completed = true;
            }
            let mut out = Vec::new();
            if !piece.is_empty() {
                out.extend(self.chunk(json!({"content": piece}), None));
            }
            return out;
        }
        if et != 2001 {
            return Vec::new();
        }
        let piece = self.text_of(&payload);
        if piece.is_empty() {
            return Vec::new();
        }
        if let Ok(mut s) = self.state.lock() {
            s.text.push_str(&piece);
        }
        self.chunk(json!({"content": piece}), None)
    }

    /// 从 2001/2003 的事件体里取正文。content 是**字符串化的 JSON**，
    /// 里面可能没有 text（建议词那种块），没有就当作不是正文。
    fn text_of(&self, payload: &Value) -> String {
        let msg = &payload["message"];
        let content = match &msg["content"] {
            Value::String(s) => s.clone(),
            Value::Object(_) => msg["content"].to_string(),
            _ => return String::new(),
        };
        if content.is_empty() {
            return String::new();
        }
        if let Ok(v) = serde_json::from_str::<Value>(&content) {
            if let Some(t) = v.get("text").and_then(|t| t.as_str()) {
                return t.to_string();
            }
            // 只有 suggest/suggestions 的块不是正文
            if v.get("suggest").is_some() || v.get("suggestions").is_some() {
                return String::new();
            }
        }
        // content 本身就是纯文本（部分事件就是这样）
        content
    }
}

impl<R: Read> Iterator for DoubaoTranslator<R> {
    type Item = Vec<u8>;

    fn next(&mut self) -> Option<Vec<u8>> {
        loop {
            // 先把上一轮读出来的整行发完
            if self.pos < self.pending.len() {
                let end = self.pending[self.pos..]
                    .iter()
                    .position(|b| *b == b'\n')
                    .map(|i| self.pos + i + 1);
                let (line, next_pos) = match end {
                    Some(e) => (self.pending[self.pos..e].to_vec(), e),
                    None => (self.pending[self.pos..].to_vec(), self.pending.len()),
                };
                self.pos = next_pos;
                let text = String::from_utf8_lossy(&line).trim_end().to_string();
                let out = self.feed_line(&text);
                if !out.is_empty() {
                    return Some(out);
                }
                continue;
            }
            if self.eof {
                if self.done_sent {
                    return None;
                }
                self.done_sent = true;
                let mut out = self.chunk(json!({}), Some("stop"));
                out.extend_from_slice(b"data: [DONE]\n\n");
                return Some(out);
            }
            let mut line = String::new();
            match self.inner.read_line(&mut line) {
                Ok(0) => {
                    self.eof = true;
                }
                Ok(_) => {
                    self.pending = line.into_bytes();
                    self.pos = 0;
                }
                Err(e) => {
                    self.eof = true;
                    if let Ok(mut s) = self.state.lock() {
                        s.error = Some(("upstream".into(), format!("读豆包流失败：{e}")));
                    }
                }
            }
        }
    }
}

/// tiny_http 的 Response 要的是 Read：把 Iterator 的字节块按缓冲吐出去
impl<R: Read> Read for DoubaoTranslator<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        loop {
            if self.out_pos < self.out.len() {
                let n = (self.out.len() - self.out_pos).min(buf.len());
                buf[..n].copy_from_slice(&self.out[self.out_pos..self.out_pos + n]);
                self.out_pos += n;
                return Ok(n);
            }
            match self.next() {
                Some(v) => {
                    self.out = v;
                    self.out_pos = 0;
                }
                None => return Ok(0),
            }
        }
    }
}

/// 非流式收尾：把整条流读干，拼成 chat.completion
pub fn collect_completion<R: Read>(tr: &mut DoubaoTranslator<R>, label: &str) -> Value {
    let mut sink = Vec::new();
    let _ = tr.read_to_end(&mut sink);
    let s = tr.shared().lock().map(|g| g.clone()).unwrap_or_default();
    json!({
        "id": format!("chatcmpl-doubao-{}", now_ms()),
        "object": "chat.completion",
        "created": now_secs() as i64,
        "model": label,
        "choices": [{
            "index": 0,
            "message": { "role": "assistant", "content": s.text },
            "finish_reason": if s.completed { "stop" } else { "stop" },
        }],
        // 豆包这条链路不返回 token 用量（只有字符流），不编数字，如实留空
        "usage": Value::Null,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_mark_matches() {
        assert!(is_doubao_base("doubao://chat"));
        assert!(is_doubao_base(" doubao://chat/ "));
        assert!(!is_doubao_base("catpaw://agent"));
    }

    #[test]
    fn model_mapping_collapses() {
        assert_eq!(model_for_stream("doubao-think", &[]), "doubao-think");
        assert_eq!(model_for_stream("doubao/doubao-think", &[]), "doubao-think");
        assert_eq!(model_for_stream("豆包深度思考", &[]), "doubao-think");
        assert_eq!(model_for_stream("gpt-4o", &[]), "doubao");
    }

    #[test]
    fn flatten_keeps_roles() {
        let msgs = json!([
            {"role": "system", "content": "你是助手"},
            {"role": "user", "content": [{"type": "text", "text": "第一问"}]},
            {"role": "assistant", "content": "第一答"},
            {"role": "user", "content": "第二问"}
        ]);
        let p = flatten(&msgs);
        assert!(p.contains("【系统设定】"), "{p}");
        assert!(p.contains("【用户】\n第一问"), "{p}");
        assert!(p.contains("【助手】\n第一答"), "{p}");
    }

    #[test]
    fn suggest_blocks_are_not_text() {
        let tr = DoubaoTranslator::new(std::io::Cursor::new(Vec::new()), "doubao");
        let payload = json!({"message": {"content": "{\"suggest\":\"x\",\"suggestions\":[\"x\"]}"}});
        assert_eq!(tr.text_of(&payload), "");
        let ok = json!({"message": {"content": "{\"text\":\"你好\"}"}});
        assert_eq!(tr.text_of(&ok), "你好");
    }
}
