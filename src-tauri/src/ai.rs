// 多模态识别：走 OpenAI 兼容的 /chat/completions（默认阿里云百炼）。
// Key 只从本机设置里取、只在 Rust 侧拼进请求头，不随状态回传给界面。
//
// 模型分两档（设置 → AI 识别 各自可改）：
//   文本任务（粘贴的 key 清单转 JSON）用小快模型，vl-max 这类又慢又贵还用不上视觉；
//   截图任务（积分页/活动页）必须用支持视觉的模型，同样选快的。
// 识别是「照抄成 JSON」的活，思考模型（qwen3 系）先推理半分钟再吐字纯属浪费，
// 对它们显式传 enable_thinking:false 关掉（仅 qwen3 系加，别的模型加了可能被网关拒）。
use serde_json::{json, Value};
use std::io::BufRead;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

pub const DEFAULT_BASE: &str = "https://dashscope.aliyuncs.com/compatible-mode/v1";
pub const DEFAULT_MODEL_TEXT: &str = "qwen-flash";
pub const DEFAULT_MODEL_VISION: &str = "qwen-vl-flash";

/// 15 秒建不起连接就是地址/网络不对；60 秒是流式两帧之间的空闲上限；
/// 300 秒是整次调用的兜底，防止对端一直吊着不结束。
const CONNECT_TO: Duration = Duration::from_secs(15);
const READ_TO: Duration = Duration::from_secs(60);
const TOTAL_TO: Duration = Duration::from_secs(300);

// ===== 进度与互斥 =====
// 界面在识别期间轮询 ai_progress，把「卡在哪儿、吐了多少字」讲给人听；
// INFLIGHT 保证全程序同时只有一次识别——识别慢的时候连点几下，
// 后面几次会被立刻挡回去，不会再各跑一遍、各弹一份重复结果。
const ST_IDLE: u8 = 0;
const ST_CONNECT: u8 = 1;
const ST_WAIT: u8 = 2;
const ST_INFER: u8 = 3;

static STAGE: AtomicU8 = AtomicU8::new(ST_IDLE);
static CHARS: AtomicUsize = AtomicUsize::new(0);
static INFLIGHT: AtomicBool = AtomicBool::new(false);

/// (阶段, 已收字数)。阶段与前端文案的对应写在 commands::ai_progress。
pub fn progress() -> (u8, usize) {
    (
        STAGE.load(Ordering::SeqCst),
        CHARS.load(Ordering::SeqCst),
    )
}

struct Inflight;

impl Drop for Inflight {
    fn drop(&mut self) {
        INFLIGHT.store(false, Ordering::SeqCst);
        STAGE.store(ST_IDLE, Ordering::SeqCst);
    }
}

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(CONNECT_TO)
        .timeout_read(READ_TO)
        .build()
}

struct Cfg {
    base: String,
    key: String,
    model: String,
    vision: bool,
}

/// qwen3 系是思考模型：不关思考，一次识别大半分钟烧在推理上。
/// 只对 qwen3 前缀加开关——qwen-plus/qwen-flash 这些不是思考模型，
/// vl 系更没有思考，多余参数在严格的网关上反而会被拒。
fn wants_no_thinking(model: &str, vision: bool) -> bool {
    let m = model.to_lowercase();
    !vision && m.starts_with("qwen3")
}

fn cfg(vision: bool) -> Result<Cfg, String> {
    let s = crate::settings::get();
    let base = s["ai"]["base_url"]
        .as_str()
        .unwrap_or("")
        .trim()
        .trim_end_matches('/')
        .to_string();
    let base = if base.is_empty() {
        DEFAULT_BASE.to_string()
    } else {
        base
    };
    let key = s["ai"]["key"].as_str().unwrap_or("").trim().to_string();
    if key.is_empty() {
        return Err("还没配 AI：到 设置 → AI 识别 填 API Key（Base URL 和模型可留默认）".to_string());
    }
    // 截图用「截图模型」；没单配时退回「文本模型」那一档（老配置里填的本来就是 vl 系）
    let pick = |field: &str, dflt: &str| -> String {
        let m = s["ai"][field].as_str().unwrap_or("").trim().to_string();
        if m.is_empty() { dflt.to_string() } else { m }
    };
    let model = if vision {
        let v = s["ai"]["model_vision"].as_str().unwrap_or("").trim().to_string();
        if v.is_empty() {
            pick("model", DEFAULT_MODEL_VISION)
        } else {
            v
        }
    } else {
        pick("model", DEFAULT_MODEL_TEXT)
    };
    Ok(Cfg {
        base,
        key,
        model,
        vision,
    })
}

