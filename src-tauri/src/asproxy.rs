//! AStudio（科大讯飞 星辰/星火）模型反代。
//!
//! AStudio 是讯飞把 Codex 内核改造的 Electron IDE：模型上游不是私有协议，
//! 使用 OpenAI Responses 网关 `…/model-gateway/v1/responses`。
//! 鉴权只要一行 `Authorization: Bearer <modelBearerToken>`，
//! 令牌明文躺在产品自己的 `astron-session.json` 里（不是同文件的 `token`——
//! 那是 Astron SSO 会话令牌，拿去当 Bearer 会 401）。
//!
//! spark-x2.6-4b 的 Responses 支持流式输出和工具回传。
//! 令牌拿不到时用 Cookie 打 `/xingchen-studio/bot/models/configs` 换 `api_key`。
//!
//! 注意方向：往 AStudio 里写自定义模型（provider 配置）有签名校验，那条路做不了；
//! 这里是把它的模型反代出来，两条路不冲突。
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub const APPS: [&str; 1] = ["astudio"];

/// 客户端标记：上游组成员的 base_url 用它表示「这是 AStudio 上游」。
/// proxy.rs 靠它识别，models_for 靠它决定列哪些模型。
pub const BASE_MARK: &str = "astudio://spark";
/// 云端模型网关（**不带 /v1**：客户端发 `/v1/chat/completions`，
/// 通用拼接会原样接在后面，正好是 `…/model-gateway/v1/chat/completions`）。
pub const GATEWAY_BASE: &str =
    "https://astronstudio-api-volces-prod.xf-yun.com/model-gateway";
/// 模型配置接口（Cookie 鉴权）：既拿 api_key，也拿账号可用模型清单。
pub const MODELS_URL: &str =
    "https://agent.xfyun.cn/xingchen-studio/bot/models/configs";
/// 客户端身份头：Windows=21、macOS=22（真实客户端发的就是这两个）。
/// 实测只带 Bearer 也能过，带上更贴近官方客户端。
pub const CLIENT_TYPE: &str = "21";

/// 这条上游是不是 AStudio：认内部标记，也认云端网关域名
/// （resolve_member 给的是真实网关地址，用户也可能手填）。
pub fn is_astudio_base(base: &str) -> bool {
    let b = base.trim().trim_end_matches('/');
    b == BASE_MARK
        || b.starts_with(GATEWAY_BASE)
        || b.contains("astronstudio-api-volces-prod.xf-yun.com")
        || b.contains("astronstudio-api-volces-pre.xf-yun.com")
}

fn cfg() -> Value {
    crate::registry::load_config()
        .get("astudio")
        .cloned()
        .unwrap_or_else(|| json!({}))
}

/// 客户端版本（真实客户端发的是产品版本号；拿不到就不发这个头）
fn studio_version() -> String {
    cfg()["studio_version"]
        .as_str()
        .unwrap_or("")
        .trim()
        .to_string()
}

fn session_file() -> Option<PathBuf> {
    crate::registry::profile_dir("astudio").map(|d| d.join("astron-session.json"))
}

fn read_session() -> Value {
    let path = match session_file() {
        Some(p) => p,
        None => return json!({}),
    };
    match std::fs::read_to_string(&path) {
        Ok(s) => serde_json::from_str::<Value>(&s).unwrap_or_else(|_| json!({})),
        Err(_) => json!({}),
    }
}

/// 账号 id / SSO 会话令牌（刷新模型令牌要用它们拼 Cookie）
fn session_cookie(session: &Value) -> String {
    let mut parts = Vec::new();
    for (k, key) in [
        ("account_id", "accountId"),
        ("ssoSessionId", "ssoSessionId"),
        ("sso_sessionid", "ssoSessionId"),
        ("token", "token"),
    ] {
        if let Some(v) = session[key].as_str() {
            if !v.trim().is_empty() {
                parts.push(format!("{}={}", k, v.trim()));
            }
        }
    }
    parts.join("; ")
}

// ───────────────────────── 模型令牌（带缓存） ─────────────────────────

struct TokenCache {
    token: String,
    from: String,
    at: u64,
}

