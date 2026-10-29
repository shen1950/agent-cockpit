//! MiniMax Code（MiniMax 的 mcode 客户端）模型反代。
//!
//! 客户端自己的 provider 配置（`~/.minimax/config.yaml`）写明：`npm: '@ai-sdk/anthropic'`、
//! `baseURL: https://agent.minimax.cn/mavis/api/v1/llm/v1` —— 上游是 **Anthropic 协议**
//! （`POST /messages`，SSE 事件 `message_start` / `content_block_delta` / `message_delta` …），
//! 鉴权只要一行 `Authorization: Bearer <accessToken>`，令牌明文躺在
//! `~/.minimax/auth/<env>/<region>/mcode-public/auth.json`（OAuth 设备码登录，`mmoat_` 前缀）。
//!
//! 2026-10-26 本机实测（不吃积分的小请求）：`MiniMax-M3.1-Flash` / `MiniMax-M3` /
//! `MiniMax-M2.7` / `MiniMax-M2.7-highspeed` 全部 200；`stream: true` 200；
//! 工具调用正常返回 `tool_use` 块；只带 Bearer 也 200（`anthropic-version` 可省，带上更稳）。
//! 上游**没有** `/models`（503 `direct_route_not_configured`）、**没有** `/chat/completions`
//! （同样 503），所以模型清单按客户端 whitelist 静态给出，OpenAI 形状由本模块翻译。
//!
//! 方向说明：MiniMax Code 往自己的 `config.yaml` 里塞第三方 provider 是另一条路（本模块不做）；
//! 这里是把它的模型反代出来。
use serde_json::{json, Value};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub const APPS: [&str; 1] = ["minimax"];

/// 客户端标记：上游组成员的 base_url 用它表示「这是 MiniMax 上游」。
/// proxy.rs 靠它分流到 Anthropic 翻译层，models_for 靠它决定列哪些模型。
pub const BASE_MARK: &str = "minimax://mcode";
/// 云端网关（**不带 `/messages`**：由本模块拼）。
pub const GATEWAY_BASE: &str = "https://agent.minimax.cn/mavis/api/v1/llm/v1";
/// OAuth 刷新端点（form 编码，见 HANDOFF 2.20）。
pub const TOKEN_URL: &str = "https://account.minimax.cn/oauth2/token";
/// Anthropic 协议必须带 max_tokens；客户端没给就用这个。
pub const DEFAULT_MAX_TOKENS: u64 = 8192;
/// 官方模型上限（config.yaml 的 limit.output）。
pub const MAX_OUTPUT_TOKENS: u64 = 128_000;

/// 这条上游是不是 MiniMax：认内部标记，也认云端网关域名（用户可能手填）。
pub fn is_mm_base(base: &str) -> bool {
    let b = base.trim().trim_end_matches('/');
    b == BASE_MARK || b.starts_with(GATEWAY_BASE) || b.contains("agent.minimax.cn/mavis")
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

// ───────────────────────── 凭据：读 / 刷新 / 写回 ─────────────────────────
//
// 刷新令牌是「轮换 + 重放检测」的：换过一次，旧的立刻作废；再拿旧的去换，
// 服务端会把整族令牌撤销（invalid_grant: this refresh token can no longer be used）。
// 所以这里的纪律是硬性的：**同一时刻只允许一次刷新、刷新成功立刻写回、
// 失败绝不重试**（失败后 30 秒内连试都不试，直接如实报错）。

pub struct Cred {
    pub path: PathBuf,
    pub record_key: String,
    pub access: String,
    pub refresh: String,
    pub expires_ms: i64,
    pub generation: u64,
    pub client_id: String,
    pub audience: String,
    pub scopes: Vec<String>,
}

/// MiniMax 的 mcode 凭据文件：`<家目录>/auth/<env>/<region>/mcode-public/auth.json`
/// （env/region 两段可变，逐层遍历；有多个就取最新写的那个）。
pub fn auth_files(root: &Path) -> Vec<PathBuf> {
    let mut out: Vec<(i64, PathBuf)> = Vec::new();
    let Ok(envs) = std::fs::read_dir(root.join("auth")) else {
        return Vec::new();
    };
    for e in envs.flatten() {
        let Ok(regions) = std::fs::read_dir(e.path()) else {
            continue;
        };
        for r in regions.flatten() {
            let f = r.path().join("mcode-public").join("auth.json");
            let Ok(md) = std::fs::metadata(&f) else {
                continue;
            };
            let ts = md
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);
            out.push((ts, f));
        }
    }
    out.sort_by(|a, b| b.0.cmp(&a.0));
    out.into_iter().map(|(_, p)| p).collect()
}

fn read_cred_at(path: &Path) -> Option<Cred> {
    let text = std::fs::read_to_string(path).ok()?;
    let v: Value = serde_json::from_str(&text).ok()?;
    let records = v["records"].as_object()?;
    let (key, rec) = records.iter().next()?;
    let access = rec["accessToken"].as_str().unwrap_or("").trim().to_string();
    if access.is_empty() {
        return None;
    }
    let scopes = rec["scopes"]
        .as_array()
        .map(|a| a.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect())
        .unwrap_or_default();
    Some(Cred {
        path: path.to_path_buf(),
        record_key: key.clone(),
        access,
        refresh: rec["refreshToken"].as_str().unwrap_or("").trim().to_string(),
        expires_ms: rec["expiresAtMs"].as_i64().unwrap_or(0),
        generation: rec["generation"].as_u64().unwrap_or(0),
        client_id: rec["clientId"].as_str().unwrap_or("mcode-public").to_string(),
        audience: rec["audience"].as_str().unwrap_or("agent-backend").to_string(),
        scopes,
    })
}

/// 当前登录档案里的凭据（读不到 = 这台机器上 MiniMax Code 没登录过）
pub fn read_cred() -> Option<Cred> {
    let root = crate::registry::profile_dir("minimax")?;
    for f in auth_files(&root) {
        if let Some(c) = read_cred_at(&f) {
            return Some(c);
        }
    }
    None
}

static TOKEN_LOCK: Mutex<()> = Mutex::new(());
static TOKEN_CACHE: Mutex<Option<(String, i64)>> = Mutex::new(None);
static REFRESH_FAIL_AT: Mutex<i64> = Mutex::new(0);
/// 提前 60 秒续期。
const REFRESH_MARGIN_MS: i64 = 60_000;

