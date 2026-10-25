// CLI 类产品的账号登录管理（Codex 起）。
//
// 多开与切换靠 CODEX_HOME 指向每个账号自己的目录（launch_cli 注入的就是它），
// 登录态同样落在那一个目录里的 auth.json——所以"换号"对 CLI 产品就是换 CODEX_HOME。
//
// 官方三种登录口径（`codex login --help` 实测 0.163.0）：
//   ① 无参        → ChatGPT 账号 OAuth，浏览器授权后 tokens 落 auth.json
//   ② --device-auth → 设备码授权（没有浏览器/SSH 场景）
//   ③ --with-api-key → 从 stdin 读 OPENAI_API_KEY，auth_mode=apikey
// 全部通过官方命令完成，不自己拼 auth.json——格式是厂商的，拼错了它自己都不认。
use crate::registry::{self as reg};
use serde_json::{json, Value};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Codex 可执行文件：registry 里登记的第一个存在的候选（原生 exe 优先，.cmd 兜底）
pub fn codex_exe() -> Option<PathBuf> {
    reg::resolve_exe(&reg::app_def("codex")?)
}

/// 密文/key 只露头尾：界面要能认出"是哪一个"，又不能整串照抄
fn mask(s: &str) -> String {
    let n = s.chars().count();
    if n <= 12 {
        return "***".to_string();
    }
    let head: String = s.chars().take(7).collect();
    let tail: String = s.chars().skip(n - 4).collect();
    format!("{}***{}", head, tail)
}

/// 读这个账号目录的登录态：auth.json + `codex login status` 双来源。
/// auth.json 给 mode 与 key 的打码串；status 给一句人话（"Logged in using ChatGPT …"）。
pub fn status(data_dir: &Path) -> Value {
    let auth_file = data_dir.join("auth.json");
    let mut mode = Value::Null;
    let mut key = Value::Null;
    let mut account = Value::Null;
    if let Ok(s) = std::fs::read_to_string(&auth_file) {
        if let Ok(v) = serde_json::from_str::<Value>(&s) {
            mode = json!(v["auth_mode"].as_str().unwrap_or(""));
            if let Some(k) = v["OPENAI_API_KEY"].as_str() {
                key = json!(mask(k));
            }
            // ChatGPT 模式的账号信息在 tokens 里，字段名随版本漂移，只认常见的几个
            for p in [
                &["tokens", "account", "email"][..],
                &["tokens", "email"][..],
                &["account", "email"][..],
            ] {
                if let Some(e) = v.pointer(&p.join("/")).and_then(|x| x.as_str()) {
                    account = json!(e);
                    break;
                }
            }
        }
    }
    let text = run(data_dir, &["login", "status"])
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    if !text.is_empty() && text.contains("ChatGPT") {
        mode = json!("chatgpt");
    }
    json!({
        "mode": mode,
        "key": key,
        "account": account,
        "text": text,
        "has_auth": auth_file.exists(),
    })
}

/// 跑一条 codex 子命令，拿回 stdout。CODEX_HOME 必须显式给：
/// 不指定它就落到 ~/.codex，读/写的会是本机默认那个号的登录态。
fn run(data_dir: &Path, args: &[&str]) -> Result<String, String> {
    let exe = match codex_exe() {
        Some(e) => e,
        None => return Err("找不到 Codex 可执行文件".into()),
    };
    let out = Command::new(&exe)
        .args(args)
        .env("CODEX_HOME", data_dir)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("跑 codex 失败：{e}"))?;
    let s = String::from_utf8_lossy(&out.stdout).to_string();
    let e = String::from_utf8_lossy(&out.stderr).trim().to_string();
    if out.status.success() {
        return Ok(s);
    }
    Err(if e.is_empty() { s } else { e }.lines().take(3).collect::<Vec<_>>().join(" "))
}

/// API Key 登录：key 走 stdin，不出现在命令行/进程表里
pub fn login_apikey(data_dir: &Path, key: &str) -> Result<String, String> {
    let key = key.trim();
    if key.is_empty() {
        return Err("API Key 是空的".into());
    }
    let exe = codex_exe().ok_or("找不到 Codex 可执行文件")?;
    let mut child = Command::new(&exe)
        .args(["login", "--with-api-key"])
        .env("CODEX_HOME", data_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("启动 codex 失败：{e}"))?;
    if let Some(si) = child.stdin.take() {
        let mut si = si;
        let _ = si.write_all(key.as_bytes());
        // 写完就关，codex 才知道输入结束
    }
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    if out.status.success() {
        return Ok("已用 API Key 登录".into());
    }
    let e = String::from_utf8_lossy(&out.stderr).trim().to_string();
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    Err((if e.is_empty() { s } else { e })
        .lines()
        .find(|l| !l.contains("WARNING:"))
        .unwrap_or("登录失败")
        .to_string())
}

/// ChatGPT 账号 OAuth / 设备码登录：都要用户在一次交互里完成（浏览器授权或输设备码），
/// 所以起一个**新的终端窗口**跑，把 CODEX_HOME 指向这个账号。终端关掉即授权流程结束。
pub fn login_browser(data_dir: &Path, device: bool) -> Result<u32, String> {
    let exe = codex_exe().ok_or("找不到 Codex 可执行文件")?;
    let mut args = vec!["login".to_string()];
    if device {
        args.push("--device-auth".into());
    }
    let inner = format!(
        "title Codex 登录（这个窗口关掉即结束） && set \"CODEX_HOME={}\" && \"{}\" {}",
        data_dir.display(),
        exe.display(),
        args[1..].join(" ")
    );
    let mut c = Command::new("cmd");
    crate::procs::scrub_env_pub(&mut c);
    c.args(["/K", &inner]);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(0x00000010 /*CREATE_NEW_CONSOLE*/);
    }
    c.spawn().map(|ch| ch.id()).map_err(|e| e.to_string())
}

/// 退出登录：删掉这个账号目录的 auth.json。官方没有 logout 子命令
/// （`codex login --help` 只列 status/help），文件删除后 status 即报未登录。
pub fn logout(data_dir: &Path) -> Result<String, String> {
    let f = data_dir.join("auth.json");
    if !f.exists() {
        return Err("这个账号本来就没有登录态".into());
    }
    std::fs::remove_file(&f).map_err(|e| format!("删除 auth.json 失败：{e}"))?;
    Ok("已退出登录".into())
}
