// WorkBuddy 国内版 / 国际版的免费模型反代：把「登录态文件里的 accessToken」变成 OpenAI 兼容上游。
//
// 两个区域走同一条路径，只差 origin 与归属常量。与 Qoder 国内版不同，这里**没有签名墙**：
// 2026-10-20 本机实测 https://www.workbuddy.cn 与 https://www.workbuddy.ai 的
// `/v2/chat/completions` 都直接 200 真流式，请求体是标准 OpenAI 形状，响应是标准 OpenAI SSE。
//
// 三个反直觉的点，改代码前先读：
//
//   ① `11128` 有两个相反的含义，只能靠 msg 区分：
//      · msg="first message is not system prompt" → **请求形状**，补 system 重发，**不换号不冷却**
//      · msg="request illegal"                    → **渠道校验没过（我们的头没配齐）**，是配置 bug，同样不换号
//      而 `11140` + msg="request illegal" 才是**账号级**拦截，换号是唯一出路（这三条 msg 长得几乎一样）。
//      把 11128 当账号故障去冷却，会白锁整池 —— magpie 为此让全池休息过 5 天。
//
//   ② 国际版**强制 messages[0] 是 system**，缺了就是 400/11128，而 displayMsg 会把它
//      伪装成「请求被拦截 / 没通过渠道校验」，照那个文案去重试是白费。所以幂等注入 system。
//
//   ③ 头族**一律发全 28 个**，不做最小化。国内版 11 个头也能过，但国际版会 400/11128；
//      更重要的是：把头发少只会让流量更像非官方客户端。可持续的前提是不可区分。
//
// 凭据：`$wbEncrypted` 信封（AES-256-GCM + 自定义 AAD），根密钥 atRestSecretKey 硬编码在
// WorkBuddy.exe（TEA-18，RVA 0xA0F5798），对所有安装通用、国内版国际版共用同一把。
// **该值不写在本文件里**，存在驾驶舱保管箱（secrets.rs：DPAPI + 可选主密码），
// 首次由 `import_secret_from_agenthub()` 从本机 agenthub 读一次导入。
// 也**绝不碰 refreshToken**（一次性，用一次就把客户端踢下线）；401 时重读登录态文件，
// 客户端自己会续（本机那份 mtime 当天还在动），续不动就如实报「开一次客户端」。
use crate::registry::{base_dir, load_config, load_instances};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

// ───────────────────────── 区域与端点 ─────────────────────────

/// 只有这两个产品的登录态能当上游。
pub const APPS: [&str; 2] = ["workbuddy", "workbuddy-intl"];

/// 保管箱里那把密钥的 id
pub const SECRET_ID: &str = "wbproxy:at_rest_secret";

pub fn region_of(app_id: &str) -> Option<&'static str> {
    match app_id {
        "workbuddy" => Some("cn"),
        "workbuddy-intl" => Some("global"),
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

/// 推理 origin（拼 `/v2/chat/completions`）。取自 asar 的 CN_SAAS_CORE_API_PRIMARY_ORIGIN
/// 与 intl 的区域三元式，且与本机日志实测一致（CN 1878 次 / intl 7804 次）。
pub fn model_base(region: &str) -> &'static str {
    if region == "cn" {
        "https://www.workbuddy.cn"
    } else {
        "https://www.workbuddy.ai"
    }
}

/// 反代标记：上游组成员的 base_url 用 `workbuddy://<region>` 表示「这是 WorkBuddy 上游」。
/// 带上区域是因为 Upstream 结构里没有 app_id，而转发时要靠区域反推产品。
/// 不能直接用 origin 判断，因为客户端也可能把成员 base_url 指到别处。
pub fn base_mark(region: &str) -> String {
    format!("workbuddy://{region}")
}

pub fn is_wb_base(base: &str) -> bool {
    base.trim().to_ascii_lowercase().starts_with("workbuddy://")
}

pub fn region_from_base(base: &str) -> Option<&'static str> {
    match base.trim().to_ascii_lowercase().as_str() {
        "workbuddy://cn" => Some("cn"),
        "workbuddy://global" => Some("global"),
        _ => None,
    }
}

pub fn app_for_region(region: &str) -> &'static str {
    if region == "cn" {
        "workbuddy"
    } else {
        "workbuddy-intl"
    }
}

/// 单次请求最多换几个号。Qoder-free 的教训：没有这个上限，一个坏请求能把整池账号
/// 挨个打上失败标记，一晚上废掉半个上游组。她两区各一个号，实际值 = min(3, 池内成员数)。
pub const MAX_RETRY_ACCOUNTS: usize = 3;

pub fn app_display(app_id: &str) -> String {
    crate::registry::all_apps()
        .into_iter()
        .find(|a| a.id == app_id)
        .map(|a| a.name)
        .unwrap_or_else(|| app_id.to_string())
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ───────────────────────── SHA-256（手写，零新增依赖） ─────────────────────────
// Cargo 依赖树是钉死的（见 Cargo.toml 的 indexmap 构建修复注释），qproxy 已手写 AES 并钉了
// NIST 向量；这里照同一纪律手写 SHA-256，钉 FIPS 180-4 三条向量。

const SHA256_K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

const SHA256_H0: [u32; 8] = [
    0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
];

pub fn sha256(msg: &[u8]) -> [u8; 32] {
    let mut h = SHA256_H0;
    let bit_len = (msg.len() as u64).wrapping_mul(8);
    let mut data = msg.to_vec();
    data.push(0x80);
    while data.len() % 64 != 56 {
        data.push(0);
    }
    data.extend_from_slice(&bit_len.to_be_bytes());

    for chunk in data.chunks(64) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([chunk[i * 4], chunk[i * 4 + 1], chunk[i * 4 + 2], chunk[i * 4 + 3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16].wrapping_add(s0).wrapping_add(w[i - 7]).wrapping_add(s1);
        }
        let (mut a, mut b, mut c, mut d) = (h[0], h[1], h[2], h[3]);
        let (mut e, mut f, mut g, mut hh) = (h[4], h[5], h[6], h[7]);
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let t1 = hh.wrapping_add(s1).wrapping_add(ch).wrapping_add(SHA256_K[i]).wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
        h[5] = h[5].wrapping_add(f);
        h[6] = h[6].wrapping_add(g);
        h[7] = h[7].wrapping_add(hh);
    }

    let mut out = [0u8; 32];
    for i in 0..8 {
        out[i * 4..i * 4 + 4].copy_from_slice(&h[i].to_be_bytes());
    }
    out
}

pub fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(char::from_digit((b >> 4) as u32, 16).unwrap_or('0'));
        s.push(char::from_digit((b & 0x0f) as u32, 16).unwrap_or('0'));
    }
    s
}

// ───────────────────────── $wbEncrypted 解密 ─────────────────────────

/// 拼 AAD。逆向自 WorkBuddy 的 `buildAuthenticatedContextAad`（sym-v1，field framing）。
/// 结构：`WB-AAD\0` `0x01` lp(tag) lp(scheme) u32be(suite) lp(keyId) framingType 0x00 0x00
fn lp(out: &mut Vec<u8>, s: &[u8]) {
    out.extend_from_slice(&(s.len() as u32).to_be_bytes());
    out.extend_from_slice(s);
}

pub fn aad_field(suite: u32, key_id: &str) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(b"WB-AAD\x00");
    out.push(0x01);
    lp(&mut out, b"WBEV1"); // framing tag：field
    lp(&mut out, b"sym-v1"); // scheme
    out.extend_from_slice(&suite.to_be_bytes());
    lp(&mut out, key_id.as_bytes());
    out.push(0x02); // framing type：field=2
    out.extend_from_slice(&[0x00, 0x00]);
    out
}

/// 解一个 `$wbEncrypted` 字段。字段本来就是明文串时原样返回（客户端关闭加密时的形状）。
pub fn decrypt_field(key: &[u8; 32], field: &Value) -> Result<String, String> {
    if let Some(s) = field.as_str() {
        return Ok(s.to_string());
    }
    let obj = field
        .as_object()
        .ok_or_else(|| "既不是字符串也不是 $wbEncrypted 信封".to_string())?;
    if obj.get("$wbEncrypted").and_then(|v| v.as_u64()) != Some(1) {
        return Err("不是 $wbEncrypted 信封".into());
    }
    let env_b64 = obj
        .get("envelope")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "信封里没有 envelope".to_string())?;
    let raw = crate::secrets::b64_decode(env_b64)?;
    let env: Value = serde_json::from_slice(&raw).map_err(|e| format!("信封不是 JSON：{e}"))?;
    // suite 白名单：只认 1。出现别的值不许猜（cockpit-tools 同款纪律）。
    let suite = env["suite"].as_u64().unwrap_or(0) as u32;
    if suite != 1 {
        return Err(format!("不支持的加密套件 suite={suite}（只认 1）"));
    }
    let key_id = env["keyId"]
        .as_str()
        .ok_or_else(|| "信封里没有 keyId".to_string())?;
    let nonce = crate::secrets::b64_decode(env["nonce"].as_str().unwrap_or(""))?;
    let mut ct = crate::secrets::b64_decode(env["ciphertext"].as_str().unwrap_or(""))?;
    let tag = crate::secrets::b64_decode(env["authTag"].as_str().unwrap_or(""))?;
    ct.extend_from_slice(&tag);
    let aad = aad_field(suite, key_id);
    let plain = crate::qproxy::gcm_open(key, &nonce, &ct, &aad)
        .ok_or_else(|| "GCM 解不开（密钥不对或 AAD 不符）".to_string())?;
    String::from_utf8(plain).map_err(|e| format!("明文不是 UTF-8：{e}"))
}

/// 保管箱里那把 atRestSecretKey（base64 文本）。**取不到就 None，绝不回落硬编码值。**
pub fn at_rest_secret() -> Option<String> {
    let s = crate::secrets::get(SECRET_ID)?;
    let s = s.trim().to_string();
    if s.is_empty() {
        return None;
    }
    Some(s)
}

/// 真正参与解密的 K = SHA256(atRestSecretKey 的 utf8 字节)。
pub fn at_rest_key() -> Option<[u8; 32]> {
    let s = at_rest_secret()?;
    // 顺手做个形状自检：那串是 base64，解出来应当正好 32 字节
    let raw = crate::secrets::b64_decode(&s).ok()?;
    if raw.len() != 32 {
        return None;
    }
    Some(sha256(s.as_bytes()))
}

pub fn secret_fingerprint() -> Option<String> {
    at_rest_secret().map(|s| hex(&sha256(s.as_bytes()))[..8].to_string())
}

/// 首次导入：从本机 agenthub 的源码里读一次常量，存进保管箱。
/// 只在用户点「导入密钥」时调用，不在请求路径上。
/// 从 `WORKBUDDY_AT_REST_SECRET_KEY = "…"` 这一行里取值。
/// ⚠️ 必须 split_once：密钥本身以 '=' 填充结尾，`split('=').nth(1)` 会把填充切掉，
/// 剩下 43 个字符仍能解出 32 字节 —— 形状自检拦不住，只会在解密时表现为 GCM 失败。
fn parse_secret_line(line: &str) -> String {
    let after = line.split_once('=').map(|(_, r)| r).unwrap_or("");
    // 优先按引号取：从第一个引号到匹配的同类引号。引号里的 '=' 与 '#' 都是值的一部分，
    // 行尾的注释（agenthub 里很常见）也就不用手工剥了。
    if let Some(qpos) = after.find(['"', '\'']) {
        let q = after.as_bytes()[qpos] as char;
        let rest = &after[qpos + 1..];
        if let Some(end) = rest.find(q) {
            return rest[..end].trim().to_string();
        }
    }
    after
        .split('#')
        .next()
        .unwrap_or("")
        .trim()
        .trim_end_matches(|c| c == ';' || c == ',')
        .trim()
        .to_string()
}