/// AgentHub 的额度和签到也会续期同一个一次性 refresh token。
/// 统一交给它在跨进程锁内续期，绝不由驾驶舱再独立兑换。
fn refresh_via_agenthub() -> bool {
    let Some(home) = std::env::var_os("USERPROFILE") else { return false; };
    let root = PathBuf::from(home).join(".agenthub");
    let python = root.join("venv").join("Scripts").join("python.exe");
    let script = root.join("agenthub.py");
    let mut cmd = std::process::Command::new(python);
    cmd.arg("-X").arg("utf8").arg("-c")
        .arg("import runpy,sys; runpy.run_path(sys.argv[1])['_minimax_access']()")
        .arg(script);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    cmd.output().map(|o| o.status.success()).unwrap_or(false)
}

/// 取当前可用的 access token。空串 = 读不到 / 过期且刷新失败，如实返回空，
/// 由调用方把话说清楚（不拿过期令牌冒充可用）。
pub fn token() -> String {
    if let Ok(g) = TOKEN_CACHE.lock() {
        if let Some((t, exp)) = g.as_ref() {
            if *exp - now_ms() > REFRESH_MARGIN_MS && !t.is_empty() {
                return t.clone();
            }
        }
    }
    let _g = TOKEN_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    // 拿到锁后重读文件：可能另一个进程（官方客户端 / 另一个驾驶舱窗口）刚换过
    let Some(c) = read_cred() else {
        return String::new();
    };
    if c.expires_ms - now_ms() > REFRESH_MARGIN_MS {
        if let Ok(mut g) = TOKEN_CACHE.lock() {
            *g = Some((c.access.clone(), c.expires_ms));
        }
        return c.access;
    }
    // AgentHub 内部先落下尝试标记，再发一次请求；网络结果不明时不会重放。
    if now_ms() - *REFRESH_FAIL_AT.lock().unwrap_or_else(|p| p.into_inner()) < 30_000 {
        return String::new();
    }
    if refresh_via_agenthub() {
        if let Some(updated) = read_cred() {
            if updated.expires_ms - now_ms() > REFRESH_MARGIN_MS {
                if let Ok(mut g) = TOKEN_CACHE.lock() {
                    *g = Some((updated.access.clone(), updated.expires_ms));
                }
                return updated.access;
            }
        }
    }
    if let Ok(mut f) = REFRESH_FAIL_AT.lock() { *f = now_ms(); }
    String::new()
}

/// 换号 / 重登后清缓存
pub fn invalidate() {
    if let Ok(mut g) = TOKEN_CACHE.lock() {
        *g = None;
    }
    if let Ok(mut f) = REFRESH_FAIL_AT.lock() {
        *f = 0;
    }
}

/// 只读地拿令牌：**绝不触发刷新**。给界面 4 秒一轮的状态轮询用 ——
/// 刷新是轮换制的一次性动作，不能让「看一眼」把它打出去。
pub fn token_quiet() -> String {
    if let Ok(g) = TOKEN_CACHE.lock() {
        if let Some((t, _)) = g.as_ref() {
            if !t.is_empty() {
                return t.clone();
            }
        }
    }
    read_cred().map(|c| c.access).unwrap_or_default()
}

/// 令牌状态（界面用）：不打上游、不花积分
pub fn probe() -> Value {
    let Some(c) = read_cred() else {
        let dir = crate::registry::profile_dir("minimax")
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();
        return json!({
            "ok": false, "state": "no_login", "masked": "", "from": "",
            "expires_at": 0, "remain_min": 0,
            "text": "本机没有 MiniMax Code 的登录档案",
            "detail": if dir.is_empty() { "没找到 ~/.minimax".to_string() }
                      else { format!("{dir}/auth 下没有 mcode-public/auth.json，先打开 MiniMax Code 登录一次") },
        });
    };
    let remain_min = (c.expires_ms - now_ms()) / 60_000;
    let expired = c.expires_ms > 0 && c.expires_ms <= now_ms();
    json!({
        "ok": !expired,
        "state": if expired { "expired" } else { "ok" },
        "masked": mask(&c.access),
        "from": c.path.to_string_lossy().to_string(),
        "expires_at": c.expires_ms,
        "remain_min": remain_min,
        "text": if expired {
            "access token 已过期（点「刷新令牌」或重开 MiniMax Code）".to_string()
        } else {
            format!("令牌可读，还有 {} 分钟到期", remain_min.max(0))
        },
        "detail": "",
    })
}

pub fn mask(tok: &str) -> String {
    let t = tok.trim();
    if t.len() <= 12 {
        return "***".to_string();
    }
    format!("{}…{}", &t[..8], &t[t.len() - 4..])
}

/// 手动刷新（界面按钮）：过期了才换，没过期就把当前令牌报回去
pub fn refresh_now() -> Value {
    let Some(c) = read_cred() else {
        return json!({ "ok": false, "msg": "读不到登录档案，先打开 MiniMax Code 登录一次" });
    };
    if c.expires_ms - now_ms() > REFRESH_MARGIN_MS {
        return json!({ "ok": true, "msg": "令牌还没过期，不用换", "remain_min": (c.expires_ms - now_ms()) / 60_000 });
    }
    invalidate();
    let before = c.generation;
    let tok = token();
    if tok.is_empty() {
        return json!({
            "ok": false,
            "msg": "刷新失败。MiniMax 的 refresh token 是轮换制，一旦被拒只能重新登录：\
                    打开 MiniMax Code 走一次登录，或在终端跑 minimax 的登录命令。",
            "generation_before": before,
        });
    }
    let after = read_cred().map(|c| c.generation).unwrap_or(before);
    json!({ "ok": true, "msg": "令牌已刷新并写回 auth.json", "generation": after })
}

// ───────────────────────── 模型清单 ─────────────────────────

/// 静态清单 = 客户端 config.yaml 的 whitelist（上游没有 /models，只能这样给）。
/// 四条 2026-10-26 本机实测都 200。
pub fn models() -> Vec<Value> {
    vec![
        json!({ "id": "MiniMax-M3.1-Flash", "name": "M3.1-Flash（订阅限免）", "free": true,
                "context_window": 512000, "max_output_tokens": 128000,
                "reasoning": true, "tools": true, "vision": true }),
        json!({ "id": "MiniMax-M3", "name": "M3",
                "context_window": 512000, "max_output_tokens": 128000,
                "reasoning": true, "tools": true, "vision": true }),
        json!({ "id": "MiniMax-M2.7", "name": "M2.7",
                "context_window": 204800, "max_output_tokens": 65536,
                "reasoning": true, "tools": true, "vision": false }),
        json!({ "id": "MiniMax-M2.7-highspeed", "name": "M2.7-Highspeed",
                "context_window": 204800, "max_output_tokens": 65536,
                "reasoning": true, "tools": true, "vision": false }),
    ]
}