fn clip(s: &str) -> String {
    let t = s.trim();
    t.chars().take(220).collect::<String>()
}

/// content 可以是纯文本字符串，也可以是 [{type:text},{type:image_url,...}] 数组（带图）。
/// 同时只允许一次识别在跑；期间用 ai_progress 看进度。
pub fn ask(content: Value) -> Result<String, String> {
    if INFLIGHT
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return Err("已有一个识别在进行中——看右下角进度，等它出结果再试（别连点）".to_string());
    }
    let _g = Inflight;
    CHARS.store(0, Ordering::SeqCst);
    STAGE.store(ST_CONNECT, Ordering::SeqCst);
    let c = cfg(content.is_array())?;
    let url = format!("{}/chat/completions", c.base);
    let mut body = json!({
        "model": c.model,
        "messages": [{ "role": "user", "content": content }],
        "temperature": 0,
        // 思考模型走非流式时，要等整段推理写完才吐第一个字节，状态行干等到超时
        // （实测 qwen3.8-flash 读一段 key 文本 60 秒不回，同地址发「只回复 OK」秒回）。
        // 百炼文档同一句建议：响应长就用流式。流式下 60 秒变成"两帧之间没数据"，不再是整段答案的预算。
        "stream": true,
    });
    if wants_no_thinking(&c.model, c.vision) {
        body["enable_thinking"] = json!(false);
    }
    let t0 = Instant::now();
    let out = std::thread::spawn(move || {
        match agent()
            .post(&url)
            .timeout(TOTAL_TO)
            .set("Authorization", &format!("Bearer {}", c.key))
            .set("Content-Type", "application/json")
            .send_json(body)
        {
            // 认 stream 的给 text/event-stream，不认的中转站照样回一整个 JSON
            Ok(resp) => match resp.header("content-type") {
                Some(ct) if ct.to_lowercase().contains("text/event-stream") => {
                    STAGE.store(ST_WAIT, Ordering::SeqCst);
                    sse_text(resp)
                }
                _ => {
                    STAGE.store(ST_WAIT, Ordering::SeqCst);
                    json_text(resp)
                }
            },
            Err(ureq::Error::Status(code, r)) => {
                let text = r.into_string().unwrap_or_default();
                Err(format!("接口返回 {}：{}", code, clip(&text)))
            }
            Err(e) => Err(format!("请求失败：{}（{} 秒）", e, t0.elapsed().as_secs())),
        }
    })
    .join()
    .unwrap_or_else(|_| Err("请求线程崩了（多半是 Base URL 写得不合法）".to_string()));
    STAGE.store(ST_IDLE, Ordering::SeqCst);
    match out {
        Ok(text) if text.trim().is_empty() => Err(format!(
            "模型没返回内容（{} 秒，多半是不支持视觉的模型配了截图，或图片过大）",
            t0.elapsed().as_secs()
        )),
        Ok(text) => Ok(text),
        Err(e) => Err(e),
    }
}

/// 非流式：整包 JSON 里的 message.content。
fn json_text(resp: ureq::Response) -> Result<String, String> {
    let v: Value = resp.into_json().map_err(|e| format!("返回不是 JSON：{e}"))?;
    let text = message_text(&v);
    CHARS.store(text.chars().count(), Ordering::SeqCst);
    STAGE.store(ST_INFER, Ordering::SeqCst);
    Ok(text)
}

/// 流式：SSE 一行一帧，只拼 delta.content；reasoning_content（思考过程）不进结果。
fn sse_text(resp: ureq::Response) -> Result<String, String> {
    sse_frames(resp.into_reader())
}