pub fn import_secret_from_agenthub() -> Result<String, String> {
    let home = std::env::var("USERPROFILE").unwrap_or_default();
    if home.is_empty() {
        return Err("读不到 USERPROFILE，找不到家目录".into());
    }
    let p = PathBuf::from(home).join(".agenthub").join("agenthub.py");
    let txt = std::fs::read_to_string(&p)
        .map_err(|e| format!("读不到 {}：{e}（agenthub 没装？可以改用「手动粘贴」）", p.display()))?;
    let marker = "WORKBUDDY_AT_REST_SECRET_KEY";
    let line = txt
        .lines()
        .find(|l| l.contains(marker) && l.contains('='))
        .ok_or_else(|| format!("{} 里没有 {} 这一行", p.display(), marker))?;
    set_secret(&parse_secret_line(line))
}

/// 手动粘贴 / 导入都走这里。校验形状后落保管箱。
pub fn set_secret(val: &str) -> Result<String, String> {
    let v = val.trim();
    if v.is_empty() {
        return Err("密钥是空串".into());
    }
    let raw = crate::secrets::b64_decode(v).map_err(|e| format!("不是合法 base64：{e}"))?;
    if raw.len() != 32 {
        return Err(format!("解出来是 {} 字节，应该是 32", raw.len()));
    }
    // 严格形状：重新编码必须与输入逐字一致。少了尾部 '=' 的串照样能解出 32 字节
    // （43 个字符 = 32 字节），只会在解密时表现成「GCM 解不开」，非常难查。
    if crate::secrets::b64_encode(&raw) != v {
        return Err("这串 base64 不完整（填充位被截掉了？）——去 agenthub 里整行重抄一次".into());
    }
    if crate::secrets::locked() {
        return Err("保管箱此刻锁着，先解锁再导入".into());
    }
    if !crate::secrets::put(SECRET_ID, v) {
        return Err("保管箱没接住（可能未解锁）".into());
    }
    // 落盘失败必须说出来：只在内存里存着，程序一重启就没了
    crate::secrets::flush()?;
    Ok(format!("已导入，指纹 {}", hex(&sha256(v.as_bytes()))[..8].to_string()))
}

// ───────────────────────── 登录态 ─────────────────────────

#[derive(Debug, Clone, Default)]
pub struct Cred {
    pub token: String,
    pub user_id: String,
    pub user_name: String,
    pub domain: String,
    pub expires_at: i64,
    pub from: String,
}

static CREDS: Mutex<Option<HashMap<String, (Cred, u64)>>> = Mutex::new(None);
const CRED_TTL: u64 = 300;

fn cache_get(app: &str) -> Option<Cred> {
    let g = CREDS.lock().ok()?;
    let (c, at) = g.as_ref()?.get(app)?.clone();
    if now_secs().saturating_sub(at) > CRED_TTL {
        return None;
    }
    Some(c)
}

fn cache_put(app: &str, c: &Cred) {
    if let Ok(mut g) = CREDS.lock() {
        g.get_or_insert_with(HashMap::new).insert(app.to_string(), (c.clone(), now_secs()));
    }
}

pub fn invalidate(app: &str) {
    if let Ok(mut g) = CREDS.lock() {
        if let Some(m) = g.as_mut() {
            m.remove(app);
        }
    }
}

/// 登录态文件路径：走 registry 的 auth_pack_files 登记（带 {LOCALAPPDATA} 占位符）。
/// **不要用 profile_dir** —— 那是档案目录，登录态整机一份在 CodeBuddyExtension 下。
pub fn login_path(app_id: &str) -> Option<PathBuf> {
    crate::registry::auth_pack_files(app_id)
        .first()
        .and_then(|f| crate::registry::auth_pack_path(app_id, f))
}

pub fn read_login(app_id: &str) -> Result<Cred, String> {
    if let Some(c) = cache_get(app_id) {
        return Ok(c);
    }
    let key = at_rest_key()
        .ok_or_else(|| "还没导入 WorkBuddy 解密密钥（先点「导入密钥」或手动粘贴）".to_string())?;
    let p = login_path(app_id).ok_or_else(|| format!("{app_id} 没登记登录态文件"))?;
    if !p.is_file() {
        return Err(format!("{} 不存在（这个产品还没在这台机器登录过）", p.display()));
    }
    let txt = std::fs::read_to_string(&p).map_err(|e| format!("读不到 {}：{e}", p.display()))?;
    let v: Value = serde_json::from_str(&txt).map_err(|e| format!("登录态不是 JSON：{e}"))?;
    let token = decrypt_field(&key, &v["auth"]["accessToken"])
        .map_err(|e| format!("accessToken {e}"))?;
    if token.trim().is_empty() {
        return Err("登录态里 accessToken 解出来是空的".into());
    }
    // 昵称解不开不算错——只影响展示
    let user_name = decrypt_field(&key, &v["account"]["nickname"]).unwrap_or_default();
    let c = Cred {
        token: token.trim().to_string(),
        user_id: v["account"]["uid"].as_str().unwrap_or("").trim().to_string(),
        user_name,
        domain: v["auth"]["domain"].as_str().unwrap_or("").trim().to_string(),
        expires_at: v["auth"]["expiresAt"].as_i64().unwrap_or(0),
        from: p.to_string_lossy().into_owned(),
    };
    cache_put(app_id, &c);
    Ok(c)
}

/// 反代要用的令牌。拿不到 / 身份对不上就 None（上游组把这条成员算作不可用），
/// 绝不拿别人的号顶替。
pub fn token_for(app_id: &str, name: &str) -> Option<String> {
    region_of(app_id)?;
    let cred = read_login(app_id).ok()?;
    if !mismatch_is_ok(app_id, name, &cred) {
        return None;
    }
    Some(cred.token)
}

/// 登录态里的 uid 与这张卡记录的 quota_user_id 对不上 = 这个文件现在装的是别人。
fn mismatch_is_ok(app_id: &str, name: &str, cred: &Cred) -> bool {
    let Some(meta) = load_instances(app_id)
        .into_iter()
        .find(|m| m["name"].as_str().unwrap_or("") == name)
    else {
        return false;
    };
    let want = meta["quota_user_id"].as_str().unwrap_or("").trim();
    if want.is_empty() || cred.user_id.is_empty() {
        return true;
    }
    want == cred.user_id
}

pub struct Acct {
    pub app: String,
    pub app_name: String,
    pub name: String,
    pub region: &'static str,
    pub cred: Result<Cred, String>,
    pub mismatch: bool,
}

pub fn accounts() -> Vec<Acct> {
    let mut out = Vec::new();
    for app in APPS {
        let disp = app_display(app);
        let cred = read_login(app);
        let list = load_instances(app);
        if list.is_empty() {
            // 没建过卡也要能探测：用产品显示名当占位账号名
            out.push(Acct {
                app: app.to_string(),
                app_name: disp,
                name: "（未建卡）".into(),
                region: region_of(app).unwrap_or("global"),
                mismatch: false,
                cred,
            });
            continue;
        }
        for m in list {
            let name = m["name"].as_str().unwrap_or("").to_string();
            if name.is_empty() {
                continue;
            }
            let c2 = match &cred {
                Ok(c) => Ok(c.clone()),
                Err(e) => Err(e.clone()),
            };
            let mismatch = match (&c2, m["quota_user_id"].as_str().unwrap_or("").trim()) {
                (Ok(c), want) if !want.is_empty() && !c.user_id.is_empty() => c.user_id != want,
                _ => false,
            };
            out.push(Acct {
                app: app.to_string(),
                app_name: disp.clone(),
                name,
                region: region_of(app).unwrap_or("global"),
                mismatch,
                cred: c2,
            });
        }
    }
    out
}

// ───────────────────────── 模型目录 ─────────────────────────

#[derive(Debug, Clone, Default)]
pub struct ModelInfo {
    pub id: String,
    pub name: String,
    pub credits: String,
    pub free: bool,
    pub supports_images: bool,
    pub supports_reasoning: bool,
    pub supports_tools: bool,
    pub max_output: u64,
    pub context: u64,
}

#[derive(Debug, Clone, Default)]
pub struct Catalog {
    pub models: Vec<ModelInfo>,
    pub fetched_at: u64,
    pub source: String,
}

pub fn catalog_path(app_id: &str) -> Option<PathBuf> {
    crate::registry::profile_dir(app_id)
        .map(|d| d.join("cache").join("acc-product-config-v3.json"))
}

/// `credits` 形如 `x0.21` / `x0.00` / `0.34 credits` / 空串。
pub fn is_free(credits: &str) -> bool {
    let s = credits.trim();
    if s.is_empty() {
        return false;
    }
    let s = s.trim_end_matches("credits").trim();
    let s = s.strip_prefix('x').or_else(|| s.strip_prefix('X')).unwrap_or(s);
    s.parse::<f64>().map(|v| v == 0.0).unwrap_or(false)
}

/// 读 `/v3/config` 的本机缓存。模型必须**同时**满足两条才算可用：
/// ① 在顶层 `models[]` 里（有倍率信息）；② 在 `agents[cli].models` 白名单里。
/// 实测 `hy3-b`/`hy3-c` 是 0 倍率但不在 cli 白名单里 —— 那两条调不动，别写进清单。
pub fn catalog(app_id: &str) -> Result<Catalog, String> {
    let p = catalog_path(app_id).ok_or_else(|| format!("{app_id} 没有档案目录"))?;
    if !p.is_file() {
        return Err(format!(
            "还没有模型目录缓存 {}（开一次 WorkBuddy 让它拉 /v3/config）",
            p.display()
        ));
    }
    let meta = std::fs::metadata(&p).ok();
    let fetched_at = meta
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let txt = std::fs::read_to_string(&p).map_err(|e| format!("读不到 {}：{e}", p.display()))?;
    let v: Value = serde_json::from_str(&txt).map_err(|e| format!("模型目录不是 JSON：{e}"))?;

    let cli_ids: Vec<String> = v["agents"]
        .as_array()
        .and_then(|a| a.iter().find(|x| x["name"].as_str() == Some("cli")))
        .and_then(|a| a["models"].as_array())
        .map(|arr| arr.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect())
        .unwrap_or_default();

    let mut models = Vec::new();
    if let Some(arr) = v["models"].as_array() {
        for m in arr {
            let id = m["id"].as_str().unwrap_or("").trim().to_string();
            if id.is_empty() {
                continue;
            }
            if !cli_ids.is_empty() && !cli_ids.iter().any(|x| x == &id) {
                continue;
            }
            let credits = m["credits"].as_str().unwrap_or("").to_string();
            models.push(ModelInfo {
                id: id.clone(),
                name: m["name"].as_str().unwrap_or(&id).to_string(),
                free: is_free(&credits),
                credits,
                supports_images: m["supportsImages"].as_bool().unwrap_or(false),
                supports_reasoning: m["supportsReasoning"].as_bool().unwrap_or(false),
                supports_tools: m["supportsToolCall"].as_bool().unwrap_or(false),
                max_output: m["maxOutputTokens"].as_u64().unwrap_or(0),
                context: m["maxInputTokens"].as_u64().unwrap_or(0),
            });
        }
    }
    if models.is_empty() {
        return Err(format!("{} 里没有可用的 cli 模型", p.display()));
    }
    Ok(Catalog {
        models,
        fetched_at,
        source: p.to_string_lossy().into_owned(),
    })
}

