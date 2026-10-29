//! Antigravity（Google AI IDE）账号登记与切换。
//!
//! 登录态落点（实测本机 2026-10-28 + 参照 Cockpit Tools 的机制）：
//!   - Windows 凭据管理器 target `gemini:antigravity`（username `antigravity`，
//!     CRED_TYPE_GENERIC，payload 是 JSON：
//!     `{token:{access_token,refresh_token,expiry}, auth_method:"consumer", id_token}`）。
//!   - 回退文件 `~/.gemini/jetski-standalone-oauth-token`（同一 payload）。
//!
//! 官方客户端启动时读这套凭据（compositeTokenStorage），用 refresh_token 换 access_token
//! 登录。所以「换号」= 把目标账号的凭据写回上述两处 + 重启客户端，**不做目录 junction**
//! （Antigravity 的数据目录 `{APPDATA}\Antigravity` 与 `~/.gemini` 是整机一份共享的，切号只切登录态）。
//!
//! 旧版登录态在 `{APPDATA}\Antigravity\User\globalStorage\state.vscdb` 的 `antigravityAuthStatus`，
//! 本机已是 2.0 新版（凭据管理器），旧路径只作兜底、这里暂不解析（交给 agenthub 读）。
//!
//! 参考：本模块照 zcode.rs 的「凭据注入换号」模式实现，凭据读写用标准 Windows
//! CredWriteW/CredReadW API（机制同 Cockpit Tools，代码自写）。

use crate::{procs, registry as reg, secrets};
use serde_json::{json, Value};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const APP: &str = "antigravity";
static LOCK: Mutex<()> = Mutex::new(());

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()
}
fn home() -> PathBuf {
    reg::home()
}
fn fallback_path() -> PathBuf {
    home().join(".gemini").join("jetski-standalone-oauth-token")
}
fn catalog_file() -> PathBuf {
    reg::base_dir().join("antigravity-logins.json")
}
fn vault_id(email: &str) -> String {
    format!("antigravity-login:{}", email.trim().to_lowercase())
}

