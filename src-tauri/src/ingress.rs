//! 反代的多协议进站层。
//!
//! 8788 反代原来是「只进 OpenAI Chat Completions」：不管客户端说什么协议，
//! 路径不是 `…/chat/completions` 就被上游适配器当 404 拒掉（实测翻车点：
//! Codex 桌面端走 Responses API，一发消息就撞
//! "MiniMax 上游只实现了 /v1/chat/completions，没有 /v1/responses"）。
//!
//! 本模块把三种进站协议**统一翻译成 OpenAI Chat 报文**，然后由 proxy.rs 回环
//! 打到自己的 `/v1/chat/completions` 上 —— 路由、各家上游翻译、日志、记账全部
//! 原链复用，一行不改。回环安全的前提是 proxy 每个请求一个 worker 线程，
//! 不会把 accept 循环堵死。
//!
//! - **Responses**（`POST /v1/responses`）：Codex 原生协议。SSE 事件序列按
//!   codex 客户端认的样子发（response.created → output_item.added → … →
//!   response.completed）；上游的 `reasoning_content` 翻成 reasoning 摘要事件。
//! - **Anthropic**（`POST /v1/messages`）：Claude Code 等 Anthropic 系客户端。
//!   SSE 翻成 message_start / content_block_* / message_delta / message_stop。
//! - **Gemini**（`POST /v1beta/models/{model}:generateContent[?alt=sse]`）：
//!   Google 系客户端。流式按 alt=sse 的 `data: {candidates:[…]}` 帧发。
//!
//! 模型名照旧支持 `agent/模型` 前缀（Gemini 从 URL 里取，其余从报文里取，
//! proxy 的前缀路由在进站翻译之前已经跑完）。
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::Read;

/// 回环请求的标记头：proxy 看到它就不再当进站协议处理（防回环），也不外发上游。
pub const LOOPBACK_MARK: &str = "x-ac-ingress";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Ingress {
    Responses,
    Anthropic,
    Gemini,
}

impl Ingress {
    pub fn label(self) -> &'static str {
        match self {
            Ingress::Responses => "responses",
            Ingress::Anthropic => "anthropic",
            Ingress::Gemini => "gemini",
        }
    }

    /// 按方法+路径认进站协议。URL 带 query 也行（Gemini 的 ?alt=sse&key=…）。
    pub fn detect(method: &str, url: &str) -> Option<Ingress> {
        let p = url.split('?').next().unwrap_or(url).trim_end_matches('/');
        if method == "POST" {
            if p.ends_with("/responses") {
                return Some(Ingress::Responses);
            }
            if p.ends_with("/messages") {
                return Some(Ingress::Anthropic);
            }
            if p.contains(":generateContent")
                || p.contains(":streamGenerateContent")
                || p.contains(":countTokens")
            {
                return Some(Ingress::Gemini);
            }
        }
        // Gemini 的模型清单：GET /v1beta/models（OpenAI 形状的 /v1/models 不在这认）
        if method == "GET" && p.ends_with("/models") && p.contains("v1beta") {
            return Some(Ingress::Gemini);
        }
        None
    }
}

/// Gemini 把模型名放 URL 里（/v1beta/models/{model}:generateContent），
/// 报文体里没有 model 字段 —— proxy 的日志和前缀路由都要用它。
pub fn model_from_url(method: &str, url: &str) -> Option<String> {
    if method != "POST" && method != "GET" {
        return None;
    }
    let p = url.split('?').next().unwrap_or(url);
    let seg = p.split("/models/").nth(1)?;
    let seg = seg.split(':').next().unwrap_or(seg);
    let seg = seg.trim_end_matches('/');
    if seg.is_empty() {
        return None;
    }
    // 带 `agent/模型` 前缀的也放行 —— 前缀路由在 proxy 里靠它分流
    Some(seg.to_string())
}

// ───────────────────────── 进站请求 → OpenAI Chat ─────────────────────────

/// 把进站报文翻译成 `/v1/chat/completions` 的请求体。
/// 返回的报文只含各家 chat 上游都认的字段（model/messages/tools/stream/采样参数）。
pub fn to_chat_request(ing: Ingress, url: &str, body: &[u8]) -> Result<Value, String> {
    if body.is_empty() {
        return Err("请求体是空的".into());
    }
    let v: Value = serde_json::from_slice(body).map_err(|_| "请求体不是合法 JSON".to_string())?;
    match ing {
        Ingress::Responses => responses_to_chat(&v),
        Ingress::Anthropic => anthropic_to_chat(&v),
        Ingress::Gemini => {
            let model = model_from_url("POST", url)
                .or_else(|| v["model"].as_str().map(str::to_string))
                .unwrap_or_default();
            gemini_to_chat(&v, &model, url_is_stream(url))
        }
    }
}

fn base_chat(model: &str, msgs: Vec<Value>, stream: bool) -> Value {
    json!({ "model": model, "messages": msgs, "stream": stream })
}

/// Responses 请求 → Chat。要点：
/// - `instructions` 进 system；`input` 是 string 或 item 数组；
/// - function_call / function_call_output 翻成 chat 的 tool_calls / tool 消息，
///   否则带工具的 Agent 循环第二轮就断；
/// - `reasoning` item 原样丢弃 —— 上游没法回放加密推理，回放必 400。
fn responses_to_chat(v: &Value) -> Result<Value, String> {
    let mut msgs: Vec<Value> = Vec::new();
    if let Some(s) = v["instructions"].as_str() {
        if !s.trim().is_empty() {
            msgs.push(json!({ "role": "system", "content": s }));
        }
    }
    match &v["input"] {
        Value::String(s) => msgs.push(json!({ "role": "user", "content": s })),
        Value::Array(items) => {
            for it in items {
                push_rs_item(&mut msgs, it)?;
            }
        }
        Value::Null => {}
        _ => return Err("input 只支持字符串或 item 数组".into()),
    }
    if msgs.is_empty() {
        return Err("input 里没有可用的消息".into());
    }
    let model = v["model"].as_str().unwrap_or("");
    if model.is_empty() {
        return Err("请求缺 model 字段".into());
    }
    lift_tool_images(&mut msgs);
    let mut out = base_chat(model, msgs, v["stream"].as_bool().unwrap_or(false));
    if let Some(m) = v["max_output_tokens"].as_u64() {
        out["max_tokens"] = json!(m);
    }
    for (src, dst) in [("temperature", "temperature"), ("top_p", "top_p")] {
        if let Some(x) = v[src].as_f64() {
            out[dst] = json!(x);
        }
    }
    let mut tools: Vec<Value> = Vec::new();
    for t in v["tools"].as_array().cloned().unwrap_or_default() {
        if t["type"].as_str() != Some("function") {
            continue; // web_search / local_shell 等内置工具上游没有对应物，跳过
        }
        let params = if t["parameters"].is_null() {
            json!({ "type": "object", "properties": {} })
        } else {
            t["parameters"].clone()
        };
        tools.push(json!({
            "type": "function",
            "function": {
                "name": t["name"].as_str().unwrap_or(""),
                "description": t["description"].as_str().unwrap_or(""),
                "parameters": params,
            }
        }));
    }
    if !tools.is_empty() {
        out["tools"] = json!(tools);
    }
    match &v["tool_choice"] {
        Value::String(s) if !s.is_empty() && s != "auto" => out["tool_choice"] = json!(s),
        Value::Object(_) if v["tool_choice"]["type"].as_str() == Some("function") => {
            out["tool_choice"] = json!({
                "type": "function",
                "function": { "name": v["tool_choice"]["name"].as_str().unwrap_or("") }
            });
        }
        _ => {}
    }
    Ok(out)
}

fn chat_image(part: &Value) -> Result<Value, String> {
    let url = crate::gateway_vision::image_url(part)
        .ok_or("图片缺少 URL 或内联数据，网关无法转发")?;
    let mut image = json!({"type":"image_url","image_url":{"url":url}});
    if let Some(detail) = part.get("detail").or_else(|| part["image_url"].get("detail")) {
        image["image_url"]["detail"] = detail.clone();
    }
    Ok(image)
}

fn tool_content(blocks: &[Value]) -> Result<Value, String> {
    let mut out = Vec::new();
    for b in blocks {
        match b["type"].as_str() {
            Some("image" | "input_image" | "image_url") => out.push(chat_image(b)?),
            Some("text" | "input_text" | "output_text") => out.push(json!({"type":"text","text":b["text"]})),
            _ => {}
        }
    }
    if out.iter().any(|b| b["type"] == "image_url") { Ok(json!(out)) }
    else { Ok(json!(blocks_text(&out))) }
}