static TOKEN: Mutex<Option<TokenCache>> = Mutex::new(None);
const TOKEN_TTL: u64 = 300;

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 取模型 Bearer：优先会话文件里的 `modelBearerToken`，没有就刷新一次。
/// 返回空串 = 这个号没登录 / 令牌读不到，如实返回空，不拿别人的号顶替。
pub fn token() -> String {
    if let Some(c) = TOKEN.lock().ok().and_then(|g| {
        let t = g.as_ref()?;
        (t.token.is_empty() || now_secs().saturating_sub(t.at) < TOKEN_TTL)
            .then(|| (t.token.clone(), t.from.clone()))
    }) {
        return c.0;
    }
    let session = read_session();
    let direct = session["modelBearerToken"]
        .as_str()
        .unwrap_or("")
        .trim()
        .to_string();
    let (tok, from) = if !direct.is_empty() {
        (direct, "session".to_string())
    } else {
        match refresh_token(&session) {
            Some(t) => (t, "refresh".to_string()),
            None => (String::new(), String::new()),
        }
    };
    if let Ok(mut g) = TOKEN.lock() {
        *g = Some(TokenCache {
            token: tok.clone(),
            from,
            at: now_secs(),
        });
    }
    tok
}

/// 用会话 Cookie 换模型 api_key（同一账号所有模型共用一把）。
/// 顺便把账号可用模型清单带回来。
pub fn refresh_token(session: &Value) -> Option<String> {
    let cookie = session_cookie(session);
    if cookie.is_empty() {
        return None;
    }
    let res = ureq::get(MODELS_URL)
        .set("Accept", "application/json")
        .set("Cookie", &cookie)
        .set("User-Agent", "Mozilla/5.0")
        .timeout(Duration::from_secs(15))
        .call();
    match res {
        Ok(resp) => {
            let v: Value = resp.into_json().unwrap_or_else(|_| json!({}));
            let data = v["data"].as_array().cloned().unwrap_or_default();
            let pick = data
                .iter()
                .find(|m| m["is_current"] == json!(true))
                .or_else(|| data.iter().find(|m| m["is_default"] == json!(true)))
                .or_else(|| data.first())?;
            pick["api_key"].as_str().map(|s| s.trim().to_string())
        }
        Err(_) => None,
    }
}

/// 清掉令牌缓存（换号/重登后调用）
pub fn invalidate() {
    if let Ok(mut g) = TOKEN.lock() {
        *g = None;
    }
}

// ───────────────────────── 模型清单 ─────────────────────────

/// AStudio writes this catalog after fetching descriptors for the active account.
/// Match its account fingerprint before advertising any of the cached slugs.
pub fn account_models() -> Vec<Value> {
    let Some(dir) = crate::registry::profile_dir("astudio") else { return Vec::new() };
    let Some(catalog) = std::fs::read(dir.join("provider-model-catalog-v1.json"))
        .ok().and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
    else { return Vec::new() };
    let session = read_session();
    let Some(account) = session["accountId"].as_str().filter(|s| !s.is_empty()) else {
        return Vec::new();
    };
    catalog_rows(&catalog, account)
}

fn catalog_rows(catalog: &Value, account: &str) -> Vec<Value> {
    let matches_account = ["AStudio", "AstronStudio"].iter().any(|namespace| {
        let scope = format!("{namespace}:provider-model-catalog:v1:account\0{account}");
        let hash = crate::wbproxy::hex(&crate::wbproxy::sha256(scope.as_bytes()));
        catalog["accountScopeHash"].as_str() == Some(hash.as_str())
    });
    if catalog["schemaVersion"] != 1 || !matches_account {
        return Vec::new();
    }
    catalog["models"].as_array().into_iter().flatten().filter_map(|m| {
        let id = m["slug"].as_str()?.trim();
        if id.is_empty() { return None; }
        let levels: Vec<Value> = m["supportedReasoningEfforts"].as_array().into_iter()
            .flatten().filter_map(|level| {
                let effort = level["value"].as_str()?;
                Some(json!({"effort": wire_effort(effort), "description": level["description"]}))
            }).collect();
        Some(json!({ "id": id, "name": m["name"].as_str().unwrap_or(id),
                     "free": id == "spark-x2.6-4b",
                     "supported_reasoning_levels": levels,
                     "default_reasoning_level": wire_effort(m["defaultReasoningEffort"].as_str().unwrap_or("high")) }))
    }).collect()
}

// AStudio's UI uses max, while the Responses gateway accepts xhigh.
// Older generated Codex catalogs advertised medium/minimal, which the gateway rejects.
fn wire_effort(effort: &str) -> &str {
    match effort {
        "max" => "xhigh",
        "medium" => "high",
        "minimal" => "none",
        other => other,
    }
}