// Antigravity 官方 OAuth 客户端凭据与 Code Assist 额度端点（来源：开源 quota-core
// crate 的 local_usage.rs，以及 Cockpit Tools 同款机制）。Google 把 refresh_token 绑定到
// 签发它的 client，所以换 token 必须用这一组 client，换错会返回 unauthorized_client。
const OAUTH_CLIENT_ID: &str = "1071006060591-tmhssin2h21lcre235vtolojh4g403ep.apps.googleusercontent.com";
const OAUTH_CLIENT_SECRET: &str = "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";
const TOKEN_ENDPOINT: &str = "https://oauth2.googleapis.com/token";
// prod 优先，daily 作兜底（两者都是官方 Code Assist 后端，daily 是沙箱）
const CODE_ASSIST_HOSTS: &[&str] = &[
    "https://cloudcode-pa.googleapis.com",
    "https://daily-cloudcode-pa.googleapis.com",
];
const IDE_VERSION: &str = "1.20.5";
// 四档额度（对应 Cockpit Tools 截图里的 Gemini 每周/5小时、Claude/GPT 每周/5小时）
const QUOTA_BUCKETS: &[(&str, &str)] = &[
    ("gemini-5h", "Gemini 模型 · 5 小时额度"),
    ("gemini-weekly", "Gemini 模型 · 每周额度"),
    ("3p-5h", "Claude / GPT 模型 · 5 小时额度"),
    ("3p-weekly", "Claude / GPT 模型 · 每周额度"),
];
fn plain(path: &Path) -> Result<(), String> {
    if !path.is_absolute() || path.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err("Antigravity 数据路径无效".into());
    }
    if path.ancestors().any(reg::is_reparse_point) {
        return Err("Antigravity 数据目录包含旧链接，请保留资料后单独处理".into());
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Windows 凭据管理器（Credential Manager）
// ---------------------------------------------------------------------------

#[cfg(windows)]
mod wincred {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;
    use std::ptr;

    const CRED_TYPE_GENERIC: u32 = 1;
    const CRED_PERSIST_LOCAL_MACHINE: u32 = 2;
    const ERROR_NOT_FOUND: i32 = 1168;
    const TARGET: &str = "gemini:antigravity";
    const USER: &str = "antigravity";

    #[repr(C)]
    struct FileTime {
        dw_low_date_time: u32,
        dw_high_date_time: u32,
    }

    #[repr(C)]
    struct CredentialW {
        flags: u32,
        cred_type: u32,
        target_name: *const u16,
        comment: *const u16,
        last_written: FileTime,
        credential_blob_size: u32,
        credential_blob: *const u8,
        persist: u32,
        attribute_count: u32,
        attributes: *const std::ffi::c_void,
        target_alias: *const u16,
        user_name: *const u16,
    }

    #[link(name = "advapi32")]
    extern "system" {
        fn CredWriteW(credential: *const CredentialW, flags: u32) -> i32;
        fn CredReadW(
            target_name: *const u16,
            type_: u32,
            flags: u32,
            credential: *mut *mut CredentialW,
        ) -> i32;
        fn CredFree(buffer: *mut std::ffi::c_void);
    }

    fn wide(s: &str) -> Vec<u16> {
        OsStr::new(s).encode_wide().chain(std::iter::once(0)).collect()
    }

    /// 写（或覆盖）`gemini:antigravity` 这条 Generic 凭据。
    pub fn write(secret: &str) -> Result<(), String> {
        let target = wide(TARGET);
        let user = wide(USER);
        let bytes = secret.as_bytes();
        let credential = CredentialW {
            flags: 0,
            cred_type: CRED_TYPE_GENERIC,
            target_name: target.as_ptr(),
            comment: ptr::null(),
            last_written: FileTime { dw_low_date_time: 0, dw_high_date_time: 0 },
            credential_blob_size: bytes.len() as u32,
            credential_blob: bytes.as_ptr(),
            persist: CRED_PERSIST_LOCAL_MACHINE,
            attribute_count: 0,
            attributes: ptr::null(),
            target_alias: ptr::null(),
            user_name: user.as_ptr(),
        };
        // CredWriteW 对已存在的 target 是直接覆盖，不必先删。
        let ok = unsafe { CredWriteW(&credential, 0) };
        if ok == 0 {
            return Err(format!("写入 Windows 凭据管理器失败：{}", std::io::Error::last_os_error()));
        }
        Ok(())
    }

    /// 读 `gemini:antigravity` 这条凭据；不存在返回 Ok(None)。
    pub fn read() -> Result<Option<String>, String> {
        let target = wide(TARGET);
        let mut credential_ptr: *mut CredentialW = ptr::null_mut();
        unsafe {
            if CredReadW(target.as_ptr(), CRED_TYPE_GENERIC, 0, &mut credential_ptr) == 0 {
                let err = std::io::Error::last_os_error();
                if err.raw_os_error() == Some(ERROR_NOT_FOUND) {
                    return Ok(None);
                }
                return Err(format!("读取 Windows 凭据管理器失败：{err}"));
            }
            if credential_ptr.is_null() {
                return Ok(None);
            }
            let c = &*credential_ptr;
            let secret = if c.credential_blob.is_null() || c.credential_blob_size == 0 {
                String::new()
            } else {
                let bytes = std::slice::from_raw_parts(c.credential_blob, c.credential_blob_size as usize);
                String::from_utf8_lossy(bytes).trim().to_string()
            };
            CredFree(credential_ptr.cast());
            Ok(if secret.is_empty() { None } else { Some(secret) })
        }
    }
}

// ---------------------------------------------------------------------------
// 凭据载荷结构
// ---------------------------------------------------------------------------

#[derive(serde::Serialize, serde::Deserialize, Clone, PartialEq)]
struct Token {
    #[serde(default)]
    access_token: String,
    #[serde(default)]
    token_type: String,
    refresh_token: String,
    #[serde(default)]
    expiry: String,
}

#[derive(serde::Serialize, serde::Deserialize, Clone, PartialEq)]
struct Payload {
    token: Token,
    #[serde(default)]
    auth_method: String,
    #[serde(default)]
    id_token: Option<String>,
}

impl Payload {
    /// 规范 refresh_token：Antigravity 侧存在 `{refresh_token}|{project_id}` 这种
    /// 拼接格式（project_id 决定配额归属），但凭据里官方客户端要的是纯 token。
    fn clean_refresh_token(&self) -> &str {
        self.token.refresh_token.split('|').next().unwrap_or("").trim()
    }
    fn email(&self) -> Option<String> {
        self.id_token.as_deref().and_then(email_from_id_token)
    }
}

// ---------------------------------------------------------------------------
// 身份解析
// ---------------------------------------------------------------------------

/// 从 id_token（JWT）的 payload 段解码 email。只用于展示身份，**不验签**。
fn email_from_id_token(id_token: &str) -> Option<String> {
    let payload = id_token.split('.').nth(1)?;
    let bytes = b64url_decode(payload).ok()?;
    let v: Value = serde_json::from_slice(&bytes).ok()?;
    v.get("email").and_then(Value::as_str).map(str::to_string)
}

/// base64url（无 padding，`-`/`_`）解码，补成标准 base64 后交给 secrets 的译码器。
fn b64url_decode(s: &str) -> Result<Vec<u8>, String> {
    let mut t = s.trim().replace('-', "+").replace('_', "/");
    while t.len() % 4 != 0 {
        t.push('=');
    }
    secrets::b64_decode(&t)
}

fn parse_payload(secret: &str) -> Result<Payload, String> {
    let p: Payload = serde_json::from_str(secret).map_err(|_| "凭据内容无法解析")?;
    if p.clean_refresh_token().is_empty() {
        return Err("凭据里缺少 refresh_token".into());
    }
    Ok(p)
}

// ---------------------------------------------------------------------------
// 读取当前登录凭据
// ---------------------------------------------------------------------------

/// 三级读：系统凭据 → 回退文件 → 旧 state.vscdb（旧路径交给 agenthub，这里不解析）。
fn read_current_payload() -> Result<Payload, String> {
    #[cfg(windows)]
    {
        if let Some(secret) = wincred::read()? {
            if let Ok(p) = parse_payload(&secret) {
                return Ok(p);
            }
        }
    }
    let fb = fallback_path();
    if fb.is_file() {
        let bytes = fs::read(&fb).map_err(|_| "无法读取 Antigravity 回退凭据")?;
        if let Ok(p) = parse_payload(&String::from_utf8_lossy(&bytes)) {
            return Ok(p);
        }
    }
    Err("未找到 Antigravity 登录凭据：请先在本机登录官方客户端，或用 refresh_token 导入".into())
}

// ---------------------------------------------------------------------------
// 台账（catalog）
// ---------------------------------------------------------------------------

fn read_catalog() -> Vec<Value> {
    fs::read(catalog_file())
        .ok()
        .and_then(|b| serde_json::from_slice::<Vec<Value>>(&b).ok())
        .unwrap_or_default()
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    plain(path)?;
    fs::create_dir_all(path.parent().ok_or("文件路径无父目录")?).map_err(|_| "无法创建 Antigravity 记录目录")?;
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos();
    let temporary = path.with_file_name(format!(".antigravity-{}-{stamp}.tmp", std::process::id()));
    fs::write(&temporary, bytes).map_err(|_| "无法写入 Antigravity 临时记录")?;
    fs::rename(&temporary, path).map_err(|_| {
        let _ = fs::remove_file(&temporary);
        "无法保存 Antigravity 记录".to_string()
    })
}

fn save_snapshot(email: &str, payload: &Payload) -> Result<(), String> {
    if !secrets::available() {
        return Err("请先解锁凭据保管箱".into());
    }
    let id = vault_id(email);
    let serialized = serde_json::to_string(payload).map_err(|_| "凭据序列化失败")?;
    if !secrets::put(&id, &serialized) {
        return Err("保存登录凭据失败：保管箱未解锁".into());
    }
    secrets::flush()?;
    let mut catalog = read_catalog();
    catalog.retain(|item| item["email"].as_str().map(|e| e.eq_ignore_ascii_case(email)) != Some(true));
    catalog.push(json!({ "email": email, "at": now() }));
    atomic_write(&catalog_file(), &serde_json::to_vec_pretty(&catalog).map_err(|_| "账号目录序列化失败")?)
}

fn snapshot_for_email(email: &str) -> Result<Payload, String> {
    let raw = secrets::get(&vault_id(email)).ok_or("未保存该账号登录，请重新录入")?;
    let p: Payload = serde_json::from_str(&raw).map_err(|_| "已保存的登录记录无效")?;
    Ok(p)
}

fn card(name: &str) -> Result<Value, String> {
    if name.is_empty() || reg::bad_account_name(name).is_some() {
        return Err("账号名称无效".into());
    }
    reg::find_instance(APP, name).ok_or_else(|| "账号不存在".into())
}

fn failure(message: impl Into<String>) -> Value {
    json!({ "ok": false, "msg": message.into() })
}

// ---------------------------------------------------------------------------
// 登记 / 切换
// ---------------------------------------------------------------------------

/// 登记本机当前登录的账号（读凭据管理器当前那套）。
fn capture(name: &str) -> Result<Value, String> {
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if name.is_empty() || reg::bad_account_name(name).is_some() {
        return Err("账号名称无效".into());
    }
    let payload = read_current_payload()?;
    let email = payload.email().ok_or("登录凭据里没有 email（id_token 缺失），无法确认身份；请改用 refresh_token 导入")?;
    register(name, &email, &payload)?;
    let meta = reg::find_instance(APP, name).ok_or("账号登记失败")?;
    Ok(json!({ "msg": format!("已登记账号 {email}"), "account": email, "antigravity_auth": status(&meta) }))
}

/// refresh_token 导入：payload 里给 `refresh_token`（+ 可选 `email` / 完整 id_token）。
/// 没有 email 时会尝试从 id_token 解，解不出就报错让用户补。
fn import(name: &str, p: &Value) -> Result<Value, String> {
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if name.is_empty() || reg::bad_account_name(name).is_some() {
        return Err("账号名称无效".into());
    }
    // 兼容三种输入：完整 payload 对象 / {"refresh_token","email"} / 纯 refresh_token 字符串
    let payload: Payload = if let Ok(pl) = serde_json::from_value::<Payload>(p.clone()) {
        pl
    } else {
        let rt = p["refresh_token"].as_str().or_else(|| p.as_str()).ok_or("请提供 refresh_token")?;
        Payload {
            token: Token {
                access_token: String::new(),
                token_type: "Bearer".to_string(),
                refresh_token: rt.to_string(),
                expiry: String::new(),
            },
            auth_method: "consumer".to_string(),
            id_token: p["id_token"].as_str().map(str::to_string),
        }
    };
    if payload.clean_refresh_token().is_empty() {
        return Err("refresh_token 为空".into());
    }
    let email = payload
        .email()
        .or_else(|| p["email"].as_str().map(|s| s.trim().to_string()))
        .filter(|e| !e.is_empty())
        .ok_or("无法从凭据里确认 email：请同时提供 email，或补上 id_token")?;
    register(name, &email, &payload)?;
    let meta = reg::find_instance(APP, name).ok_or("账号登记失败")?;
    Ok(json!({ "msg": format!("已导入账号 {email}"), "account": email, "antigravity_auth": status(&meta) }))
}

/// 登记一个账号：存凭据快照 + 绑定卡片。capture 与 import 共用。
fn register(name: &str, email: &str, payload: &Payload) -> Result<(), String> {
    // 同一个 email 只能挂一张卡：另开卡登记同一登录会串号。
    if reg::load_instances(APP)
        .iter()
        .any(|m| m["name"] != name && m["quota_user_id"].as_str().is_some_and(|u| u.eq_ignore_ascii_case(email)))
    {
        return Err("该登录已登记，请使用对应账号卡片".into());
    }
    let existing = reg::find_instance(APP, name);
    if let Some(m) = existing.as_ref() {
        if m["quota_user_id"].as_str().is_some_and(|old| !old.is_empty() && !old.eq_ignore_ascii_case(email)) {
            return Err("当前登录与这张卡片不符，请选择对应账号".into());
        }
    }
    save_snapshot(email, payload)?;
    let original = existing.unwrap_or_else(|| {
        json!({ "app": APP, "name": name, "created": chrono::Local::now().format("%Y-%m-%d").to_string(), "workdir": "" })
    });
    let meta = bind(original, email);
    reg::save_instance_checked(APP, &meta)
}

/// 换号：把目标账号凭据写回系统凭据 + 回退文件，再重启客户端。
fn materialize(name: &str, force: bool) -> Result<Value, Value> {
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let meta = card(name).map_err(failure)?;
    let email = meta["quota_user_id"].as_str().unwrap_or("").trim().to_string();
    if email.is_empty() {
        return Err(failure("该账号尚未登记登录凭据"));
    }
    let payload = snapshot_for_email(&email).map_err(failure)?;
    let serialized = serde_json::to_string(&payload).map_err(|_| failure("凭据序列化失败"))?;

    let restarted = stop(force)?;
    invalidate();

    // 写系统凭据 + 回退文件；两处都写成功才算切号完成。
    #[cfg(windows)]
    wincred::write(&serialized).map_err(failure)?;
    atomic_write(&fallback_path(), serialized.as_bytes()).map_err(failure)?;

    // 回读核验身份
    let verified = read_current_payload()
        .map(|cur| cur.email().is_some_and(|e| e.eq_ignore_ascii_case(&email)))
        .unwrap_or(false);
    if !verified {
        if restarted {
            let _ = launch_native();
        }
        return Err(failure("登录核验失败：凭据已写入但身份不符，请检查该账号的 refresh_token 是否有效"));
    }
    if restarted {
        launch_native().map_err(failure)?;
    }
    Ok(json!({ "msg": "账号已切换", "account": email }))
}

fn bind(mut meta: Value, email: &str) -> Value {
    let previous = meta["quota_user_id"].as_str().unwrap_or("").trim().to_lowercase();
    let same = previous == email.trim().to_lowercase();
    if !same {
        // 换了一个号，之前读到的额度（如果有）作废，绝不串号。
        let keep = ["quota_user_id", "quota_account", "quota_url", "quota_adapter", "quota_params", "quota_auto"];
        if let Some(fields) = meta.as_object_mut() {
            let remove: Vec<String> = fields
                .keys()
                .filter(|key| {
                    matches!(key.as_str(), "credits" | "credits_left" | "credits_total" | "expires_on")
                        || (key.starts_with("quota_") && !keep.contains(&key.as_str()))
                })
                .cloned()
                .collect();
            for key in remove {
                fields.remove(&key);
            }
        }
        meta["quota_local"] = json!(false);
        meta["quota_live_error"] = json!("尚未读取此账号额度");
    }
    meta["quota_user_id"] = json!(email);
    meta["quota_account"] = json!(email);
    meta["origin"] = json!("shared");
    if let Some(o) = meta.as_object_mut() {
        o.remove("released");
        o.remove("released_to");
        o.remove("pid");
    }
    meta
}

fn pids() -> Vec<u32> {
    procs::program_pids("antigravity.exe", &procs::snapshot_fresh())
}

fn stop(force: bool) -> Result<bool, Value> {
    let initial = pids();
    if initial.is_empty() {
        return Ok(false);
    }
    if !force {
        return Err(json!({ "ok": false, "need_force": true, "msg": "此操作需要关闭 Antigravity，请先保存当前任务。" }));
    }
    procs::kill_pids(&initial);
    for _ in 0..16 {
        if pids().is_empty() {
            return Ok(true);
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    Err(json!({ "ok": false, "msg": "Antigravity 尚未退出，凭据未修改" }))
}

fn launch_native() -> Result<Value, String> {
    let exe = reg::resolve_exe(&reg::app_def(APP).ok_or("未登记 Antigravity")?).ok_or("未找到 Antigravity 客户端")?;
    procs::launch_plain(&exe)?;
    Ok(json!({ "msg": "已打开 Antigravity" }))
}

pub fn status(meta: &Value) -> Value {
    let email = meta["quota_user_id"].as_str().unwrap_or("").trim().to_string();
    let current = read_current_payload().ok().and_then(|p| p.email());
    let is_current = !email.is_empty()
        && current.as_deref().is_some_and(|c| c.eq_ignore_ascii_case(&email));
    json!({
        "current": is_current,
        "account": email,
        "ready": !email.is_empty(),
        "shared_data": true,
        "multi_open": false,
        "auth_status": if secrets::available() {
            if email.is_empty() { "needs_login" } else { "ready" }
        } else { "locked" },
        "running": !pids().is_empty(),
    })
}

fn catalog() -> Value {
    json!({ "accounts": read_catalog() })
}

fn unregister(name: &str) -> Result<Value, String> {
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let meta = card(name)?;
    let email = meta["quota_user_id"].as_str().unwrap_or("").trim().to_string();
    let path = reg::instances_dir(APP).join(format!("{name}.json"));
    plain(&path)?;
    fs::remove_file(path).map_err(|_| "移除账号登记失败")?;
    if !email.is_empty() {
        // 登录凭据保留在保管箱里，只摘掉台账条目；用户要彻底删凭据再单独清保管箱。
        let mut catalog = read_catalog();
        catalog.retain(|item| item["email"].as_str().map(|e| !e.eq_ignore_ascii_case(&email)) != Some(true));
        let _ = atomic_write(&catalog_file(), &serde_json::to_vec_pretty(&catalog).unwrap_or_default());
    }
    Ok(json!({ "msg": "账号已移除，登录凭据与本机数据保留" }))
}

fn invalidate() {
    crate::machine::invalidate(APP);
    procs::invalidate_caches();
}

/// 给 machine.rs 的「本机现在登的是谁」用：直接读凭据管理器，返回格式与
/// local_quota::fetch 的成功结果对齐（account_only = true，无额度数字）。
pub fn machine_state() -> Value {
    match read_current_payload() {
        Ok(p) => {
            let email = p.email().unwrap_or_default();
            json!({
                "ok": true, "app": APP, "account": email, "user_id": email,
                "user_type": "", "left": null, "total": null, "expires_on": "",
                "source": "server", "stale": false, "live_error": "",
                "auth_from": fallback_path().display().to_string(),
                "account_only": true, "portions": [], "usage_only": false,
                "profile_dir": reg::profile_dir(APP).map(|p| p.display().to_string()).unwrap_or_default(),
            })
        }
        Err(e) => json!({ "ok": false, "app": APP, "err": e }),
    }
}

// ---------------------------------------------------------------------------
// 阶段 2：刷新 token + 读额度（Code Assist 后端）
// ---------------------------------------------------------------------------

#[cfg(windows)]
fn system_proxy() -> Option<String> {
    use std::os::windows::process::CommandExt;
    // 读与用户浏览器同一份 Windows 系统代理；代理地址不进日志、不进报错。
    let script = r#"$p=Get-ItemProperty -LiteralPath 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Internet Settings';if($p.ProxyEnable -eq 1){[Console]::Write([string]$p.ProxyServer)}"#;
    let output = std::process::Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-WindowStyle", "Hidden", "-Command", script])
        .creation_flags(0x0800_0000)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    proxy_address(String::from_utf8(output.stdout).ok()?.trim())
}
#[cfg(not(windows))]
fn system_proxy() -> Option<String> {
    None
}

fn proxy_address(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() || value.chars().any(char::is_whitespace) {
        return None;
    }
    let server = if value.contains('=') {
        value
            .split(';')
            .find_map(|part| part.strip_prefix("https="))
            .or_else(|| value.split(';').find_map(|part| part.strip_prefix("http=")))?
    } else {
        value
    };
    if server.is_empty() {
        None
    } else if server.contains("://") {
        Some(server.into())
    } else {
        Some(format!("http://{server}"))
    }
}

/// 带代理的 Agent：先试环境变量代理，再读 Windows 系统代理。
/// Google 的端点在大陆必须走代理，否则直连超时。
fn agent() -> ureq::Agent {
    let mut builder = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(25))
        .timeout_connect(Duration::from_secs(10))
        .try_proxy_from_env(true);
    let env_proxy = ["HTTPS_PROXY", "https_proxy", "HTTP_PROXY", "http_proxy", "ALL_PROXY", "all_proxy"]
        .iter()
        .any(|k| std::env::var(k).is_ok_and(|v| !v.trim().is_empty()));
    if !env_proxy {
        if let Some(address) = system_proxy() {
            if let Ok(proxy) = ureq::Proxy::new(address) {
                builder = builder.proxy(proxy);
            }
        }
    }
    builder.build()
}

/// 读本机 Antigravity 真实版本号（User-Agent 要用）。版本号旧会被 Google omit Gemini 组：
/// 实测 1.20.5 只出 3p 两档、2.11.0 四档全出。从安装目录 resources/app.asar 的
/// package.json 里读 version 字段，读不到回退 IDE_VERSION。
fn detect_ide_version() -> String {
    let asar = reg::app_def(APP)
        .and_then(|app| reg::resolve_exe(&app))
        .and_then(|exe| exe.parent().map(|d| d.join("resources").join("app.asar")))
        .filter(|p| p.is_file());
    if let Some(asar) = asar {
        if let Ok(bytes) = fs::read(&asar) {
            let head = &bytes[..bytes.len().min(512 * 1024)];
            if let Some(v) = first_version_in(head) {
                return v;
            }
        }
    }
    IDE_VERSION.to_string()
}

/// 在 app.asar 头部找第一个 `"version": "x.y.z"`（主 package.json 的 version 字段）。
fn first_version_in(bytes: &[u8]) -> Option<String> {
    let needle = b"\"version\"";
    let mut i = 0;
    while i + needle.len() <= bytes.len() {
        if &bytes[i..i + needle.len()] == needle {
            let mut j = i + needle.len();
            while j < bytes.len() && matches!(bytes[j], b' ' | b':' | b'\t' | b'\n' | b'\r') {
                j += 1;
            }
            if j < bytes.len() && bytes[j] == b'"' {
                j += 1;
                let start = j;
                while j < bytes.len() && bytes[j] != b'"' {
                    j += 1;
                }
                let v = String::from_utf8_lossy(&bytes[start..j]);
                let dots = v.chars().filter(|c| *c == '.').count();
                if dots >= 2 && !v.is_empty() && v.chars().all(|c| c.is_ascii_digit() || c == '.') {
                    return Some(v.to_string());
                }
            }
        }
        i += 1;
    }
    None
}

fn http_post_json(url: &str, access_token: &str, body: &Value) -> Result<Value, String> {
    // User-Agent 必须带 `antigravity/` 前缀，否则 retrieveUserQuotaSummary 直接 403。
    let ua = format!("antigravity/{} windows/amd64", detect_ide_version());
    let res = agent()
        .post(url)
        .set("Content-Type", "application/json")
        .set("Accept", "application/json")
        .set("Authorization", &format!("Bearer {access_token}"))
        .set("User-Agent", &ua)
        .set("x-goog-api-client", "gl-node/22.21.1")
        .timeout(Duration::from_secs(25))
        .send_string(&body.to_string());
    match res {
        Ok(r) => {
            let text = r.into_string().unwrap_or_default();
            serde_json::from_str(&text).map_err(|e| format!("响应不是 JSON：{e}"))
        }
        Err(ureq::Error::Status(code, r)) => {
            let text = r.into_string().unwrap_or_default();
            Err(format!("服务端拒绝（{code}）：{}", text.chars().take(200).collect::<String>()))
        }
        Err(e) => Err(format!("请求失败：{e}")),
    }
}

/// form-urlencoded 转义（只放行 RFC 3986 的非保留字符）。
fn urlencode(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' | '~' => c.to_string(),
            _ => format!("%{:02X}", c as u32),
        })
        .collect()
}