/// 客户端发来的模型名 → 上游认的名字。
/// `minimax/MiniMax-M3` 的前缀由 proxy 剥掉；这里只管别名与大小写。
pub fn normalize_model(m: &str) -> String {
    // 客户端可能带着 Agent 前缀发过来（`minimax/MiniMax-M3`）。proxy 一般已经剥了，
    // 但直连上游组或手填地址时会漏到这里 —— 再剥一次，别把 `minimax/` 当模型名发给上游。
    let t = m.trim();
    let t = t
        .strip_prefix("minimax/")
        .or_else(|| t.strip_prefix("MiniMax/"))
        .unwrap_or(t);
    if t.is_empty() {
        return "MiniMax-M3.1-Flash".to_string();
    }
    let lower = t.to_lowercase();
    for id in ["MiniMax-M3.1-Flash", "MiniMax-M3", "MiniMax-M2.7-highspeed", "MiniMax-M2.7"] {
        if lower == id.to_lowercase() {
            return id.to_string();
        }
    }
    // `m3.1-flash` / `m3` / `m2.7` 这类简写
    let short = lower.trim_start_matches("minimax-");
    match short {
        "m3.1-flash" | "m3-1-flash" => "MiniMax-M3.1-Flash".into(),
        "m3" => "MiniMax-M3".into(),
        "m2.7-highspeed" => "MiniMax-M2.7-highspeed".into(),
        "m2.7" => "MiniMax-M2.7".into(),
        _ => t.to_string(),
    }
}

// ───────────────────────── OpenAI → Anthropic ─────────────────────────

fn text_blocks(content: &Value, role: &str) -> Vec<Value> {
    match content {
        Value::String(s) => {
            if s.is_empty() {
                Vec::new()
            } else {
                vec![json!({ "type": "text", "text": s })]
            }
        }
        Value::Array(parts) => {
            let mut out = Vec::new();
            for p in parts {
                let ty = p["type"].as_str().unwrap_or("");
                match ty {
                    "text" | "input_text" => {
                        if let Some(t) = p["text"].as_str() {
                            if !t.is_empty() {
                                out.push(json!({ "type": "text", "text": t }));
                            }
                        }
                    }
                    "image_url" | "input_image" => {
                        let url = p["image_url"]["url"]
                            .as_str()
                            .or_else(|| p["image_url"].as_str())
                            .or_else(|| p["url"].as_str())
                            .unwrap_or("");
                        if let Some(b) = image_block(url) {
                            out.push(b);
                        }
                    }
                    // 音频/文件等 Anthropic 侧形状不同，直接丢掉而不是让上游 400
                    _ => {
                        if let Some(t) = p["text"].as_str() {
                            if !t.is_empty() {
                                out.push(json!({ "type": "text", "text": t }));
                            }
                        }
                    }
                }
            }
            let _ = role;
            out
        }
        _ => Vec::new(),
    }
}

/// data URL → base64 source；普通 URL → url source（Anthropic 新版支持）
fn image_block(url: &str) -> Option<Value> {
    let u = url.trim();
    if u.is_empty() {
        return None;
    }
    if let Some(rest) = u.strip_prefix("data:") {
        let (meta, data) = rest.split_once(',')?;
        let media = meta.split(';').next().unwrap_or("image/png");
        return Some(json!({
            "type": "image",
            "source": { "type": "base64", "media_type": media, "data": data }
        }));
    }
    Some(json!({ "type": "image", "source": { "type": "url", "url": u } }))
}

fn push_msg(msgs: &mut Vec<Value>, role: &str, blocks: Vec<Value>) {
    if blocks.is_empty() {
        return;
    }
    // Anthropic 要求 user/assistant 交替；连着同角色就并进上一条
    if let Some(last) = msgs.last_mut() {
        if last["role"].as_str() == Some(role) {
            if let Some(arr) = last["content"].as_array_mut() {
                arr.extend(blocks);
                return;
            }
        }
    }
    msgs.push(json!({ "role": role, "content": blocks }));
}

fn tool_result_blocks(m: &Value) -> Vec<Value> {
    let id = m["tool_call_id"].as_str().unwrap_or("").to_string();
    let blocks = text_blocks(&m["content"], "tool");
    let content = if blocks.is_empty() {
        json!("(no output)")
    } else {
        Value::Array(blocks)
    };
    vec![json!({ "type": "tool_result", "tool_use_id": id, "content": content })]
}

/// OpenAI `tools` → Anthropic `tools`。两种形状都收（已经是的就原样过）。
pub fn tools_of(v: &Value) -> Vec<Value> {
    let mut out = Vec::new();
    for t in v["tools"].as_array().cloned().unwrap_or_default() {
        if t["type"].as_str() == Some("function") {
            let f = &t["function"];
            let name = f["name"].as_str().unwrap_or("").trim();
            if name.is_empty() {
                continue;
            }
            let mut item = json!({
                "name": name,
                "input_schema": if f["parameters"].is_object() {
                    f["parameters"].clone()
                } else {
                    json!({ "type": "object", "properties": {} })
                },
            });
            if let Some(d) = f["description"].as_str() {
                if !d.trim().is_empty() {
                    item["description"] = json!(d);
                }
            }
            out.push(item);
        } else if t["name"].as_str().is_some() && t["input_schema"].is_object() {
            out.push(t.clone());
        }
    }
    out
}

fn tool_choice_of(v: &Value, has_tools: bool) -> Option<Value> {
    if !has_tools {
        return None;
    }
    match &v["tool_choice"] {
        Value::String(s) => match s.as_str() {
            "auto" => Some(json!({ "type": "auto" })),
            "required" | "any" => Some(json!({ "type": "any" })),
            "none" => None,
            _ => None,
        },
        Value::Object(o) => {
            let name = o["function"]["name"].as_str().or_else(|| o["name"].as_str());
            name.map(|n| json!({ "type": "tool", "name": n }))
        }
        _ => None,
    }
}