pub fn prepare_responses(body: &[u8]) -> Vec<u8> {
    let Ok(mut request) = serde_json::from_slice::<Value>(body) else { return body.to_vec() };
    if let Some(effort) = request["reasoning"]["effort"].as_str() {
        let normalized = wire_effort(effort).to_owned();
        request["reasoning"]["effort"] = json!(normalized);
    }
    serde_json::to_vec(&request).expect("JSON request serializes")
}

pub fn models_live() -> Value {
    let models = account_models();
    if models.is_empty() {
        return json!({"ok": false, "models": [], "error": "请先在 AStudio 用当前账号打开模型选择器，以更新模型目录"});
    }
    json!({"ok": true, "models": models})
}

// ───────────────────────── 探测 / 测试 ─────────────────────────

fn identity_headers(call: ureq::Request) -> ureq::Request {
    let mut call = call.set("clientType", CLIENT_TYPE);
    let ver = studio_version();
    if !ver.is_empty() {
        call = call.set("studioVersion", &ver);
    }
    call
}

/// 只验令牌：不发模型请求（不花积分）
pub fn probe() -> Value {
    let session = read_session();
    let tok = token();
    if tok.is_empty() {
        let from_file = session_file()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();
        return json!({
            "ok": false,
            "state": "no_token",
            "text": "会话里没有模型令牌，也刷新不出来",
            "detail": if from_file.is_empty() {
                "没找到 AStudio 的登录档案".to_string()
            } else {
                format!("{} 里没有 modelBearerToken，且 Cookie 刷新失败（可能已退出登录）", from_file)
            },
            "masked": "",
            "from": "",
        });
    }
    json!({
        "ok": true,
        "state": "ok",
        "text": "令牌可读",
        "masked": crate::qproxy::mask(&tok),
        "from": if session["modelBearerToken"].as_str().unwrap_or("").trim().is_empty() {
            "refresh"
        } else {
            "session"
        },
        "detail": "",
    })
}

