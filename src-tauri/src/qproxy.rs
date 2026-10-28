// Qoder / Qoder CN 的免费模型反代：把「账号档案里的登录态」变成 OpenAI 兼容上游。
//
// 两个区域走的路不一样，这不是实现偷懒，是实测出来的：
//
//   国际版  POST https://api2-v2.qoder.sh/model/v1/chat/completions
//           —— 客户端自己就在用这条，OpenAI 兼容（messages / stream / choices），
//              Authorization 直接用档案里的 `dt-` 令牌，**不需要任何签名**。已实测。
//   国内版  https://gateway.qoder.com.cn/model/*   → 503（这条路由在国内网关上没开）
//           真正的推理通道是 /algo/api/v2/service/pro/sse/agent_chat_generation，
//           带 Bearer 也是 403 {"code":"101","message":"Signature invalid"}——
//           它要的是官方客户端用 WASM 机器身份签出来的签名，令牌本身不够。
//           所以国内版只做**如实探测**，不伪造能力：状态会写 signature_required，
//           这种账号不进池（宁可让客户端看到"没配上游"，也不能给它一个必挂的上游）。
//
// 令牌从哪来：`<data_dir>/auth.v1.dat`，Chromium v10 信封
//   "v10" + nonce(12) + AES-256-GCM(明文, tag)；主密钥在 `<data_dir>/Local State`
//   的 os_crypt.encrypted_key（DPAPI 包着，去掉前 5 字节 "DPAPI" 再解）。
//   明文 = {token, refreshToken, expiresAt, user}。**只读 token，永不碰 refreshToken**
//   （refresh token 是一次性的，用一次就把客户端踢下线）。
use crate::registry::{all_apps, base_dir, load_instances, load_config, now_secs};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::UNIX_EPOCH;

// ───────────────────────── 区域 ─────────────────────────

/// 只有这两个产品的登录态能拿来当上游；qoder-cn-ide 没有独立登录态，不算。
pub const APPS: [&str; 2] = ["qoder", "qoder-cn"];

pub fn region_of(app_id: &str) -> Option<&'static str> {
    match app_id {
        "qoder" => Some("global"),
        "qoder-cn" => Some("cn"),
        _ => None,
    }
}

pub fn region_label(region: &str) -> &'static str {
    if region == "cn" {
        "国内版"
    } else {
        "国际版"
    }
}

/// 推理基址（拼 `/model/v1/chat/completions`）
pub fn model_base(region: &str) -> &'static str {
    if region == "cn" {
        "https://gateway.qoder.com.cn"
    } else {
        "https://api2-v2.qoder.sh"
    }
}

/// 轻量探针用（只读额度，不花积分）
pub fn openapi_base(region: &str) -> &'static str {
    if region == "cn" {
        "https://openapi.qoder.com.cn"
    } else {
        "https://openapi.qoder.sh"
    }
}

pub fn is_qoder_base(base: &str) -> bool {
    let b = base.trim_end_matches('/');
    b.starts_with(model_base("cn")) || b.starts_with(model_base("global"))
}

/// Qoder 只认它自己清单里的模型名。这里收的是 **2026-10-27 实测被 `/model/v1` 接受**的：
///
///   接受：auto、gpt-5、gmodel（GLM-5.3）、dmodel（DeepSeek-V4-Pro）、
///         mmodel（MiniMax-M3）、performance、efficient、ultimate、deepseek-v4-flash
///   拒绝（200 + `invalid_model_error`）：cmodel、dfmodel、qmodel_38max、kmodel_latest
///
/// 客户端缓存里那些 `qmodel_38max` / `dfmodel` 是**显示别名**，API 层不认；以前一律归一成
/// auto，结果所有客户端全挤在 auto 这一条路上，Qoder 一紧张就整片排不上队。
/// 现在把实测能用的具体模型放行，用户选 gmodel 就真的走 GLM-5.3，能分散压力。
pub const QODER_MODELS: &[&str] = &[
    "auto",
    "gpt-5",
    "gmodel",
    "dmodel",
    "mmodel",
    "performance",
    "efficient",
    "ultimate",
    "deepseek-v4-flash",
];

/// 客户端（WorkBuddy / Cherry Studio / 各家 Agent）习惯发自己的模型名——`DeepSeek-Flash`、
/// `claude-sonnet-4-5`、`gpt-4o`……Qoder 不认识，但**仍然回 HTTP 200**，正文是一帧 SSE
/// `event: error / invalid_model_error`。客户端读不到正文字节就直接把这一轮判成"用户已取消"，
/// 界面上看起来像上游挂了，其实只是名字不对。所以在反代这一层先归一到 auto。
pub fn normalize_model(model: &str) -> (String, bool) {
    let m = model.trim();
    if m.is_empty() {
        return ("auto".to_string(), true);
    }
    if QODER_MODELS.iter().any(|x| x.eq_ignore_ascii_case(m)) {
        return (m.to_string(), false);
    }
    ("auto".to_string(), true)
}

/// 从上游响应的开头几百字节里认出"流量内错误帧"，把话说清楚。
/// 认不出就返回 None（照常流式转发）。
pub fn sse_error_message(head: &[u8]) -> Option<String> {
    let t = String::from_utf8_lossy(head);
    if !t.contains("event: error") && !t.contains("\"code\":") {
        return None;
    }
    let body = match t.split_once("data:") {
        Some((_, b)) => b.trim_start(),
        None => t.trim(),
    };
    let line = body.lines().next().unwrap_or("").trim();
    if let Ok(v) = serde_json::from_str::<Value>(line) {
        // 两种帧都见过：`{"code":..,"message":..}` 和 OpenAI 风格的
        // `{"error":{"code":"invalid_parameter_error","type":..,"message":..}}`。
        // 只认前一种的话，后一种会被当成"认不出的行"整段截断丢给界面。
        let e = if v["error"].is_object() { &v["error"] } else { &v };
        let code = e["code"].as_str().unwrap_or("").trim();
        let msg = e["message"].as_str().unwrap_or("").trim();
        let typ = e["type"].as_str().unwrap_or("").trim();
        if !code.is_empty() || !msg.is_empty() {
            let head = if typ.is_empty() {
                code.to_string()
            } else if code.is_empty() {
                typ.to_string()
            } else {
                format!("{}（{}）", code, typ)
            };
            return Some(format!("{} {}", head, msg).trim().to_string());
        }
    }
    if line.is_empty() {
        return Some(t.chars().take(120).collect::<String>().trim().to_string());
    }
    Some(line.chars().take(160).collect())
}

/// 客户端走的是 OpenAI 的 `/v1/chat/completions`（base_url 带不带 /v1 都有），
/// 而 Qoder 的推理路径是固定的 `/model/v1/...`。在这里归一，别指望每个客户端都能改路径。
pub fn upstream_url(base: &str, path: &str) -> String {
    let b = base.trim_end_matches('/');
    if is_qoder_base(b) {
        let (p, q) = match path.find('?') {
            Some(i) => (&path[..i], &path[i..]),
            None => (path, ""),
        };
        let tail = p.strip_prefix("/model").unwrap_or(p);
        let tail = tail.strip_prefix("/v1").unwrap_or(tail);
        return format!("{}/model/v1{}{}", b, tail, q);
    }
    // 通用：上游 base 自己已经带 /v1（本地桥、各家中转都这样）时，别再拼一遍客户端的 /v1
    if b.ends_with("/v1") && (path == "/v1" || path.starts_with("/v1/")) {
        return format!("{}{}", b, &path[3..]);
    }
    format!("{}{}", b, path)
}

pub fn mask(tok: &str) -> String {
    let t = tok.trim();
    if t.is_empty() {
        return String::new();
    }
    if t.chars().count() <= 8 {
        return "…".to_string();
    }
    let head: String = t.chars().take(5).collect();
    let tail: String = t.chars().rev().take(4).collect::<Vec<_>>().into_iter().rev().collect();
    format!("{}…{}", head, tail)
}

// ───────────────────────── 账号档案里的令牌 ─────────────────────────

#[derive(Debug, Clone, Default)]
pub struct Cred {
    pub token: String,
    pub expires_at: String,
    pub user_id: String,
    pub user_name: String,
    pub from: String,
}

/// 解出来的令牌按目录缓存一会儿：一条请求解一次要起一次 powershell（DPAPI），太贵
static CREDS: Mutex<Option<HashMap<String, (Cred, u64)>>> = Mutex::new(None);
const CRED_TTL: u64 = 300;

fn cache_get(dir: &Path) -> Option<Cred> {
    let key = dir.to_string_lossy().to_string();
    let g = CREDS.lock().ok()?;
    let (c, at) = g.as_ref()?.get(&key)?.clone();
    if now_secs().saturating_sub(at) > CRED_TTL {
        return None;
    }
    Some(c)
}

fn cache_put(dir: &Path, c: &Cred) {
    if let Ok(mut g) = CREDS.lock() {
        let m = g.get_or_insert_with(HashMap::new);
        m.insert(dir.to_string_lossy().to_string(), (c.clone(), now_secs()));
    }
}

/// Chromium os_crypt 主密钥：Local State → os_crypt.encrypted_key → 去 DPAPI 前缀 → DPAPI 解
fn master_key(local_state: &Path) -> Result<[u8; 32], String> {
    let txt = std::fs::read_to_string(local_state)
        .map_err(|e| format!("读不到 {}：{}", local_state.display(), e))?;
    let v: Value = serde_json::from_str(&txt).map_err(|e| format!("{} 不是 JSON：{}", local_state.display(), e))?;
    let b64 = v["os_crypt"]["encrypted_key"]
        .as_str()
        .ok_or_else(|| "Local State 里没有 os_crypt.encrypted_key".to_string())?;
    let raw = crate::secrets::b64_decode(b64)?;
    let body = raw
        .strip_prefix(b"DPAPI".as_ref())
        .ok_or_else(|| "主密钥不是 DPAPI 信封（格式可能变了）".to_string())?;
    let plain = crate::secrets::dpapi_open(&crate::secrets::b64_encode(body))?;
    if plain.len() < 32 {
        return Err(format!("主密钥长度不对：{}", plain.len()));
    }
    let mut k = [0u8; 32];
    k.copy_from_slice(&plain[..32]);
    Ok(k)
}