// Chat tool outputs are text. Keep consecutive tool replies together and put
// their images in a user message immediately afterwards, linked to the call ID.
fn lift_tool_images(messages: &mut Vec<Value>) {
    let mut output = Vec::new();
    let mut images = Vec::new();
    for mut m in messages.drain(..) {
        if m["role"] == "tool" {
            if let Some(parts) = m["content"].as_array() {
                images.push(json!({"type":"text","text":format!("工具结果 {} 的图片", m["tool_call_id"].as_str().unwrap_or(""))}));
                images.extend(parts.iter().filter(|b| b["type"] == "image_url").cloned());
                m["content"] = json!(blocks_text(parts));
            }
        } else if !images.is_empty() {
            output.push(json!({"role":"user","content":std::mem::take(&mut images)}));
        }
        output.push(m);
    }
    if !images.is_empty() { output.push(json!({"role":"user","content":images})); }
    *messages = output;
}

fn push_rs_item(msgs: &mut Vec<Value>, it: &Value) -> Result<(), String> {
    match it["type"].as_str().unwrap_or("message") {
        "message" => {
            let role = it["role"].as_str().unwrap_or("user");
            let c = &it["content"];
            if let Some(s) = c.as_str() {
                if !s.is_empty() {
                    msgs.push(json!({ "role": role, "content": s }));
                }
                return Ok(());
            }
            let mut parts: Vec<Value> = Vec::new();
            for p in c.as_array().cloned().unwrap_or_default() {
                match p["type"].as_str().unwrap_or("") {
                    "input_text" | "output_text" | "text" | "refusal" => {
                        let t = p["text"].as_str().unwrap_or("");
                        if !t.is_empty() {
                            parts.push(json!({ "type": "text", "text": t }));
                        }
                    }
                    "input_image" => parts.push(chat_image(&p)?),
                    _ => {}
                }
            }
            if !parts.is_empty() {
                let content = if parts.len() == 1 && parts[0]["type"] == "text" {
                    json!(parts[0]["text"].as_str().unwrap_or(""))
                } else {
                    json!(parts)
                };
                msgs.push(json!({ "role": role, "content": content }));
            }
        }
        "function_call" => {
            let args = it["arguments"].as_str().unwrap_or("{}");
            msgs.push(json!({
                "role": "assistant",
                "content": Value::Null,
                "tool_calls": [{
                    "id": it["call_id"].as_str().unwrap_or("call_0"),
                    "type": "function",
                    "function": {
                        "name": it["name"].as_str().unwrap_or(""),
                        "arguments": args,
                    }
                }]
            }));
        }
        "function_call_output" => {
            let output = match &it["output"] {
                Value::String(s) => json!(s),
                Value::Array(blocks) if blocks.iter().any(|b| matches!(b["type"].as_str(), Some("input_image" | "image_url" | "image" | "input_text"))) => tool_content(blocks)?,
                Value::Null => json!(""),
                other => json!(other.to_string()),
            };
            msgs.push(json!({
                "role": "tool",
                "tool_call_id": it["call_id"].as_str().unwrap_or("call_0"),
                "content": output,
            }));
        }
        // reasoning / item_reference / 等未知 item：没法向上游回放，跳过
        _ => {}
    }
    Ok(())
}

/// Anthropic Messages 请求 → Chat。要点：
/// - tool_result 块单独翻成 role=tool 消息（Anthropic 把它塞在 user 消息里）；
/// - tool_use 的 input 是对象，chat 的 arguments 是字符串；
/// - thinking 块丢弃：上游没法验证 Anthropic 的签名。
fn anthropic_to_chat(v: &Value) -> Result<Value, String> {
    let mut msgs: Vec<Value> = Vec::new();
    match &v["system"] {
        Value::String(s) if !s.is_empty() => msgs.push(json!({ "role": "system", "content": s })),
        Value::Array(blocks) => {
            let t = blocks_text(blocks);
            if !t.is_empty() {
                msgs.push(json!({ "role": "system", "content": t }));
            }
        }
        _ => {}
    }
    for m in v["messages"].as_array().cloned().unwrap_or_default() {
        let role = if m["role"].as_str() == Some("assistant") { "assistant" } else { "user" };
        // 一个Anthropic消息可能混着 text / tool_use / tool_result，翻出来是几条 chat 消息
        let mut pending: Option<Value> = None; // 暂存的 user/assistant 文本消息
        let flush = |pending: &mut Option<Value>, msgs: &mut Vec<Value>| {
            if let Some(p) = pending.take() {
                msgs.push(p);
            }
        };
        match &m["content"] {
            Value::String(s) => {
                if !s.is_empty() {
                    msgs.push(json!({ "role": role, "content": s }));
                }
            }
            Value::Array(blocks) => {
                let mut texts: Vec<Value> = Vec::new();
                let mut tool_calls: Vec<Value> = Vec::new();
                for b in blocks {
                    match b["type"].as_str().unwrap_or("") {
                        "text" => {
                            let t = b["text"].as_str().unwrap_or("");
                            if !t.is_empty() {
                                texts.push(json!({ "type": "text", "text": t }));
                            }
                        }
                        "image" => texts.push(chat_image(b)?),
                        "tool_use" => {
                            tool_calls.push(json!({
                                "id": b["id"].as_str().unwrap_or("call_0"),
                                "type": "function",
                                "function": {
                                    "name": b["name"].as_str().unwrap_or(""),
                                    "arguments": b["input"].to_string(),
                                }
                            }));
                        }
                        "tool_result" => {
                            flush(&mut pending, &mut msgs);
                            let content = if let Some(s) = b["content"].as_str() {
                                json!(s)
                            } else {
                                tool_content(b["content"].as_array().map(|a| a.as_slice()).unwrap_or(&[]))?
                            };
                            msgs.push(json!({
                                "role": "tool",
                                "tool_call_id": b["tool_use_id"].as_str().unwrap_or("call_0"),
                                "content": content,
                            }));
                        }
                        _ => {} // thinking / redacted_thinking：丢弃
                    }
                }
                if !tool_calls.is_empty() {
                    let content = if texts.len() == 1 && texts[0]["type"] == "text" {
                        json!(texts[0]["text"].as_str().unwrap_or(""))
                    } else if texts.is_empty() {
                        Value::Null
                    } else {
                        json!(texts)
                    };
                    msgs.push(json!({ "role": "assistant", "content": content, "tool_calls": tool_calls }));
                } else if !texts.is_empty() {
                    let content = if texts.len() == 1 && texts[0]["type"] == "text" {
                        json!(texts[0]["text"].as_str().unwrap_or(""))
                    } else {
                        json!(texts)
                    };
                    pending = Some(json!({ "role": role, "content": content }));
                }
            }
            _ => {}
        }
        flush(&mut pending, &mut msgs);
    }
    if msgs.is_empty() {
        return Err("messages 里没有可用的内容".into());
    }
    let model = v["model"].as_str().unwrap_or("");
    if model.is_empty() {
        return Err("请求缺 model 字段".into());
    }
    lift_tool_images(&mut msgs);
    let mut out = base_chat(model, msgs, v["stream"].as_bool().unwrap_or(false));
    if let Some(m) = v["max_tokens"].as_u64() {
        out["max_tokens"] = json!(m);
    }
    for (src, dst) in [("temperature", "temperature"), ("top_p", "top_p")] {
        if let Some(x) = v[src].as_f64() {
            out[dst] = json!(x);
        }
    }
    let mut tools: Vec<Value> = Vec::new();
    for t in v["tools"].as_array().cloned().unwrap_or_default() {
        let params = if t["input_schema"].is_null() {
            json!({ "type": "object", "properties": {} })
        } else {
            t["input_schema"].clone()
        };
        tools.push(json!({
            "type": "function",
            "function": {
                "name": t["name"].as_str().unwrap_or(""),
                "description": t["description"].as_str().unwrap_or(""),
                "parameters": params,
            }
        }));
    }
    if !tools.is_empty() {
        out["tools"] = json!(tools);
    }
    let tc = &v["tool_choice"];
    match tc["type"].as_str().unwrap_or("") {
        "auto" => out["tool_choice"] = json!("auto"),
        "none" => out["tool_choice"] = json!("none"),
        "any" => out["tool_choice"] = json!("required"),
        "tool" => out["tool_choice"] = json!({
            "type": "function",
            "function": { "name": tc["name"].as_str().unwrap_or("") }
        }),
        _ => {}
    }
    Ok(out)
}