pub fn free_models(app_id: &str) -> Vec<ModelInfo> {
    catalog(app_id)
        .map(|c| c.models.into_iter().filter(|m| m.free).collect())
        .unwrap_or_default()
}

/// 全量模型：反代不再分免费/收费，目录里能调的一律给出去。
pub fn all_models(app_id: &str) -> Vec<ModelInfo> {
    catalog(app_id).map(|c| c.models).unwrap_or_default()
}

// ───────────────────────── 模型别名 ─────────────────────────

/// `"@free"` = 落到该区域目录里第一个免费模型。别名默认全指 `@free`，
/// 这样上游改了免费清单，别名不会指向一个已经收费的模型。
pub const ALIAS_FREE: &str = "@free";

const DEFAULT_ALIASES: &[(&str, &str)] = &[
    ("gpt-4o", ALIAS_FREE),
    ("gpt-4o-mini", ALIAS_FREE),
    ("gpt-4.1", ALIAS_FREE),
    ("gpt-5", ALIAS_FREE),
    ("gpt-5-codex", ALIAS_FREE),
    ("o3", ALIAS_FREE),
    ("o4-mini", ALIAS_FREE),
    ("claude-3-5-sonnet", ALIAS_FREE),
    ("claude-3-5-haiku", ALIAS_FREE),
    ("claude-sonnet-4", ALIAS_FREE),
    ("claude-opus-4", ALIAS_FREE),
    ("deepseek-chat", ALIAS_FREE),
    ("deepseek-reasoner", ALIAS_FREE),
    ("qwen-max", ALIAS_FREE),
    ("qwen-plus", ALIAS_FREE),
    ("glm-4", ALIAS_FREE),
    ("kimi-k2", ALIAS_FREE),
];

fn cfg() -> Value {
    load_config().get("wproxy").cloned().unwrap_or_else(|| json!({}))
}

fn flag_s(key: &str, dflt: &str) -> String {
    cfg()[key].as_str().unwrap_or(dflt).to_string()
}

fn flag_b(key: &str, dflt: bool) -> bool {
    cfg()[key].as_bool().unwrap_or(dflt)
}

fn flag_u(key: &str, dflt: u64) -> u64 {
    cfg()[key].as_u64().unwrap_or(dflt)
}

/// 别名表 = 代码里的默认 + config.json 的 `wproxy.aliases` 覆盖。
pub fn aliases() -> Vec<(String, String)> {
    let mut m: Vec<(String, String)> = DEFAULT_ALIASES
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    if let Some(o) = cfg()["aliases"].as_object() {
        for (k, v) in o {
            if let Some(s) = v.as_str() {
                let key = k.trim().to_lowercase();
                if key.is_empty() || s.trim().is_empty() {
                    continue;
                }
                if let Some(slot) = m.iter_mut().find(|(a, _)| a == &key) {
                    slot.1 = s.trim().to_string();
                } else {
                    m.push((key, s.trim().to_string()));
                }
            }
        }
    }
    m
}

pub fn first_free(app_id: &str) -> Option<String> {
    free_models(app_id).into_iter().next().map(|m| m.id)
}

/// 把调用方要的模型名落到真实模型 id。
/// 返回 (真实 id, 说明)。认不出来就报错并**带上可用清单** —— 原样透传必然 400，
/// 而且错误信息对不上，她只会看到「连不上」。
pub fn resolve_model(app_id: &str, asked: &str) -> Result<(String, String), PrepError> {
    let cat = catalog(app_id).map_err(|e| PrepError::NoCredential(e))?;
    let want = asked.trim();
    if want.is_empty() {
        return cat
            .models
            .first()
            .map(|m| (m.id.clone(), "未指定模型，用了目录里的第一个模型".to_string()))
            .ok_or_else(|| PrepError::ModelUnknown {
                asked: "(空)".into(),
                available: vec![],
            });
    }
    if let Some(m) = cat.models.iter().find(|m| m.id.eq_ignore_ascii_case(want)) {
        return Ok((m.id.clone(), String::new()));
    }
    if let Some((_, target)) = aliases()
        .into_iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(want))
    {
        if target == ALIAS_FREE {
            if let Some(id) = cat.models
                .iter()
                .find(|m| m.free)
                .or_else(|| cat.models.first())
                .map(|m| m.id.clone())
            {
                return Ok((id.clone(), format!("{want} → {id}（别名）")));
            }
            return Err(PrepError::ModelUnknown {
                asked: want.to_string(),
                available: vec![],
            });
        }
        if cat.models.iter().any(|m| m.id == target) {
            return Ok((target.clone(), format!("{want} → {target}（别名）")));
        }
    }
    Err(PrepError::ModelUnknown {
        asked: want.to_string(),
        available: cat.models.iter().map(|m| m.id.clone()).collect(),
    })
}

// ───────────────────────── 请求准备 ─────────────────────────

#[derive(Debug, Clone)]
pub enum PrepError {
    /// 账号级：没有可用令牌
    NoCredential(String),
    /// 请求级：模型认不出来（换号无用，要把可用清单回给调用方）
    ModelUnknown { asked: String, available: Vec<String> },
    /// 账号级：护栏挡下
    Guardrail(String),
    /// 账号级竞态兜底：选路之后、发出之前另一个线程把这条打进了冷却
    Cooldown { until: u64 },
    /// 请求级：body 不合法
    MalformedBody(String),
}

impl PrepError {
    /// 这条错误该不该换个账号重试。
    /// **请求级一律不换号** —— 换了也一样错，还会把整池打上失败标记。
    pub fn is_account_level(&self) -> bool {
        matches!(
            self,
            PrepError::NoCredential(_) | PrepError::Guardrail(_) | PrepError::Cooldown { .. }
        )
    }