/// 读一个档案目录的登录态。**只读 token**，refreshToken 一概不碰。
pub fn read_auth(dir: &Path) -> Result<Cred, String> {
    if let Some(c) = cache_get(dir) {
        return Ok(c);
    }
    let f = dir.join("auth.v1.dat");
    if !f.is_file() {
        return Err(format!("{} 不存在（这个号还没登录过）", f.display()));
    }
    let blob = std::fs::read(&f).map_err(|e| format!("读不到 {}：{}", f.display(), e))?;
    if blob.len() < 15 + 16 || &blob[..3] != b"v10" {
        return Err("auth.v1.dat 不是 v10 信封（格式可能变了）".into());
    }
    let key = master_key(&dir.join("Local State"))?;
    let plain = gcm_open(&key, &blob[3..15], &blob[15..], &[])
        .ok_or_else(|| "auth.v1.dat 解不开（主密钥和密文不配对）".to_string())?;
    let v: Value = serde_json::from_slice(&plain).map_err(|e| format!("登录态不是 JSON：{}", e))?;
    let token = v["token"].as_str().unwrap_or("").trim().to_string();
    if token.is_empty() {
        return Err("登录态里没有 token".into());
    }
    let c = Cred {
        token,
        expires_at: v["expiresAt"].as_str().unwrap_or("").to_string(),
        user_id: v["user"]["id"].as_str().unwrap_or("").to_string(),
        user_name: v["user"]["name"].as_str().unwrap_or("").to_string(),
        from: f.to_string_lossy().to_string(),
    };
    cache_put(dir, &c);
    Ok(c)
}

pub fn days_left(iso: &str) -> Option<i64> {
    let t = chrono::DateTime::parse_from_rfc3339(iso.trim()).ok()?;
    let secs = t.timestamp() - chrono::Local::now().timestamp();
    Some(secs / 86400)
}

// ───────────────────────── 账号枚举 ─────────────────────────

pub struct Acct {
    pub app: String,
    pub app_name: String,
    pub name: String,
    pub region: &'static str,
    pub data_dir: PathBuf,
    pub cred: Result<Cred, String>,
    /// 档案里的 user_id 和账号记录里的 quota_user_id 对不上 = 这个目录现在登的是别人
    pub mismatch: bool,
}

pub fn app_display(app_id: &str) -> String {
    all_apps()
        .into_iter()
        .find(|a| a.id == app_id)
        .map(|a| a.name)
        .unwrap_or_else(|| app_id.to_string())
}

/// 列所有能参与反代的 Qoder 账号（按实例，不是按产品）
pub fn accounts() -> Vec<Acct> {
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
            let cred = read_auth(&dir);
            let want_uid = m["quota_user_id"].as_str().unwrap_or("").trim().to_string();
            let mismatch = match (&cred, want_uid.is_empty()) {
                (Ok(c), false) => !c.user_id.is_empty() && c.user_id != want_uid,
                _ => false,
            };
            out.push(Acct {
                app: app.to_string(),
                app_name: disp.clone(),
                name,
                region: region_of(app).unwrap_or("global"),
                data_dir: dir,
                cred,
                mismatch,
            });
        }
    }
    out
}

/// 反代真正要用的令牌：拿不到就 None（上游组会把这条成员算作不可用）
pub fn token_for(app_id: &str, name: &str) -> Option<String> {
    let region = region_of(app_id)?;
    // 国内版：网关侧还没开 /model，除非常驻状态已经是 ok，否则不给假上游
    if region == "cn" && stored_state(app_id, name).as_deref() != Some("ok") {
        return None;
    }
    let a = accounts()
        .into_iter()
        .find(|a| a.app == app_id && a.name == name)?;
    if a.mismatch {
        return None;
    }
    a.cred.ok().map(|c| c.token)
}

// ───────────────────────── 状态落盘 ─────────────────────────

pub fn store_path() -> PathBuf {
    base_dir().join("qproxy.json")
}

/// 反代是多线程的，每次请求都会记账；读-改-写必须串起来，否则并发时互相覆盖
static STORE_LOCK: Mutex<()> = Mutex::new(());

fn load_store() -> Value {
    std::fs::read_to_string(store_path())
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .filter(|v| v.is_object())
        .unwrap_or_else(|| json!({ "accounts": {} }))
}

fn save_store(v: &Value) {
    let _ = std::fs::create_dir_all(base_dir());
    let _ = std::fs::write(store_path(), serde_json::to_string_pretty(v).unwrap_or_default());
}

fn key_of(app: &str, name: &str) -> String {
    format!("{}/{}", app, name)
}

fn entry(store: &Value, app: &str, name: &str) -> Value {
    store["accounts"][key_of(app, name)].clone()
}

pub fn stored_state(app: &str, name: &str) -> Option<String> {
    let e = entry(&load_store(), app, name);
    e["state"].as_str().map(|s| s.to_string())
}

fn put_account(app: &str, name: &str, patch: Value) {
    let _g = STORE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let mut st = load_store();
    if !st["accounts"].is_object() {
        st["accounts"] = json!({});
    }
    let k = key_of(app, name);
    let mut cur = st["accounts"][&k].clone();
    if !cur.is_object() {
        cur = json!({});
    }
    if let (Some(dst), Some(src)) = (cur.as_object_mut(), patch.as_object()) {
        for (k2, v) in src {
            dst.insert(k2.clone(), v.clone());
        }
    }
    st["accounts"][&k] = cur;
    st["updated"] = json!(now_secs());
    save_store(&st);
}

// ───────────────────────── 探测 ─────────────────────────

fn http_status(url: &str, token: &str, body: Option<(&str, &str)>) -> (i32, String) {
    use std::io::Read;
    let call = ureq::request(if body.is_some() { "POST" } else { "GET" }, url)
        .timeout(std::time::Duration::from_secs(25))
        .set("Authorization", &format!("Bearer {}", token))
        .set("User-Agent", "qoder-cli/1.1.69")
        .set("Accept", "application/json");
    let res = match body {
        Some((ct, b)) => call.set("Content-Type", ct).send_string(b),
        None => call.call(),
    };
    match res {
        Ok(r) => {
            let code = r.status() as i32;
            let mut s = String::new();
            let _ = r.into_reader().take(4096).read_to_string(&mut s);
            (code, s)
        }
        Err(ureq::Error::Status(code, r)) => {
            let mut s = String::new();
            let _ = r.into_reader().take(2048).read_to_string(&mut s);
            (code as i32, s)
        }
        Err(e) => (-1, e.to_string()),
    }
}

/// 把一次探测的结果翻成状态码。**三维分开**：令牌 / 网关 / 内容，别混成一个"失败"。
///
/// `region` 只影响 5xx 的说法：国内版 `gateway.qoder.com.cn` 的 `/model` 路由确实没开
/// （稳定 503），那是"网关没开这条路由"；国际版同一段 5xx 是上游推理自己挂了，
/// 拿国内版的结论去描述它，界面就会出现「国际版 + 网关没开这条路由」这种自相矛盾。
pub fn classify(auth: i32, infer: i32, body: &str, region: &str) -> &'static str {
    if auth == 401 || auth == 403 {
        return "token_invalid";
    }
    if auth < 0 || infer < 0 {
        return "unreachable";
    }
    if auth != 200 {
        return "auth_unknown";
    }
    match infer {
        200 => {
            // Qoder 的业务错误也会包在 HTTP 200 里（invalid_model_error 等）。
            // 只看状态码会把「模型不支持」误判成探针通过。
            if let Ok(v) = serde_json::from_str::<Value>(body) {
                let bad_code = v.get("code").map(|c| {
                    c.as_str().is_some() || c.as_i64().map(|n| n != 0).unwrap_or(false)
                }).unwrap_or(false);
                let bad_error = v.get("error").map(|e| !e.is_null()).unwrap_or(false);
                if bad_code || bad_error {
                    return "infer_failed";
                }
            }
            "ok"
        }
        401 => "token_invalid",
        403 if body.contains("Signature") => "signature_required",
        403 => "forbidden",
        429 => "rate_limited",
        // 国内版：/model 这条路由在网关侧没开，503 是路由不存在，不是推理坏了
        502 | 503 | 504 if region == "cn" => "gateway_missing",
        // 其余 5xx（含国际版全部 5xx）：上游推理真的挂了，如实说"上游故障"
        500..=599 => "upstream_error",
        _ => "infer_failed",
    }
}

fn state_text(s: &str) -> &'static str {
    match s {
        "ok" => "可推理",
        "token_invalid" => "登录已失效",
        "signature_required" => "要官方客户端签名（纯 HTTP 打不通）",
        "gateway_missing" => "网关没开这条路由",
        "upstream_error" => "上游故障（5xx）",
        "rate_limited" => "被限流",
        "forbidden" => "被拒（账号权限）",
        "infer_failed" => "推理失败",
        "unreachable" => "连不上",
        "auth_unknown" => "令牌验不了",
        _ => "没探过",
    }
}