/// refresh_token 换 access_token + id_token（用 Antigravity 官方 client）。
fn refresh_access_token(refresh_token: &str) -> Result<(String, Option<String>), String> {
    let form = format!(
        "grant_type=refresh_token&client_id={}&client_secret={}&refresh_token={}",
        urlencode(OAUTH_CLIENT_ID),
        urlencode(OAUTH_CLIENT_SECRET),
        urlencode(refresh_token)
    );
    let res = agent()
        .post(TOKEN_ENDPOINT)
        .set("Content-Type", "application/x-www-form-urlencoded")
        .set("Accept", "application/json")
        .set("User-Agent", "agent-cockpit/2.1")
        .send_string(&form);
    let (status, text) = match res {
        Ok(r) => (r.status(), r.into_string().unwrap_or_default()),
        Err(ureq::Error::Status(code, r)) => (code, r.into_string().unwrap_or_default()),
        Err(e) => return Err(format!("刷新请求失败：{e}")),
    };
    if status >= 400 {
        let msg = serde_json::from_str::<Value>(&text)
            .ok()
            .and_then(|v| v["error_description"].as_str().or_else(|| v["error"].as_str()).map(str::to_string))
            .unwrap_or_else(|| text.chars().take(160).collect());
        return Err(format!("刷新被拒（{status}）：{msg}"));
    }
    let v: Value = serde_json::from_str(&text).map_err(|e| format!("刷新响应不是 JSON：{e}"))?;
    let access = v["access_token"].as_str().unwrap_or("").trim().to_string();
    if access.is_empty() {
        return Err("刷新成功但没有 access_token".into());
    }
    Ok((access, v["id_token"].as_str().map(str::to_string)))
}