    pub fn message(&self) -> String {
        match self {
            PrepError::NoCredential(s) => s.clone(),
            PrepError::ModelUnknown { asked, available } => {
                if available.is_empty() {
                    format!("没有可用模型（要的不是 {asked}，而且本区域目录里没有免费模型）")
                } else {
                    format!("模型「{asked}」不在可用清单里。现在能用：{}", available.join("、"))
                }
            }
            PrepError::Guardrail(s) => s.clone(),
            PrepError::Cooldown { until } => {
                format!("这个账号×模型正在冷却，剩 {} 秒", until.saturating_sub(now_secs()))
            }
            PrepError::MalformedBody(s) => s.clone(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Prepared {
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    pub model_used: String,
    pub stream: bool,
    pub note: String,
}

fn caller_hint(body: &Value, headers: &[(String, String)]) -> Option<String> {
    if let Some(u) = body["user"].as_str() {
        if !u.trim().is_empty() {
            return Some(u.trim().to_string());
        }
    }
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("x-conversation-id"))
        .map(|(_, v)| v.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// 稳定派生一个 UUID 形状的串（8-4-4-4-12）。同一 (盐, uid, key) 永远同一个值。
fn derive_uuid(salt: &str, uid: &str, dist_key_id: &str) -> String {
    let h = hex(&sha256(format!("{salt}|{uid}|{dist_key_id}").as_bytes()));
    format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32])
}

fn derive_u32(salt: &str, uid: &str, dist_key_id: &str) -> String {
    let h = hex(&sha256(format!("{salt}|{uid}|{dist_key_id}").as_bytes()));
    h[..32].to_string()
}

/// 调用方不许伪造归属头：这些一律丢掉，由我们重新生成。
fn strip_attribution(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    n == "authorization"
        || n == "x-api-key"
        || n == "user-agent"
        || n == "host"
        || n == "content-length"
        || n == "connection"
        || n == "accept-encoding"
        || n == "accept"
        || n == "content-type"
        || n == "traceparent"
        || n == "b3"
        || n.starts_with("x-ide-")
        || n.starts_with("x-b3-")
        || n == "x-trace-id"
        || n == "x-domain"
        || n == "x-user-id"
        || n == "x-enterprise-id"
        || n == "x-tenant-id"
        || n == "x-product"
        || n == "x-product-code"
        || n == "x-private-data"
        || n == "x-agent-intent"
        || n == "x-agent-purpose"
        || n == "x-requested-with"
        || n == "x-codebuddy-request"
        || n.starts_with("x-conversation-")
        || n == "x-request-id"
        || n == "acp-connection-id"
}

/// 客户端版本：CN 5.8.3 / intl 5.8.4（本机安装实测）。读不到就退一个保守值。
/// exe 路径里有 `{LOCALAPPDATA}` 占位符，必须经 `resolve_exe` 展开，不能自己拼。
fn client_version(app_id: &str) -> String {
    let dflt = if app_id == "workbuddy" { "5.8.3" } else { "5.8.4" };
    crate::registry::app_def(app_id)
        .and_then(|d| crate::registry::resolve_exe(&d))
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .and_then(|dir| {
            std::fs::read_to_string(dir.join("resources").join("app").join("package.json")).ok()
        })
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| v["version"].as_str().map(|s| s.to_string()))
        .unwrap_or_else(|| dflt.to_string())
}

/// 发出去之前把请求改写成 WorkBuddy 要的形状。
///
/// `app_id` 决定区域；`dist_key_id` 参与会话标识的稳定派生（同一把 key 的流量在服务端
/// 看起来是一条连续会话，而不是每秒新建会话的机器人；不同账号之间不可关联）。
pub fn prepare(
    app_id: &str,
    dist_key_id: &str,
    caller_headers: &[(String, String)],
    body: &[u8],
) -> Result<Prepared, PrepError> {
    let region = region_of(app_id).ok_or_else(|| PrepError::NoCredential("不是 WorkBuddy 产品".into()))?;
    let cred = read_login(app_id).map_err(PrepError::NoCredential)?;

    let mut v: Value = serde_json::from_slice(body)
        .map_err(|e| PrepError::MalformedBody(format!("请求体不是 JSON：{e}")))?;
    if !v.is_object() {
        return Err(PrepError::MalformedBody("请求体不是 JSON 对象".into()));
    }
    if !v["messages"].is_array() {
        return Err(PrepError::MalformedBody("缺 messages 数组".into()));
    }

    // 1) 模型落地
    let asked = v["model"].as_str().unwrap_or("").trim().to_string();
    let (model_used, note) = resolve_model(app_id, &asked)?;
    v["model"] = json!(model_used);

    // 2) 幂等注入 system 首条：国际版硬要求，国内版是保险。
    //    判据不依赖「另一个区域碰巧不校验」——那样将来加区域必踩。
    let sys_fallback = flag_s("system_fallback", "You are a helpful assistant.");
    let mut injected_system = false;
    let first_is_system = v["messages"]
        .as_array()
        .and_then(|a| a.first())
        .and_then(|m| m["role"].as_str())
        .map(|r| r == "system")
        .unwrap_or(false);
    if !first_is_system {
        if let Some(arr) = v["messages"].as_array_mut() {
            arr.insert(0, json!({ "role": "system", "content": sys_fallback }));
            injected_system = true;
        }
    }

    // 3) 幂等注入 prompt_cache_key：不带则缓存命中恒 0（实测同一段 8k 前缀费用差 17 倍）。
    //    必须**稳定派生**，随机 UUID 等于白扔。
    let hint = caller_hint(&v, caller_headers).unwrap_or_default();
    let cache_key = hex(&sha256(
        format!("pc|{}|{}|{}", cred.user_id, dist_key_id, hint).as_bytes(),
    ))[..32]
        .to_string();
    if v["prompt_cache_key"].as_str().map(|s| s.trim().is_empty()).unwrap_or(true) {
        v["prompt_cache_key"] = json!(cache_key);
    }

    // 4) stream：上游一律要流式；调用方要非流式就由上层聚合（见 proxy 侧）。
    let stream = v["stream"].as_bool().unwrap_or(false);
    v["stream"] = json!(true);

    let out_body = serde_json::to_vec(&v).map_err(|e| PrepError::MalformedBody(e.to_string()))?;

    // 5) 头族：28 个，逐条照官方客户端
    let ver = client_version(app_id);
    let conv = derive_uuid("conv", &cred.user_id, dist_key_id);
    let acp = derive_uuid("conn", &cred.user_id, dist_key_id);
    let trace = derive_u32("trace", &cred.user_id, dist_key_id);
    let span = &derive_u32("span", &cred.user_id, dist_key_id)[..16];
    let parent = &derive_u32("parent", &cred.user_id, dist_key_id)[..16];

    let mut h: Vec<(String, String)> = Vec::new();
    for (k, val) in caller_headers {
        if strip_attribution(k) {
            continue;
        }
        h.push((k.clone(), val.clone()));
    }
    h.push(("Authorization".into(), format!("Bearer {}", cred.token)));
    h.push(("Accept".into(), "application/json".into()));
    h.push(("Content-Type".into(), "application/json".into()));
    h.push(("Accept-Encoding".into(), "identity".into()));
    h.push(("x-requested-with".into(), "XMLHttpRequest".into()));
    h.push(("x-codebuddy-request".into(), "1".into()));
    h.push(("X-Conversation-ID".into(), conv));
    h.push(("X-Conversation-Request-ID".into(), derive_uuid("req", &cred.user_id, &format!("{dist_key_id}|{}", now_secs()))));
    h.push(("X-Conversation-Message-ID".into(), derive_uuid("msg", &cred.user_id, &format!("{dist_key_id}|{}", std::process::id()))));
    h.push(("X-Request-ID".into(), derive_uuid("rid", &cred.user_id, &format!("{dist_key_id}|{}", now_secs()))));
    h.push(("acp-connection-id".into(), acp));
    h.push(("X-Agent-Intent".into(), "craft".into()));
    h.push(("X-Agent-Purpose".into(), "conversation".into()));
    h.push(("X-IDE-Type".into(), "WorkBuddy".into()));
    h.push(("X-IDE-Name".into(), "WorkBuddy".into()));
    h.push(("X-IDE-Version".into(), ver.clone()));
    h.push(("X-Private-Data".into(), "true".into()));
    h.push(("X-Product".into(), "SaaS".into()));
    if !cred.user_id.is_empty() {
        h.push(("X-User-Id".into(), cred.user_id.clone()));
    }
    // X-Domain 取登录态原值。本机 CN 的 auth.domain 是 www.codebuddy.cn 而端点是
    // www.workbuddy.cn —— 客户端每天就这么发，别「纠正」它。
    if !cred.domain.is_empty() {
        h.push(("X-Domain".into(), cred.domain.clone()));
    }
    h.push(("User-Agent".into(), format!("WorkBuddy/{ver} WorkBuddy/{ver}")));
    h.push(("traceparent".into(), format!("00-{trace}-{span}-01")));
    h.push(("b3".into(), format!("{trace}-{span}-1-{parent}")));
    h.push(("X-B3-TraceId".into(), trace.clone()));
    h.push(("X-B3-SpanId".into(), span.to_string()));
    h.push(("X-B3-ParentSpanId".into(), parent.to_string()));
    h.push(("X-B3-Sampled".into(), "1".into()));
    h.push(("X-Trace-ID".into(), trace));

    let mut bits = vec![format!("{}→{}", if asked.is_empty() { "未指定" } else { &asked }, model_used)];
    if !note.is_empty() {
        bits.push(note);
    }
    if injected_system {
        bits.push("补了 system".into());
    }
    if !stream {
        bits.push("非流式→本地聚合".into());
    }
    let _ = region;

    Ok(Prepared {
        headers: h,
        body: out_body,
        model_used,
        stream,
        note: bits.join("；"),
    })
}

// ───────────────────────── 拒判 ─────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Ok,
    /// 请求形状错 —— 换号无用、绝不冷却
    RequestShape(String),
    /// 账号级策略拦截（11140）—— 换号是唯一出路
    AccountBlocked,
    /// 令牌过期（401）—— 重读登录态后重发一次
    AccountUnauthorized,
    /// 限流（429）
    AccountRateLimited { retry_after: Option<u64> },
    /// 模型容量排队（6020/6021/6022）—— 不换号
    ModelCapacity(u16),
    /// 模型不存在 / 不支持
    ModelUnavailable,
    /// 上游网关故障
    UpstreamGateway,
    /// 对不上号的一律 unknown：不许当 empty，更不许当账号故障
    Unknown(u16),
}

impl Outcome {
    pub fn name(&self) -> &'static str {
        match self {
            Outcome::Ok => "ok",
            Outcome::RequestShape(_) => "request_shape",
            Outcome::AccountBlocked => "account_blocked",
            Outcome::AccountUnauthorized => "account_unauthorized",
            Outcome::AccountRateLimited { .. } => "account_rate_limited",
            Outcome::ModelCapacity(_) => "model_capacity",
            Outcome::ModelUnavailable => "model_unavailable",
            Outcome::UpstreamGateway => "upstream_gateway",
            Outcome::Unknown(_) => "unknown",
        }
    }

    /// 给人看的一句话。**不许把账号级与请求级说成同一件事** ——
    /// 请求形状错让她去查配置，账号级错让她去换号，两者混了会让人白折腾。
    pub fn message(&self) -> String {
        match self {
            Outcome::Ok => "成功".into(),
            Outcome::RequestShape(why) => format!(
                "请求形状不对（{why}）——换号没用，这是配置/报头问题，不是账号问题"
            ),
            Outcome::AccountBlocked => "这个账号被上游安全策略拦了（11140），换另一个号".into(),
            Outcome::AccountUnauthorized => {
                "令牌失效了：开一次 WorkBuddy 客户端让它自己续，再回来点刷新".into()
            }
            Outcome::AccountRateLimited { .. } => "上游限流，稍后重试".into(),
            Outcome::ModelCapacity(c) => match c {
                6020 => "模型在排队（6020），稍等".into(),
                6021 => "模型满载（6021），稍等".into(),
                _ => "这个模型今天用满了（6022），换一个模型".into(),
            },
            Outcome::ModelUnavailable => "这个模型不可用".into(),
            Outcome::UpstreamGateway => "上游网关故障，稍后重试".into(),
            Outcome::Unknown(c) => format!("上游回了没见过的状态（{c}），没动账号状态"),
        }
    }

    /// 该不该给这个 (账号, 模型) 上冷却。**RequestShape / ModelCapacity 一律不上。**
    pub fn should_cool(&self) -> bool {
        matches!(
            self,
            Outcome::AccountBlocked
                | Outcome::AccountUnauthorized
                | Outcome::AccountRateLimited { .. }
                | Outcome::ModelUnavailable
                | Outcome::UpstreamGateway
        )
    }

    /// 该不该换下一个账号。
    pub fn should_rotate(&self) -> bool {
        matches!(
            self,
            Outcome::AccountBlocked
                | Outcome::AccountUnauthorized
                | Outcome::AccountRateLimited { .. }
                | Outcome::UpstreamGateway
        )
    }

    pub fn cooldown_secs(&self) -> u64 {
        match self {
            Outcome::AccountRateLimited { retry_after } => retry_after.unwrap_or(60).max(10),
            Outcome::AccountBlocked => 30 * 60,
            Outcome::AccountUnauthorized => 5 * 60,
            Outcome::ModelUnavailable => 12 * 3600,
            Outcome::UpstreamGateway => 60,
            _ => 0,
        }
    }
}

fn biz_code(body: &str) -> Option<u16> {
    let v: Value = serde_json::from_str(body).ok()?;
    v["code"].as_u64().map(|c| c as u16)
}

fn biz_msg(body: &str) -> String {
    serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| v["msg"].as_str().map(|s| s.to_string()))
        .unwrap_or_default()
}

fn retry_after(headers: &[(String, String)]) -> Option<u64> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("retry-after"))
        .and_then(|(_, v)| v.trim().parse::<u64>().ok())
}

/// 三维拒判。判据是 **(code, msg) 配对**，不是只看 code ——
/// `11128 request illegal`（请求形状，换号无用）与 `11140 request illegal`（账号级，换号是唯一出路）
/// 的 msg 一模一样。
pub fn classify(status: u16, body_peek: &str, resp_headers: &[(String, String)]) -> Outcome {
    let code = biz_code(body_peek);
    let msg = biz_msg(body_peek);
    let msg_l = msg.to_ascii_lowercase();

    if status == 200 && code.is_none() {
        return Outcome::Ok;
    }
    // 业务码优先（有些错误回 HTTP 200 + code，见 Trae 的教训）
    if let Some(c) = code {
        match c {
            // 11128：请求形状。两个 msg 都要能区分开，但两者都**不换号**
            11128 => {
                if msg_l.contains("system") {
                    return Outcome::RequestShape("首条消息不是 system".into());
                }
                if msg_l.contains("request illegal") || msg_l.is_empty() {
                    return Outcome::RequestShape("渠道校验没过（头族没配齐）".into());
                }
                return Outcome::RequestShape(format!("11128：{msg}"));
            }
            // 11140：账号级安全策略拦截
            11140 => return Outcome::AccountBlocked,
            // 容量排队
            6020 | 6021 | 6022 => return Outcome::ModelCapacity(c),
            _ => {}
        }
    }
    match status {
        200..=299 => Outcome::Ok,
        401 => Outcome::AccountUnauthorized,
        403 => {
            // 403 但没有 11140：多半还是渠道/形状问题，按请求形状处理更安全
            Outcome::RequestShape(format!("HTTP 403：{}", if msg.is_empty() { body_peek.chars().take(120).collect() } else { msg }))
        }
        404 => Outcome::ModelUnavailable,
        408 | 502 | 503 | 504 => Outcome::UpstreamGateway,
        429 => Outcome::AccountRateLimited {
            retry_after: retry_after(resp_headers),
        },
        _ => Outcome::Unknown(status),
    }
}

// ───────────────────────── 状态落盘 ─────────────────────────