/// Gemini generateContent 请求 → Chat。要点：
/// - 模型名在 URL 里（to_chat_request 已取好传进来），保留 `agent/模型` 直到最终选择上游；
/// - functionCall 没有调用 id，翻成 tool_calls 时合成 `call_N`，并按名字记账，
///   之后 functionResponse 靠名字找回同一个 id（chat 的 tool 消息必须对得上 tool_call_id）；
/// - role "model" 对应 chat 的 assistant。
fn gemini_to_chat(v: &Value, model: &str, stream: bool) -> Result<Value, String> {
    if model.is_empty() {
        return Err("URL 里取不到模型名（应为 /v1beta/models/{model}:generateContent）".into());
    }
    let mut msgs: Vec<Value> = Vec::new();
    for key in ["systemInstruction", "system_instruction"] {
        let si = &v[key];
        if si.is_null() {
            continue;
        }
        let t = si["parts"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|p| p["text"].as_str().map(str::to_string))
            .collect::<Vec<_>>()
            .join("\n");
        if !t.is_empty() {
            msgs.push(json!({ "role": "system", "content": t }));
        }
        break;
    }
    let mut call_ids: HashMap<String, String> = HashMap::new();
    let mut call_n = 0usize;
    for c in v["contents"].as_array().cloned().unwrap_or_default() {
        let role = if c["role"].as_str() == Some("model") { "assistant" } else { "user" };
        let mut texts: Vec<Value> = Vec::new();
        let mut tool_calls: Vec<Value> = Vec::new();
        let mut tool_msgs: Vec<Value> = Vec::new();
        for p in c["parts"].as_array().cloned().unwrap_or_default() {
            let fd = p.get("functionCall").or_else(|| p.get("function_call"));
            let fr = p.get("functionResponse").or_else(|| p.get("function_response"));
            let image = ["inlineData","inline_data","fileData","file_data"].iter()
                .filter_map(|k| p.get(*k)).any(|d| d.get("mimeType").or_else(|| d.get("mime_type"))
                    .and_then(Value::as_str).is_some_and(|s| s.starts_with("image/")));
            if let Some(fc) = fd {
                let name = fc["name"].as_str().unwrap_or("").to_string();
                let id = format!("call_{}", call_n);
                call_n += 1;
                call_ids.insert(name.clone(), id.clone());
                let args = if fc["args"].is_null() { json!({}) } else { fc["args"].clone() };
                tool_calls.push(json!({
                    "id": id, "type": "function",
                    "function": { "name": name, "arguments": args.to_string() }
                }));
            } else if let Some(fr) = fr {
                let name = fr["name"].as_str().unwrap_or("").to_string();
                let id = call_ids
                    .get(&name)
                    .cloned()
                    .unwrap_or_else(|| format!("call_{}", {
                        call_n += 1; call_n
                    }));
                let resp = if fr["response"].is_null() { json!({}) } else { fr["response"].clone() };
                tool_msgs.push(json!({ "role": "tool", "tool_call_id": id, "content": resp.to_string() }));
            } else if image {
                texts.push(chat_image(&p)?);
            } else if let Some(t) = p["text"].as_str() {
                if !t.is_empty() {
                    texts.push(json!({ "type": "text", "text": t }));
                }
            }
        }
        if !tool_calls.is_empty() {
            let content = if texts.len() == 1 && texts[0]["type"] == "text" {
                json!(texts[0]["text"].as_str().unwrap_or(""))
            } else if texts.is_empty() {
                Value::Null
            } else {
                json!(texts)
            };
            msgs.push(json!({ "role": "assistant", "content": content, "tool_calls": tool_calls }));
        } else if !texts.is_empty() {
            let content = if texts.len() == 1 && texts[0]["type"] == "text" {
                json!(texts[0]["text"].as_str().unwrap_or(""))
            } else {
                json!(texts)
            };
            msgs.push(json!({ "role": role, "content": content }));
        }
        msgs.append(&mut tool_msgs);
    }
    if msgs.is_empty() {
        return Err("contents 里没有可用的内容".into());
    }
    let mut out = base_chat(model, msgs, stream);
    let gc = &v["generationConfig"].as_object().cloned().unwrap_or_default();
    let g = Value::Object(gc.clone());
    if let Some(m) = g["maxOutputTokens"]
        .as_u64()
        .or_else(|| g["max_output_tokens"].as_u64())
    {
        out["max_tokens"] = json!(m);
    }
    for (src, dst) in [("temperature", "temperature"), ("topP", "top_p"), ("top_p", "top_p")] {
        if let Some(x) = g[src].as_f64() {
            out[dst] = json!(x);
        }
    }
    let mut tools: Vec<Value> = Vec::new();
    for t in v["tools"].as_array().cloned().unwrap_or_default() {
        for fd in t["functionDeclarations"]
            .as_array()
            .or_else(|| t["function_declarations"].as_array())
            .cloned()
            .unwrap_or_default()
        {
            let params = if fd["parameters"].is_null() {
                json!({ "type": "object", "properties": {} })
            } else {
                fd["parameters"].clone()
            };
            tools.push(json!({
                "type": "function",
                "function": {
                    "name": fd["name"].as_str().unwrap_or(""),
                    "description": fd["description"].as_str().unwrap_or(""),
                    "parameters": params,
                }
            }));
        }
    }
    if !tools.is_empty() {
        out["tools"] = json!(tools);
    }
    let mode = v["toolConfig"]["functionCallingConfig"]["mode"]
        .as_str()
        .or_else(|| {
            v["tool_config"]["function_calling_config"]["mode"].as_str()
        })
        .unwrap_or("");
    match mode {
        "ANY" => out["tool_choice"] = json!("required"),
        "NONE" => out["tool_choice"] = json!("none"),
        "AUTO" => out["tool_choice"] = json!("auto"),
        _ => {}
    }
    Ok(out)
}