/// 一次 OpenAI `/v1/chat/completions` 请求体 → Anthropic `/messages` 请求体。
pub fn translate_request(v: &Value) -> Value {
    let mut system: Vec<String> = Vec::new();
    let mut msgs: Vec<Value> = Vec::new();

    for m in v["messages"].as_array().cloned().unwrap_or_default() {
        let role = m["role"].as_str().unwrap_or("user");
        match role {
            "system" | "developer" => {
                let t = match &m["content"] {
                    Value::String(s) => s.clone(),
                    other => text_blocks(other, "system")
                        .iter()
                        .filter_map(|b| b["text"].as_str().map(|s| s.to_string()))
                        .collect::<Vec<_>>()
                        .join("\n"),
                };
                if !t.trim().is_empty() {
                    system.push(t);
                }
            }
            "tool" | "function" => {
                let blocks = tool_result_blocks(&m);
                push_msg(&mut msgs, "user", blocks);
            }
            "assistant" => {
                let mut blocks = text_blocks(&m["content"], "assistant");
                for tc in m["tool_calls"].as_array().cloned().unwrap_or_default() {
                    let name = tc["function"]["name"].as_str().unwrap_or("").to_string();
                    if name.is_empty() {
                        continue;
                    }
                    let args = tc["function"]["arguments"]
                        .as_str()
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| tc["function"]["arguments"].to_string());
                    let input = serde_json::from_str::<Value>(&args)
                        .unwrap_or_else(|_| json!({ "_raw": args }));
                    blocks.push(json!({
                        "type": "tool_use",
                        "id": tc["id"].as_str().unwrap_or("call_mm"),
                        "name": name,
                        "input": input,
                    }));
                }
                push_msg(&mut msgs, "assistant", blocks);
            }
            _ => {
                let blocks = text_blocks(&m["content"], "user");
                push_msg(&mut msgs, "user", blocks);
            }
        }
    }
    if msgs.is_empty() {
        msgs.push(json!({ "role": "user", "content": [{ "type": "text", "text": "(empty)" }] }));
    }

    let tools = tools_of(v);
    let max_tokens = v["max_tokens"]
        .as_u64()
        .or_else(|| v["max_completion_tokens"].as_u64())
        .unwrap_or(DEFAULT_MAX_TOKENS)
        .clamp(1, MAX_OUTPUT_TOKENS);

    let mut body = json!({
        "model": normalize_model(v["model"].as_str().unwrap_or("")),
        "max_tokens": max_tokens,
        "messages": msgs,
        "stream": v["stream"].as_bool().unwrap_or(false),
    });
    if !system.is_empty() {
        body["system"] = json!(system.join("\n\n"));
    }
    if !tools.is_empty() {
        body["tools"] = Value::Array(tools);
        if let Some(tc) = tool_choice_of(v, true) {
            body["tool_choice"] = tc;
        }
    }
    for k in ["temperature", "top_p"] {
        if v[k].is_number() {
            body[k] = v[k].clone();
        }
    }
    if v["top_k"].is_number() {
        body["top_k"] = v["top_k"].clone();
    }
    if let Some(stop) = v.get("stop") {
        let arr: Vec<Value> = match stop {
            Value::String(s) => vec![json!(s)],
            Value::Array(a) => a.clone(),
            _ => Vec::new(),
        };
        if !arr.is_empty() {
            body["stop_sequences"] = Value::Array(arr);
        }
    }
    // 客户端明确带 thinking（Anthropic 形状，如 Claude Code 系）就原样透传；
    // 不带就让上游按自己的默认（M3 是 adaptive、M3.1-Flash 强制开）走。
    if v["thinking"].is_object() {
        body["thinking"] = v["thinking"].clone();
    }
    body
}

// ───────────────────────── Anthropic → OpenAI ─────────────────────────

fn finish_of(stop: &str) -> &'static str {
    match stop {
        "tool_use" => "tool_calls",
        "max_tokens" => "length",
        "stop_sequence" => "stop",
        "refusal" => "content_filter",
        _ => "stop",
    }
}

fn usage_of(v: &Value) -> Value {
    let inp = v["input_tokens"].as_u64().unwrap_or(0);
    let cache_w = v["cache_creation_input_tokens"].as_u64().unwrap_or(0);
    let cache_r = v["cache_read_input_tokens"].as_u64().unwrap_or(0);
    let out = v["output_tokens"].as_u64().unwrap_or(0);
    let prompt = inp + cache_w + cache_r;
    json!({
        "prompt_tokens": prompt,
        "completion_tokens": out,
        "total_tokens": prompt + out,
        "prompt_tokens_details": { "cached_tokens": cache_r },
        "completion_tokens_details": {
            "reasoning_tokens": v["output_tokens_details"]["thinking_tokens"].as_u64().unwrap_or(0)
        },
    })
}

/// 非流式：Anthropic message → OpenAI chat.completion
pub fn translate_response(v: &Value, model: &str) -> Value {
    let id = v["id"]
        .as_str()
        .map(|s| format!("chatcmpl-mm-{s}"))
        .unwrap_or_else(|| format!("chatcmpl-mm-{}", now_secs()));
    let mut text = String::new();
    let mut reasoning = String::new();
    let mut calls: Vec<Value> = Vec::new();
    for b in v["content"].as_array().cloned().unwrap_or_default() {
        match b["type"].as_str().unwrap_or("") {
            "text" => text.push_str(b["text"].as_str().unwrap_or("")),
            "thinking" => reasoning.push_str(b["thinking"].as_str().unwrap_or("")),
            "tool_use" => calls.push(json!({
                "id": b["id"].as_str().unwrap_or("call_mm"),
                "type": "function",
                "function": {
                    "name": b["name"].as_str().unwrap_or(""),
                    "arguments": b["input"].to_string(),
                },
            })),
            _ => {}
        }
    }
    let stop = v["stop_reason"].as_str().unwrap_or("end_turn");
    let mut msg = json!({
        "role": "assistant",
        "content": if text.is_empty() && !calls.is_empty() { Value::Null } else { json!(text) },
    });
    if !reasoning.is_empty() {
        msg["reasoning_content"] = json!(reasoning);
    }
    if !calls.is_empty() {
        msg["tool_calls"] = Value::Array(calls);
    }
    json!({
        "id": id,
        "object": "chat.completion",
        "created": now_secs() as i64,
        "model": if model.is_empty() { v["model"].clone() } else { json!(model) },
        "choices": [{ "index": 0, "message": msg, "finish_reason": finish_of(stop) }],
        "usage": usage_of(&v["usage"]),
    })
}

/// 上游把错误放在 200 里时（`base_resp.status_code != 0`）也要认出来
pub fn upstream_error(v: &Value) -> Option<String> {
    let code = v["base_resp"]["status_code"].as_i64().unwrap_or(0);
    if code != 0 {
        return Some(format!(
            "{} (code {})",
            v["base_resp"]["status_msg"].as_str().unwrap_or("上游返回错误"),
            code
        ));
    }
    if v["type"].as_str() == Some("error") {
        return Some(
            v["error"]["message"]
                .as_str()
                .unwrap_or("上游返回 error 事件")
                .to_string(),
        );
    }
    None
}

// ───────────────────────── 流式：Anthropic SSE → OpenAI SSE ─────────────────────────