/// 探一个账号：先验令牌（只读额度，不花积分），再试一次极小的推理（max_tokens=8）。
/// `deep=false` 时跳过国内版那条慢探测（最坏 25 秒）——它的结论本来就能从 /model 的 503 推出来，
/// 一键流程用快档，手动「探测」才走全档。
pub fn probe(app: &str, name: &str, with_infer: bool, deep: bool, model: &str) -> Value {
    // 模型目录里那些名字（qmodel_38max、dfmodel…）是 Qoder 客户端的显示别名，
    // 上游推理接口只认 auto / gpt-5。转发路径（proxy.rs）早就归一到 auto 了，
    // 但探针这条没归——于是「测试」把别名原样发上去，Qoder 回 invalid_model_error，
    // 界面上显示"测试失败"，看着像接入坏了，其实真跑起来是通的。这里补上同一套归一。
    let asked = model.trim().to_string();
    let (model_owned, model_changed) = normalize_model(&asked);
    let model = model_owned.as_str();
    let a = match accounts().into_iter().find(|a| a.app == app && a.name == name) {
        Some(a) => a,
        None => return json!({ "ok": false, "msg": "没找到这个账号" }),
    };
    if a.mismatch {
        return json!({ "ok": false, "msg": format!("{} 这份档案现在登的不是「{}」，先切回本账号", a.data_dir.display(), name) });
    }
    let cred = match a.cred {
        Ok(c) => c,
        Err(e) => {
            put_account(app, name, json!({ "state": "no_token", "detail": e, "checked": now_secs() }));
            return json!({ "ok": false, "msg": e });
        }
    };
    let t0 = std::time::Instant::now();
    let (auth, _) = http_status(
        &format!("{}/api/v2/quota/usage", openapi_base(a.region)),
        &cred.token,
        None,
    );
    let (infer, body) = if with_infer && auth == 200 {
        let b = json!({
            "model": model,
            "messages": [{ "role": "user", "content": "ping" }],
            "stream": false,
            "max_tokens": 8,
        })
        .to_string();
        http_status(
            &format!("{}/model/v1/chat/completions", model_base(a.region)),
            &cred.token,
            Some(("application/json", &b)),
        )
    } else if auth == 200 {
        (0, String::new())
    } else {
        (-2, String::new())
    };
    // 国内版的 /model 路由没开（稳定 503）。真正在用的是 /algo 那条老通道，
    // 再探一次它才能拿到"要签名"这个明确答复。它对限流/排队很敏感，所以只拿它"确认签名"，
    // 没确认也不算反证——结论以 /model 那条稳定路由为主。
    let mut sig_seen = false;
    let (infer, body) = if deep && with_infer && a.region == "cn" && infer != 200 && infer != 0 {
        let b = json!({
            "model": model,
            "messages": [{ "role": "user", "content": "ping" }],
            "stream": true,
            "stream_options": { "include_usage": true },
            "metadata": { "context": { "client_type": "qoder-cli" } },
        })
        .to_string();
        let (c, t) = http_status(
            &format!("{}/algo/api/v2/service/pro/sse/agent_chat_generation?Encode=1", model_base("cn")),
            &cred.token,
            Some(("application/json", &b)),
        );
        sig_seen = t.contains("Signature invalid") || t.contains("\"code\":\"101\"");
        if sig_seen {
            (403, t)
        } else if c == 200 {
            (200, t)
        } else {
            (infer, body)
        }
    } else {
        (infer, body)
    };
    // 没跑推理那一步时，只有令牌结论
    let mut state = if infer == 0 { "token_ok" } else { classify(auth, infer, &body, a.region) };
    let mut detail: String = body.chars().take(160).collect();
    // 国内版的定论：/model 这条路由没开（稳定 503），真正在用的 /algo 通道要官方客户端签名。
    // 两个证据都指向同一件事，所以不管这次 /algo 探针有没有排上队，都如实按"要签名"报。
    if a.region == "cn" && (sig_seen || state == "gateway_missing") {
        state = "signature_required";
        if !sig_seen {
            detail = "国内网关不开 /model（503）；/algo 通道要官方客户端签名（实测 403 Signature invalid）".into();
        }
    }
    let ms = t0.elapsed().as_millis() as u64;
    put_account(
        app,
        name,
        json!({
            "state": state, "detail": detail, "checked": now_secs(),
            "latency_ms": ms, "auth_code": auth, "infer_code": infer,
            "user": cred.user_name, "expires_at": cred.expires_at,
        }),
    );
    let parsed: Option<Value> = serde_json::from_str(&body).ok();
    let model_used = parsed
        .as_ref()
        .and_then(|v| v["model"].as_str().map(|s| s.to_string()))
        .or_else(|| Some(model.to_string()));
    let usage = parsed
        .as_ref()
        .and_then(|v| v.get("usage").cloned())
        .filter(|v| v.is_object());
    json!({
        "ok": state == "ok" || state == "token_ok",
        "state": state, "text": state_text(state),
        "auth_code": auth, "infer_code": infer, "latency_ms": ms, "detail": detail,
        "model_asked": asked, "model_used": model_used,
        "model_note": if model_changed {
            format!("{} → {}（Qoder 上游只认 auto / gpt-5，别名已自动收敛）", asked, model)
        } else {
            String::new()
        },
        "usage": usage,
    })
}

/// 反代转发完记一笔：成功/失败、延迟、最近一次错误。只按 base_url 认区域，不按文案认账号。
pub fn note_by_base(base_url: &str, status: u16, ms: u64, label: &str) {
    let region = if base_url.starts_with(model_base("cn")) {
        "cn"
    } else if base_url.starts_with(model_base("global")) {
        "global"
    } else {
        return;
    };
    // label 形如 "Qoder/nick9994374911"
    let (app_name, name) = match label.split_once('/') {
        Some(x) => x,
        None => return,
    };
    let app = if region == "cn" { "qoder-cn" } else { "qoder" };
    if app_name != app_display(app) && app_name != app {
        return;
    }
    let _g = STORE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let mut st = load_store();
    if !st["accounts"].is_object() {
        st["accounts"] = json!({});
    }
    let k = key_of(app, name);
    let mut cur = st["accounts"][&k].clone();
    if !cur.is_object() {
        cur = json!({});
    }
    let calls = cur["calls"].as_u64().unwrap_or(0) + 1;
    let mut okn = cur["ok"].as_u64().unwrap_or(0);
    let mut failn = cur["fail"].as_u64().unwrap_or(0);
    let good = (200..300).contains(&status);
    if good {
        okn += 1;
    } else {
        failn += 1;
    }
    cur["calls"] = json!(calls);
    cur["ok"] = json!(okn);
    cur["fail"] = json!(failn);
    cur["last_status"] = json!(status);
    cur["last_used"] = json!(now_secs());
    cur["latency_ms"] = json!(ms);
    if good {
        // 网关通了就顺手把状态扶正（探测可能还没跑过）
        cur["state"] = json!("ok");
    } else {
        let now_s = now_secs();
        cur["last_error"] = json!(format!("HTTP {} @ {}", status, chrono::Local::now().format("%m-%d %H:%M:%S")));
        cur["last_error_at"] = json!(now_s);
        if status == 429 {
            cur["state"] = json!("rate_limited");
            cur["cooldown_until"] = json!(now_s + 60);
        } else if status == 401 {
            cur["state"] = json!("token_invalid");
        } else if status >= 500 {
            // 别把两种完全不同的故障说成一句话：
            //   国内版 502/503/504 = 网关侧根本没开 /model 这条路由（稳定现象）
            //   其余 5xx（含国际版全部 5xx）= 上游推理自己挂了，跟"路由没开"无关
            let missing = region == "cn" && matches!(status, 502 | 503 | 504);
            cur["state"] = json!(if missing { "gateway_missing" } else { "upstream_error" });
        }
    }
    st["accounts"][&k] = cur;
    st["updated"] = json!(now_secs());
    save_store(&st);
}

// ───────────────────────── 给界面看的总览 ─────────────────────────

pub fn proxy_port() -> u16 {
    crate::settings::get()["proxy"]["port"].as_u64().unwrap_or(8788) as u16
}

// ───────────────────────── 模型目录（客户端缓存） ─────────────────────────

static QMODEL_CACHE: Mutex<Option<(String, i64, Value, u64)>> = Mutex::new(None);

/// Qoder 的模型目录不在接口里，客户端把它缓存在 `state.vscdb`。
/// 优先用这个账号实际在用的档案目录；读不到再退回产品的默认档案。
fn state_db_for(app_id: &str) -> Option<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(a) = accounts().into_iter().find(|a| a.app == app_id) {
        dirs.push(a.data_dir);
    }
    if let Some(p) = crate::registry::profile_dir(app_id) {
        dirs.push(p);
    }
    // Qoder 的 token 档案（com.qoder*.app.stable）与 Electron userData（Qoder / Qoder CN）
    // 不是同一个目录；state.vscdb 在后者的 User/globalStorage 下。
    if let Ok(appdata) = std::env::var("APPDATA") {
        let base = PathBuf::from(appdata);
        match app_id {
            "qoder" => dirs.push(base.join("Qoder")),
            "qoder-cn" => {
                dirs.push(base.join("Qoder CN"));
                dirs.push(base.join("QoderCN.cockpit-original"));
                dirs.push(base.join("QoderCN"));
            }
            _ => {}
        }
    }
    dirs.into_iter()
        .map(|d| d.join("User").join("globalStorage").join("state.vscdb"))
        .find(|p| p.is_file())
}