fn load_code_assist(host: &str, access_token: &str) -> Result<Value, String> {
    // 实测（2026-10-28）：旧版 FULL_ELIGIBILITY_CHECK 请求体返回 allowedTiers/ineligibleTiers、
    // 且 free-tier 标 UNSUPPORTED_CLIENT；正确的请求体是只带 metadata.ideType=ANTIGRAVITY，
    // 响应里的 paidTier/currentTier 才是套餐名（Google AI Pro / Free 等）。
    let body = json!({
        "metadata": {
            "ideType": "ANTIGRAVITY",
            "ideVersion": IDE_VERSION,
            "platform": "WINDOWS_AMD64",
            "pluginType": "GEMINI"
        }
    });
    http_post_json(&format!("{host}/v1internal:loadCodeAssist"), access_token, &body)
}

fn collect_buckets(v: &Value, out: &mut Vec<Value>) {
    match v {
        Value::Object(o) => {
            if let Some(id) = o.get("bucketId").and_then(Value::as_str) {
                out.push(json!({
                    "bucketId": id,
                    "remainingFraction": o.get("remainingFraction").cloned().unwrap_or(Value::Null),
                    "resetTime": o.get("resetTime").cloned().unwrap_or(Value::Null),
                }));
            }
            for val in o.values() {
                collect_buckets(val, out);
            }
        }
        Value::Array(a) => {
            for val in a {
                collect_buckets(val, out);
            }
        }
        _ => {}
    }
}