/// 把上游 Anthropic SSE 边读边翻成 OpenAI `chat.completion.chunk`。
/// 用 `Read` 实现，直接交给 tiny_http 当响应体 —— 客户端看到的是逐字流，
/// 不是「等上游写完再一次性吐」。
pub struct StreamTranslator<R: Read> {
    inner: R,
    raw: Vec<u8>,
    out: Vec<u8>,
    pos: usize,
    eof: bool,
    done_sent: bool,
    role_sent: bool,
    id: String,
    created: i64,
    model: String,
    /// Anthropic content block index → 该块是什么
    kinds: std::collections::HashMap<u64, String>,
    /// Anthropic content block index → OpenAI tool_calls 里的序号
    tool_ord: std::collections::HashMap<u64, usize>,
    tool_count: usize,
    /// 上游 Anthropic 口径的 usage 原样留着（message_start 给一次、message_delta 再补一次），
    /// 出帧时才换算成 OpenAI 口径 —— 边收边换会把两处字段搅在一起。
    raw_usage: Value,
    finish: Option<String>,
    pub error: Option<String>,
    pub started: Instant,
}

impl<R: Read> StreamTranslator<R> {
    pub fn new(inner: R, model: &str) -> Self {
        Self {
            inner,
            raw: Vec::new(),
            out: Vec::new(),
            pos: 0,
            eof: false,
            done_sent: false,
            role_sent: false,
            id: format!("chatcmpl-mm-{}", now_secs()),
            created: now_secs() as i64,
            model: model.to_string(),
            kinds: std::collections::HashMap::new(),
            tool_ord: std::collections::HashMap::new(),
            tool_count: 0,
            raw_usage: json!({}),
            finish: None,
            error: None,
            started: Instant::now(),
        }
    }

    fn chunk(&self, delta: Value, finish: Option<&str>) -> Value {
        json!({
            "id": self.id,
            "object": "chat.completion.chunk",
            "created": self.created,
            "model": self.model,
            "choices": [{ "index": 0, "delta": delta, "finish_reason": finish }],
        })
    }

    fn emit(&mut self, v: &Value) {
        self.out
            .extend_from_slice(format!("data: {}\n\n", v).as_bytes());
    }

    fn handle_event(&mut self, ev: &str, data: &str) {
        if data.trim() == "[DONE]" {
            return;
        }
        let Ok(v) = serde_json::from_str::<Value>(data) else {
            return;
        };
        let ty = v["type"].as_str().unwrap_or(ev);
        match ty {
            "message_start" => {
                if v["message"]["usage"].is_object() {
                    self.raw_usage = v["message"]["usage"].clone();
                }
                if !self.role_sent {
                    let c = self.chunk(json!({ "role": "assistant", "content": "" }), None);
                    self.emit(&c);
                    self.role_sent = true;
                }
            }
            "content_block_start" => {
                let idx = v["index"].as_u64().unwrap_or(0);
                let bt = v["content_block"]["type"].as_str().unwrap_or("text");
                self.kinds.insert(idx, bt.to_string());
                if bt == "tool_use" {
                    let ord = self.tool_count;
                    self.tool_count += 1;
                    self.tool_ord.insert(idx, ord);
                    let c = self.chunk(
                        json!({ "tool_calls": [{
                            "index": ord,
                            "id": v["content_block"]["id"].as_str().unwrap_or("call_mm"),
                            "type": "function",
                            "function": {
                                "name": v["content_block"]["name"].as_str().unwrap_or(""),
                                "arguments": "",
                            }
                        }]}),
                        None,
                    );
                    self.emit(&c);
                }
            }
            "content_block_delta" => {
                let idx = v["index"].as_u64().unwrap_or(0);
                let d = &v["delta"];
                match d["type"].as_str().unwrap_or("") {
                    "text_delta" => {
                        let t = d["text"].as_str().unwrap_or("");
                        if !t.is_empty() {
                            let c = self.chunk(json!({ "content": t }), None);
                            self.emit(&c);
                        }
                    }
                    "thinking_delta" => {
                        let t = d["thinking"].as_str().unwrap_or("");
                        if !t.is_empty() {
                            let c = self.chunk(json!({ "reasoning_content": t }), None);
                            self.emit(&c);
                        }
                    }
                    "input_json_delta" => {
                        let part = d["partial_json"].as_str().unwrap_or("");
                        if !part.is_empty() {
                            let ord = self.tool_ord.get(&idx).copied().unwrap_or(0);
                            let c = self.chunk(
                                json!({ "tool_calls": [{ "index": ord, "function": { "arguments": part } }] }),
                                None,
                            );
                            self.emit(&c);
                        }
                    }
                    _ => {}
                }
            }
            "message_delta" => {
                if let Some(u) = v["usage"].as_object() {
                    let mut merged = self.raw_usage.as_object().cloned().unwrap_or_default();
                    for (k, val) in u {
                        merged.insert(k.clone(), val.clone());
                    }
                    self.raw_usage = Value::Object(merged);
                }
                if let Some(r) = v["delta"]["stop_reason"].as_str() {
                    self.finish = Some(finish_of(r).to_string());
                }
            }
            "error" => {
                let msg = v["error"]["message"].as_str().unwrap_or("上游 error 事件");
                self.error = Some(msg.to_string());
                self.emit(&json!({ "error": { "message": msg, "type": "upstream_error" } }));
            }
            _ => {}
        }
    }

    fn pump(&mut self) {
        loop {
            let Some((i, len)) = find_sep(&self.raw) else {
                break;
            };
            let block: Vec<u8> = self.raw.drain(..i + len).collect();
            let text = String::from_utf8_lossy(&block[..i]).to_string();
            self.handle_block(&text);
        }
    }