pub fn store_path() -> PathBuf {
    base_dir().join("wbproxy.json")
}

static STORE_LOCK: Mutex<()> = Mutex::new(());

fn load_store() -> Value {
    std::fs::read_to_string(store_path())
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .filter(|v| v.is_object())
        .unwrap_or_else(|| json!({ "accounts": {}, "cooldowns": {}, "days": {} }))
}

fn save_store(v: &Value) {
    let _ = std::fs::create_dir_all(base_dir());
    let tmp = store_path().with_extension("json.tmp");
    if std::fs::write(&tmp, serde_json::to_string_pretty(v).unwrap_or_default()).is_ok() {
        let _ = std::fs::rename(&tmp, store_path());
    }
}

pub fn acct_key(app: &str, name: &str) -> String {
    format!("{}/{}", app, name)
}

fn cool_key(acct: &str, model: &str) -> String {
    format!("{}|{}", acct, model)
}

pub fn cooldown_until(acct: &str, model: &str) -> u64 {
    load_store()["cooldowns"][cool_key(acct, model)]
        .as_u64()
        .unwrap_or(0)
}

pub fn is_cooling(acct: &str, model: &str) -> bool {
    cooldown_until(acct, model) > now_secs()
}

/// 冷却**只许变长不许变短**（OFM 的硬规则）：并发失败时短的别覆盖长的。
pub fn set_cooldown(acct: &str, model: &str, secs: u64) {
    if secs == 0 {
        return;
    }
    let until = now_secs() + secs;
    let _g = STORE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let mut st = load_store();
    if !st["cooldowns"].is_object() {
        st["cooldowns"] = json!({});
    }
    let k = cool_key(acct, model);
    let cur = st["cooldowns"][&k].as_u64().unwrap_or(0);
    if until > cur {
        st["cooldowns"][&k] = json!(until);
        save_store(&st);
    }
}

fn today() -> String {
    chrono::Local::now().format("%Y-%m-%d").to_string()
}

pub struct DayStat {
    pub credit: f64,
    pub tokens: u64,
    pub calls: u64,
}

pub fn day_stat(app: &str, name: &str) -> DayStat {
    let st = load_store();
    let d = &st["days"][today()][acct_key(app, name)];
    DayStat {
        credit: d["credit"].as_f64().unwrap_or(0.0),
        tokens: d["tokens"].as_u64().unwrap_or(0),
        calls: d["calls"].as_u64().unwrap_or(0),
    }
}

fn bump_day(app: &str, name: &str, credit: f64, tokens: u64) {
    let _g = STORE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let mut st = load_store();
    let day = today();
    let k = acct_key(app, name);
    if !st["days"].is_object() {
        st["days"] = json!({});
    }
    // 只留最近 8 天，别让账本无限长
    if let Some(o) = st["days"].as_object() {
        let mut keys: Vec<String> = o.keys().cloned().collect();
        keys.sort();
        while keys.len() > 8 {
            let drop = keys.remove(0);
            if let Some(m) = st["days"].as_object_mut() {
                m.remove(&drop);
            }
        }
    }
    let cur = &st["days"][&day][&k];
    let c = cur["credit"].as_f64().unwrap_or(0.0) + credit;
    let t = cur["tokens"].as_u64().unwrap_or(0) + tokens;
    let n = cur["calls"].as_u64().unwrap_or(0) + 1;
    st["days"][&day][&k] = json!({ "credit": c, "tokens": t, "calls": n });
    save_store(&st);
}

/// 四条护栏（wb2api-hub）。机制默认启用，只有「保留积分下限」默认给非零值。
pub fn guardrail_check(app: &str, name: &str) -> Result<(), PrepError> {
    let st = day_stat(app, name);
    let daily_credit = flag_u("daily_credit_cap", 0);
    if daily_credit > 0 && st.credit >= daily_credit as f64 {
        return Err(PrepError::Guardrail(format!(
            "今日积分已用 {:.2}，到上限 {daily_credit}",
            st.credit
        )));
    }
    let daily_tokens = flag_u("daily_token_cap", 0);
    if daily_tokens > 0 && st.tokens >= daily_tokens {
        return Err(PrepError::Guardrail(format!(
            "今日 token 已用 {}，到上限 {daily_tokens}",
            st.tokens
        )));
    }
    Ok(())
}

pub fn guardrail_reserve() -> f64 {
    flag_u("reserve_credit", 200) as f64
}

// ───────────────────────── 记账 ─────────────────────────

/// 转发之后记一笔。`usage` 来自末 chunk（含厂商第一方的 `credit`）。
pub fn note(
    app: &str,
    name: &str,
    model: &str,
    outcome: &Outcome,
    status: u16,
    ms: u64,
    usage: Option<&Value>,
) {
    let credit = usage.and_then(|u| u["credit"].as_f64()).unwrap_or(0.0);
    let tokens = usage
        .map(|u| {
            u["prompt_tokens"].as_u64().unwrap_or(0) + u["completion_tokens"].as_u64().unwrap_or(0)
        })
        .unwrap_or(0);
    bump_day(app, name, credit, tokens);
    if outcome.should_cool() {
        set_cooldown(&acct_key(app, name), model, outcome.cooldown_secs());
    }
    let _g = STORE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let mut st = load_store();
    let k = acct_key(app, name);
    if !st["accounts"].is_object() {
        st["accounts"] = json!({});
    }
    let cur = st["accounts"][&k].clone();
    let calls = cur["calls"].as_u64().unwrap_or(0) + 1;
    let ok = matches!(outcome, Outcome::Ok);
    let okn = cur["ok"].as_u64().unwrap_or(0) + if ok { 1 } else { 0 };
    let failn = cur["fail"].as_u64().unwrap_or(0) + if ok { 0 } else { 1 };
    st["accounts"][&k] = json!({
        "state": if ok { "ok" } else { "fail" },
        "calls": calls,
        "ok": okn,
        "fail": failn,
        "last_status": status,
        "last_error": if ok { Value::Null } else { json!(outcome.name()) },
        "last_error_at": if ok { Value::Null } else { json!(now_secs()) },
        "last_used": now_secs(),
        "last_ms": ms,
    });
    save_store(&st);
}

// ───────────────────────── 逐请求日志（JSONL） ─────────────────────────

pub fn log_dir() -> PathBuf {
    base_dir().join("logs")
}

fn log_path() -> PathBuf {
    log_dir().join(format!("proxy-{}.jsonl", chrono::Local::now().format("%Y%m%d")))
}

/// 追加一行。**绝不记 body / messages / 令牌明文**；只记模型名与用量。
pub fn append_log(line: Value) {
    if std::fs::create_dir_all(log_dir()).is_err() {
        return;
    }
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path())
    {
        use std::io::Write;
        let _ = writeln!(f, "{}", serde_json::to_string(&line).unwrap_or_default());
    }
}

/// 启动时清掉 30 天前的日志。失败不阻塞启动。
pub fn prune_logs() {
    let Ok(rd) = std::fs::read_dir(log_dir()) else {
        return;
    };
    let cutoff = now_secs().saturating_sub(30 * 86400);
    for e in rd.flatten() {
        let p = e.path();
        let is_ours = p
            .file_name()
            .and_then(|n| n.to_str())
            .map(|n| n.starts_with("proxy-") && n.ends_with(".jsonl"))
            .unwrap_or(false);
        if !is_ours {
            continue;
        }
        let old = e
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() < cutoff)
            .unwrap_or(false);
        if old {
            let _ = std::fs::remove_file(&p);
        }
    }
}

/// 最近 n 行（给界面看）
pub fn recent_logs(n: usize) -> Vec<Value> {
    let Ok(txt) = std::fs::read_to_string(log_path()) else {
        return Vec::new();
    };
    let mut out: Vec<Value> = txt
        .lines()
        .rev()
        .take(n)
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .collect();
    out.reverse();
    out
}

// ───────────────────────── 探活 ─────────────────────────

/// 免费探针：只打额度接口，不花积分、不占模型额度。
pub fn probe(app_id: &str, name: &str) -> Value {
    let t0 = std::time::Instant::now();
    let cred = match read_login(app_id) {
        Ok(c) => c,
        Err(e) => {
            return json!({ "ok": false, "state": "no_credential", "msg": e, "app": app_id, "name": name });
        }
    };
    if !mismatch_is_ok(app_id, name, &cred) {
        return json!({
            "ok": false, "state": "mismatch",
            "msg": "登录态里的账号和这张卡记录的对不上（这台机器现在登的是另一个号）",
            "app": app_id, "name": name,
        });
    }
    let region = region_of(app_id).unwrap_or("cn");
    let url = format!("{}/billing/meter/get-user-resource-summary", model_base(region));
    let res = ureq::post(&url)
        .timeout(std::time::Duration::from_secs(20))
        .set("Content-Type", "application/json")
        .set("Accept", "application/json")
        .set("Authorization", &format!("Bearer {}", cred.token))
        .set("X-User-Id", &cred.user_id)
        // 国内版不带 UA 会 403「请求不合法」
        .set("User-Agent", "WorkBuddy/1.0")
        .send_string("{}");
    let ms = t0.elapsed().as_millis() as u64;
    match res {
        Ok(resp) => {
            let status = resp.status() as u16;
            let txt = resp.into_string().unwrap_or_default();
            let code = biz_code(&txt).unwrap_or(0);
            let ok = status == 200 && code == 0;
            let outcome = classify(status, &txt, &[]);
            note(app_id, name, "(probe)", &outcome, status, ms, None);
            json!({
                "ok": ok, "state": outcome.name(), "status": status, "ms": ms,
                "msg": if ok { "探活通过".to_string() } else { biz_msg(&txt) },
                "app": app_id, "name": name,
            })
        }
        Err(ureq::Error::Status(st, resp)) => {
            // into_reader() 给的是 trait 对象，`take` 要 Sized，这里直接定长读一段
            let mut rd = resp.into_reader();
            let mut buf = vec![0u8; 2000];
            let n = std::io::Read::read(&mut rd, &mut buf).unwrap_or(0);
            let txt = String::from_utf8_lossy(&buf[..n]).to_string();
            let outcome = classify(st, &txt, &[]);
            note(app_id, name, "(probe)", &outcome, st, ms, None);
            json!({
                "ok": false, "state": outcome.name(), "status": st, "ms": ms,
                "msg": biz_msg(&txt), "app": app_id, "name": name,
            })
        }
        Err(e) => {
            let outcome = Outcome::UpstreamGateway;
            note(app_id, name, "(probe)", &outcome, 0, ms, None);
            json!({ "ok": false, "state": outcome.name(), "status": 0, "ms": ms, "msg": e.to_string(), "app": app_id, "name": name })
        }
    }
}

// ───────────────────────── 视图 ─────────────────────────

pub fn dist_name() -> &'static str {
    "WorkBuddy 接入"
}

pub fn pool_name() -> &'static str {
    "WorkBuddy 上游组"
}