fn format_reset_time(v: &Value) -> String {
    if let Some(num) = v.as_f64() {
        let secs = if num > 1e10 { num / 1000.0 } else { num };
        return chrono::DateTime::from_timestamp(secs as i64, 0)
            .map(|d| d.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M").to_string())
            .unwrap_or_default();
    }
    if let Some(s) = v.as_str() {
        if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s.trim()) {
            return dt.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M").to_string();
        }
        return s.trim().to_string();
    }
    String::new()
}

fn fraction_percent(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64().map(|f| (f * 100.0).round().clamp(0.0, 100.0)),
        Value::String(s) => s.trim().parse::<f64>().ok().map(|f| (f * 100.0).round().clamp(0.0, 100.0)),
        _ => None,
    }
}

fn read_quota_on(host: &str, access_token: &str) -> Result<(String, Vec<Value>), String> {
    let loaded = load_code_assist(host, access_token)?;
    let tier = ["paidTier", "currentTier"]
        .iter()
        .find_map(|k| loaded[k]["name"].as_str().filter(|s| !s.is_empty()))
        .unwrap_or("")
        .to_string();
    // 实测：retrieveUserQuotaSummary 传空对象 {}（不是 project），返回 groups[].buckets[]
    // （window=weekly/5h、bucketId、remainingFraction、resetTime）。
    let summary = http_post_json(
        &format!("{host}/v1internal:retrieveUserQuotaSummary"),
        access_token,
        &json!({}),
    )?;
    let mut buckets = Vec::new();
    collect_buckets(&summary, &mut buckets);
    let mut portions = Vec::new();
    for (id, label) in QUOTA_BUCKETS {
        let found = buckets.iter().find(|b| b["bucketId"].as_str() == Some(id));
        let (left, reset) = match found {
            Some(b) => (
                fraction_percent(&b["remainingFraction"]),
                format_reset_time(&b["resetTime"]),
            ),
            None => (None, String::new()),
        };
        portions.push(json!({
            "label": label,
            "pool": label,
            "left": left,
            "total": 100.0,
            "used": left.map(|l| 100.0 - l),
            "unit": "%",
            "expires_on": reset,
        }));
    }
    Ok((tier, portions))
}