/// 发一次极小补全，验证「令牌 + 网关 + 模型名」整条链。
pub fn test(model: &str) -> Value {
    let tok = token();
    if tok.is_empty() {
        return json!({ "ok": false, "error": "没有可用的模型令牌，先点「刷新」" });
    }
    let t0 = Instant::now();
    let body = json!({
        "model": model,
        "input": "Reply OK",
        "stream": false,
        "max_output_tokens": 128,
    })
    .to_string();
    let res = identity_headers(
        ureq::post(&format!("{}/v1/responses", GATEWAY_BASE))
            .set("Authorization", &format!("Bearer {}", tok))
            .set("Content-Type", "application/json")
            .set("Accept", "application/json"),
    )
    .timeout(Duration::from_secs(90))
    .send_string(&body);
    match res {
        Ok(resp) => {
            let status = resp.status();
            let v: Value = resp.into_json().unwrap_or_else(|_| json!({}));
            json!({
                "ok": v["status"] == "completed",
                "status": status,
                "ms": t0.elapsed().as_millis() as u64,
                "model_used": v["model"].clone(),
                "content": v["output"].as_array().into_iter().flatten()
                    .filter_map(|item| item["content"].as_array()).flatten()
                    .filter_map(|part| part["text"].as_str()).collect::<String>(),
                "usage": v["usage"].clone(),
                "error": v["error"].clone(),
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

// ───────────────────────── 视图 / 一键 ─────────────────────────

fn pool_name() -> &'static str {
    "AStudio 上游组"
}
fn dist_name() -> &'static str {
    "AStudio 接入"
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

/// 这个号有没有进上游组（按 `instance:astudio/<name>` 认）
fn in_pool(name: &str) -> bool {
    let inst = crate::gateway::member_src_instance("astudio", name);
    find_pool()
        .and_then(|p| p["members"].as_array().cloned())
        .map(|ms| ms.iter().any(|m| m["src"].as_str() == Some(inst.as_str())))
        .unwrap_or(false)
}

pub fn view() -> Value {
    let tok = token();
    let p = probe();
    let mut rows = Vec::new();
    for m in crate::registry::load_instances("astudio") {
        let name = m["name"].as_str().unwrap_or("").to_string();
        if name.is_empty() {
            continue;
        }
        rows.push(json!({
            "app": "astudio",
            "app_name": "AStudio",
            "name": name,
            "state": p["state"].clone(),
            "text": p["text"].clone(),
            "masked": p["masked"].clone(),
            "from": p["from"].clone(),
            "in_pool": in_pool(&name),
            "usable": !tok.is_empty(),
        }));
    }
    let base = format!(
        "http://127.0.0.1:{}/v1",
        crate::registry::PROXY_PORT
    );
    json!({
        "accounts": rows,
        "models": account_models(),
        "token_from": p["from"].clone(),
        "token_masked": p["masked"].clone(),
        "pool": find_pool().map(|p| json!({ "id": p["id"], "name": p["name"] })),
        "dist": find_dist().map(|d| json!({
            "id": d["id"], "name": d["name"],
            "masked": crate::qproxy::mask(d["key"].as_str().unwrap_or("")),
            "calls": d["calls"].as_u64().unwrap_or(0),
        })),
        "base_url": base,
        "gateway": GATEWAY_BASE,
        "port": crate::registry::PROXY_PORT,
    })
}

/// 一键：验证令牌 → 起反代。上游组和接入密钥**不再代建**——归组、选 key 是用户的事，
/// 到「上游组」「接入密钥」两个表里手动做（历史教训：按名复用一旦名字对不上，
/// 每轮一键都会新建组+key，堆出一排同名残留）。
pub fn one_click(proxy: &std::sync::Arc<crate::proxy::Proxy>) -> Value {
    let accts = crate::registry::load_instances("astudio");
    if accts.is_empty() {
        return json!({ "ok": false, "msg": "没有 AStudio 账号。先在账号页建一个或用「导入已有数据」" });
    }
    if token().is_empty() {
        return json!({
            "ok": false,
            "msg": "AStudio 的模型令牌读不到（会话里没有 modelBearerToken，Cookie 也刷新不出来）。先在 AStudio 里登录一次再回来点。",
        });
    }
    let mut names = Vec::new();
    for m in &accts {
        let name = m["name"].as_str().unwrap_or("");
        if !name.is_empty() {
            names.push(name.to_string());
        }
    }
    if names.is_empty() {
        return json!({ "ok": false, "msg": "AStudio 账号名是空的，先去账号页修一下" });
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

    json!({
        "ok": true,
        "msg": format!(
            "令牌读到了（{} 个账号）；{}。账号还没动上游组和接入密钥：到「上游组」把账号勾进组，客户端用「接入密钥」表里的 key。",
            names.len(),
            proxy_msg
        ),
        "base_url": format!("http://127.0.0.1:{}/v1", port),
        "members": names.len(),
    })
}

pub fn dist_reveal() -> String {
    find_dist()
        .and_then(|d| d["key"].as_str().map(|s| s.to_string()))
        .unwrap_or_default()
}

/// 换号/重登后调用：清令牌缓存，下次请求重新读会话文件
pub fn on_account_changed() {
    invalidate();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn responses_normalizes_legacy_effort_without_losing_tools_or_history() {
        let original = json!({
            "model": "spark-x2.6-4b", "reasoning": {"effort": "medium", "summary": "auto"},
            "input": [{"type": "function_call_output", "call_id": "c1", "output": "OK"}],
            "tools": [{"type": "function", "name": "echo", "parameters": {"type": "object"}}]
        });
        let prepared: Value = serde_json::from_slice(&prepare_responses(&serde_json::to_vec(&original).unwrap())).unwrap();
        assert_eq!(prepared["reasoning"]["effort"], "high");
        assert_eq!(prepared["reasoning"]["summary"], "auto");
        assert_eq!(prepared["input"], original["input"]);
        assert_eq!(prepared["tools"], original["tools"]);
        assert_eq!(wire_effort("max"), "xhigh");
        assert_eq!(wire_effort("none"), "none");
    }

    #[test]
    fn catalog_is_scoped_to_the_logged_in_account() {
        let catalog = json!({
            "schemaVersion": 1, "accountScopeHash": "c8781b40b03a26f6e2148b2ab0146ece3ce740e734ffd0f63482929fa7bb1189",
            "models": [{"slug": "spark-x2.6-4b", "name": "Lite"}]
        });
        assert_eq!(catalog_rows(&catalog, "alice")[0]["id"], "spark-x2.6-4b");
        assert!(catalog_rows(&catalog, "bob").is_empty());
    }
}