fn parse_models(arr: &Value) -> Value {
    let mut out = Vec::new();
    for m in arr.as_array().cloned().unwrap_or_default() {
        let id = m["name"].as_str().unwrap_or("").trim().to_string();
        if id.is_empty() {
            continue;
        }
        let price = m["priceFactor"].as_f64().unwrap_or(0.0);
        out.push(json!({
            "id": id,
            "name": m["displayName"].as_str().unwrap_or(&id),
            "price_factor": price,
            "free": price <= 0.0,
            "reasoning": m["reasoningSupported"].as_bool().unwrap_or(false),
            "context": m["maxInputTokens"].as_u64().unwrap_or(0),
            "default": m["isDefault"].as_bool().unwrap_or(false),
        }));
    }
    Value::Array(out)
}

fn read_models_from_db(path: &Path) -> Result<Value, String> {
    // 项目里已经有 Python（Trae 的模型目录也走 agenthub），这里不再为读一个键引入
    // SQLite 的 Rust 依赖。按文件 mtime 缓存，实际起进程的次数很少。
    use std::process::Command;
    const SCRIPT: &str = r#"
import sqlite3, sys
p = sys.argv[1]
con = sqlite3.connect('file:' + p + '?mode=ro', uri=True)
for k in ('aicoding.modelConfigs.cache.assistant', 'aicoding.modelConfigs.cache.quest'):
    row = con.execute('select value from ItemTable where key=?', (k,)).fetchone()
    if row:
        v = row[0]
        if isinstance(v, bytes):
            v = v.decode('utf-8', 'replace')
        print(v)
        break
else:
    sys.exit(2)
"#;
    let mut c = Command::new("python");
    c.arg("-c")
        .arg(SCRIPT)
        .arg(path)
        .env("PYTHONIOENCODING", "utf-8");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(0x08000000);
    }
    let out = c
        .output()
        .map_err(|e| format!("调不到 python（{e}）：Qoder 模型目录需要本机 Python"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(if err.trim().is_empty() {
            "模型缓存还没有，开一次 Qoder 让它拉模型目录".to_string()
        } else {
            err.trim().chars().take(180).collect()
        });
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let arr: Value =
        serde_json::from_str(text.trim()).map_err(|e| format!("模型缓存不是 JSON：{e}"))?;
    Ok(parse_models(&arr))
}

/// 返回 (模型清单, 缓存文件 mtime 毫秒)。按文件 mtime 缓存，4 秒轮询不会反复开库。
pub fn models_for(app_id: &str) -> Result<(Value, i64), String> {
    let p = state_db_for(app_id)
        .ok_or_else(|| "找不到 Qoder 的 state.vscdb，开一次客户端再试".to_string())?;
    let key = p.display().to_string();
    let mt = std::fs::metadata(&p)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    if let Ok(g) = QMODEL_CACHE.lock() {
        if let Some((k, m, v, at)) = g.as_ref() {
            // state.vscdb 会因为聊天/窗口状态频繁改 mtime，但模型目录很少变。
            // 5 分钟内直接复用，避免每 4 秒起一次 Python。
            if *k == key && (*m == mt || now_secs().saturating_sub(*at) < 300) {
                return Ok((v.clone(), *m));
            }
        }
    }
    let v = read_models_from_db(&p)?;
    if let Ok(mut g) = QMODEL_CACHE.lock() {
        *g = Some((key, mt, v.clone(), now_secs()));
    }
    Ok((v, mt))
}

pub fn models_view() -> Value {
    Value::Array(
        APPS.iter()
            .map(|app| match models_for(app) {
                Ok((models, fetched_at)) => json!({
                    "app": app, "models": models, "fetched_at": fetched_at, "error": null,
                }),
                Err(e) => json!({
                    "app": app, "models": [], "fetched_at": 0, "error": e,
                }),
            })
            .collect::<Vec<_>>(),
    )
}

fn pool_name() -> &'static str {
    "Qoder 上游组"
}
fn dist_name() -> &'static str {
    "Qoder 接入"
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

pub fn view() -> Value {
    let store = load_store();
    let pool = find_pool();
    let dist = find_dist();
    let mut rows = Vec::new();
    for a in accounts() {
        let e = entry(&store, &a.app, &a.name);
        let state = if a.mismatch {
            "mismatch"
        } else {
            match &a.cred {
                Err(_) => "no_token",
                Ok(_) => e["state"].as_str().unwrap_or("unknown"),
            }
        };
        let masked = a.cred.as_ref().map(|c| mask(&c.token)).unwrap_or_default();
        let expires = a
            .cred
            .as_ref()
            .map(|c| c.expires_at.clone())
            .unwrap_or_else(|_| e["expires_at"].as_str().unwrap_or("").to_string());
        let in_pool = pool
            .as_ref()
            .and_then(|p| p["members"].as_array().cloned())
            .map(|ms| {
                let inst = crate::gateway::member_src_instance(&a.app, &a.name);
                let brg = format!("bridge:{}/{}", a.app, a.name);
                ms.iter().any(|m| {
                    let s = m["src"].as_str().unwrap_or("");
                    s == inst || s == brg
                })
            })
            .unwrap_or(false);
        rows.push(json!({
            "app": a.app, "app_name": a.app_name, "name": a.name,
            "region": a.region, "region_label": region_label(a.region),
            "state": state, "text": if a.mismatch { "档案身份不符" } else { state_text(state) },
            "detail": if a.mismatch { format!("{} 现在登的不是这个号", a.data_dir.display()) }
                      else { e["detail"].as_str().unwrap_or("").to_string() },
            "masked": masked, "data_dir": a.data_dir.to_string_lossy(),
            "auth_from": a.cred.as_ref().map(|c| c.from.clone()).unwrap_or_default(),
            "expires_at": expires, "days_left": days_left(&expires),
            "latency_ms": e["latency_ms"].as_u64().unwrap_or(0),
            "checked": e["checked"].as_u64().unwrap_or(0),
            "calls": e["calls"].as_u64().unwrap_or(0),
            "ok": e["ok"].as_u64().unwrap_or(0),
            "fail": e["fail"].as_u64().unwrap_or(0),
            "last_status": e["last_status"].as_u64().unwrap_or(0),
            "last_error": e["last_error"].as_str().unwrap_or(""),
            "in_pool": in_pool,
            // 国内版：CLI 登录 + 本地桥在跑 = 可用（走的是官方运行时那条路）
            "usable": state == "ok"
                || state == "token_ok"
                || (a.region != "cn" && state == "unknown")
                || (a.region == "cn" && cn_logged_in(&a.name) && cn_bridge_running()),
            // 国内版额外的三条路：CLI 登录 → 本地桥 → 挂进池
            "cn": if a.region == "cn" { cn_status(&a.name) } else { Value::Null },
        }));
    }
    let base = format!("http://127.0.0.1:{}/v1", proxy_port());
    json!({
        "accounts": rows,
        "base_url": base,
        "models": models_view(),
        "pool": pool.map(|p| json!({ "id": p["id"], "name": p["name"] })),
        "dist": dist.map(|d| json!({
            "id": d["id"], "name": d["name"],
            "masked": mask(d["key"].as_str().unwrap_or("")),
            "calls": d["calls"].as_u64().unwrap_or(0),
        })),
        "port": proxy_port(),
    })
}