/// 读额度：换 token → loadCodeAssist → fetchAvailableModels → retrieveUserQuotaSummary。
fn read_quota(payload: &Payload) -> Result<(String, Vec<Value>), String> {
    let rt = payload.clean_refresh_token();
    if rt.is_empty() {
        return Err("凭据里没有 refresh_token".into());
    }
    let (access, _id_token) = refresh_access_token(rt)?;
    let mut last_err = String::new();
    for host in CODE_ASSIST_HOSTS {
        match read_quota_on(host, &access) {
            Ok(v) => return Ok(v),
            Err(e) => last_err = e,
        }
    }
    Err(last_err)
}

/// 命令入口：读某张卡的额度并写回。
fn refresh_quota(name: &str) -> Result<Value, String> {
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let meta = card(name)?;
    let email = meta["quota_user_id"].as_str().unwrap_or("").trim().to_string();
    if email.is_empty() {
        return Err("该账号尚未登记登录凭据".into());
    }
    let payload = snapshot_for_email(&email)?;
    let (tier, portions) = read_quota(&payload)?;
    // 最紧一档 = 剩余百分比最低的那档（作概览数字）
    let tightest = portions
        .iter()
        .filter_map(|p| p["left"].as_f64())
        .fold(None::<f64>, |acc, x| Some(match acc {
            Some(a) => a.min(x),
            None => x,
        }));
    let tightest_reset = portions
        .iter()
        .find(|p| p["left"].as_f64() == tightest)
        .and_then(|p| p["expires_on"].as_str())
        .unwrap_or("")
        .to_string();
    let q = crate::local_quota::Quota {
        user_id: email.clone(),
        account: if tier.is_empty() { email.clone() } else { format!("{email} · {tier}") },
        user_type: tier.clone(),
        left: tightest,
        total: Some(100.0),
        expires_on: tightest_reset,
        exceeded: false,
        portions: portions.clone(),
        groups: portions
            .iter()
            .map(|p| json!({
                "name": p["label"].as_str().unwrap_or(""),
                "unit": "%",
                "left": p["left"].clone(),
                "total": 100.0,
                "used": p["used"].clone(),
                "items": [p.clone()],
            }))
            .collect(),
        plan_name: tier,
        unit: "%".into(),
        kind: "full".into(),
        used: 0.0,
        sessions: 0,
        last_active: String::new(),
        source: "server".into(),
        live_error: String::new(),
        auth_from: "凭据管理器".into(),
        trend: Value::Null,
    };
    let latest = card(name)?;
    if latest["quota_user_id"].as_str().unwrap_or("") != email {
        return Err("账号登录已变化，请重新读取".into());
    }
    let next = crate::local_quota::apply(&latest, &q);
    reg::save_instance_checked(APP, &next)?;
    Ok(json!({
        "msg": format!("额度已更新：最紧一档剩 {}%", tightest.map(|t| t as i64).unwrap_or(0)),
        "quota_state": "ready",
    }))
}

// ---------------------------------------------------------------------------
// 阶段 3：OAuth 浏览器授权（PKCE + 本地回调服务器）
// ---------------------------------------------------------------------------

const OAUTH_AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const OAUTH_SCOPES: &str = "openid email profile https://www.googleapis.com/auth/cloud-platform https://www.googleapis.com/auth/cclog https://www.googleapis.com/auth/experimentsandconfigs";

/// 系统密码学随机源。失败时拒绝，不退回可预测随机数。
#[cfg(windows)]
fn secure_random(bytes: &mut [u8]) -> Result<(), String> {
    #[link(name = "bcrypt")]
    unsafe extern "system" {
        fn BCryptGenRandom(algorithm: *mut std::ffi::c_void, buffer: *mut u8, len: u32, flags: u32) -> i32;
    }
    let len = u32::try_from(bytes.len()).map_err(|_| "随机数长度超限".to_string())?;
    let status = unsafe { BCryptGenRandom(std::ptr::null_mut(), bytes.as_mut_ptr(), len, 2) };
    if status < 0 { Err("系统安全随机数不可用".into()) } else { Ok(()) }
}
#[cfg(not(windows))]
fn secure_random(bytes: &mut [u8]) -> Result<(), String> {
    use std::io::Read;
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(bytes))
        .map_err(|e| format!("随机数不可用：{e}"))
}

fn b64url(bytes: &[u8]) -> String {
    secrets::b64_encode(bytes).trim_end_matches('=').replace('+', "-").replace('/', "_")
}

fn random_token(len: usize) -> String {
    let mut b = vec![0u8; len];
    if secure_random(&mut b).is_err() {
        return String::new();
    }
    b64url(&b)
}