    fn handle_block(&mut self, text: &str) {
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
        if !data.is_empty() {
            self.handle_event(&ev, &data);
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

impl<R: Read> Read for StreamTranslator<R> {
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
                    // 收尾：带 usage 的最后一帧 + [DONE]
                    let fin = self.finish.clone().unwrap_or_else(|| "stop".to_string());
                    let mut c = self.chunk(json!({}), Some(&fin));
                    let u = usage_of(&self.raw_usage);
                    if self.raw_usage.as_object().map(|o| !o.is_empty()).unwrap_or(false) {
                        c["usage"] = u;
                    }
                    self.emit(&c);
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

/// 非流式收集（probe / 测试用）
pub fn collect_completion<R: Read>(tr: &mut StreamTranslator<R>, label: &str) -> Value {
    let mut text = String::new();
    let mut reasoning = String::new();
    let mut finish = String::new();
    let mut usage = json!({});
    let mut sink = Vec::new();
    let _ = tr.read_to_end(&mut sink);
    for line in String::from_utf8_lossy(&sink).lines() {
        let Some(d) = line.strip_prefix("data: ") else {
            continue;
        };
        if d.trim() == "[DONE]" {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(d) else {
            continue;
        };
        if let Some(t) = v["choices"][0]["delta"]["content"].as_str() {
            text.push_str(t);
        }
        if let Some(t) = v["choices"][0]["delta"]["reasoning_content"].as_str() {
            reasoning.push_str(t);
        }
        if let Some(f) = v["choices"][0]["finish_reason"].as_str() {
            finish = f.to_string();
        }
        if v["usage"].is_object() {
            usage = v["usage"].clone();
        }
    }
    let mut msg = json!({ "role": "assistant", "content": text });
    if !reasoning.is_empty() {
        msg["reasoning_content"] = json!(reasoning);
    }
    json!({
        "id": tr.id,
        "object": "chat.completion",
        "created": tr.created,
        "model": if label.is_empty() { tr.model.clone() } else { label.to_string() },
        "choices": [{ "index": 0, "message": msg,
                      "finish_reason": if finish.is_empty() { "stop".into() } else { finish } }],
        "usage": usage,
    })
}

// ───────────────────────── 探测 / 测试 ─────────────────────────

/// 发一次极小补全，验证「令牌 + 网关 + 模型名」整条链（约 10 token，几乎不花额度）
pub fn test(model: &str) -> Value {
    let tok = token();
    if tok.is_empty() {
        return json!({ "ok": false, "error": "没有可用的令牌：先打开 MiniMax Code 登录一次，或点「刷新令牌」" });
    }
    let m = normalize_model(model);
    let t0 = Instant::now();
    let body = json!({
        "model": m,
        "max_tokens": 16,
        "messages": [{ "role": "user", "content": [{ "type": "text", "text": "ping" }] }],
    })
    .to_string();
    let res = ureq::post(&format!("{GATEWAY_BASE}/messages"))
        .set("Authorization", &format!("Bearer {tok}"))
        .set("anthropic-version", "2023-06-01")
        .set("Content-Type", "application/json")
        .set("Accept", "application/json")
        .timeout(Duration::from_secs(90))
        .send_string(&body);
    let (status, text) = match res {
        Ok(r) => (r.status(), r.into_string().unwrap_or_default()),
        Err(ureq::Error::Status(code, r)) => (code, r.into_string().unwrap_or_default()),
        Err(e) => {
            return json!({ "ok": false, "ms": t0.elapsed().as_millis() as u64, "error": e.to_string() })
        }
    };
    let v: Value = serde_json::from_str(&text).unwrap_or_else(|_| json!({}));
    if status >= 400 {
        return json!({
            "ok": false, "status": status, "ms": t0.elapsed().as_millis() as u64,
            "error": v["error"]["message"].as_str()
                .or_else(|| v["error"].as_str())
                .map(|s| s.to_string())
                .unwrap_or_else(|| text.chars().take(300).collect()),
        });
    }
    if let Some(e) = upstream_error(&v) {
        return json!({ "ok": false, "status": status, "ms": t0.elapsed().as_millis() as u64, "error": e });
    }
    let o = translate_response(&v, &m);
    json!({
        "ok": true, "status": status, "ms": t0.elapsed().as_millis() as u64,
        "model_used": v["model"].clone(),
        "content": o["choices"][0]["message"]["content"].clone(),
        "usage": o["usage"].clone(),
    })
}

// ───────────────────────── 视图 / 一键 ─────────────────────────

fn pool_name() -> &'static str {
    "MiniMax Code 上游组"
}
fn dist_name() -> &'static str {
    "MiniMax Code 接入"
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

fn in_pool(name: &str) -> bool {
    let inst = crate::gateway::member_src_instance("minimax", name);
    find_pool()
        .and_then(|p| p["members"].as_array().cloned())
        .map(|ms| ms.iter().any(|m| m["src"].as_str() == Some(inst.as_str())))
        .unwrap_or(false)
}

pub fn view() -> Value {
    let p = probe();
    let usable = p["ok"].as_bool().unwrap_or(false);
    let mut rows = Vec::new();
    for m in crate::registry::load_instances("minimax") {
        let name = m["name"].as_str().unwrap_or("").to_string();
        if name.is_empty() {
            continue;
        }
        rows.push(json!({
            "app": "minimax",
            "app_name": "MiniMax Code",
            "name": name,
            "state": p["state"].clone(),
            "text": p["text"].clone(),
            "masked": p["masked"].clone(),
            "remain_min": p["remain_min"].clone(),
            "in_pool": in_pool(&name),
            "usable": usable,
        }));
    }
    json!({
        "accounts": rows,
        "models": models(),
        "token_state": p["state"].clone(),
        "token_text": p["text"].clone(),
        "token_masked": p["masked"].clone(),
        "token_from": p["from"].clone(),
        "remain_min": p["remain_min"].clone(),
        "pool": find_pool().map(|p| json!({ "id": p["id"], "name": p["name"] })),
        "dist": find_dist().map(|d| json!({
            "id": d["id"], "name": d["name"],
            "masked": mask(d["key"].as_str().unwrap_or("")),
            "calls": d["calls"].as_u64().unwrap_or(0),
        })),
        "base_url": format!("http://127.0.0.1:{}/v1", crate::registry::PROXY_PORT),
        "gateway": GATEWAY_BASE,
        "port": crate::registry::PROXY_PORT,
    })
}

/// 一键：验令牌 → 建上游组（含这个号）→ 建接入密钥 → 起反代。
/// 与 AStudio 的「不代建」不同：MiniMax 只有一个整机登录态、没有账号页入口，
/// 不代建的话用户得自己拼 `instance:minimax/<名字>`，太容易配错。
/// 已存在同名组/密钥就复用，不重复堆。
pub fn one_click(proxy: &std::sync::Arc<crate::proxy::Proxy>) -> Value {
    if token().is_empty() {
        return json!({
            "ok": false,
            "msg": "MiniMax Code 的令牌读不到：先打开 MiniMax Code 登录一次，回来再点。",
        });
    }
    let accts = crate::registry::load_instances("minimax");
    let name = accts
        .iter()
        .filter_map(|m| m["name"].as_str().map(|s| s.to_string()))
        .find(|s| !s.is_empty())
        .unwrap_or_else(|| "MiniMax Code".to_string());
    let src = crate::gateway::member_src_instance("minimax", &name);

    // 上游组：有就补成员，没有就建
    let pool_id = match find_pool() {
        Some(p) => {
            let id = p["id"].as_str().unwrap_or("").to_string();
            let mut members = p["members"].as_array().cloned().unwrap_or_default();
            if !members.iter().any(|m| m["src"].as_str() == Some(src.as_str())) {
                members.push(json!({ "src": src, "base_url": "", "enabled": true }));
                let (ok, msg) = crate::gateway::upsert_pool(&json!({
                    "id": id, "name": pool_name(), "enabled": true, "members": members,
                }));
                if !ok {
                    return json!({ "ok": false, "msg": format!("补成员失败：{msg}") });
                }
            }
            id
        }
        None => {
            let (ok, msg) = crate::gateway::upsert_pool(&json!({
                "name": pool_name(),
                "enabled": true,
                "members": [{ "src": src, "base_url": "", "enabled": true }],
            }));
            if !ok {
                return json!({ "ok": false, "msg": format!("建上游组失败：{msg}") });
            }
            // upsert 返回的是提示串，重新按名找一次拿 id
            match find_pool() {
                Some(p) => p["id"].as_str().unwrap_or("").to_string(),
                None => return json!({ "ok": false, "msg": "上游组建好了但找不到，去「上游组」页看一眼" }),
            }
        }
    };

    // 接入密钥：复用同名的，没有就建
    let mut key_msg = "接入密钥已存在".to_string();
    if find_dist().is_none() {
        let (ok, msg) = crate::gateway::upsert_dist(&json!({
            "name": dist_name(),
            "enabled": true,
            "pools": [pool_id.clone()],
        }));
        if !ok {
            return json!({ "ok": false, "msg": format!("建接入密钥失败：{msg}") });
        }
        key_msg = msg;
    }

    let port = crate::registry::PROXY_PORT;
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

    let dist = find_dist();
    json!({
        "ok": true,
        "msg": format!("{proxy_msg}；上游组「{}」已含 {name}；{key_msg}", pool_name()),
        "base_url": format!("http://127.0.0.1:{port}/v1"),
        "key_masked": dist.as_ref().map(|d| mask(d["key"].as_str().unwrap_or(""))).unwrap_or_default(),
        "models": ["minimax/MiniMax-M3.1-Flash", "minimax/MiniMax-M3"],
    })
}

pub fn dist_reveal() -> String {
    find_dist()
        .and_then(|d| d["key"].as_str().map(|s| s.to_string()))
        .unwrap_or_default()
}

pub fn on_account_changed() {
    invalidate();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn oai(messages: Value, extra: Value) -> Value {
        let mut v = json!({ "model": "minimax/MiniMax-M3.1-Flash", "messages": messages });
        if let (Some(a), Some(b)) = (v.as_object_mut(), extra.as_object()) {
            for (k, val) in b {
                a.insert(k.clone(), val.clone());
            }
        }
        v
    }

    #[test]
    fn base_marker_is_recognized() {
        assert!(is_mm_base(BASE_MARK));
        assert!(is_mm_base("minimax://mcode/"));
        assert!(is_mm_base("https://agent.minimax.cn/mavis/api/v1/llm/v1"));
        assert!(!is_mm_base("https://api.openai.com"));
    }

    #[test]
    fn request_keeps_system_and_alternates() {
        let v = oai(
            json!([
                { "role": "system", "content": "你是猫" },
                { "role": "user", "content": "你好" },
                { "role": "assistant", "content": "喵" },
                { "role": "user", "content": [{ "type": "text", "text": "再说一句" }] },
            ]),
            json!({ "max_tokens": 32, "temperature": 0.3, "stop": "END" }),
        );
        let a = translate_request(&v);
        assert_eq!(a["system"], "你是猫");
        assert_eq!(a["max_tokens"], 32);
        assert_eq!(a["stop_sequences"], json!(["END"]));
        assert_eq!(a["messages"].as_array().unwrap().len(), 3);
        assert_eq!(a["messages"][0]["role"], "user");
        assert_eq!(a["messages"][1]["role"], "assistant");
        // 前缀被剥掉、模型名归一
        assert_eq!(a["model"], "MiniMax-M3.1-Flash");
        assert_eq!(a["stream"], false);
    }

    #[test]
    fn request_maps_tools_and_results() {
        let v = oai(
            json!([
                { "role": "user", "content": "北京天气" },
                { "role": "assistant", "content": null, "tool_calls": [{
                    "id": "call_1", "type": "function",
                    "function": { "name": "get_weather", "arguments": "{\"city\":\"北京\"}" }
                }]},
                { "role": "tool", "tool_call_id": "call_1", "content": "晴 25 度" },
            ]),
            json!({
                "tools": [{ "type": "function", "function": {
                    "name": "get_weather", "description": "查天气",
                    "parameters": { "type": "object", "properties": { "city": { "type": "string" } } }
                }}],
                "tool_choice": "required",
            }),
        );
        let a = translate_request(&v);
        assert_eq!(a["tools"][0]["name"], "get_weather");
        assert!(a["tools"][0]["input_schema"].is_object());
        assert_eq!(a["tool_choice"], json!({ "type": "any" }));
        let asst = &a["messages"][1];
        assert_eq!(asst["content"][0]["type"], "tool_use");
        assert_eq!(asst["content"][0]["input"]["city"], "北京");
        let res = &a["messages"][2];
        assert_eq!(res["role"], "user");
        assert_eq!(res["content"][0]["type"], "tool_result");
        assert_eq!(res["content"][0]["tool_use_id"], "call_1");
    }

    #[test]
    fn request_defaults_max_tokens_and_clamps() {
        let a = translate_request(&oai(json!([{ "role": "user", "content": "hi" }]), json!({})));
        assert_eq!(a["max_tokens"], DEFAULT_MAX_TOKENS);
        let b = translate_request(&oai(
            json!([{ "role": "user", "content": "hi" }]),
            json!({ "max_tokens": 999999 }),
        ));
        assert_eq!(b["max_tokens"], MAX_OUTPUT_TOKENS);
    }

    #[test]
    fn data_url_becomes_base64_image() {
        let v = oai(
            json!([{ "role": "user", "content": [
                { "type": "text", "text": "看图" },
                { "type": "image_url", "image_url": { "url": "data:image/png;base64,QUJD" } }
            ]}]),
            json!({}),
        );
        let a = translate_request(&v);
        let blocks = a["messages"][0]["content"].as_array().unwrap();
        assert_eq!(blocks[1]["type"], "image");
        assert_eq!(blocks[1]["source"]["media_type"], "image/png");
        assert_eq!(blocks[1]["source"]["data"], "QUJD");
    }

    #[test]
    fn response_maps_text_thinking_and_tools() {
        let up = json!({
            "id": "abc", "type": "message", "role": "assistant", "model": "MiniMax-M3.1-Flash",
            "content": [
                { "type": "thinking", "thinking": "想一想", "signature": "s" },
                { "type": "text", "text": "收到" },
                { "type": "tool_use", "id": "call_9", "name": "f", "input": { "a": 1 } }
            ],
            "usage": { "input_tokens": 10, "output_tokens": 5,
                       "cache_read_input_tokens": 7, "cache_creation_input_tokens": 3 },
            "stop_reason": "tool_use",
            "base_resp": { "status_code": 0, "status_msg": "" }
        });
        assert!(upstream_error(&up).is_none());
        let o = translate_response(&up, "minimax/MiniMax-M3.1-Flash");
        let msg = &o["choices"][0]["message"];
        assert_eq!(msg["content"], "收到");
        assert_eq!(msg["reasoning_content"], "想一想");
        assert_eq!(msg["tool_calls"][0]["id"], "call_9");
        assert_eq!(msg["tool_calls"][0]["function"]["arguments"], "{\"a\":1}");
        assert_eq!(o["choices"][0]["finish_reason"], "tool_calls");
        assert_eq!(o["usage"]["prompt_tokens"], 20);
        assert_eq!(o["usage"]["completion_tokens"], 5);
        assert_eq!(o["usage"]["prompt_tokens_details"]["cached_tokens"], 7);
    }

    #[test]
    fn stream_translates_to_openai_chunks() {
        let raw = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":9,\"output_tokens\":0}}}\n\n",
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\"}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"想\"}}\n\n",
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"text_delta\",\"text\":\"收到\"}}\n\n",
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":2,\"content_block\":{\"type\":\"tool_use\",\"id\":\"call_1\",\"name\":\"f\"}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":2,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"a\\\":\"}}\n\n",
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":12}}\n\n",
            "event: message_stop\n",
            "data: {\"type\":\"message_stop\"}\n\n",
        );
        let mut t = StreamTranslator::new(std::io::Cursor::new(raw.as_bytes().to_vec()), "m");
        let o = collect_completion(&mut t, "m");
        assert_eq!(o["choices"][0]["message"]["content"], "收到");
        assert_eq!(o["choices"][0]["message"]["reasoning_content"], "想");
        assert_eq!(o["choices"][0]["finish_reason"], "tool_calls");
        assert_eq!(o["usage"]["completion_tokens"], 12);
    }

    /// 真机用例：真连 MiniMax 上游，走完整条链路（上游组成员解析 → Anthropic 翻译 → 上游 → 翻回 OpenAI）。
    /// 默认批次不跑（要真令牌、真花十几 token）；单独跑：
    ///   target/release/deps/agent_cockpit-<hash>.exe minimax_end_to_end --ignored --nocapture
    /// 前置：本机 MiniMax Code 登录过，且 config.json 里有「MiniMax Code 接入」这把分发 key
    /// （面板上点一次「一键反代」就会建好）。监听 18788，不抢正在跑的 8788。
    #[test]
    #[ignore]
    fn minimax_end_to_end() {
        let dist = crate::gateway::dist_keys()
            .into_iter()
            .find(|d| d["name"].as_str() == Some("MiniMax Code 接入"))
            .and_then(|d| d["key"].as_str().map(|s| s.to_string()))
            .unwrap_or_default();
        assert!(!dist.is_empty(), "先点一次「一键反代」，把「MiniMax Code 接入」这把 key 建出来");
        assert!(!token().is_empty(), "MiniMax Code 令牌读不到：先打开客户端登录一次");

        let p = crate::proxy::Proxy::shared();
        p.set_config(Some("https://api.openai.com"), None);
        let port = 18788u16;
        let (ok, msg) = p.start(port);
        assert!(ok, "反代起不来：{msg}");
        let url = format!("http://127.0.0.1:{port}/v1/chat/completions");

        // ① 非流式
        let body = json!({
            "model": "minimax/MiniMax-M3.1-Flash",
            "max_tokens": 32,
            "messages": [{ "role": "user", "content": "只回答两个字：收到" }],
        })
        .to_string();
        let v: Value = match ureq::post(&url)
            .set("Authorization", &format!("Bearer {dist}"))
            .set("Content-Type", "application/json")
            .timeout(Duration::from_secs(120))
            .send_string(&body)
        {
            Ok(r) => r.into_json().unwrap_or_else(|_| json!({})),
            Err(ureq::Error::Status(c, r)) => {
                panic!("非流式 HTTP {c}: {}", r.into_string().unwrap_or_default())
            }
            Err(e) => panic!("非流式请求失败：{e}"),
        };
        let text = v["choices"][0]["message"]["content"].as_str().unwrap_or("");
        assert!(!text.trim().is_empty(), "非流式没有内容：{v}");
        assert!(
            v["usage"]["completion_tokens"].as_u64().unwrap_or(0) > 0,
            "非流式没有 usage：{v}"
        );
        assert_eq!(v["model"], "minimax/MiniMax-M3.1-Flash", "回的模型名不对：{v}");

        // ② 流式：必须是 OpenAI chunk，且以 [DONE] 收尾
        let body = json!({
            "model": "minimax/MiniMax-M3.1-Flash",
            "max_tokens": 32,
            "stream": true,
            "messages": [{ "role": "user", "content": "数到二，用中文数字" }],
        })
        .to_string();
        let mut resp = match ureq::post(&url)
            .set("Authorization", &format!("Bearer {dist}"))
            .set("Content-Type", "application/json")
            .timeout(Duration::from_secs(120))
            .send_string(&body)
        {
            Ok(r) => r,
            Err(ureq::Error::Status(c, r)) => {
                panic!("流式 HTTP {c}: {}", r.into_string().unwrap_or_default())
            }
            Err(e) => panic!("流式请求失败：{e}"),
        };
        let mut s = String::new();
        let _ = resp.into_reader().read_to_string(&mut s);
        assert!(s.contains("chat.completion.chunk"), "流里没有 OpenAI chunk：{s}");
        assert!(s.contains("data: [DONE]"), "流没有 [DONE] 收尾：{s}");
        p.stop();
    }

    #[test]
    fn model_aliases_normalize() {
        assert_eq!(normalize_model("m3"), "MiniMax-M3");
        assert_eq!(normalize_model("M3.1-Flash"), "MiniMax-M3.1-Flash");
        assert_eq!(normalize_model(""), "MiniMax-M3.1-Flash");
        assert_eq!(normalize_model("MiniMax-M2.7-highspeed"), "MiniMax-M2.7-highspeed");
    }
}