/// 一键：把能用的 Qoder 账号归进上游组 → 生成/复用接入密钥 → 起反代。
/// 幂等：上游组和 Key 按名字复用，不会越点越多。
pub fn one_click(proxy: &std::sync::Arc<crate::proxy::Proxy>, with_infer: bool) -> Value {
    let accts = accounts();
    if accts.is_empty() {
        return json!({ "ok": false, "msg": "没有 Qoder / Qoder CN 账号。先在账号页建一个或用「导入已有数据」" });
    }
    // 先探一遍，拿到的状态直接决定谁能进池。一键走快档（不跑国内版那条慢探测）
    let mut probed = Vec::new();
    for a in &accts {
        if a.mismatch {
            continue;
        }
        if a.cred.is_err() {
            put_account(&a.app, &a.name, json!({ "state": "no_token", "checked": now_secs() }));
            continue;
        }
        let r = probe(&a.app, &a.name, with_infer, false, "auto");
        probed.push((a.app.clone(), a.name.clone(), r));
    }

    let mut members = Vec::new();
    let mut skipped = Vec::new();
    for (app, name, r) in &probed {
        let st = r["state"].as_str().unwrap_or("");
        let region = region_of(app).unwrap_or("global");
        let usable = st == "ok" || (region == "global" && st == "token_ok");
        if usable {
            members.push(json!({ "src": crate::gateway::member_src_instance(app, name), "enabled": true }));
        } else {
            skipped.push(format!(
                "{}/{}：{}",
                app_display(app),
                name,
                r["text"].as_str().unwrap_or(st)
            ));
        }
    }
    // 国内版：CLI 登录过、本地桥在跑，就把桥也挂进池（它走官方运行时，签名由客户端自己做）
    if cn_bridge_running() {
        for a in &accts {
            if a.app == "qoder-cn" && cn_logged_in(&a.name) {
                members.push(json!({
                    "src": format!("bridge:qoder-cn/{}", a.name),
                    "enabled": true,
                }));
            }
        }
    }
    if members.is_empty() {
        return json!({
            "ok": false,
            "msg": if skipped.is_empty() {
                "所有账号都没探通（令牌读不到或档案身份不符）".to_string()
            } else {
                format!("没有一个账号探通：{}", skipped.join("；"))
            },
        });
    }

    // 上游组：**先按名字找，找到就复用它的 id，找不到才建**。
    // 历史教训是"按名复用一旦名字对不上，每轮一键都会新建组+key，堆出一排同名残留"——
    // 病根不是复用，是只按 id 复用；按名字找一次就幂等了。
    let n_members = members.len();
    let existing_pool = crate::gateway::pools()
        .into_iter()
        .find(|p| p["name"].as_str().map(|s| s.trim()) == Some(pool_name()));
    let mut pool_id = existing_pool
        .as_ref()
        .and_then(|p| p["id"].as_str().map(|s| s.to_string()))
        .unwrap_or_default();
    // 成员合并而不是覆盖：本次探通的按最新结果写进去；本次没探通（临时限流、令牌刚好过期）
    // 的账号只要还是这几个 Agent 的，就留在组里不动——一键不该因为一次探针失败把号踢出组。
    let app_ids: Vec<String> = accts.iter().map(|a| a.app.clone()).collect();
    let mut merged: Vec<Value> = existing_pool
        .as_ref()
        .and_then(|p| p["members"].as_array().cloned())
        .unwrap_or_default()
        .into_iter()
        .filter(|m| {
            let src = m["src"].as_str().unwrap_or("");
            !app_ids.iter().any(|id| {
                src.starts_with(&format!("instance:{}/", id))
                    || src.starts_with(&format!("bridge:{}/", id))
            })
        })
        .collect();
    merged.extend(members);
    let (okp, msgp) = crate::gateway::upsert_pool(&json!({
        "id": pool_id,
        "name": pool_name(),
        "enabled": true,
        "members": merged,
    }));
    if !okp {
        return json!({ "ok": false, "msg": format!("上游组没建成：{}", msgp) });
    }
    pool_id = find_pool()
        .and_then(|p| p["id"].as_str().map(|s| s.to_string()))
        .unwrap_or(pool_id);

    // 接入密钥：同样按名字复用；复用时 upsert_dist 只改名字和授权范围，不动 key，
    // 客户端里已经填过的那把继续有效。
    let existing_dist = find_dist().and_then(|d| d["id"].as_str().map(|s| s.to_string()));
    let (okd, msgd) = match existing_dist {
        Some(id) => crate::gateway::upsert_dist(&json!({
            "id": id, "name": dist_name(), "pools": [pool_id],
        })),
        None => crate::gateway::upsert_dist(&json!({
            "name": dist_name(), "pools": [pool_id],
        })),
    };
    if !okd {
        return json!({ "ok": false, "msg": format!("接入密钥没建好：{}", msgd) });
    }
    let (dist_id, key) = find_dist()
        .map(|d| {
            let id = d["id"].as_str().unwrap_or("").to_string();
            let k = crate::gateway::reveal_dist(&id).unwrap_or_default();
            (id, k)
        })
        .unwrap_or_default();

    let port = proxy_port();
    let mut proxy_msg = "反代已在运行".to_string();
    if !proxy.is_running() {
        proxy.set_config(Some("https://api.openai.com"), None);
        let mut cfg = load_config();
        if !cfg["proxy"].is_object() {
            cfg["proxy"] = json!({});
        }
        cfg["proxy"]["port"] = json!(port);
        crate::registry::save_config(&cfg);
        let (o, m) = proxy.start(port);
        if !o {
            return json!({ "ok": false, "msg": format!("反代起不来：{}", m) });
        }
        proxy_msg = m;
    }

    let mut msg = format!(
        "{} 个账号已归入「{}」，接入密钥已就绪；{}。客户端填下面的地址和 Key 就能用。",
        n_members,
        pool_name(),
        proxy_msg
    );
    if !skipped.is_empty() {
        msg.push_str(&format!("。没探通：{}", skipped.join("；")));
    }
    json!({
        "ok": true, "msg": msg,
        "base_url": format!("http://127.0.0.1:{}/v1", port),
        "members": n_members,
        "key": key,
        "pool": { "id": pool_id, "name": pool_name() },
        "dist": { "id": dist_id, "name": dist_name() },
    })
}

pub fn dist_reveal() -> String {
    find_dist()
        .and_then(|d| d["key"].as_str().map(|s| s.to_string()))
        .unwrap_or_default()
}

// ───────────────────────── Qoder CN 本地桥 ─────────────────────────
//
// 国内版没法用 HTTP 直连（要官方客户端的 WASM 签名），所以换一条：**跑它自己的 CLI**。
// Qoder CN 装的那份 qoder-worker-runtime.obf.mjs 就是 qoderclicn 本体，签名它自己做；
// 我们在外面套一个 OpenAI 兼容的小桥（assets/cnbridge.mjs），挂成上游组里的一个上游。
//
// 登录也要它自己做：`login` 是设备码流程，凭据落在 **该账号的配置目录**（<dir>/.auth/user），
// 所以每个账号一份，互不串号——这是驾驶舱「档案是唯一事实源」的延续。

pub const CN_PORT: u16 = 18211;
pub const CN_KEY: &str = "qcn-bridge";

fn safe_name(s: &str) -> String {
    s.chars()
        .map(|c| if "\\/:*?\"<>|".contains(c) { '_' } else { c })
        .collect()
}

/// 每个 CN 账号一份 CLI 配置目录（登录态在它下面）
pub fn cn_config_dir(name: &str) -> PathBuf {
    base_dir().join("qproxy").join("cn").join(safe_name(name))
}

fn cn_bridge_js() -> PathBuf {
    base_dir().join("qproxy").join("cnbridge.mjs")
}

pub fn cn_logged_in(name: &str) -> bool {
    let f = cn_config_dir(name).join(".auth").join("user");
    std::fs::metadata(&f)
        .map(|m| m.len() > 64)
        .unwrap_or(false)
}

/// 找 CN 客户端本体：exe（当 node 用）+ 最新版本里的 worker runtime
pub fn cn_runtime() -> Option<(PathBuf, PathBuf)> {
    let app = all_apps().into_iter().find(|a| a.id == "qoder-cn")?;
    let exe = crate::registry::resolve_exe(&app)?;
    // resolve_exe 会给「最新一版」的 exe：<安装目录>/.qoder-versions/<版本>/Qoder CN.exe
    // 要找的是安装目录本身，所以得往上退两层的那个特例
    let p = exe.parent()?.to_path_buf();
    let base = match p.parent() {
        Some(up) if up.file_name().map(|n| n == ".qoder-versions").unwrap_or(false) => {
            up.parent()?.to_path_buf()
        }
        _ => p,
    };
    // 版本目录按名字排序取最后一个（0.4.10 > 0.4.9）
    let versions = base.join(".qoder-versions");
    let mut cands: Vec<PathBuf> = std::fs::read_dir(&versions)
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    cands.sort_by(|a, b| a.file_name().cmp(&b.file_name()));
    for dir in cands.iter().rev() {
        let rt = dir.join(
            "resources/app.asar.unpacked/node_modules/@qoder-ai/qoder-cn-agent-sdk/dist/_worker/qoder-worker-runtime.obf.mjs",
        );
        if rt.is_file() {
            return Some((exe, rt));
        }
    }
    None
}

pub fn cn_bridge_running() -> bool {
    std::net::TcpStream::connect_timeout(
        &std::net::SocketAddr::from(([127, 0, 0, 1], CN_PORT)),
        std::time::Duration::from_millis(300),
    )
    .is_ok()
}

static CN_PID: Mutex<Option<u32>> = Mutex::new(None);

/// 起桥：把 cnbridge.mjs 落到数据目录，用 CN 客户端自己的 Electron 当 node 跑
pub fn cn_bridge_start(name: &str) -> (bool, String) {
    if cn_bridge_running() {
        return (true, "本地桥已经在跑".into());
    }
    let (node, rt) = match cn_runtime() {
        Some(x) => x,
        None => return (false, "没找到 Qoder CN 的运行时（先装/更新一次客户端）".into()),
    };
    if !cn_logged_in(name) {
        return (false, "这个账号还没给 CLI 登录过，先点「登录」".into());
    }
    let dir = cn_config_dir(name);
    let _ = std::fs::create_dir_all(&dir);
    let js = cn_bridge_js();
    if let Some(p) = js.parent() {
        let _ = std::fs::create_dir_all(p);
    }
    if std::fs::write(&js, include_str!("../assets/cnbridge.mjs")).is_err() {
        return (false, "桥脚本写不下去（数据目录不可写？）".into());
    }
    let mut c = std::process::Command::new(&node);
    c.arg(&js)
        .env("ELECTRON_RUN_AS_NODE", "1")
        .env("QODERCN_CONFIG_DIR", &dir)
        .env("CN_CONFIG_DIR", &dir)
        .env("CN_NODE", &node)
        .env("CN_RUNTIME", &rt)
        .env("CNBRIDGE_PORT", CN_PORT.to_string())
        .env("CNBRIDGE_KEY", CN_KEY)
        .env("CNBRIDGE_PARENT_PID", std::process::id().to_string());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(0x0800_0000);
    }
    match c.spawn() {
        Ok(ch) => {
            *CN_PID.lock().unwrap() = Some(ch.id());
            // 等它就绪（最多 10 秒）
            for _ in 0..40 {
                std::thread::sleep(std::time::Duration::from_millis(250));
                if cn_bridge_running() {
                    return (true, format!("本地桥已起：127.0.0.1:{}", CN_PORT));
                }
            }
            (false, "桥进程起了但端口没通（稍等 2 秒再点一次；若一直不通，多半是安全软件拦了）".into())
        }
        Err(e) => (false, format!("起桥失败：{}", e)),
    }
}

pub fn cn_bridge_stop() -> (bool, String) {
    let pid = CN_PID.lock().ok().and_then(|g| *g);
    match pid {
        Some(p) => {
            #[cfg(windows)]
            {
                let mut c = std::process::Command::new("taskkill");
                c.args(["/PID", &p.to_string(), "/T", "/F"]);
                use std::os::windows::process::CommandExt;
                c.creation_flags(0x0800_0000);
                let _ = c.output();
            }
            *CN_PID.lock().unwrap() = None;
            (true, "本地桥已停".into())
        }
        None => {
            if cn_bridge_running() {
                (false, "端口上还有桥，但不是这个进程起的".into())
            } else {
                (false, "本地桥没在跑".into())
            }
        }
    }
}