struct OAuthFlow {
    state: String,
    name: String,
    status: String, // waiting / success / error
    detail: String, // 成功=email，失败=错误信息
}
static OAUTH: std::sync::OnceLock<std::sync::Mutex<Option<OAuthFlow>>> = std::sync::OnceLock::new();

fn set_oauth_status(status: &str, detail: &str) {
    if let Ok(mut g) = OAUTH.get_or_init(|| std::sync::Mutex::new(None)).lock() {
        if let Some(f) = g.as_mut() {
            f.status = status.to_string();
            f.detail = detail.to_string();
        }
    }
}

fn open_browser(url: &str) -> Result<(), String> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let windir = std::env::var("WINDIR").unwrap_or_else(|_| "C:\\Windows".into());
        std::process::Command::new(std::path::PathBuf::from(&windir).join("explorer.exe"))
            .arg(url)
            .creation_flags(0x0800_0000)
            .spawn()
            .map_err(|e| format!("打开浏览器失败：{e}"))?;
    }
    #[cfg(not(windows))]
    {
        let _ = std::process::Command::new("xdg-open").arg(url).spawn();
    }
    Ok(())
}

/// authorization_code 换 access_token + refresh_token + id_token。
fn exchange_code(code: &str, verifier: &str, redirect: &str) -> Result<(String, String, Option<String>), String> {
    let form = format!(
        "grant_type=authorization_code&client_id={}&client_secret={}&redirect_uri={}&code={}&code_verifier={}",
        urlencode(OAUTH_CLIENT_ID),
        urlencode(OAUTH_CLIENT_SECRET),
        urlencode(redirect),
        urlencode(code),
        urlencode(verifier)
    );
    let res = agent()
        .post(TOKEN_ENDPOINT)
        .set("Content-Type", "application/x-www-form-urlencoded")
        .set("Accept", "application/json")
        .send_string(&form);
    let (status, text) = match res {
        Ok(r) => (r.status(), r.into_string().unwrap_or_default()),
        Err(ureq::Error::Status(code, r)) => (code, r.into_string().unwrap_or_default()),
        Err(e) => return Err(format!("授权码交换失败：{e}")),
    };
    if status >= 400 {
        let msg = serde_json::from_str::<Value>(&text)
            .ok()
            .and_then(|v| v["error_description"].as_str().or_else(|| v["error"].as_str()).map(str::to_string))
            .unwrap_or_else(|| text.chars().take(160).collect());
        return Err(format!("授权码交换被拒（{status}）：{msg}"));
    }
    let v: Value = serde_json::from_str(&text).map_err(|e| format!("授权码交换响应不是 JSON：{e}"))?;
    let access = v["access_token"].as_str().unwrap_or("").trim().to_string();
    let refresh = v["refresh_token"].as_str().unwrap_or("").trim().to_string();
    if access.is_empty() || refresh.is_empty() {
        return Err("授权码交换没有返回 access_token 或 refresh_token".into());
    }
    Ok((access, refresh, v["id_token"].as_str().map(str::to_string)))
}

/// 授权成功后：写凭据（系统凭据 + 回退文件）+ 存台账 + 绑定卡片。
fn finish_oauth(name: &str, access_token: &str, refresh_token: &str, id_token: Option<String>) -> Result<String, String> {
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let email = id_token.as_deref().and_then(email_from_id_token).ok_or("授权返回的 id_token 里没有 email")?;
    let expiry = (chrono::Utc::now() + chrono::Duration::seconds(3600)).to_rfc3339_opts(chrono::SecondsFormat::Micros, true);
    let payload = Payload {
        token: Token {
            access_token: access_token.to_string(),
            token_type: "Bearer".to_string(),
            refresh_token: refresh_token.to_string(),
            expiry,
        },
        auth_method: "consumer".to_string(),
        id_token,
    };
    let serialized = serde_json::to_string(&payload).map_err(|_| "凭据序列化失败")?;
    #[cfg(windows)]
    wincred::write(&serialized)?;
    atomic_write(&fallback_path(), serialized.as_bytes())?;
    register(name, &email, &payload)?;
    invalidate();
    Ok(email)
}

/// 启动 OAuth 授权：开浏览器，后台等本地回调、换 token、登记账号。
pub fn oauth_start(name: &str) -> Result<Value, String> {
    if name.is_empty() || reg::bad_account_name(name).is_some() {
        return Err("账号名称无效".into());
    }
    if let Ok(g) = OAUTH.get_or_init(|| std::sync::Mutex::new(None)).lock() {
        if g.as_ref().is_some_and(|f| f.status == "waiting") {
            return Err("已有一次授权在进行中，请先完成或取消".into());
        }
    }
    let state = random_token(32);
    let verifier = random_token(64);
    if state.is_empty() || verifier.is_empty() {
        return Err("无法生成安全随机数，无法授权".into());
    }
    let challenge = b64url(&crate::wbproxy::sha256(verifier.as_bytes()));

    let server = tiny_http::Server::http("127.0.0.1:0").map_err(|e| format!("启动回调监听失败：{e}"))?;
    let port = server.server_addr().to_ip().ok_or("获取回调端口失败")?.port();
    let redirect = format!("http://127.0.0.1:{port}/oauth-callback");
    let url = format!(
        "{OAUTH_AUTH_URL}?client_id={}&redirect_uri={}&response_type=code&scope={}&code_challenge={}&code_challenge_method=S256&state={}&access_type=offline&prompt=consent",
        urlencode(OAUTH_CLIENT_ID),
        urlencode(&redirect),
        urlencode(OAUTH_SCOPES),
        urlencode(&challenge),
        urlencode(&state),
    );
    {
        let slot = OAUTH.get_or_init(|| std::sync::Mutex::new(None));
        let mut g = slot.lock().unwrap_or_else(|e| e.into_inner());
        *g = Some(OAuthFlow { state: state.clone(), name: name.to_string(), status: "waiting".into(), detail: String::new() });
    }
    let state2 = state.clone();
    let verifier2 = verifier.clone();
    let redirect2 = redirect.clone();
    let name2 = name.to_string();
    std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + Duration::from_secs(600);
        loop {
            if std::time::Instant::now() > deadline {
                set_oauth_status("error", "授权超时，请重试");
                break;
            }
            match server.recv_timeout(Duration::from_secs(1)) {
                Ok(Some(req)) => {
                    let req_url = req.url().to_string();
                    let path = req_url.split('?').next().unwrap_or("");
                    if path == "/oauth-callback" {
                        let q = req_url.split('?').nth(1).unwrap_or("");
                        let code = query_param(q, "code");
                        let got_state = query_param(q, "state");
                        let (status, detail) = if got_state.as_deref() != Some(state2.as_str()) {
                            ("error".to_string(), "回调 state 不匹配".to_string())
                        } else if let Some(c) = code {
                            match exchange_code(&c, &verifier2, &redirect2) {
                                Ok((at, rt, idt)) => match finish_oauth(&name2, &at, &rt, idt) {
                                    Ok(email) => ("success".to_string(), email),
                                    Err(e) => ("error".to_string(), e),
                                },
                                Err(e) => ("error".to_string(), e),
                            }
                        } else {
                            ("error".to_string(), "回调里没有授权码".to_string())
                        };
                        let body = if status == "success" {
                            format!("<html><body style=\"font-family:sans-serif;padding:24px\"><h2>授权成功</h2><p>账号 {} 已登记，可关闭此页返回驾驶舱。</p></body></html>", detail)
                        } else {
                            format!("<html><body style=\"font-family:sans-serif;padding:24px\"><h2>授权失败</h2><p>{}</p></body></html>", detail)
                        };
                        let _ = req.respond(
                            tiny_http::Response::from_string(body).with_header(
                                tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"text/html; charset=utf-8"[..]).unwrap(),
                            ),
                        );
                        set_oauth_status(&status, &detail);
                        break;
                    } else {
                        let _ = req.respond(tiny_http::Response::from_string("not found").with_status_code(404));
                    }
                }
                Ok(None) => {}
                Err(_) => {
                    set_oauth_status("error", "回调监听中断");
                    break;
                }
            }
        }
    });

    open_browser(&url)?;
    Ok(json!({ "url": url, "port": port }))
}