/// 用指定模型发一次极小补全，验证账号、模型名和整条上游链路。
pub fn test(app_id: &str, name: &str, model: &str) -> Value {
    use std::io::{Cursor, Read};
    let t0 = std::time::Instant::now();
    let accts = accounts();
    let acct = accts
        .iter()
        .find(|a| a.app == app_id && (name.is_empty() || a.name == name));
    let Some(acct) = acct else {
        return json!({ "ok": false, "error": "没找到这个 WorkBuddy 账号" });
    };
    if acct.mismatch {
        return json!({ "ok": false, "error": format!("账号「{}」的档案身份对不上，先切回本账号", acct.name) });
    }
    if let Err(e) = guardrail_check(app_id, &acct.name) {
        return json!({ "ok": false, "error": e.message() });
    }
    let body = json!({
        "model": model,
        "messages": [{ "role": "user", "content": "ping" }],
        "stream": true,
        "max_tokens": 8,
    })
    .to_string();
    let prepared = match prepare(app_id, "wproxy-test", &[], body.as_bytes()) {
        Ok(p) => p,
        Err(e) => return json!({ "ok": false, "error": e.message(), "model_asked": model }),
    };
    let region = region_of(app_id).unwrap_or("global");
    let url = format!("{}/v2/chat/completions", model_base(region));
    let mut call = ureq::post(&url).timeout(std::time::Duration::from_secs(60));
    for (k, v) in &prepared.headers {
        call = call.set(k, v);
    }
    let res = call.send(&prepared.body[..]);
    match res {
        Ok(resp) => {
            let status = resp.status() as u16;
            let mut raw = String::new();
            if let Err(e) = resp.into_reader().read_to_string(&mut raw) {
                return json!({ "ok": false, "status": status, "error": e.to_string(), "model_asked": model });
            }
            let mut rd = Cursor::new(raw.as_bytes());
            match collect_completion(&mut rd, &prepared.model_used) {
                Ok((completion, usage)) => {
                    let ms = t0.elapsed().as_millis() as u64;
                    let content = completion["choices"][0]["message"]["content"]
                        .as_str()
                        .unwrap_or("")
                        .to_string();
                    let text = if content.is_empty() && raw.contains("\"code\"") {
                        raw.chars().take(300).collect::<String>()
                    } else {
                        content
                    };
                    let ok = !(text.is_empty() && raw.contains("\"code\""));
                    if ok {
                        note(app_id, &acct.name, &prepared.model_used, &Outcome::Ok, status, ms, usage.as_ref());
                        append_log(json!({
                            "ts": now_secs(), "app": app_id, "region": region,
                            "account": acct.name, "model_asked": model,
                            "model_used": prepared.model_used, "status": status,
                            "outcome": "ok", "ms": ms, "usage": usage,
                            "stream": true, "note": prepared.note, "via": "模型测试",
                        }));
                    }
                    json!({
                        "ok": ok, "status": status, "ms": ms,
                        "model_asked": model, "model_used": prepared.model_used,
                        "usage": usage, "content": text, "note": prepared.note,
                    })
                }
                Err(e) => json!({ "ok": false, "status": status, "error": e, "model_asked": model }),
            }
        }
        Err(ureq::Error::Status(code, resp)) => {
            let mut txt = String::new();
            let _ = resp.into_reader().read_to_string(&mut txt);
            let outcome = classify(code as u16, &txt, &[]);
            json!({
                "ok": false, "status": code, "error": outcome.message(),
                "detail": txt.chars().take(300).collect::<String>(),
                "model_asked": model,
            })
        }
        Err(e) => json!({ "ok": false, "error": e.to_string(), "model_asked": model }),
    }
}

fn model_json(m: &ModelInfo) -> Value {
    json!({
        "id": m.id, "name": m.name, "credits": m.credits, "free": m.free,
        "context": m.context, "max_output": m.max_output,
        "tools": m.supports_tools, "images": m.supports_images,
        "reasoning": m.supports_reasoning,
    })
}

pub fn view() -> Value {
    let st = load_store();
    let pool = crate::gateway::pools()
        .into_iter()
        .find(|p| p["name"].as_str() == Some(pool_name()));
    let mut accts = Vec::new();
    for a in accounts() {
        let (state, detail, masked, expires_days, user) = match &a.cred {
            Ok(c) => {
                let days = if c.expires_at > 0 {
                    Some((c.expires_at / 1000 - now_secs() as i64) / 86400)
                } else {
                    None
                };
                let s = if a.mismatch { "mismatch" } else { "ok" };
                (
                    s.to_string(),
                    if a.mismatch {
                        "登录态里的账号与这张卡对不上".to_string()
                    } else {
                        String::new()
                    },
                    crate::qproxy::mask(&c.token),
                    days,
                    c.user_name.clone(),
                )
            }
            Err(e) => ("no_credential".to_string(), e.clone(), String::new(), None, String::new()),
        };
        let k = acct_key(&a.app, &a.name);
        let e = &st["accounts"][&k];
        let day = day_stat(&a.app, &a.name);
        let in_pool = pool
            .as_ref()
            .and_then(|p| p["members"].as_array())
            .map(|ms| {
                ms.iter().any(|m| {
                    m["src"].as_str()
                        == Some(&crate::gateway::member_src_instance(&a.app, &a.name))
                })
            })
            .unwrap_or(false);
        let cooling: Vec<Value> = st["cooldowns"]
            .as_object()
            .map(|o| {
                o.iter()
                    .filter(|(kk, _)| kk.starts_with(&format!("{}|", k)))
                    .filter_map(|(kk, v)| {
                        let until = v.as_u64().unwrap_or(0);
                        if until <= now_secs() {
                            return None;
                        }
                        let model = kk.split_once('|').map(|(_, m)| m).unwrap_or("");
                        Some(json!({ "model": model, "seconds_left": until - now_secs() }))
                    })
                    .collect()
            })
            .unwrap_or_default();
        accts.push(json!({
            "app": a.app,
            "app_name": a.app_name,
            "name": a.name,
            "region": a.region,
            "region_label": region_label(a.region),
            "state": state,
            "detail": detail,
            "masked": masked,
            "user": user,
            "expires_days": expires_days,
            "in_pool": in_pool,
            "calls": e["calls"].as_u64().unwrap_or(0),
            "ok": e["ok"].as_u64().unwrap_or(0),
            "fail": e["fail"].as_u64().unwrap_or(0),
            "last_status": e["last_status"].as_u64(),
            "last_error": e["last_error"].clone(),
            "last_used": e["last_used"].as_u64(),
            "today_credit": day.credit,
            "today_tokens": day.tokens,
            "today_calls": day.calls,
            "cooling": cooling,
        }));
    }
    let models: Vec<Value> = APPS
        .iter()
        .map(|app| {
            let cat = catalog(app);
            let (free, all, at, source, err) = match cat {
                Ok(c) => (
                    c.models
                        .iter()
                        .filter(|m| m.free)
                        .map(model_json)
                        .collect::<Vec<_>>(),
                    c.models.iter().map(model_json).collect::<Vec<_>>(),
                    Some(c.fetched_at),
                    c.source,
                    Value::Null,
                ),
                Err(e) => (Vec::new(), Vec::new(), None, String::new(), json!(e)),
            };
            json!({
                "app": app, "region": region_of(app).unwrap_or("cn"),
                "region_label": region_label(region_of(app).unwrap_or("cn")),
                "free": free, "all": all,
                "fetched_at": at, "source": source, "error": err,
            })
        })
        .collect();
    let dist = crate::gateway::dist_keys()
        .into_iter()
        .find(|d| d["name"].as_str() == Some(dist_name()));
    json!({
        "accounts": accts,
        "models": models,
        "dist": dist.map(|d| json!({
            "id": d["id"], "name": d["name"],
            "masked": crate::qproxy::mask(d["key"].as_str().unwrap_or("")),
            "calls": d["calls"].as_u64().unwrap_or(0),
        })),
        "secret": {
            "present": at_rest_secret().is_some(),
            "fingerprint": secret_fingerprint(),
        },
        "aliases": aliases().into_iter().map(|(k, v)| json!({ "alias": k, "target": v })).collect::<Vec<_>>(),
        "guardrails": {
            "reserve_credit": guardrail_reserve(),
            "daily_credit_cap": flag_u("daily_credit_cap", 0),
            "daily_token_cap": flag_u("daily_token_cap", 0),
        },
        "logs": recent_logs(30),
        "pool_name": pool_name(),
        "dist_name": dist_name(),
    })
}

// ───────────────────────── 非流式聚合 ─────────────────────────

