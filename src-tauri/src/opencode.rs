//! OpenCode 本地桥的只读探测：模型清单 + 一次极小补全。
//!
//! 桥把本机 OpenCode 的会话接口包成 OpenAI 兼容端点（见 opencode-bridge/README.md）。
//! 驾驶舱这边只做两件事：拉 `/v1/models` 给界面选模型，以及发一次 ping 验证整条桥。
//! 地址和 key 默认 127.0.0.1:8211 / local；用户改过桥端口时，在 config.json 的
//! `opencode.base_url` / `opencode.bridge_token` 覆盖。
use serde_json::{json, Value};
use std::time::{Duration, Instant};

fn cfg() -> Value {
    crate::registry::load_config()
        .get("opencode")
        .cloned()
        .unwrap_or_else(|| json!({}))
}

pub fn base_url() -> String {
    cfg()["base_url"]
        .as_str()
        .unwrap_or("http://127.0.0.1:8211")
        .trim_end_matches('/')
        .to_string()
}

fn token() -> String {
    cfg()["bridge_token"].as_str().unwrap_or("local").to_string()
}

pub fn models() -> Value {
    let t0 = Instant::now();
    let res = ureq::get(&format!("{}/v1/models", base_url()))
        .set("Authorization", &format!("Bearer {}", token()))
        .timeout(Duration::from_secs(5))
        .call();
    match res {
        Ok(resp) => {
            let v: Value = resp.into_json().unwrap_or_else(|_| json!({}));
            let arr = v["data"].as_array().cloned().unwrap_or_default();
            json!({
                "ok": true, "base_url": base_url(), "models": arr,
                "count": arr.len(), "ms": t0.elapsed().as_millis() as u64,
            })
        }
        Err(e) => json!({
            "ok": false, "base_url": base_url(), "error": e.to_string(),
            "models": [], "count": 0, "ms": t0.elapsed().as_millis() as u64,
        }),
    }
}

pub fn test(model: &str) -> Value {
    let t0 = Instant::now();
    let body = json!({
        "model": model,
        "messages": [{ "role": "user", "content": "ping" }],
        "stream": false,
        "max_tokens": 16,
    })
    .to_string();
    let res = ureq::post(&format!("{}/v1/chat/completions", base_url()))
        .set("Authorization", &format!("Bearer {}", token()))
        .set("Content-Type", "application/json")
        .timeout(Duration::from_secs(60))
        .send_string(&body);
    match res {
        Ok(resp) => {
            let status = resp.status();
            let v: Value = resp.into_json().unwrap_or_else(|_| json!({}));
            let content = v["choices"][0]["message"]["content"]
                .as_str()
                .unwrap_or("")
                .to_string();
            json!({
                "ok": status < 400, "status": status,
                "ms": t0.elapsed().as_millis() as u64,
                "model": v["model"].clone(), "content": content, "usage": v["usage"].clone(),
            })
        }
        Err(ureq::Error::Status(code, resp)) => {
            let text = resp.into_string().unwrap_or_default();
            json!({
                "ok": false, "status": code,
                "ms": t0.elapsed().as_millis() as u64,
                "error": text.chars().take(300).collect::<String>(),
            })
        }
        Err(e) => json!({
            "ok": false, "ms": t0.elapsed().as_millis() as u64, "error": e.to_string(),
        }),
    }
}