fn blocks_text(blocks: &[Value]) -> String {
    blocks
        .iter()
        .filter(|b| b["type"].as_str() == Some("text"))
        .filter_map(|b| b["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

/// streamGenerateContent 就是流式；generateContent 是非流式。alt=sse 只是编码提示。
fn url_is_stream(url: &str) -> bool {
    url.contains(":streamGenerateContent")
}

// ───────────────────────── OpenAI Chat 响应 → 进站协议 ─────────────────────────

/// 非流式：把上游链产出的 OpenAI chat 响应翻成进站协议的形状。
pub fn rewrite_json(ing: Ingress, chat: &Value, model: &str) -> Result<Value, String> {
    let choice = chat["choices"]
        .get(0)
        .ok_or_else(|| "上游响应没有 choices".to_string())?;
    let msg = &choice["message"];
    let finish = choice["finish_reason"].as_str().unwrap_or("stop");
    let (pt, ct, tt) = usage_of(chat["usage"].as_object());
    match ing {
        Ingress::Responses => {
            let mut output: Vec<Value> = Vec::new();
            let msg_id = format!("msg_{}", chat["id"].as_str().unwrap_or("0"));
            let text = content_text_of(msg);
            if !text.is_empty() {
                output.push(json!({
                    "type": "message", "id": msg_id, "role": "assistant", "status": "completed",
                    "content": [{ "type": "output_text", "text": text, "annotations": [] }],
                }));
            }
            for tc in msg["tool_calls"].as_array().cloned().unwrap_or_default() {
                output.push(json!({
                    "type": "function_call", "id": format!("fc_{}", tc["id"].as_str().unwrap_or("0")),
                    "call_id": tc["id"].as_str().unwrap_or("call_0"),
                    "name": tc["function"]["name"].as_str().unwrap_or(""),
                    "arguments": tc["function"]["arguments"].as_str().unwrap_or("{}"),
                    "status": "completed",
                }));
            }
            Ok(json!({
                "id": chat["id"].as_str().unwrap_or("resp_0"),
                "object": "response",
                "created_at": crate::registry::now_secs() as i64,
                "status": "completed",
                "model": model,
                "output": output,
                "usage": {
                    "input_tokens": pt, "output_tokens": ct, "total_tokens": tt,
                    "input_tokens_details": { "cached_tokens": 0 },
                    "output_tokens_details": { "reasoning_tokens": 0 },
                },
            }))
        }
        Ingress::Anthropic => {
            let mut content: Vec<Value> = Vec::new();
            if let Some(t) = msg["reasoning_content"].as_str() {
                if !t.is_empty() {
                    content.push(json!({ "type": "thinking", "thinking": t, "signature": "" }));
                }
            }
            let text = content_text_of(msg);
            if !text.is_empty() {
                content.push(json!({ "type": "text", "text": text }));
            }
            for tc in msg["tool_calls"].as_array().cloned().unwrap_or_default() {
                let input: Value =
                    serde_json::from_str(tc["function"]["arguments"].as_str().unwrap_or("{}"))
                        .unwrap_or_else(|_| json!({}));
                content.push(json!({
                    "type": "tool_use",
                    "id": tc["id"].as_str().unwrap_or("call_0"),
                    "name": tc["function"]["name"].as_str().unwrap_or(""),
                    "input": input,
                }));
            }
            Ok(json!({
                "id": chat["id"].as_str().unwrap_or("msg_0"),
                "type": "message", "role": "assistant", "model": model,
                "content": content,
                "stop_reason": finish_to_anthropic(finish),
                "stop_sequence": Value::Null,
                "usage": { "input_tokens": pt, "output_tokens": ct },
            }))
        }
        Ingress::Gemini => {
            // 推理文本不进正文：Gemini 语义里那就是模型输出
            let mut parts: Vec<Value> = Vec::new();
            let text = content_text_of(msg);
            if !text.is_empty() {
                parts.push(json!({ "text": text }));
            }
            for tc in msg["tool_calls"].as_array().cloned().unwrap_or_default() {
                let args: Value =
                    serde_json::from_str(tc["function"]["arguments"].as_str().unwrap_or("{}"))
                        .unwrap_or_else(|_| json!({}));
                parts.push(json!({
                    "functionCall": {
                        "name": tc["function"]["name"].as_str().unwrap_or(""),
                        "args": args,
                    }
                }));
            }
            Ok(json!({
                "candidates": [{
                    "content": { "role": "model", "parts": parts },
                    "finishReason": finish_to_gemini(finish),
                    "index": 0,
                }],
                "usageMetadata": {
                    "promptTokenCount": pt, "candidatesTokenCount": ct, "totalTokenCount": tt,
                },
                "modelVersion": model,
                "responseId": chat["id"].as_str().unwrap_or("resp_0"),
            }))
        }
    }
}

/// 上游链（或本轮进站翻译）给的错误翻成各协议的报错形状。
/// Responses 本来就是 OpenAI 形状，原样回；其余两家用各自的 error 包裹。
pub fn error_json(ing: Ingress, status: u16, msg: &str) -> Value {
    match ing {
        Ingress::Responses => json!({ "error": { "message": msg, "type": "invalid_request_error", "code": null } }),
        Ingress::Anthropic => json!({ "type": "error", "error": { "type": "invalid_request_error", "message": msg } }),
        Ingress::Gemini => json!({ "error": { "code": status, "message": msg, "status": gemini_status(status) } }),
    }
}

/// 上游链回的错误（OpenAI 形状）→ 进站协议的报错形状。
pub fn rewrite_error(ing: Ingress, status: u16, upstream: &Value) -> Value {
    let msg = upstream["error"]["message"]
        .as_str()
        .or_else(|| upstream["error"].as_str())
        .unwrap_or("上游报错（无详情）")
        .to_string();
    let ty = upstream["error"]["type"].as_str().unwrap_or("api_error").to_string();
    match ing {
        // Responses 客户端就吃 OpenAI 形状，原样透传
        Ingress::Responses => json!({ "error": { "message": msg, "type": ty, "code": upstream["error"]["code"] } }),
        Ingress::Anthropic => json!({ "type": "error", "error": { "type": anthropic_err_type(&ty), "message": msg } }),
        Ingress::Gemini => json!({ "error": { "code": status, "message": msg, "status": gemini_status(status) } }),
    }
}

fn anthropic_err_type(openai_ty: &str) -> &'static str {
    let t = openai_ty.to_lowercase();
    if t.contains("rate") {
        "rate_limit_error"
    } else if t.contains("auth") || t.contains("api_key") || t.contains("permission") {
        "authentication_error"
    } else if t.contains("invalid") || t.contains("request") {
        "invalid_request_error"
    } else {
        "api_error"
    }
}

fn gemini_status(status: u16) -> &'static str {
    match status {
        400 => "INVALID_ARGUMENT",
        401 | 403 => "PERMISSION_DENIED",
        404 => "NOT_FOUND",
        429 => "RESOURCE_EXHAUSTED",
        500..=599 => "INTERNAL",
        _ => "UNKNOWN",
    }
}

fn finish_to_anthropic(finish: &str) -> &'static str {
    match finish {
        "length" => "max_tokens",
        "tool_calls" | "function_call" => "tool_use",
        _ => "end_turn",
    }
}

fn finish_to_gemini(finish: &str) -> &'static str {
    match finish {
        "length" => "MAX_TOKENS",
        _ => "STOP",
    }
}

fn usage_of(u: Option<&serde_json::Map<String, Value>>) -> (u64, u64, u64) {
    let Some(u) = u else { return (0, 0, 0) };
    let g = |k: &str| u.get(k).and_then(Value::as_u64).unwrap_or(0);
    let pt = g("prompt_tokens");
    let ct = g("completion_tokens");
    let tt = g("total_tokens").max(pt + ct);
    (pt, ct, tt)
}