/// 让 CLI 登录这个账号。设备码流程：返回要打开的那个 URL；已经登录就直接说已登录。
pub fn cn_login(name: &str) -> Value {
    if cn_logged_in(name) {
        return json!({ "ok": true, "msg": "这个账号已经登录过了", "already": true });
    }
    let (node, rt) = match cn_runtime() {
        Some(x) => x,
        None => return json!({ "ok": false, "msg": "没找到 Qoder CN 的运行时" }),
    };
    let dir = cn_config_dir(name);
    let _ = std::fs::create_dir_all(&dir);
    let log = base_dir().join("qproxy").join(format!("cn-login-{}.log", safe_name(name)));
    let parent = log.parent().map(|p| p.to_path_buf()).unwrap_or_else(|| dir.clone());
    let _ = std::fs::create_dir_all(&parent);
    let f = match std::fs::File::create(&log) {
        Ok(f) => f,
        Err(e) => return json!({ "ok": false, "msg": format!("写不了日志：{}", e) }),
    };
    let f2 = match f.try_clone() {
        Ok(x) => x,
        Err(_) => return json!({ "ok": false, "msg": "日志句柄复制失败" }),
    };
    let mut c = std::process::Command::new(&node);
    c.arg(&rt)
        .arg("login")
        .env("ELECTRON_RUN_AS_NODE", "1")
        .env("QODERCN_CONFIG_DIR", &dir)
        .stdout(std::process::Stdio::from(f))
        .stderr(std::process::Stdio::from(f2));
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(0x0800_0000);
    }
    let child = match c.spawn() {
        Ok(x) => x,
        Err(e) => return json!({ "ok": false, "msg": format!("起登录进程失败：{}", e) }),
    };
    // 等它把 URL 打出来（最多 8 秒）
    let mut url = String::new();
    let mut done = false;
    for _ in 0..32 {
        std::thread::sleep(std::time::Duration::from_millis(250));
        let t = std::fs::read_to_string(&log).unwrap_or_default();
        if t.contains("Login successful") {
            done = true;
            break;
        }
        if url.is_empty() {
            if let Some(line) = t.lines().find(|l| l.trim_start().starts_with("https://")) {
                url = line.trim().to_string();
            }
        }
        if !url.is_empty() {
            break;
        }
    }
    if done || cn_logged_in(name) {
        return json!({ "ok": true, "msg": "登录成功", "already": true });
    }
    if url.is_empty() {
        return json!({
            "ok": false,
            "msg": format!("登录进程没给出授权链接，看日志：{}", log.display())
        });
    }
    json!({
        "ok": true, "url": url,
        "msg": "把下面这个链接在浏览器里打开并确认授权；确认后回来点「登录状态」",
        "pid": child.id(),
    })
}

pub fn cn_status(name: &str) -> Value {
    let (node_ok, rt) = match cn_runtime() {
        Some((a, b)) => (true, b),
        None => (false, PathBuf::new()),
    };
    json!({
        "logged_in": cn_logged_in(name),
        "bridge_running": cn_bridge_running(),
        "port": CN_PORT,
        "runtime_ok": node_ok,
        "runtime": rt.to_string_lossy(),
        "config_dir": cn_config_dir(name).to_string_lossy(),
    })
}

// ───────────────────────── AES-256-GCM（只为解 Chromium v10 信封） ─────────────────────────
//
// 只实现"解密"要用到的部分：AES-256 加密一个块（CTR 的密钥流就是它）+ GHASH。
// 不引外部 crate 是刻意的：这个仓库的依赖树被钉死过（见 Cargo.toml 里 indexmap 那段），
// 为几百字节的解密再加一个加密库，风险大于收益。下面用 NIST 向量锁住正确性。

fn gmul(mut a: u8, mut b: u8) -> u8 {
    let mut p = 0u8;
    for _ in 0..8 {
        if b & 1 != 0 {
            p ^= a;
        }
        let hi = a & 0x80;
        a <<= 1;
        if hi != 0 {
            a ^= 0x1b;
        }
        b >>= 1;
    }
    p
}

fn rotl8(v: u8, n: u32) -> u8 {
    (v << n) | (v >> (8 - n))
}

fn sbox() -> &'static [u8; 256] {
    use std::sync::OnceLock;
    static S: OnceLock<[u8; 256]> = OnceLock::new();
    S.get_or_init(|| {
        let mut t = [0u8; 256];
        for i in 0..256usize {
            // 先求 GF(2^8) 乘法逆元，再做仿射变换——比手抄 256 字节的表可靠
            let a = i as u8;
            let inv = if a == 0 {
                0
            } else {
                let mut r = 1u8;
                for _ in 0..254 {
                    r = gmul(r, a);
                }
                r
            };
            t[i] = inv ^ rotl8(inv, 1) ^ rotl8(inv, 2) ^ rotl8(inv, 3) ^ rotl8(inv, 4) ^ 0x63;
        }
        t
    })
}

fn expand_key(key: &[u8; 32]) -> [u32; 60] {
    let mut w = [0u32; 60];
    for i in 0..8 {
        w[i] = u32::from_be_bytes([key[4 * i], key[4 * i + 1], key[4 * i + 2], key[4 * i + 3]]);
    }
    let s = sbox();
    let mut rcon: u8 = 1;
    for i in 8..60 {
        let mut t = w[i - 1];
        if i % 8 == 0 {
            // RotWord + SubWord + Rcon
            t = t.rotate_left(8);
            let b = t.to_be_bytes();
            t = u32::from_be_bytes([s[b[0] as usize], s[b[1] as usize], s[b[2] as usize], s[b[3] as usize]]);
            t ^= (rcon as u32) << 24;
            rcon = gmul(rcon, 2);
        } else if i % 8 == 4 {
            let b = t.to_be_bytes();
            t = u32::from_be_bytes([s[b[0] as usize], s[b[1] as usize], s[b[2] as usize], s[b[3] as usize]]);
        }
        w[i] = w[i - 8] ^ t;
    }
    w
}

fn add_round_key(st: &mut [u8; 16], w: &[u32; 60], round: usize) {
    for c in 0..4 {
        let k = w[round * 4 + c].to_be_bytes();
        for r in 0..4 {
            st[4 * c + r] ^= k[r];
        }
    }
}

fn shift_rows(st: &mut [u8; 16]) {
    let old = *st;
    for r in 1..4 {
        for c in 0..4 {
            st[4 * c + r] = old[4 * ((c + r) % 4) + r];
        }
    }
}

fn mix_columns(st: &mut [u8; 16]) {
    for c in 0..4 {
        let a = [st[4 * c], st[4 * c + 1], st[4 * c + 2], st[4 * c + 3]];
        st[4 * c] = gmul(a[0], 2) ^ gmul(a[1], 3) ^ a[2] ^ a[3];
        st[4 * c + 1] = a[0] ^ gmul(a[1], 2) ^ gmul(a[2], 3) ^ a[3];
        st[4 * c + 2] = a[0] ^ a[1] ^ gmul(a[2], 2) ^ gmul(a[3], 3);
        st[4 * c + 3] = gmul(a[0], 3) ^ a[1] ^ a[2] ^ gmul(a[3], 2);
    }
}

pub fn aes256_encrypt_block(key: &[u8; 32], block: &[u8; 16]) -> [u8; 16] {
    let w = expand_key(key);
    let s = sbox();
    let mut st = *block;
    add_round_key(&mut st, &w, 0);
    for round in 1..14 {
        for b in st.iter_mut() {
            *b = s[*b as usize];
        }
        shift_rows(&mut st);
        mix_columns(&mut st);
        add_round_key(&mut st, &w, round);
    }
    for b in st.iter_mut() {
        *b = s[*b as usize];
    }
    shift_rows(&mut st);
    add_round_key(&mut st, &w, 14);
    st
}

fn gf128_mul(x: u128, y: u128) -> u128 {
    let mut z: u128 = 0;
    let mut v = y;
    for i in 0..128 {
        if (x >> (127 - i)) & 1 == 1 {
            z ^= v;
        }
        if v & 1 == 1 {
            v = (v >> 1) ^ 0xe100_0000_0000_0000_0000_0000_0000_0000u128;
        } else {
            v >>= 1;
        }
    }
    z
}

fn ghash(h: u128, data: &[u8]) -> u128 {
    let mut y: u128 = 0;
    for chunk in data.chunks(16) {
        let mut blk = [0u8; 16];
        blk[..chunk.len()].copy_from_slice(chunk);
        y ^= u128::from_be_bytes(blk);
        y = gf128_mul(y, h);
    }
    y
}