fn query_param(q: &str, key: &str) -> Option<String> {
    q.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        if k == key { Some(percent_decode(v)) } else { None }
    })
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        if bytes[i] == b'+' {
            out.push(b' ');
        } else {
            out.push(bytes[i]);
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub fn oauth_status() -> Value {
    let g = OAUTH.get_or_init(|| std::sync::Mutex::new(None)).lock().unwrap_or_else(|e| e.into_inner());
    match g.as_ref() {
        Some(f) => json!({ "status": f.status, "detail": f.detail, "name": f.name }),
        None => json!({ "status": "idle", "detail": "", "name": "" }),
    }
}

// ---------------------------------------------------------------------------
// 命令分发
// ---------------------------------------------------------------------------

pub fn action(act: &str, p: &Value) -> Option<Value> {
    if p["app"] != APP {
        return None;
    }
    let supported = matches!(
        act,
        "antigravity_accounts"
            | "antigravity_capture"
            | "antigravity_import"
            | "antigravity_status"
            | "antigravity_switch"
            | "antigravity_remove"
            | "new_instance"
            | "auth_switch"
            | "swap_set"
            | "delete"
            | "local_read"
            | "pull_local"
            | "quota_refresh"
            | "restore"
            | "antigravity_oauth_start"
            | "antigravity_oauth_status"
    );
    if !supported {
        return None;
    }
    let name = p["name"].as_str().unwrap_or("");
    let force = p["force"].as_bool().unwrap_or(false);
    let result: Result<Value, Value> = match act {
        "antigravity_accounts" => Ok(catalog()),
        "antigravity_capture" | "new_instance" => capture(name).map_err(failure),
        "antigravity_import" => import(name, p).map_err(failure),
        "antigravity_status" => card(name).map(|m| json!({ "antigravity_auth": status(&m) })).map_err(failure),
        "antigravity_switch" | "auth_switch" | "swap_set" | "restore" => materialize(name, force),
        "antigravity_remove" | "delete" => unregister(name).map_err(failure),
        "local_read" | "pull_local" | "quota_refresh" => refresh_quota(name).map_err(failure),
        "antigravity_oauth_start" => oauth_start(name).map_err(failure),
        "antigravity_oauth_status" => Ok(oauth_status()),
        _ => Err(failure("Antigravity 共享本机登录态，请用账号登录管理")),
    };
    Some(match result {
        Ok(mut v) => {
            v["ok"] = json!(true);
            v
        }
        Err(v) => v,
    })
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn b64url_decodes_jwt_payload_without_padding() {
        // "eyJlbWFpbCI6InhAeS5jb20ifQ" = base64url({"email":"x@y.com"})，无 padding
        let bytes = b64url_decode("eyJlbWFpbCI6InhAeS5jb20ifQ").unwrap();
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["email"], "x@y.com");
    }

    #[test]
    fn email_extracted_from_id_token() {
        let header = "e30"; // {}
        let payload = b64url_encode(br#"{"email":"someone@gmail.com","sub":"1"}"#);
        let id_token = format!("{header}.{payload}.sig");
        assert_eq!(email_from_id_token(&id_token).as_deref(), Some("someone@gmail.com"));
    }

    #[test]
    fn refresh_token_splits_project_id_suffix() {
        let p = Payload {
            token: Token {
                access_token: String::new(),
                token_type: "Bearer".into(),
                refresh_token: "1//0gAbCdEf|rising-fact-p41fc".into(),
                expiry: String::new(),
            },
            auth_method: "consumer".into(),
            id_token: None,
        };
        assert_eq!(p.clean_refresh_token(), "1//0gAbCdEf");
    }

    #[test]
    fn payload_email_comes_from_id_token_only() {
        let p = Payload {
            token: Token {
                access_token: String::new(),
                token_type: "Bearer".into(),
                refresh_token: "1//x".into(),
                expiry: String::new(),
            },
            auth_method: "consumer".into(),
            id_token: None,
        };
        assert!(p.email().is_none());
    }

    #[test]
    fn parse_payload_rejects_missing_refresh_token() {
        let bad = r#"{"token":{"access_token":"a"},"auth_method":"consumer"}"#;
        assert!(parse_payload(bad).is_err());
        let good = r#"{"token":{"refresh_token":"1//x"},"auth_method":"consumer","id_token":"e30.e30.e30"}"#;
        assert!(parse_payload(good).is_ok());
    }

    fn b64url_encode(bytes: &[u8]) -> String {
        let std = secrets::b64_encode(bytes);
        std.trim_end_matches('=').replace('+', "-").replace('/', "_")
    }
}