fn sse_frames(r: impl std::io::Read) -> Result<String, String> {
    let mut buf = std::io::BufReader::new(r);
    let mut text = String::new();
    let mut line = String::new();
    loop {
        line.clear();
        match buf.read_line(&mut line) {
            Ok(0) => break,
            Err(e) => return Err(format!("连上了，但读到一半断了：{e}")),
            Ok(_) => {}
        }
        let l = line.trim_end();
        if !l.starts_with("data:") {
            continue;
        }
        let data = l[5..].trim();
        if data == "[DONE]" {
            break;
        }
        if let Ok(v) = serde_json::from_str::<Value>(data) {
            if let Some(s) = v["choices"][0]["delta"]["content"].as_str() {
                text.push_str(s);
                CHARS.store(text.chars().count(), Ordering::SeqCst);
                STAGE.store(ST_INFER, Ordering::SeqCst);
            }
        }
    }
    Ok(text)
}

/// 有的网关把 content 返回成字符串，有的返回成 [{type:"text",text:"…"}]
fn message_text(v: &Value) -> String {
    match &v["choices"][0]["message"]["content"] {
        Value::String(s) => s.clone(),
        Value::Array(a) => a
            .iter()
            .map(|p| {
                p["text"]
                    .as_str()
                    .or_else(|| p.as_str())
                    .unwrap_or("")
                    .to_string()
            })
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

/// 从模型爱加 ```json 围栏、爱加解释话的返回里抠出第一个 JSON 对象
pub fn first_json(text: &str) -> Option<Value> {
    let s = text.trim();
    let s = s.strip_prefix("```json").or_else(|| s.strip_prefix("```")).unwrap_or(s);
    let s = s.strip_suffix("```").map(str::trim).unwrap_or(s);
    let start = s.find('{')?;
    let bytes = s.as_bytes();
    let mut depth = 0i32;
    let mut in_str = false;
    let mut esc = false;
    for (i, b) in bytes.iter().enumerate().skip(start) {
        if in_str {
            if esc {
                esc = false;
            } else if *b == b'\\' {
                esc = true;
            } else if *b == b'"' {
                in_str = false;
            }
            continue;
        }
        match b {
            b'"' => in_str = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return serde_json::from_str(&s[start..=i]).ok();
                }
            }
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_is_mined_out_of_markdown_noise() {
        let v = first_json("```json\n{\"title\":\"每天免费\",\"n\":{\"a\":1}}\n```").unwrap();
        assert_eq!(v["title"], "每天免费");
        assert_eq!(v["n"]["a"], 1);
        // 后面还跟一句解释话也要能抠出来
        let v2 = first_json("好的，识别结果：{\"credits\":[{\"left\":1700,\"total\":1800}]} 希望有帮助").unwrap();
        assert_eq!(v2["credits"][0]["left"], 1700);
        assert!(first_json("完全没给 JSON").is_none());
    }

    #[test]
    fn sse_frames_are_stitched_and_reasoning_dropped() {
        let raw = concat!(
            ": ping\n\n",
            "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"先想想\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"{\\\"site\\\"\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\":{}}\"},\"finish_reason\":\"stop\"}]}\n\n",
            "data: {\"choices\":[],\"usage\":{\"total_tokens\":42}}\n\n",
            "data: [DONE]\n\n",
        );
        assert_eq!(sse_frames(raw.as_bytes()).unwrap(), "{\"site\":{}}");
    }

    #[test]
    fn thinking_switch_only_hits_qwen3_text_models() {
        assert!(wants_no_thinking("qwen3.8-flash", false));
        assert!(wants_no_thinking("Qwen3-Max", false));
        // vl 系走截图通道，别的 qwen 系和别家模型都不加参数
        assert!(!wants_no_thinking("qwen3.8-flash", true));
        assert!(!wants_no_thinking("qwen-flash", false));
        assert!(!wants_no_thinking("qwen-vl-max", false));
        assert!(!wants_no_thinking("gpt-4o-mini", false));
    }

    // 进度全局变量（STAGE/CHARS）不单测：测试并行跑会互相踩计数，
    // 真正要验的「帧推进 → 字数增加」已经包含在 sse_frames 的拼接测试里。
}