/// AES-256-GCM 解密；tag 不对返回 None
pub fn gcm_open(key: &[u8; 32], nonce: &[u8], ct_and_tag: &[u8], aad: &[u8]) -> Option<Vec<u8>> {
    if ct_and_tag.len() < 16 || nonce.len() != 12 {
        return None;
    }
    let (ct, tag) = ct_and_tag.split_at(ct_and_tag.len() - 16);
    let h = u128::from_be_bytes(aes256_encrypt_block(key, &[0u8; 16]));
    let mut j0 = [0u8; 16];
    j0[..12].copy_from_slice(nonce);
    j0[15] = 1;
    // 密文
    let mut out = Vec::with_capacity(ct.len());
    let mut ctr = j0;
    let mut i: u32 = 2; // inc32(J0) 之后从 2 开始
    let mut off = 0usize;
    while off < ct.len() {
        ctr[12..16].copy_from_slice(&i.to_be_bytes());
        let ks = aes256_encrypt_block(key, &ctr);
        let n = std::cmp::min(16, ct.len() - off);
        for k in 0..n {
            out.push(ct[off + k] ^ ks[k]);
        }
        off += n;
        i = i.wrapping_add(1);
    }
    // 认证：GHASH(AAD || CT || len(AAD) || len(CT)) ，长度按 bit 计
    let mut buf = Vec::with_capacity(aad.len() + ct.len() + 32);
    buf.extend_from_slice(aad);
    if !aad.is_empty() {
        let pad = (16 - aad.len() % 16) % 16;
        buf.extend(std::iter::repeat(0u8).take(pad));
    }
    buf.extend_from_slice(ct);
    if !ct.is_empty() {
        let pad = (16 - ct.len() % 16) % 16;
        buf.extend(std::iter::repeat(0u8).take(pad));
    }
    buf.extend_from_slice(&((aad.len() as u64) * 8).to_be_bytes());
    buf.extend_from_slice(&((ct.len() as u64) * 8).to_be_bytes());
    let s = ghash(h, &buf);
    let ek = u128::from_be_bytes(aes256_encrypt_block(key, &j0));
    let expect = (ek ^ s).to_be_bytes();
    // 定长比较，别提前 return
    let mut diff = 0u8;
    for k in 0..16 {
        diff |= expect[k] ^ tag[k];
    }
    if diff != 0 {
        return None;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// FIPS-197 / 教科书向量：全零密钥加密全零块
    #[test]
    fn aes256_block_matches_known_vector() {
        let key = [0u8; 32];
        let ct = aes256_encrypt_block(&key, &[0u8; 16]);
        assert_eq!(
            ct,
            [
                0xdc, 0x95, 0xc0, 0x78, 0xa2, 0x40, 0x89, 0x89, 0xad, 0x48, 0xa2, 0x14, 0x92, 0x84,
                0x20, 0x87
            ]
        );
    }

    /// 非零密钥/明文：期望值由 Python(cryptography) 的 AES-256-ECB 算出
    #[test]
    fn aes256_block_matches_second_vector() {
        let mut key = [0u8; 32];
        for i in 0..32 {
            key[i] = i as u8;
        }
        let mut pt = [0u8; 16];
        for i in 0..16 {
            pt[i] = (i * 17) as u8;
        }
        let ct = aes256_encrypt_block(&key, &pt);
        assert_eq!(
            ct,
            [
                0x8e, 0xa2, 0xb7, 0xca, 0x51, 0x67, 0x45, 0xbf, 0xea, 0xfc, 0x49, 0x90, 0x4b, 0x49,
                0x60, 0x89
            ],
            "AES-256 单块输出不对"
        );
    }

    /// NIST GCM 用例 13/14（AES-256，全零密钥空 AAD）
    #[test]
    fn gcm_matches_nist_vectors() {
        let key = [0u8; 32];
        let nonce = [0u8; 12];
        // 空明文
        let empty: Vec<u8> = vec![0x53, 0x0f, 0x8a, 0xfb, 0xc7, 0x45, 0x36, 0xb9, 0xa9, 0x63, 0xb4, 0xf1, 0xc4, 0xcb, 0x73, 0x8b];
        assert_eq!(gcm_open(&key, &nonce, &empty, &[]), Some(Vec::new()));
        // 一个全零块
        let mut blob = vec![
            0xce, 0xa7, 0x40, 0x3d, 0x4d, 0x60, 0x6b, 0x6e, 0x07, 0x4e, 0xc5, 0xd3, 0xba, 0xf3, 0x9d, 0x18,
        ];
        blob.extend_from_slice(&[
            0xd0, 0xd1, 0xc8, 0xa7, 0x99, 0x99, 0x6b, 0xf0, 0x26, 0x5b, 0x98, 0xb5, 0xd4, 0x8a, 0xb9, 0x19,
        ]);
        assert_eq!(gcm_open(&key, &nonce, &blob, &[]), Some(vec![0u8; 16]));
        // 改一个字节必须解不开
        let mut bad = blob.clone();
        bad[0] ^= 1;
        assert!(gcm_open(&key, &nonce, &bad, &[]).is_none());
    }

    #[test]
    fn regions_map_to_the_hosts_we_measured() {
        assert_eq!(region_of("qoder"), Some("global"));
        assert_eq!(region_of("qoder-cn"), Some("cn"));
        assert_eq!(region_of("zcode"), None);
        assert_eq!(model_base("global"), "https://api2-v2.qoder.sh");
        assert_eq!(model_base("cn"), "https://gateway.qoder.com.cn");
        assert_eq!(openapi_base("global"), "https://openapi.qoder.sh");
        assert_eq!(openapi_base("cn"), "https://openapi.qoder.com.cn");
    }

    /// 拒判必须分维度：签名问题、令牌问题、网关问题不能混成一个「失败」
    #[test]
    fn classify_keeps_the_three_dimensions_apart() {
        assert_eq!(classify(200, 200, "", "global"), "ok");
        assert_eq!(classify(200, 403, r#"{"code":"101","message":"Signature invalid"}"#, "cn"), "signature_required");
        assert_eq!(classify(200, 403, "forbidden", "global"), "forbidden");
        assert_eq!(classify(401, -2, "", "global"), "token_invalid");
        assert_eq!(classify(200, 503, "alb", "cn"), "gateway_missing");
        assert_eq!(classify(200, 429, "", "global"), "rate_limited");
        assert_eq!(classify(-1, -1, "", "global"), "unreachable");
        // 业务错误包在 HTTP 200 里时不能算探针通过
        assert_eq!(classify(200, 200, r#"{"code":"invalid_model_error","message":"Unsupported model"}"#, "global"), "infer_failed");
    }

    /// 同一段 5xx，两个区域的结论不一样：国内版是"路由没开"，国际版是"上游挂了"。
    /// 混成一句话，界面就会出现「国际版 + 网关没开这条路由」这种自相矛盾的组合。
    #[test]
    fn classify_splits_5xx_by_region() {
        assert_eq!(classify(200, 503, "", "cn"), "gateway_missing");
        assert_eq!(classify(200, 502, "", "cn"), "gateway_missing");
        assert_eq!(classify(200, 503, "", "global"), "upstream_error");
        assert_eq!(classify(200, 500, "", "global"), "upstream_error");
        assert_eq!(classify(200, 500, "", "cn"), "upstream_error");
        assert_eq!(state_text("upstream_error"), "上游故障（5xx）");
        assert_eq!(state_text("unknown"), "没探过");
    }

    /// Qoder 的拒答帧有两种写法，两种都得说得出人话
    #[test]
    fn sse_error_message_reads_nested_openai_error() {
        let frame = br#"data: {"error":{"code":"invalid_parameter_error","type":"invalid_request_error","message":"When using `tool_choice`, `tools` must be set."}}

"#;
        let msg = sse_error_message(frame).expect("嵌套 error 也要认出来");
        assert!(msg.contains("invalid_parameter_error"), "丢了错误码：{msg}");
        assert!(msg.contains("must be set"), "丢了错误说明：{msg}");
    }

    #[test]
    #[ignore]
    fn real_qoder_model_catalog_is_readable() {
        let v = models_view();
        let q = v.as_array().unwrap().iter().find(|x| x["app"] == "qoder").unwrap();
        let n = q["models"].as_array().map(|a| a.len()).unwrap_or(0);
        println!("qoder models: {n}\n{}", serde_json::to_string_pretty(q).unwrap());
        assert!(n > 0, "Qoder 客户端模型缓存读不到");
    }

    #[test]
    fn qoder_model_cache_parses_ids_and_prices() {
        let out = parse_models(&json!([
            { "name": "auto", "displayName": "Auto", "priceFactor": 1.0,
              "reasoningSupported": false, "maxInputTokens": 180000, "isDefault": false },
            { "name": "dmodel", "displayName": "DeepSeek-V4-Pro", "priceFactor": 0.8,
              "reasoningSupported": true, "maxInputTokens": 1000000, "isDefault": true }
        ]));
        assert_eq!(out[0]["id"], "auto");
        assert_eq!(out[0]["free"], false);
        assert_eq!(out[1]["id"], "dmodel");
        assert_eq!(out[1]["name"], "DeepSeek-V4-Pro");
        assert_eq!(out[1]["default"], true);
    }

    #[test]
    fn client_paths_land_on_the_model_route() {
        let g = model_base("global");
        assert_eq!(upstream_url(g, "/v1/chat/completions"), "https://api2-v2.qoder.sh/model/v1/chat/completions");
        assert_eq!(upstream_url(g, "/chat/completions"), "https://api2-v2.qoder.sh/model/v1/chat/completions");
        assert_eq!(upstream_url(g, "/v1/chat/completions?x=1"), "https://api2-v2.qoder.sh/model/v1/chat/completions?x=1");
        assert_eq!(upstream_url(g, "/model/v1/chat/completions"), "https://api2-v2.qoder.sh/model/v1/chat/completions");
        // 非 Qoder 上游原样拼接，别把别人的路径也改了
        assert_eq!(upstream_url("https://api.openai.com", "/v1/chat/completions"), "https://api.openai.com/v1/chat/completions");
        // 但 base 自带 /v1 的（本地桥 / 中转）不能拼成 /v1/v1/...
        assert_eq!(upstream_url("http://127.0.0.1:18211/v1", "/v1/chat/completions"), "http://127.0.0.1:18211/v1/chat/completions");
        assert_eq!(upstream_url("http://127.0.0.1:18211/v1/", "/v1/models"), "http://127.0.0.1:18211/v1/models");
        assert_eq!(upstream_url("http://127.0.0.1:18211/v1", "/health"), "http://127.0.0.1:18211/v1/health");
    }

    #[test]
    fn client_model_names_are_normalized() {
        // 客户端带来的名字一律收到 auto，只有实测可用的放行
        assert_eq!(normalize_model("DeepSeek-Flash"), ("auto".into(), true));
        assert_eq!(normalize_model("gpt-4o"), ("auto".into(), true));
        assert_eq!(normalize_model("   "), ("auto".into(), true));
        assert_eq!(normalize_model("auto"), ("auto".into(), false));
        assert_eq!(normalize_model("GPT-5"), ("GPT-5".into(), false));
    }

    /// 上游 200 + SSE 错误帧要能被认出来，否则客户端只会显示"用户已取消"
    #[test]
    fn sse_error_frames_are_recognized() {
        let frame = br#"event: error
data: {"code":"invalid_model_error","message":"Unsupported model \"DeepSeek-Flash\"","type":"invalid_model_error"}
"#;
        let msg = sse_error_message(frame).expect("该认出错误帧");
        assert!(msg.contains("invalid_model_error"), "{}", msg);
        assert!(msg.contains("DeepSeek-Flash"), "{}", msg);

        // 正常增量帧不能误判
        let ok = br#"data: {"choices":[{"delta":{"content":""},"index":0}],"object":"chat.completion.chunk"}

"#;
        assert!(sse_error_message(ok).is_none());
    }

    #[test]
    fn mask_never_leaks_the_middle() {
        assert_eq!(mask("dt-w9ozQuFGqtFQsL5aLndw0Acg"), "dt-w9…0Acg");
        assert_eq!(mask(""), "");
        assert_eq!(mask("short"), "…");
    }

    /// 真机端到端（国内版）：起本地桥 → 用 OpenAI 报文打它 → 断言拿到正文。
    /// 前提：这个账号已经做过一次 `login`（`~/.agent-cockpit/qproxy/cn/<账号>/.auth/user`）。
    /// 默认不跑：`cargo test real_cn_bridge -- --include-ignored`
    #[test]
    #[ignore]
    fn real_cn_bridge_serves_a_completion() {
        let name = "nick9994374911";
        assert!(cn_logged_in(name), "这个 CN 账号还没给 CLI 登录过");
        let (ok, msg) = cn_bridge_start(name);
        assert!(ok, "起桥失败：{}", msg);
        assert!(cn_bridge_running(), "桥端口没通");

        let body = json!({
            "model": "DeepSeek-Flash",
            "messages": [{ "role": "user", "content": "只回复两个字：收到" }],
        })
        .to_string();
        let res = ureq::post(&format!("http://127.0.0.1:{}/v1/chat/completions", CN_PORT))
            .timeout(std::time::Duration::from_secs(180))
            .set("Content-Type", "application/json")
            .set("Authorization", &format!("Bearer {}", CN_KEY))
            .send_string(&body);
        let text = match res {
            Ok(x) => {
                let mut s = String::new();
                let _ = std::io::Read::read_to_string(&mut x.into_reader(), &mut s);
                s
            }
            Err(ureq::Error::Status(c, x)) => {
                let mut s = String::new();
                let _ = std::io::Read::read_to_string(&mut x.into_reader(), &mut s);
                panic!("桥返回 {}：{}", c, s);
            }
            Err(e) => panic!("桥请求失败：{}", e),
        };
        assert!(text.contains("收到"), "没拿到正文：{}", text.chars().take(300).collect::<String>());
        let _ = cn_bridge_stop();
    }

    /// 真机端到端（国内版走池子）：桥 → 池成员 `bridge:qoder-cn/<账号>` → 反代 → 客户端。
    /// 这条才证明「国内版能像国际版一样对外发 Key」。
    #[test]
    #[ignore]
    fn real_cn_bridge_through_pool() {
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
        let name = "nick9994374911";
        assert!(cn_logged_in(name), "这个 CN 账号还没给 CLI 登录过");
        assert!(cn_bridge_start(name).0, "起桥失败");

        let cfg = crate::registry::config_file();
        let _guard = CfgGuard(std::fs::read_to_string(&cfg).ok(), cfg);

        let p = std::sync::Arc::new(crate::proxy::Proxy::shared());
        let port = 18998u16;
        assert!(p.start(port).0, "反代起不来");

        let (ok, msg) = crate::gateway::upsert_pool(&json!({
            "name": "__qcn_test_pool",
            "members": [{ "src": format!("bridge:qoder-cn/{}", name) }],
        }));
        assert!(ok, "{}", msg);
        let pid = crate::gateway::pools()
            .into_iter()
            .find(|x| x["name"] == "__qcn_test_pool")
            .and_then(|x| x["id"].as_str().map(|s| s.to_string()))
            .expect("池子没落盘");
        let (ok, msg) = crate::gateway::upsert_dist(&json!({ "name": "__qcn_test_dist", "pools": [pid] }));
        assert!(ok, "{}", msg);
        let key = crate::gateway::dist_keys()
            .into_iter()
            .find(|d| d["name"] == "__qcn_test_dist")
            .and_then(|d| d["key"].as_str().map(|s| s.to_string()))
            .expect("Key 没生成");

        let body = json!({
            "model": "DeepSeek-Flash",
            "messages": [{ "role": "user", "content": "只回复两个字：收到" }],
            "stream": false,
            "max_tokens": 16,
        })
        .to_string();
        let res = ureq::post(&format!("http://127.0.0.1:{}/v1/chat/completions", port))
            .timeout(std::time::Duration::from_secs(180))
            .set("Content-Type", "application/json")
            .set("Authorization", &format!("Bearer {}", key))
            .send_string(&body);
        let text = match res {
            Ok(x) => {
                let mut s = String::new();
                let _ = std::io::Read::read_to_string(&mut x.into_reader(), &mut s);
                s
            }
            Err(ureq::Error::Status(c, x)) => {
                let mut s = String::new();
                let _ = std::io::Read::read_to_string(&mut x.into_reader(), &mut s);
                panic!("反代返回 {}：{}", c, s);
            }
            Err(e) => panic!("反代请求失败：{}", e),
        };
        assert!(text.contains("收到"), "池子那条没拿到正文：{}", text.chars().take(300).collect::<String>());
        p.stop();
        let _ = cn_bridge_stop();
    }

    /// 真实档案往返：本机至少有一个 Qoder 号能解出令牌。
    /// 默认不跑（要在这台机器上、且登录过）：`cargo test -- --include-ignored`
    #[test]
    #[ignore]
    fn real_auth_blob_decrypts_on_this_machine() {
        let mut seen = 0;
        for a in accounts() {
            if let Ok(c) = &a.cred {
                seen += 1;
                assert!(!c.token.is_empty(), "解出来的 token 是空的");
                assert!(!c.user_id.is_empty(), "登录态里该有 user.id");
            }
        }
        assert!(seen > 0, "本机没有可解开的 Qoder 登录态");
    }

    /// 真机端到端：一键反代 → 用分发出的 Key 打一条真实补全 → 断言拿到 choices。
    /// 会临时改写 config.json（结束时原样还原），并真的花掉几个 token。
    /// 默认不跑：`cargo test -- --include-ignored`
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
        let port = 18999u16;
        let (ok, msg) = p.start(port);
        assert!(ok, "{}", msg);

        let r = one_click(&p, true);
        assert!(r["ok"].as_bool().unwrap_or(false), "一键没成功：{}", r);
        let key = r["key"].as_str().unwrap_or("").to_string();
        assert!(key.starts_with("ac-"), "接入密钥 不对：{}", r);

        let body = json!({
            "model": "auto",
            "messages": [{ "role": "user", "content": "只回复两个字：收到" }],
            "stream": false,
            "max_tokens": 16,
        })
        .to_string();
        let res = ureq::post(&format!("http://127.0.0.1:{}/v1/chat/completions", port))
            .timeout(std::time::Duration::from_secs(60))
            .set("Content-Type", "application/json")
            .set("Authorization", &format!("Bearer {}", key))
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
                panic!("反代返回 {}：{}", c, s);
            }
            Err(e) => panic!("反代请求失败：{}", e),
        };
        assert!(text.contains("choices"), "响应里没有 choices：{}", text.chars().take(300).collect::<String>());

        // 客户端拿自己的模型名来（截图里 WorkBuddy 发的是 DeepSeek-Flash）也必须能用：
        // 反代把名字收到 auto，上游才不会回 200 的 SSE 错误帧（客户端会显示"用户已取消"）
        let body2 = json!({
            "model": "DeepSeek-Flash",
            "messages": [{ "role": "user", "content": "只回复两个字：收到" }],
            "stream": false,
            "max_tokens": 16,
        })
        .to_string();
        let res2 = ureq::post(&format!("http://127.0.0.1:{}/v1/chat/completions", port))
            .timeout(std::time::Duration::from_secs(60))
            .set("Content-Type", "application/json")
            .set("Authorization", &format!("Bearer {}", key))
            .send_string(&body2);
        match res2 {
            Ok(x) => {
                let mut s = String::new();
                let _ = std::io::Read::read_to_string(&mut x.into_reader(), &mut s);
                assert!(s.contains("choices"), "外来模型名没被归一：{}", s.chars().take(300).collect::<String>());
            }
            Err(ureq::Error::Status(c, x)) => {
                let mut s = String::new();
                let _ = std::io::Read::read_to_string(&mut x.into_reader(), &mut s);
                panic!("外来模型名那条返回 {}：{}", c, s);
            }
            Err(e) => panic!("外来模型名那条失败：{}", e),
        }
        p.stop();
    }
}