/// 把上游的 OpenAI SSE 聚合成一条非流式 `chat.completion`。
/// 调用方要 `stream:false` 时用这条：上游是否原生支持非流式属未验证（我们只实测过
/// stream:true），本地聚合不依赖未文档化的上游行为。
pub fn collect_completion(
    reader: &mut dyn std::io::Read,
    model: &str,
) -> Result<(Value, Option<Value>), String> {
    use std::io::Read as _;
    let mut buf = String::new();
    reader.read_to_string(&mut buf).map_err(|e| e.to_string())?;

    let mut content = String::new();
    let mut reasoning = String::new();
    let mut tools: Vec<Value> = Vec::new();
    let mut usage: Option<Value> = None;
    let mut finish = String::new();
    let mut id = String::new();
    let mut created: i64 = 0;

    for line in buf.lines() {
        let Some(rest) = line.strip_prefix("data:") else {
            continue;
        };
        let data = rest.trim();
        if data.is_empty() || data == "[DONE]" {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(data) else {
            continue;
        };
        if id.is_empty() {
            id = v["id"].as_str().unwrap_or("").to_string();
        }
        if created == 0 {
            created = v["created"].as_i64().unwrap_or(0);
        }
        if let Some(ch) = v["choices"].as_array().and_then(|a| a.first()) {
            if let Some(d) = ch["delta"]["content"].as_str() {
                content.push_str(d);
            }
            if let Some(d) = ch["delta"]["reasoning_content"].as_str() {
                reasoning.push_str(d);
            }
            // 流式工具调用只有首个分片带真 id，后续分片只有 index —— 按 index 归并
            if let Some(tc) = ch["delta"]["tool_calls"].as_array() {
                for t in tc {
                    let idx = t["index"].as_u64().unwrap_or(0) as usize;
                    while tools.len() <= idx {
                        tools.push(json!({
                            "id": "", "type": "function",
                            "function": { "name": "", "arguments": "" }
                        }));
                    }
                    if let Some(s) = t["id"].as_str() {
                        if !s.is_empty() {
                            tools[idx]["id"] = json!(s);
                        }
                    }
                    if let Some(s) = t["function"]["name"].as_str() {
                        if !s.is_empty() {
                            tools[idx]["function"]["name"] = json!(s);
                        }
                    }
                    if let Some(s) = t["function"]["arguments"].as_str() {
                        let cur = tools[idx]["function"]["arguments"]
                            .as_str()
                            .unwrap_or("")
                            .to_string();
                        tools[idx]["function"]["arguments"] = json!(format!("{cur}{s}"));
                    }
                }
            }
            if let Some(f) = ch["finish_reason"].as_str() {
                if !f.is_empty() {
                    finish = f.to_string();
                }
            }
        }
        if v["usage"].is_object() {
            usage = Some(v["usage"].clone());
        }
    }

    let mut msg = json!({ "role": "assistant", "content": content });
    if !reasoning.is_empty() {
        msg["reasoning_content"] = json!(reasoning);
    }
    if !tools.is_empty() {
        msg["tool_calls"] = json!(tools);
    }
    let resp = json!({
        "id": if id.is_empty() { format!("wb-{}", now_secs()) } else { id },
        "object": "chat.completion",
        "created": if created == 0 { now_secs() as i64 } else { created },
        "model": model,
        "choices": [{
            "index": 0,
            "message": msg,
            "finish_reason": if finish.is_empty() { "stop".to_string() } else { finish },
        }],
        "usage": usage,
    });
    Ok((resp, usage))
}

// ───────────────────────── 流内用量 ─────────────────────────

/// 从 SSE 尾巴里抠出最后一个 `usage` 对象（末 chunk 才有）。
/// 厂商第一方字段：`credit` 是逐请求真实成本，`prompt_cache_hit_tokens` 是缓存命中。
pub fn parse_usage(sse_tail: &str) -> Option<Value> {
    let mut out = None;
    for line in sse_tail.lines() {
        let Some(rest) = line.strip_prefix("data:") else {
            continue;
        };
        let data = rest.trim();
        if data.is_empty() || data == "[DONE]" {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<Value>(data) {
            if v["usage"].is_object() {
                out = Some(v["usage"].clone());
            }
        }
    }
    out
}

pub fn parse_finish(sse_tail: &str) -> Option<String> {
    let mut out = None;
    for line in sse_tail.lines() {
        let Some(rest) = line.strip_prefix("data:") else {
            continue;
        };
        let data = rest.trim();
        if data.is_empty() || data == "[DONE]" {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<Value>(data) {
            if let Some(f) = v["choices"]
                .as_array()
                .and_then(|a| a.first())
                .and_then(|c| c["finish_reason"].as_str())
            {
                if !f.is_empty() {
                    out = Some(f.to_string());
                }
            }
        }
    }
    out
}

/// 本机宽松模式：打开才恢复「未知 key 回落全部启用的上游组」的老行为。
/// 默认关 —— 浏览器里任何网页都能借她的 token 池打上游，那是真洞。
pub fn loose_mode() -> bool {
    crate::settings::get()["proxy"]["lan_loose"]
        .as_bool()
        .unwrap_or(false)
}

// ───────────────────────── 一键开启 ─────────────────────────

pub fn proxy_port() -> u16 {
    crate::settings::get()["proxy"]["port"]
        .as_u64()
        .map(|v| v as u16)
        .unwrap_or(crate::registry::PROXY_PORT)
}

/// 某个成员现在能不能用（给 gateway 的成员面板看）。返回 (可用, 原因)。
pub fn member_usable(app_id: &str, name: &str) -> (bool, String) {
    if !APPS.contains(&app_id) {
        return (false, String::new());
    }
    match read_login(app_id) {
        Ok(c) => {
            if !mismatch_is_ok(app_id, name, &c) {
                (false, "登录态里的账号与这张卡对不上".into())
            } else {
                (true, String::new())
            }
        }
        Err(e) => (false, e),
    }
}

/// 一键：探活 → 把可用的号放进「WorkBuddy 免费模型」池 → 生成/复用下发 Key → 起反代。
/// 幂等：重复点不会重复建池、不会重复发 Key。
pub fn one_click(proxy: &std::sync::Arc<crate::proxy::Proxy>, with_infer: bool) -> Value {
    if at_rest_secret().is_none() {
        return json!({
            "ok": false,
            "need": "secret",
            "msg": "还没导入 WorkBuddy 解密密钥：点「导入密钥」从本机 agenthub 读一次，\
                    或者手动粘贴那把 base64（44 个字符，末尾带 =）。",
        });
    }

    let mut usable: Vec<Value> = Vec::new();
    let mut skipped: Vec<Value> = Vec::new();
    for a in accounts() {
        if a.name == "（未建卡）" {
            continue;
        }
        let r = probe(&a.app, &a.name);
        let ok = r["ok"].as_bool().unwrap_or(false);
        let usable_state = ok || (with_infer && r["state"].as_str() == Some("request_shape"));
        if usable_state {
            usable.push(json!({ "app": a.app, "name": a.name, "state": r["state"] }));
        } else {
            skipped.push(json!({
                "app": a.app, "name": a.name,
                "state": r["state"], "why": r["msg"],
            }));
        }
    }

    if usable.is_empty() {
        return json!({
            "ok": false,
            "msg": "两个区域的号都没探活通过，看 skipped 里每一条的原因。",
            "skipped": skipped,
        });
    }

    // 只起反代。上游组和接入密钥**不再代建**——归组、选 key 是用户的事，
    // 到「上游组」「接入密钥」两个表里手动做（历史教训：按名复用一旦名字对不上，
    // 每轮一键都会新建组+key，堆出一排同名残留）。
    let port = proxy_port();
    let (started, smsg) = if proxy.is_running() {
        (true, "反代已在运行".to_string())
    } else {
        proxy.start(port)
    };

    json!({
        "ok": started,
        "msg": format!(
            "{} 个账号探活通过；{}。账号还没动上游组和接入密钥：到「上游组」把账号勾进组，客户端用「接入密钥」表里的 key。",
            usable.len(),
            smsg
        ),
        "base_url": format!("http://127.0.0.1:{port}/v1"),
        "members": usable,
        "skipped": skipped,
        "started": started,
    })
}

pub fn dist_reveal() -> String {
    crate::gateway::dist_keys()
        .into_iter()
        .find(|d| d["name"].as_str() == Some(dist_name()))
        .and_then(|d| d["id"].as_str().and_then(crate::gateway::reveal_dist))
        .unwrap_or_default()
}

// ───────────────────────── 测试 ─────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn h(s: &str) -> String {
        hex(&sha256(s.as_bytes()))
    }

    #[test]
    fn sha256_matches_fips_vectors() {
        // FIPS 180-4 / NIST 标准向量
        assert_eq!(
            h(""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            h("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            h("abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
    }

    #[test]
    fn aad_field_matches_golden_bytes() {
        // 手算：WB-AAD\0 01 | 00000005 WBEV1 | 00000006 sym-v1 | 00000001 | 00000002 ab | 02 0000
        let a = aad_field(1, "ab");
        assert_eq!(
            hex(&a),
            "57422d4141440001\
             000000055742455631\
             0000000673796d2d7631\
             00000001\
             000000026162\
             020000"
        );
    }

    #[test]
    fn decrypt_passes_plaintext_through() {
        let k = [0u8; 32];
        assert_eq!(decrypt_field(&k, &json!("plain-token")).unwrap(), "plain-token");
    }

    #[test]
    fn decrypt_rejects_unknown_suite() {
        let k = [0u8; 32];
        let env = json!({ "suite": 2, "keyId": "x", "nonce": "", "ciphertext": "", "authTag": "" });
        let b64 = crate::secrets::b64_encode(serde_json::to_string(&env).unwrap().as_bytes());
        let f = json!({ "$wbEncrypted": 1, "envelope": b64 });
        let e = decrypt_field(&k, &f).unwrap_err();
        assert!(e.contains("suite"), "实际：{e}");
    }

    #[test]
    fn decrypt_rejects_non_envelope() {
        let k = [0u8; 32];
        assert!(decrypt_field(&k, &json!({ "foo": 1 })).is_err());
    }

    #[test]
    fn regions_map_to_the_hosts_we_measured() {
        assert_eq!(model_base("cn"), "https://www.workbuddy.cn");
        assert_eq!(model_base("global"), "https://www.workbuddy.ai");
        assert_eq!(region_of("workbuddy"), Some("cn"));
        assert_eq!(region_of("workbuddy-intl"), Some("global"));
        assert_eq!(region_of("qoder"), None);
    }

    #[test]
    fn free_detection_handles_all_observed_shapes() {
        assert!(is_free("x0.00"));
        assert!(is_free("0.00 credits"));
        assert!(is_free("0"));
        assert!(!is_free("x0.21"));
        assert!(!is_free("x3.31 credits"));
        assert!(!is_free(""));
        assert!(!is_free("x0.05"));
    }

    #[test]
    fn classify_separates_11128_from_11140() {
        // 11128 + system 提示 → 请求形状
        let o = classify(400, r#"{"code":11128,"msg":"first message is not system prompt"}"#, &[]);
        assert!(matches!(o, Outcome::RequestShape(_)), "实际 {:?}", o);
        assert!(!o.should_cool(), "11128 绝不许冷却账号");

        // 11128 + request illegal → 渠道校验没过，仍是请求形状
        let o = classify(400, r#"{"code":11128,"msg":"request illegal"}"#, &[]);
        assert!(matches!(o, Outcome::RequestShape(_)), "实际 {:?}", o);
        assert!(!o.should_cool());

        // 11140 + request illegal → 账号级，要换号要冷却
        let o = classify(403, r#"{"code":11140,"msg":"request illegal"}"#, &[]);
        assert_eq!(o, Outcome::AccountBlocked);
        assert!(o.should_cool());
        assert!(o.should_rotate());

        // 容量排队 → 模型级，不换号
        for c in [6020u16, 6021, 6022] {
            let body = format!(r#"{{"code":{c},"msg":"queue"}}"#);
            let o = classify(429, &body, &[]);
            assert_eq!(o, Outcome::ModelCapacity(c));
            assert!(!o.should_cool(), "容量排队不许冷却账号");
            assert!(!o.should_rotate());
        }
    }

    #[test]
    fn request_shape_never_cools_an_account() {
        for body in [
            r#"{"code":11128,"msg":"first message is not system prompt"}"#,
            r#"{"code":11128,"msg":"request illegal"}"#,
        ] {
            let o = classify(400, body, &[]);
            assert_eq!(o.cooldown_secs(), 0);
            assert!(!o.should_cool());
        }
    }

    #[test]
    fn unknown_is_its_own_bucket_not_empty() {
        let o = classify(418, "nonsense", &[]);
        assert_eq!(o, Outcome::Unknown(418));
        assert!(!o.should_cool(), "对不上号的code不许当账号故障");
    }

    #[test]
    fn ok_only_when_200_and_no_biz_code() {
        assert_eq!(classify(200, "", &[]), Outcome::Ok);
        assert_eq!(classify(200, "data: x", &[]), Outcome::Ok);
        assert_ne!(classify(200, r#"{"code":11140}"#, &[]), Outcome::Ok);
    }

    #[test]
    fn rate_limit_takes_retry_after_with_floor() {
        let o = classify(429, "", &[("Retry-After".into(), "3".into())]);
        assert_eq!(o, Outcome::AccountRateLimited { retry_after: Some(3) });
        assert_eq!(o.cooldown_secs(), 10, "下限 10 秒");
        let o2 = classify(429, "", &[("Retry-After".into(), "120".into())]);
        assert_eq!(o2.cooldown_secs(), 120);
    }

    #[test]
    fn strip_attribution_drops_caller_identity() {
        for k in [
            "Authorization", "x-api-key", "X-User-Id", "x-domain", "X-Product",
            "X-IDE-Version", "traceparent", "b3", "X-B3-TraceId", "x-conversation-id",
            "User-Agent", "acp-connection-id", "x-requested-with",
            // content-type / accept 也剥：这两个由我们自己按上游要求设成固定值
            "content-type", "accept",
        ] {
            assert!(strip_attribution(k), "{k} 应当被剥掉");
        }
        for k in ["x-custom-thing", "openai-organization"] {
            assert!(!strip_attribution(k), "{k} 不该被剥掉");
        }
    }

    #[test]
    fn derive_uuid_is_stable_and_distinct() {
        let a = derive_uuid("conv", "uid1", "keyA");
        let b = derive_uuid("conv", "uid1", "keyA");
        let c = derive_uuid("conv", "uid1", "keyB");
        assert_eq!(a, b, "同一组输入必须稳定");
        assert_ne!(a, c, "不同 key 必须不同");
        assert_eq!(a.len(), 36);
        assert_eq!(a.matches('-').count(), 4);
    }

    #[test]
    fn cooldown_only_lengthens() {
        // 用临时目录避免污染真账本
        let _ = std::env::var("AGENT_COCKPIT_HOME");
        // 这条只验纯逻辑：短的不覆盖长的
        let acct = "unittest/cool";
        let model = "m1";
        set_cooldown(acct, model, 100);
        let first = cooldown_until(acct, model);
        set_cooldown(acct, model, 1);
        assert_eq!(cooldown_until(acct, model), first, "短的冷却不许覆盖长的");
        // 清掉，别留垃圾
        let _g = STORE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let mut st = load_store();
        if let Some(o) = st["cooldowns"].as_object_mut() {
            o.remove(&cool_key(acct, model));
        }
        save_store(&st);
    }

    #[test]
    fn prep_error_dimensions_are_distinct() {
        let acct = vec![
            PrepError::NoCredential("x".into()),
            PrepError::Guardrail("x".into()),
            PrepError::Cooldown { until: 1 },
        ];
        for e in acct {
            assert!(e.is_account_level(), "{e:?} 应是账号级");
        }
        let req = vec![
            PrepError::ModelUnknown { asked: "x".into(), available: vec![] },
            PrepError::MalformedBody("x".into()),
        ];
        for e in req {
            assert!(!e.is_account_level(), "{e:?} 应是请求级");
        }
    }

    #[test]
    fn secret_line_parser_keeps_the_padding() {
        // 这一行就是 agenthub 里的真实形状。密钥末尾的 '=' 是它自己的填充，
        // 不是赋值号 —— 切错就会少一个字符，解密时只报「GCM 解不开」。
        let line = r#"WORKBUDDY_AT_REST_SECRET_KEY = "AAAAbbbbCCCCddddEEEEffffGGGGhhhhIIIIkkkk="  # 测试"#;
        let got = parse_secret_line(line);
        assert_eq!(got, "AAAAbbbbCCCCddddEEEEffffGGGGhhhhIIIIkkkk=");
        assert!(got.ends_with('='), "尾部填充被切掉了");
    }

    #[test]
    fn stripped_padding_would_fail_the_strict_shape_check() {
        let full = crate::secrets::b64_encode(&[7u8; 32]);
        assert_eq!(full.len(), 44);
        let stripped = full.trim_end_matches('=');
        assert_eq!(stripped.len(), 43);
        // 少填充照样能解出 32 字节 —— 所以「长度对不对」这条自检拦不住它
        assert_eq!(crate::secrets::b64_decode(stripped).unwrap().len(), 32);
        // 只有「重编码必须逐字一致」能拦住
        let round = crate::secrets::b64_encode(&crate::secrets::b64_decode(stripped).unwrap());
        assert_ne!(round, stripped);
    }

    #[test]
    fn secret_never_falls_back_to_a_hardcoded_value() {
        // 源码里不许出现那把密钥。针尖用字节拼出来，否则这条断言会命中它自己。
        let needle: String = [83u8, 105, 107, 57, 85, 53, 97, 88, 104, 67, 100, 119]
            .iter()
            .map(|b| *b as char)
            .collect();
        let src = include_str!("wbproxy.rs");
        assert!(!src.contains(&needle), "atRestSecretKey 不许出现在源码里");
    }

    // ── 以下两条是 [#[ignore]] 真机测试，用她的真账号，0 倍率模型。
    //    跑法：cargo test wbproxy::tests::real -- --ignored --test-threads=1

    #[test]
    #[ignore]
    fn real_import_secret_once() {
        // 首次导入：从本机 agenthub 读一次常量、存进 DPAPI 保管箱。
        // 这就是 spec 定的「导入」动作；重复跑是幂等的。
        // 测试进程里 CACHE 是空的，必须先 load() 把盘上的保管箱读进内存，
        // 否则 put 会返回 false（那正是「锁着时别覆盖」的保护）。
        crate::secrets::load();
        assert!(!crate::secrets::locked(), "保管箱有主密码且本进程没解锁，先在界面里解锁");
        let msg = import_secret_from_agenthub().expect("导入失败");
        eprintln!("{msg}");
        assert!(at_rest_secret().is_some(), "导入后应当读得到");
        assert!(at_rest_key().is_some(), "能派生出 K");
    }

    #[test]
    #[ignore]
    fn real_oneclick_through_http() {
        // 整条链路：一键 → 建池建 Key → 起反代 → 用那把 Key 打一次真补全。
        // 会写 config.json（这正是「一键」该做的事），跑前备份、失败自动还原。
        crate::secrets::load();
        if at_rest_secret().is_none() {
            let _ = import_secret_from_agenthub();
        }
        let cfg_path = crate::registry::base_dir().join("config.json");
        let backup = std::fs::read_to_string(&cfg_path).ok();

        let proxy = crate::proxy::Proxy::shared();
        let r = one_click(&proxy, true);
        eprintln!("one_click = {}", serde_json::to_string_pretty(&r).unwrap_or_default());
        if !r["ok"].as_bool().unwrap_or(false) {
            if let Some(b) = &backup {
                let _ = std::fs::write(&cfg_path, b);
            }
            panic!("一键没成功：{}", r["msg"].as_str().unwrap_or(""));
        }
        let key = r["key"].as_str().unwrap_or("").to_string();
        let base = r["base_url"].as_str().unwrap_or("").to_string();
        assert!(!key.is_empty(), "一键返回了空 Key");

        let model = free_models("workbuddy")
            .into_iter()
            .next()
            .map(|m| m.id)
            .expect("国内版没有免费模型");
        let res = ureq::post(&format!("{base}/chat/completions"))
            .timeout(std::time::Duration::from_secs(60))
            .set("Authorization", &format!("Bearer {key}"))
            .set("Content-Type", "application/json")
            .send_json(json!({
                "model": model,
                "messages": [{"role": "user", "content": "Reply with exactly: OK"}],
                "stream": true,
                "max_tokens": 16,
            }));

        match res {
            Ok(resp) => {
                let mut rd = resp.into_reader();
                let (v, usage) = collect_completion(&mut rd, &model).expect("聚合失败");
                eprintln!(
                    "客户端视角：content={:?} usage={}",
                    v["choices"][0]["message"]["content"].as_str().unwrap_or(""),
                    usage.clone().unwrap_or(Value::Null)
                );
                let credit = usage.as_ref().and_then(|u| u["credit"].as_f64()).unwrap_or(-1.0);
                assert_eq!(credit, 0.0, "扣了 {credit} 积分，不是免费模型");
            }
            Err(ureq::Error::Status(code, resp)) => {
                let mut rd = resp.into_reader();
                let mut buf = vec![0u8; 2000];
                let n = std::io::Read::read(&mut rd, &mut buf).unwrap_or(0);
                if let Some(b) = &backup {
                    let _ = std::fs::write(&cfg_path, b);
                }
                panic!("反代回 HTTP {code}：{}", String::from_utf8_lossy(&buf[..n]));
            }
            Err(e) => {
                if let Some(b) = &backup {
                    let _ = std::fs::write(&cfg_path, b);
                }
                panic!("打不通反代：{e}");
            }
        }
        // 通过判定后**保留**配置：上游组和 Key 就是「一键」的产物，她要能直接在界面看到
        let _ = proxy.stop();
        // 账本自检：这一发必须落在提供服务的那个账号上（可观测性靠它）。
        // ⚠️ 必须等一下：客户端把响应体读完时，服务线程可能还在 respond 的收尾里、
        // 没走到 note()。直接读会稳定地少记一发，看着像 bug。
        std::thread::sleep(std::time::Duration::from_millis(800));
        let st = load_store();
        let served: u64 = ["workbuddy/DuckDeity", "workbuddy-intl/shen1950"]
            .iter()
            .filter_map(|k| st["accounts"][k]["calls"].as_u64())
            .sum();
        eprintln!("账本 calls 合计 = {served}（探活 2 次 + 本发至少 1 次）");
        assert!(served >= 3, "聊天那一发没记账，账本只有 {served} 次");
    }

    #[test]
    #[ignore]
    fn real_prepare_and_completion() {
        crate::secrets::load();
        if at_rest_secret().is_none() {
            let _ = import_secret_from_agenthub();
        }
        assert!(at_rest_secret().is_some(), "保管箱里没有密钥");
        for app in APPS {
            let free = free_models(app);
            assert!(!free.is_empty(), "{app} 目录里没有 0 倍率模型（先开一次客户端刷新 /v3/config）");
            let model = free[0].id.clone();
            let body = serde_json::to_vec(&json!({
                "model": model,
                "messages": [{"role": "user", "content": "Reply with exactly: OK"}],
                "stream": true,
                "max_tokens": 16,
            }))
            .unwrap();
            let p = prepare(app, "test-dist", &[], &body).expect("prepare 失败");
            assert_eq!(p.model_used, model);
            assert!(p.stream);
            // 头族必须齐全
            for k in [
                "Authorization", "X-Domain", "X-User-Id", "X-Conversation-ID",
                "acp-connection-id", "x-codebuddy-request", "traceparent", "b3",
                "X-Product", "X-IDE-Version",
            ] {
                assert!(
                    p.headers.iter().any(|(n, _)| n.eq_ignore_ascii_case(k)),
                    "{app} 少了头 {k}"
                );
            }
            let region = region_of(app).unwrap();
            let url = format!("{}/v2/chat/completions", model_base(region));
            let mut call = ureq::post(&url).timeout(std::time::Duration::from_secs(60));
            for (n, v) in &p.headers {
                call = call.set(n, v);
            }
            let res = call.send(&p.body[..]);
            match res {
                Ok(resp) => {
                    let status = resp.status();
                    let mut rd = resp.into_reader();
                    let (v, usage) = collect_completion(&mut rd, &p.model_used).expect("聚合失败");
                    eprintln!(
                        "{app} → {status}, model={}, content={:?}, usage={}",
                        p.model_used,
                        v["choices"][0]["message"]["content"].as_str().unwrap_or(""),
                        usage.clone().unwrap_or(Value::Null)
                    );
                    assert_eq!(status, 200, "{app} 没通");
                    let credit = usage.as_ref().and_then(|u| u["credit"].as_f64()).unwrap_or(-1.0);
                    assert_eq!(credit, 0.0, "{app} 用的不是 0 倍率模型，扣了 {credit} 积分");
                }
                Err(ureq::Error::Status(code, resp)) => {
                    let mut rd = resp.into_reader();
                    let mut buf = vec![0u8; 2000];
                    let n = std::io::Read::read(&mut rd, &mut buf).unwrap_or(0);
                    panic!(
                        "{app} → HTTP {code}：{}",
                        String::from_utf8_lossy(&buf[..n])
                    );
                }
                Err(e) => panic!("{app} 网络错误：{e}"),
            }
        }
    }
}