/// chat 的 content 可能是 string，也可能是 [{type:text,...}] —— 统一并成纯文本。
fn content_text_of(msg: &Value) -> String {
    match &msg["content"] {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter(|p| {
                let t = p["type"].as_str().unwrap_or("text");
                t == "text" || t == "output_text"
            })
            .filter_map(|p| p["text"].as_str())
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

/// Gemini 的模型清单：把 proxy 的 OpenAI 形状清单翻成 Google 形状。
/// name 带 `models/` 前缀，id 保留 `agent/模型` 全名 —— 客户端发回来时能对上。
pub fn gemini_models_json(models: &[Value]) -> Value {
    let list: Vec<Value> = models
        .iter()
        .map(|m| {
            let mut item = json!({
                "name": format!("models/{}", m["id"].as_str().unwrap_or("")),
                "displayName": m["name"].as_str().unwrap_or(""),
                "description": m["name"].as_str().unwrap_or(""),
                "supportedGenerationMethods": ["generateContent", "streamGenerateContent", "countTokens"],
            });
            if let Some(n) = crate::model_caps::positive(m, "context_window") { item["inputTokenLimit"] = json!(n); }
            if let Some(n) = crate::model_caps::positive(m, "max_output_tokens") { item["outputTokenLimit"] = json!(n); }
            item
        })
        .collect();
    json!({ "models": list })
}

/// `:countTokens`：本地估算（≈4 字符一个 token，另按块数补零头）。
/// 只是让硬依赖这个接口的客户端别在启动时崩，数字不参与计费。
pub fn gemini_count_tokens(body: &[u8]) -> Value {
    let v: Value = serde_json::from_slice(body).unwrap_or(json!({}));
    let mut chars = 0usize;
    for key in ["systemInstruction", "system_instruction"] {
        for p in v[key]["parts"].as_array().cloned().unwrap_or_default() {
            chars += p["text"].as_str().map(str::len).unwrap_or(0);
        }
    }
    for c in v["contents"].as_array().cloned().unwrap_or_default() {
        for p in c["parts"].as_array().cloned().unwrap_or_default() {
            chars += p["text"].as_str().map(str::len).unwrap_or(0);
        }
    }
    json!({ "totalTokens": (chars / 4 + 8).max(1) })
}

// ───────────────────────── SSE：OpenAI chat 流 → 进站协议流 ─────────────────────────

/// 一段已完成的 SSE 文本帧解析成 (event, data)。
fn parse_block(text: &str) -> (String, String) {
    let mut ev = String::new();
    let mut data = String::new();
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("event:") {
            ev = rest.trim().to_string();
        } else if let Some(rest) = line.strip_prefix("data:") {
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(rest.trim_start());
        }
    }
    (ev, data)
}

/// 在缓冲里找最近的 SSE 帧分隔（空行），返回 (帧内容结束位置, 分隔长度)。
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

/// Responses 流里一个文本类 output item 的累积状态。
/// pos 是它在 items 里的下标 —— output_index、最终 completed 的 output 都靠它对齐。
struct TextAcc {
    pos: usize,
    id: String,
    acc: String,
}

/// chat 流里一个 tool_call 的累积状态（按 chat 的 tool_calls[].index 对齐）。
/// slot：Responses 里是 items 下标，Anthropic 里是 content block 下标。
struct ToolAcc {
    chat_index: u64,
    call_id: String,
    name: String,
    args: String,
    opened: bool, // 起始 item/block 事件发过没有
    slot: usize,
}

/// 把内层 `/v1/chat/completions` 的 SSE（标准 OpenAI chunk，含各家适配器补的
/// `reasoning_content`）翻成进站协议的 SSE。用法与 mmproxy::StreamTranslator 相同：
/// 包住上游 reader，直接塞给 tiny_http 的 Response。
pub struct SseTranslator<R: Read> {
    inner: R,
    ing: Ingress,
    model: String,
    id: String,
    created: i64,
    raw: Vec<u8>,
    out: Vec<u8>,
    pos: usize,
    eof: bool,
    errored: bool,
    usage: Value,
    finish: Option<String>,
    flushed: bool,
    // Responses：已开的 output item（completed 时整体带走）
    items: Vec<Value>,
    text: Option<TextAcc>,
    reason: Option<TextAcc>,
    // 三个协议共用的 tool 累积（chat_index → 状态）
    tools: Vec<ToolAcc>,
    // Responses 的 response.created 只发一次
    created_sent: bool,
    // Anthropic
    a_started: bool,
    a_block: Option<(&'static str, u64, String)>, // (text|thinking, block index, 累积)
    a_block_idx: u64,
}

impl<R: Read> SseTranslator<R> {
    pub fn new(inner: R, ing: Ingress, model: &str) -> Self {
        let now = crate::registry::now_secs() as i64;
        let id = match ing {
            Ingress::Responses => format!("resp_{}", now),
            Ingress::Anthropic => format!("msg_{}", now),
            Ingress::Gemini => format!("resp_{}", now),
        };
        Self {
            inner,
            ing,
            model: model.to_string(),
            id,
            created: now,
            raw: Vec::new(),
            out: Vec::new(),
            pos: 0,
            eof: false,
            errored: false,
            usage: json!({}),
            finish: None,
            flushed: false,
            items: Vec::new(),
            text: None,
            reason: None,
            tools: Vec::new(),
            created_sent: false,
            a_started: false,
            a_block: None,
            a_block_idx: 0,
        }
    }

    fn emit_event(&mut self, ev: &str, v: &Value) {
        self.out
            .extend_from_slice(format!("event: {ev}\ndata: {v}\n\n").as_bytes());
    }

    fn emit_data(&mut self, v: &Value) {
        self.out
            .extend_from_slice(format!("data: {v}\n\n").as_bytes());
    }

    fn usage_nums(&self) -> (u64, u64, u64) {
        usage_of(self.usage.as_object())
    }

    fn response_skeleton(&self, status: &str) -> Value {
        json!({
            "id": self.id, "object": "response", "created_at": self.created,
            "status": status, "model": self.model, "output": self.items,
            "error": Value::Null, "incomplete_details": Value::Null,
            "instructions": Value::Null, "metadata": {},
            "temperature": Value::Null, "top_p": Value::Null,
            "tool_choice": "auto", "parallel_tool_calls": true,
            "previous_response_id": Value::Null, "reasoning": { "effort": Value::Null, "summary": Value::Null },
            "store": false,
        })
    }

    fn handle_chunk(&mut self, v: &Value) {
        if self.flushed || self.errored {
            return; // completed 之后上游再吐什么都不接，别把终止事件发重了
        }
        // 上游链的错误帧：mm/qoder 等适配器会以 200 + {"error":…} 冒出来
        if let Some(msg) = v["error"]["message"].as_str().or_else(|| v["error"].as_str()) {
            self.errored = true;
            match self.ing {
                Ingress::Responses => {
                    let mut resp = self.response_skeleton("failed");
                    resp["error"] = json!({ "code": "upstream_error", "message": msg });
                    self.emit_event("response.failed", &json!({ "type": "response.failed", "response": resp }));
                }
                Ingress::Anthropic => {
                    self.emit_event("error", &json!({ "type": "error", "error": { "type": "api_error", "message": msg } }));
                }
                Ingress::Gemini => {
                    self.emit_data(&json!({ "error": { "code": 502, "message": msg, "status": "INTERNAL" } }));
                }
            }
            return;
        }
        if let Some(u) = v.get("usage") {
            if u.is_object() && !u.as_object().unwrap().is_empty() {
                self.usage = u.clone();
            }
        }
        if self.ing == Ingress::Responses && !self.created_sent {
            // codex 客户端从 response.created 开始解析一条响应，必须先于一切 item 事件
            self.created_sent = true;
            let resp = self.response_skeleton("in_progress");
            self.emit_event("response.created", &json!({ "type": "response.created", "response": resp }));
        }
        let Some(choice) = v["choices"].get(0) else { return };
        let delta = &choice["delta"];
        if let Some(fr) = choice["finish_reason"].as_str() {
            if !fr.is_empty() {
                self.finish = Some(fr.to_string());
            }
        }
        if let Some(t) = delta["reasoning_content"].as_str() {
            if !t.is_empty() {
                self.reason_delta(t);
            }
        }
        if let Some(t) = delta["content"].as_str() {
            if !t.is_empty() {
                self.text_delta(t);
            }
        }
        for tc in delta["tool_calls"].as_array().cloned().unwrap_or_default() {
            self.tool_delta(&tc);
        }
        if self.finish.is_some() {
            self.flush();
        }
    }

    fn reason_delta(&mut self, t: &str) {
        match self.ing {
            Ingress::Responses => {
                if self.reason.is_none() {
                    let pos = self.items.len();
                    let id = format!("rs_{pos}");
                    self.items.push(json!({ "id": id, "type": "reasoning", "summary": [] }));
                    self.emit_event("response.output_item.added", &json!({
                        "type": "response.output_item.added", "output_index": pos,
                        "item": { "id": id, "type": "reasoning", "summary": [] },
                    }));
                    self.emit_event("response.reasoning_summary_part.added", &json!({
                        "type": "response.reasoning_summary_part.added", "item_id": id,
                        "output_index": pos, "summary_index": 0,
                        "part": { "type": "summary_text", "text": "" },
                    }));
                    self.reason = Some(TextAcc { pos, id, acc: String::new() });
                }
                let (pos, id) = {
                    let r = self.reason.as_mut().unwrap();
                    r.acc.push_str(t);
                    (r.pos, r.id.clone())
                };
                self.emit_event("response.reasoning_summary_text.delta", &json!({
                    "type": "response.reasoning_summary_text.delta",
                    "item_id": id, "output_index": pos, "summary_index": 0, "delta": t,
                }));
            }
            Ingress::Anthropic => self.a_switch_block("thinking", t),
            Ingress::Gemini => {
                // Gemini 没有思考摘要位，混进正文会污染协议语义 —— 丢弃
            }
        }
    }

    fn text_delta(&mut self, t: &str) {
        match self.ing {
            Ingress::Responses => {
                if self.text.is_none() {
                    let pos = self.items.len();
                    let id = format!("msg_{pos}");
                    let item = json!({
                        "id": id, "type": "message", "status": "in_progress",
                        "role": "assistant", "content": [],
                    });
                    self.items.push(item.clone());
                    self.emit_event("response.output_item.added", &json!({
                        "type": "response.output_item.added", "output_index": pos, "item": item,
                    }));
                    self.emit_event("response.content_part.added", &json!({
                        "type": "response.content_part.added", "item_id": id,
                        "output_index": pos, "content_index": 0,
                        "part": { "type": "output_text", "text": "", "annotations": [] },
                    }));
                    self.text = Some(TextAcc { pos, id, acc: String::new() });
                }
                let (pos, id) = {
                    let x = self.text.as_mut().unwrap();
                    x.acc.push_str(t);
                    (x.pos, x.id.clone())
                };
                self.emit_event("response.output_text.delta", &json!({
                    "type": "response.output_text.delta", "item_id": id,
                    "output_index": pos, "content_index": 0, "delta": t,
                }));
            }
            Ingress::Anthropic => self.a_switch_block("text", t),
            Ingress::Gemini => {
                self.emit_data(&json!({
                    "candidates": [{
                        "content": { "role": "model", "parts": [{ "text": t }] },
                        "index": 0,
                    }],
                    "modelVersion": self.model,
                }));
            }
        }
    }

    fn tool_delta(&mut self, tc: &Value) {
        let chat_index = tc["index"].as_u64().unwrap_or(0);
        let which = match self.tools.iter().position(|t| t.chat_index == chat_index) {
            Some(i) => i,
            None => {
                self.tools.push(ToolAcc {
                    chat_index,
                    call_id: String::new(),
                    name: String::new(),
                    args: String::new(),
                    opened: false,
                    slot: 0,
                });
                self.tools.len() - 1
            }
        };
        if let Some(id) = tc["id"].as_str() {
            if !id.is_empty() {
                self.tools[which].call_id = id.to_string();
            }
        }
        if let Some(name) = tc["function"]["name"].as_str() {
            if !name.is_empty() {
                self.tools[which].name = name.to_string();
            }
        }
        let prev_len = self.tools[which].args.len();
        if let Some(a) = tc["function"]["arguments"].as_str() {
            self.tools[which].args.push_str(a);
        }
        let fresh = !self.tools[which].opened && !self.tools[which].name.is_empty();
        match self.ing {
            Ingress::Responses => {
                if fresh {
                    let pos = self.items.len();
                    let (call_id, name) = {
                        let t = &mut self.tools[which];
                        t.opened = true;
                        t.slot = pos;
                        (t.call_id.clone(), t.name.clone())
                    };
                    let item = json!({
                        "id": format!("fc_{pos}"), "type": "function_call", "status": "in_progress",
                        "call_id": call_id, "name": name, "arguments": "",
                    });
                    self.items.push(item.clone());
                    self.emit_event("response.output_item.added", &json!({
                        "type": "response.output_item.added", "output_index": pos, "item": item,
                    }));
                }
                if self.tools[which].opened {
                    let part = self.tools[which].args[prev_len..].to_string();
                    let slot = self.tools[which].slot;
                    if !part.is_empty() {
                        self.emit_event("response.function_call_arguments.delta", &json!({
                            "type": "response.function_call_arguments.delta",
                            "item_id": format!("fc_{slot}"), "output_index": slot, "delta": part,
                        }));
                    }
                }
            }
            Ingress::Anthropic => {
                if fresh {
                    self.a_close_block();
                    let block = self.a_block_idx;
                    self.a_block_idx += 1;
                    let (call_id, name) = {
                        let t = &mut self.tools[which];
                        t.opened = true;
                        t.slot = block as usize;
                        (t.call_id.clone(), t.name.clone())
                    };
                    self.emit_event("content_block_start", &json!({
                        "type": "content_block_start", "index": block,
                        "content_block": { "type": "tool_use", "id": call_id, "name": name, "input": {} },
                    }));
                }
                if self.tools[which].opened {
                    let part = self.tools[which].args[prev_len..].to_string();
                    let block = self.tools[which].slot;
                    if !part.is_empty() {
                        self.emit_event("content_block_delta", &json!({
                            "type": "content_block_delta", "index": block,
                            "delta": { "type": "input_json_delta", "partial_json": part },
                        }));
                    }
                }
            }
            Ingress::Gemini => {
                // functionCall 必须一整块发，攒到 flush
            }
        }
    }

    /// Anthropic：按块类型切换 content_block（text/thinking），必要时先收掉旧块。
    fn a_switch_block(&mut self, kind: &'static str, t: &str) {
        if !self.a_started {
            self.emit_event("message_start", &json!({
                "type": "message_start",
                "message": {
                    "id": self.id, "type": "message", "role": "assistant", "model": self.model,
                    "content": [], "stop_reason": Value::Null, "stop_sequence": Value::Null,
                    "usage": { "input_tokens": 0, "output_tokens": 0 },
                },
            }));
            self.emit_event("ping", &json!({ "type": "ping" }));
            self.a_started = true;
        }
        if let Some((k, idx, _)) = self.a_block.as_ref() {
            if *k == kind {
                let idx = *idx;
                let delta = if kind == "text" {
                    json!({ "type": "text_delta", "text": t })
                } else {
                    json!({ "type": "thinking_delta", "thinking": t })
                };
                self.emit_event("content_block_delta", &json!({
                    "type": "content_block_delta", "index": idx, "delta": delta,
                }));
                if let Some((_, _, acc)) = self.a_block.as_mut() {
                    acc.push_str(t);
                }
                return;
            }
            self.a_close_block();
        }
        let block = self.a_block_idx;
        self.a_block_idx += 1;
        let cb = if kind == "text" {
            json!({ "type": "text", "text": "" })
        } else {
            json!({ "type": "thinking", "thinking": "", "signature": "" })
        };
        self.emit_event("content_block_start", &json!({
            "type": "content_block_start", "index": block, "content_block": cb,
        }));
        self.a_block = Some((kind, block, t.to_string()));
        let delta = if kind == "text" {
            json!({ "type": "text_delta", "text": t })
        } else {
            json!({ "type": "thinking_delta", "thinking": t })
        };
        self.emit_event("content_block_delta", &json!({
            "type": "content_block_delta", "index": block, "delta": delta,
        }));
    }

    fn a_close_block(&mut self) {
        if let Some((_, idx, _)) = self.a_block.take() {
            self.emit_event("content_block_stop", &json!({ "type": "content_block_stop", "index": idx }));
        }
    }

    /// finish_reason 到了（或流断了）：把没合的 item 全部合上，发终止事件。
    fn flush(&mut self) {
        if self.flushed {
            return;
        }
        self.flushed = true;
        let (pt, ct, tt) = self.usage_nums();
        match self.ing {
            Ingress::Responses => {
                if let Some(r) = self.reason.take() {
                    self.emit_event("response.reasoning_summary_text.done", &json!({
                        "type": "response.reasoning_summary_text.done", "item_id": r.id,
                        "output_index": r.pos, "summary_index": 0, "text": r.acc,
                    }));
                    self.items[r.pos]["summary"] = json!([{ "type": "summary_text", "text": r.acc }]);
                    self.items[r.pos]["status"] = json!("completed");
                    self.emit_event("response.output_item.done", &json!({
                        "type": "response.output_item.done", "output_index": r.pos,
                        "item": self.items[r.pos].clone(),
                    }));
                }
                if let Some(x) = self.text.take() {
                    self.emit_event("response.output_text.done", &json!({
                        "type": "response.output_text.done", "item_id": x.id,
                        "output_index": x.pos, "content_index": 0, "text": x.acc,
                    }));
                    self.emit_event("response.content_part.done", &json!({
                        "type": "response.content_part.done", "item_id": x.id,
                        "output_index": x.pos, "content_index": 0,
                        "part": { "type": "output_text", "text": x.acc, "annotations": [] },
                    }));
                    self.items[x.pos]["content"] = json!([{ "type": "output_text", "text": x.acc, "annotations": [] }]);
                    self.items[x.pos]["status"] = json!("completed");
                    self.emit_event("response.output_item.done", &json!({
                        "type": "response.output_item.done", "output_index": x.pos,
                        "item": self.items[x.pos].clone(),
                    }));
                }
                let done: Vec<ToolAcc> = self.tools.drain(..).collect();
                for t in &done {
                    if !t.opened {
                        continue;
                    }
                    self.emit_event("response.function_call_arguments.done", &json!({
                        "type": "response.function_call_arguments.done",
                        "item_id": format!("fc_{}", t.slot), "output_index": t.slot,
                        "arguments": t.args,
                    }));
                    self.items[t.slot]["arguments"] = json!(t.args);
                    self.items[t.slot]["status"] = json!("completed");
                    self.emit_event("response.output_item.done", &json!({
                        "type": "response.output_item.done", "output_index": t.slot,
                        "item": self.items[t.slot].clone(),
                    }));
                }
                let mut resp = self.response_skeleton("completed");
                resp["usage"] = json!({
                    "input_tokens": pt, "output_tokens": ct, "total_tokens": tt,
                    "input_tokens_details": { "cached_tokens": 0 },
                    "output_tokens_details": { "reasoning_tokens": 0 },
                });
                self.emit_event("response.completed", &json!({ "type": "response.completed", "response": resp }));
            }
            Ingress::Anthropic => {
                self.a_close_block();
                let finish = self.finish.as_deref().unwrap_or("stop");
                self.emit_event("message_delta", &json!({
                    "type": "message_delta",
                    "delta": { "stop_reason": finish_to_anthropic(finish), "stop_sequence": Value::Null },
                    "usage": { "output_tokens": ct },
                }));
                self.emit_event("message_stop", &json!({ "type": "message_stop" }));
            }
            Ingress::Gemini => {
                let mut parts: Vec<Value> = Vec::new();
                let done: Vec<ToolAcc> = self.tools.drain(..).collect();
                for t in &done {
                    if t.name.is_empty() {
                        continue;
                    }
                    let args: Value = serde_json::from_str(&t.args).unwrap_or_else(|_| json!({}));
                    parts.push(json!({ "functionCall": { "name": t.name, "args": args } }));
                }
                let finish = self.finish.as_deref().unwrap_or("stop");
                let mut chunk = json!({
                    "candidates": [{
                        "content": { "role": "model", "parts": parts },
                        "finishReason": finish_to_gemini(finish),
                        "index": 0,
                    }],
                    "modelVersion": self.model,
                });
                if pt > 0 || ct > 0 || tt > 0 {
                    chunk["usageMetadata"] = json!({
                        "promptTokenCount": pt, "candidatesTokenCount": ct, "totalTokenCount": tt,
                    });
                }
                self.emit_data(&chunk);
            }
        }
    }

    fn handle_block(&mut self, text: &str) {
        let (_, data) = parse_block(text);
        if data.trim() == "[DONE]" {
            self.finish.get_or_insert_with(|| "stop".to_string());
            self.flush();
            return;
        }
        if data.trim().is_empty() {
            return;
        }
        let Ok(v) = serde_json::from_str::<Value>(&data) else {
            return;
        };
        self.handle_chunk(&v);
    }

    fn pump(&mut self) {
        while let Some((i, len)) = find_sep(&self.raw) {
            let block: Vec<u8> = self.raw.drain(..i + len).collect();
            let text = String::from_utf8_lossy(&block[..i]).to_string();
            self.handle_block(&text);
            if self.flushed || self.errored {
                break;
            }
        }
    }
}

impl<R: Read> Read for SseTranslator<R> {
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
            if self.errored {
                return Ok(0); // 错误事件已发，直接收流
            }
            if self.eof {
                if !self.flushed {
                    self.handle_chunk(&json!({"error": {"message": "上游在发送结束标志前关闭了连接"}}));
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
                    self.handle_block(&String::from_utf8_lossy(&tail));
                }
                continue;
            }
            self.raw.extend_from_slice(&tmp[..n]);
            self.pump();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn responses_and_claude_tool_screenshots_reach_trae_as_images() {
        let data = "data:image/png;base64,aGVsbG8=";
        let responses = json!({"model":"trae/eye","input":[
            {"type":"function_call_output","call_id":"one","output":[{"type":"input_image","image_url":data,"detail":"high"}]},
            {"type":"function_call_output","call_id":"two","output":"done"}]});
        let claude = json!({"model":"trae/eye","messages":[{"role":"user","content":[
            {"type":"tool_result","tool_use_id":"one","content":[{"type":"image","source":{"type":"base64","media_type":"image/png","data":"aGVsbG8="}}]},
            {"type":"tool_result","tool_use_id":"two","content":"done"}]}]});
        for (wire, body) in [(Ingress::Responses, responses), (Ingress::Anthropic, claude)] {
            let chat = to_chat_request(wire,"/v1/messages",body.to_string().as_bytes()).unwrap();
            let sent = crate::traeproxy::translate_request(&chat,"solo_agent","eye");
            assert_eq!(sent["messages"][0]["role"], "tool");
            assert_eq!(sent["messages"][1]["role"], "tool");
            assert_eq!(sent["messages"][2]["role"], "user");
            assert_eq!(sent["messages"][2]["content"][1]["image_url"]["url"],data);
        }
    }

    #[test]
    fn detect_routes_by_path() {
        assert_eq!(Ingress::detect("POST", "/v1/responses"), Some(Ingress::Responses));
        assert_eq!(Ingress::detect("POST", "/v1/messages"), Some(Ingress::Anthropic));
        assert_eq!(
            Ingress::detect("POST", "/v1beta/models/gemini-x:streamGenerateContent?alt=sse&key=k"),
            Some(Ingress::Gemini)
        );
        assert_eq!(Ingress::detect("GET", "/v1beta/models"), Some(Ingress::Gemini));
        assert_eq!(Ingress::detect("POST", "/v1/chat/completions"), None);
        assert_eq!(Ingress::detect("GET", "/v1/models"), None);
    }

    #[test]
    fn model_from_url_extracts_gemini_model() {
        assert_eq!(
            model_from_url("POST", "/v1beta/models/minimax/MiniMax-M3:generateContent"),
            Some("minimax/MiniMax-M3".into())
        );
        assert_eq!(model_from_url("POST", "/v1/chat/completions"), None);
    }

    #[test]
    fn responses_request_translates_to_chat() {
        let body = json!({
            "model": "minimax/MiniMax-M3",
            "instructions": "你是助手",
            "stream": true,
            "max_output_tokens": 512,
            "input": [
                { "type": "message", "role": "user", "content": [{ "type": "input_text", "text": "查天气" }] },
                { "type": "function_call", "call_id": "call_1", "name": "weather", "arguments": "{\"city\":\"北京\"}" },
                { "type": "function_call_output", "call_id": "call_1", "output": "晴 25 度" },
                { "type": "reasoning", "summary": [] }
            ],
            "tools": [{ "type": "function", "name": "weather", "description": "查天气", "parameters": { "type": "object" } }]
        });
        let chat = to_chat_request(Ingress::Responses, "/v1/responses", body.to_string().as_bytes()).unwrap();
        assert_eq!(chat["model"], "minimax/MiniMax-M3");
        assert_eq!(chat["stream"], true);
        assert_eq!(chat["max_tokens"], 512);
        let msgs = chat["messages"].as_array().unwrap();
        assert_eq!(msgs[0]["role"], "system");
        assert_eq!(msgs[1]["content"], "查天气");
        assert_eq!(msgs[2]["tool_calls"][0]["id"], "call_1");
        assert_eq!(msgs[3]["role"], "tool");
        assert_eq!(msgs[3]["content"], "晴 25 度");
        assert_eq!(msgs.len(), 4, "reasoning item 应被丢弃");
        assert_eq!(chat["tools"][0]["function"]["name"], "weather");
    }

    #[test]
    fn anthropic_request_translates_to_chat() {
        let body = json!({
            "model": "MiniMax-M3", "max_tokens": 100, "stream": false,
            "system": "sys",
            "messages": [
                { "role": "user", "content": "你好" },
                { "role": "assistant", "content": [
                    { "type": "text", "text": "我来查" },
                    { "type": "tool_use", "id": "toolu_1", "name": "weather", "input": { "city": "北京" } }
                ]},
                { "role": "user", "content": [
                    { "type": "tool_result", "tool_use_id": "toolu_1", "content": "晴" },
                    { "type": "text", "text": "谢谢" }
                ]}
            ],
            "tools": [{ "name": "weather", "description": "查天气", "input_schema": { "type": "object" } }],
            "tool_choice": { "type": "auto" }
        });
        let chat = to_chat_request(Ingress::Anthropic, "/v1/messages", body.to_string().as_bytes()).unwrap();
        assert_eq!(chat["messages"][0]["content"], "sys");
        assert_eq!(chat["messages"][1]["content"], "你好");
        let asst = &chat["messages"][2];
        assert_eq!(asst["role"], "assistant");
        assert_eq!(asst["tool_calls"][0]["function"]["name"], "weather");
        assert_eq!(asst["tool_calls"][0]["function"]["arguments"], r#"{"city":"北京"}"#);
        assert_eq!(chat["messages"][3]["role"], "tool");
        assert_eq!(chat["messages"][3]["tool_call_id"], "toolu_1");
        assert_eq!(chat["messages"][4]["content"], "谢谢");
        assert_eq!(chat["tools"][0]["function"]["name"], "weather");
        assert_eq!(chat["tool_choice"], "auto");
        assert_eq!(chat["max_tokens"], 100);
    }

    #[test]
    fn gemini_request_translates_to_chat() {
        let body = json!({
            "contents": [
                { "role": "user", "parts": [{ "text": "查天气" }] },
                { "role": "model", "parts": [{ "functionCall": { "name": "weather", "args": { "city": "北京" } } }] },
                { "role": "user", "parts": [
                    { "functionResponse": { "name": "weather", "response": { "result": "晴" } } }
                ]}
            ],
            "systemInstruction": { "parts": [{ "text": "sys" }] },
            "generationConfig": { "maxOutputTokens": 256 },
            "tools": [{ "functionDeclarations": [{ "name": "weather", "description": "查天气", "parameters": { "type": "object" } }] }]
        });
        let chat = to_chat_request(
            Ingress::Gemini,
            "/v1beta/models/minimax/MiniMax-M3:generateContent",
            body.to_string().as_bytes(),
        )
        .unwrap();
        assert_eq!(chat["model"], "minimax/MiniMax-M3", "回环前须保留厂商前缀");
        assert_eq!(chat["messages"][0]["content"], "sys");
        assert_eq!(chat["messages"][1]["content"], "查天气");
        let asst = &chat["messages"][2];
        let call_id = asst["tool_calls"][0]["id"].as_str().unwrap().to_string();
        assert_eq!(asst["tool_calls"][0]["function"]["name"], "weather");
        let tool = &chat["messages"][3];
        assert_eq!(tool["role"], "tool");
        assert_eq!(tool["tool_call_id"].as_str().unwrap(), call_id, "functionResponse 要对上合成的 call id");
        assert_eq!(chat["max_tokens"], 256);
        assert_eq!(chat["stream"], false);
    }

    #[test]
    fn rewrite_covers_three_protocols() {
        let chat = json!({
            "id": "chatcmpl-1", "object": "chat.completion", "created": 1, "model": "m",
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": "答案", "reasoning_content": "想了一下" },
                "finish_reason": "stop"
            }],
            "usage": { "prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15 }
        });
        let rs = rewrite_json(Ingress::Responses, &chat, "m").unwrap();
        assert_eq!(rs["object"], "response");
        assert_eq!(rs["status"], "completed");
        assert_eq!(rs["output"][0]["content"][0]["text"], "答案");
        assert_eq!(rs["usage"]["input_tokens"], 10);

        let an = rewrite_json(Ingress::Anthropic, &chat, "m").unwrap();
        assert_eq!(an["type"], "message");
        assert_eq!(an["stop_reason"], "end_turn");
        assert_eq!(an["content"][0]["type"], "thinking");
        assert_eq!(an["content"][1]["text"], "答案");

        let gm = rewrite_json(Ingress::Gemini, &chat, "m").unwrap();
        assert_eq!(gm["candidates"][0]["finishReason"], "STOP");
        assert_eq!(gm["candidates"][0]["content"]["parts"][0]["text"], "答案");
        assert_eq!(gm["usageMetadata"]["totalTokenCount"], 15);

        let tools_chat = json!({
            "id": "x", "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": Value::Null, "tool_calls": [{
                    "id": "call_9", "type": "function",
                    "function": { "name": "weather", "arguments": "{\"city\":\"北京\"}" }
                }]},
                "finish_reason": "tool_calls"
            }]
        });
        let an = rewrite_json(Ingress::Anthropic, &tools_chat, "m").unwrap();
        assert_eq!(an["stop_reason"], "tool_use");
        assert_eq!(an["content"][0]["type"], "tool_use");
        assert_eq!(an["content"][0]["input"]["city"], "北京");
        let gm = rewrite_json(Ingress::Gemini, &tools_chat, "m").unwrap();
        assert_eq!(gm["candidates"][0]["content"]["parts"][0]["functionCall"]["name"], "weather");
    }

    #[test]
    fn error_shapes_per_protocol() {
        let up = json!({ "error": { "message": "额度不足", "type": "upstream_error" } });
        let rs = rewrite_error(Ingress::Responses, 402, &up);
        assert_eq!(rs["error"]["message"], "额度不足");
        let an = rewrite_error(Ingress::Anthropic, 402, &up);
        assert_eq!(an["type"], "error");
        assert_eq!(an["error"]["message"], "额度不足");
        let gm = rewrite_error(Ingress::Gemini, 429, &up);
        assert_eq!(gm["error"]["status"], "RESOURCE_EXHAUSTED");
    }

    #[test]
    fn gemini_count_tokens_is_cheap_estimate() {
        let body = json!({ "contents": [{ "parts": [{ "text": "hello world, this is a test" }] }] });
        let out = gemini_count_tokens(body.to_string().as_bytes());
        assert!(out["totalTokens"].as_u64().unwrap() > 0);
    }

    #[test]
    fn gemini_models_json_wraps_with_prefix() {
        let models = vec![json!({ "id": "minimax/MiniMax-M3", "name": "MiniMax M3" })];
        let out = gemini_models_json(&models);
        assert_eq!(out["models"][0]["name"], "models/minimax/MiniMax-M3");
        assert!(out["models"][0]["supportedGenerationMethods"].as_array().unwrap().len() >= 2);
    }

    /// 喂一段标准 chat SSE，收全输出：内部把 Read 读到 EOF。
    fn drain_sse(ing: Ingress, model: &str, sse: &str) -> String {
        let mut tr = SseTranslator::new(Cursor::new(sse.as_bytes().to_vec()), ing, model);
        let mut out = Vec::new();
        tr.read_to_end(&mut out).unwrap();
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn responses_sse_has_codex_event_sequence() {
        let sse = concat!(
            "data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"你好\"},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"c1\",\"choices\":[{\"index\":0,\"delta\":{\"reasoning_content\":\"思考\"},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"c1\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"！\"},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"c1\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":2,\"total_tokens\":5}}\n\n",
            "data: [DONE]\n\n"
        );
        let out = drain_sse(Ingress::Responses, "m", sse);
        assert!(out.contains("event: response.created"), "{out}");
        assert!(out.contains("event: response.output_item.added"));
        assert!(out.contains("event: response.output_text.delta"));
        assert!(out.contains("\"delta\":\"你好\""));
        assert!(out.contains("event: response.reasoning_summary_text.delta"));
        assert!(out.contains("event: response.output_text.done"));
        assert!(out.contains("event: response.output_item.done"));
        let completed = out.match_indices("event: response.completed").count();
        assert_eq!(completed, 1, "completed 只发一次：{out}");
        assert!(out.contains("\"input_tokens\":3"));
        assert!(out.contains("\"status\":\"completed\""));
    }

    #[test]
    fn anthropic_sse_block_sequence() {
        let sse = concat!(
            "data: {\"id\":\"c1\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"hi\"},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"c1\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"f\",\"arguments\":\"{\\\"a\\\":\"}}]},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"c1\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"1}\"}}]},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"c1\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}],\"usage\":{\"prompt_tokens\":4,\"completion_tokens\":2,\"total_tokens\":6}}\n\n",
            "data: [DONE]\n\n"
        );
        let out = drain_sse(Ingress::Anthropic, "m", sse);
        assert!(out.contains("event: message_start"), "{out}");
        assert!(out.contains("\"text\":\"hi\""));
        assert!(out.contains("text_delta"));
        assert!(out.contains("\"type\":\"tool_use\""));
        assert!(out.contains("\"type\":\"input_json_delta\""));
        assert!(out.contains("\"partial_json\":\"1}\""));
        assert!(out.contains("\"stop_reason\":\"tool_use\""));
        assert!(out.contains("event: message_stop"));
        assert!(out.match_indices("event: message_stop").count() == 1);
    }

    #[test]
    fn gemini_sse_streams_text_and_finish() {
        let sse = concat!(
            "data: {\"id\":\"c1\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"a\"},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"c1\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"b\"},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"c1\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":2,\"total_tokens\":3}}\n\n",
            "data: [DONE]\n\n"
        );
        let out = drain_sse(Ingress::Gemini, "m", sse);
        assert!(out.contains("\"text\":\"a\""), "{out}");
        assert!(out.contains("\"finishReason\":\"STOP\""));
        assert!(out.contains("\"totalTokenCount\":3"));
    }

    #[test]
    fn responses_sse_tool_call_events() {
        let sse = concat!(
            "data: {\"id\":\"c1\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"weather\",\"arguments\":\"{\\\"city\\\":\"}}]},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"c1\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\"北京\\\"}\"}}]},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"c1\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
            "data: [DONE]\n\n"
        );
        let out = drain_sse(Ingress::Responses, "m", sse);
        assert!(out.contains("\"type\":\"function_call\""), "{out}");
        assert!(out.contains("event: response.function_call_arguments.delta"));
        assert!(out.contains("event: response.function_call_arguments.done"));
        assert!(out.contains("\"call_id\":\"call_1\""));
        assert!(out.contains("\"arguments\":\"{\\\"city\\\":\\\"北京\\\"}\""));
    }

    #[test]
    fn unexpected_eof_fails_the_response() {
        let partial = "data: {\"choices\":[{\"delta\":{\"content\":\"partial\"}}]}\n\n";
        let out = drain_sse(Ingress::Responses, "m", partial);
        assert!(out.contains("event: response.failed"));
        assert!(!out.contains("event: response.completed"));
    }

    #[test]
    fn sse_error_frame_becomes_protocol_error() {
        let sse = "data: {\"error\":{\"message\":\"上游炸了\",\"type\":\"upstream_error\"}}\n\n";
        let out = drain_sse(Ingress::Responses, "m", sse);
        assert!(out.contains("event: response.failed"), "{out}");
        assert!(out.contains("上游炸了"));
        let out = drain_sse(Ingress::Anthropic, "m", sse);
        assert!(out.contains("event: error"), "{out}");
        let out = drain_sse(Ingress::Gemini, "m", sse);
        assert!(out.contains("\"code\":502"), "{out}");
    }
}
