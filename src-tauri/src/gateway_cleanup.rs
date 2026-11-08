//! Disconnecting a client must also remove the models the gateway left behind.
//!
//! Trae keeps its own cached model list in SQLite. Once the gateway stops or an
//! Agent is disconnected, those entries still point at a port nobody listens
//! on, and the user has to delete each of them by hand. Cleanup runs at every
//! explicit detach point so that never happens.
//!
//! Two rules keep it safe:
//! 1. Only entries whose base URL names this machine's gateway are removed.
//!    A custom provider the user added by hand is left alone.
//! 2. A client that is still running keeps its own file. Its cleanup is
//!    remembered and retried after it exits, instead of rewriting a database
//!    another process holds open.
use crate::{procs, registry as reg};
use serde_json::{json, Value};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

type Result<T> = std::result::Result<T, String>;
const SCRIPT: &str = include_str!("../scripts/trae-gateway-clean.py");
const BOOTSTRAP: &str = "import io,json,sys; p=json.loads(sys.stdin.buffer.read().decode('utf-8')); sys.stdin=io.StringIO(json.dumps(p['request'])); exec(compile(p['script'],'<AgentCockpit gateway cleanup>','exec'),{'__name__':'__main__'})";
const CREATE_NO_WINDOW: u32 = 0x08000000;
const PENDING: &str = "gateway-cleanup-pending.json";
const TRAE_APPS: &[&str] = &["traework-cn", "trae", "trae-solo", "trae-solo-cn"];

fn pending_file() -> PathBuf {
    reg::base_dir().join(PENDING)
}

fn read_pending() -> Vec<String> {
    fs::read_to_string(pending_file())
        .ok()
        .and_then(|s| serde_json::from_str::<Vec<String>>(&s).ok())
        .unwrap_or_default()
}

fn write_pending(apps: &[String]) {
    let mut unique: Vec<String> = Vec::new();
    for app in apps {
        if !unique.iter().any(|x| x == app) {
            unique.push(app.clone());
        }
    }
    let text = serde_json::to_string(&unique).unwrap_or_else(|_| "[]".into());
    let _ = fs::create_dir_all(reg::base_dir());
    let _ = fs::write(pending_file(), text);
}

fn mark_pending(app: &str) {
    let mut apps = read_pending();
    if !apps.iter().any(|x| x == app) {
        apps.push(app.to_string());
        write_pending(&apps);
    }
}

fn clear_pending(app: &str) {
    let mut apps = read_pending();
    apps.retain(|x| x != app);
    write_pending(&apps);
}

/// Trae keeps its cached provider list in `User/globalStorage/state.vscdb`.
/// Trae CN and TRAE SOLO CN share that layout, so both are handled alike.
fn trae_dirs() -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Some(appdata) = std::env::var_os("APPDATA").map(PathBuf::from) else {
        return out;
    };
    for name in ["Trae CN", "TRAE SOLO CN", "Trae", "TRAE SOLO"] {
        let dir = appdata.join(name);
        let db = dir.join("User").join("globalStorage").join("state.vscdb");
        if db.is_file() && !out.iter().any(|p| p == &dir) {
            out.push(dir);
        }
    }
    out
}

fn gateway_hosts(port: u16) -> Vec<String> {
    vec![
        format!("127.0.0.1:{port}"),
        format!("localhost:{port}"),
        format!("[::1]:{port}"),
    ]
}

/// Run the bundled interpreter with the embedded cleanup script. It rewrites
/// only the cached model list and the selection keys that named a removed
/// model; credentials and conversations are never read.
fn run_script(path: &Path, port: u16, limit: Duration) -> Result<Value> {
    let python = crate::usage::bundled_python()?;
    let mut command = Command::new(python);
    command
        .args(["-I", "-X", "utf8", "-c", BOOTSTRAP])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    let mut child = command
        .spawn()
        .map_err(|e| format!("无法启动内置运行环境：{e}"))?;
    let request = json!({"script": SCRIPT, "request": {
        "path": path.to_string_lossy(), "hosts": gateway_hosts(port),
    }});
    let sent = child
        .stdin
        .take()
        .map(|mut input| input.write_all(request.to_string().as_bytes()).is_ok())
        .unwrap_or(false);
    if !sent {
        let _ = child.kill();
        let _ = child.wait();
        return Err("无法向清理环境传递数据".into());
    }
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if started.elapsed() < limit => {
                std::thread::sleep(Duration::from_millis(50))
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("清理模型列表超时".into());
            }
            Err(e) => return Err(format!("无法等待清理完成：{e}")),
        }
    }
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    if !out.status.success() {
        let why: String = String::from_utf8_lossy(&out.stderr)
            .trim()
            .chars()
            .take(200)
            .collect();
        return Err(if why.is_empty() {
            "清理模型列表失败".into()
        } else {
            format!("清理模型列表失败：{why}")
        });
    }
    serde_json::from_slice(&out.stdout).map_err(|e| format!("清理结果格式不符：{e}"))
}

/// Every executable Trae may run under. A cached database can only be rewritten
/// while none of them holds it open.
fn trae_running(snap: &std::collections::HashMap<u32, procs::ProcInfo>) -> bool {
    let mut names: Vec<String> = reg::lockers("traework-cn")
        .iter()
        .map(|n| n.to_lowercase())
        .collect();
    for app in TRAE_APPS {
        if let Some(def) = reg::app_def(app) {
            names.push(reg::exe_name(&def).to_lowercase());
        }
    }
    names
        .iter()
        .any(|n| !n.is_empty() && !procs::program_pids(n, snap).is_empty())
}

fn label(app: &str) -> String {
    reg::app_def(app)
        .map(|a| a.name)
        .unwrap_or_else(|| app.to_string())
}

/// Clients that keep their own cached model list instead of a JSON file.
fn cached_model_client(app: &str) -> bool {
    TRAE_APPS.contains(&app)
}

/// Remove gateway models from every Trae profile on this machine.
fn clean_trae(port: u16, limit: Duration) -> Result<usize> {
    let mut removed = 0;
    for dir in trae_dirs() {
        let db = dir.join("User").join("globalStorage").join("state.vscdb");
        let result = run_script(&db, port, limit)?;
        removed += result["removed"].as_u64().unwrap_or(0) as usize;
    }
    Ok(removed)
}

/// Undo a client connection and drop its cached gateway models.
fn clean_client(app: &str, port: u16, limit: Duration) -> Result<usize> {
    if cached_model_client(app) {
        return clean_trae_once(port, limit, app);
    }
    if app == "claude-desktop" {
        // 桌面版不走 client_connect 的 JSON 档案，它自己那套 Claude-3p
        // 配置库由 claude_gateway 管；这里只转一手，别写第二套还原逻辑。
        if procs::agent_running(app, &procs::snapshot_fresh()) {
            mark_pending(app);
            return Err(format!("{} 还在运行，退出后会自动清理", label(app)));
        }
        let restored = crate::claude_gateway::action(
            "disconnect",
            &format!("http://127.0.0.1:{port}"),
            "",
            "",
        )?;
        if restored["ok"].as_bool() == Some(true) {
            clear_pending(app);
        }
        return Ok(0);
    }
    let Some((_, path)) = crate::client_connect::connected(&[app]).into_iter().next() else {
        if orphan_path(app).is_none() {
            clear_pending(app);
            return Ok(0);
        }
        if procs::agent_running(app, &procs::snapshot_fresh()) {
            mark_pending(app);
            return Err(format!("{} 还在运行，退出后会自动清理", label(app)));
        }
        let removed = purge_orphans(app, port)?;
        clear_pending(app);
        return Ok(removed);
    };
    if procs::agent_running(app, &procs::snapshot_fresh()) {
        mark_pending(app);
        return Err(format!("{} 还在运行，退出后会自动清理", label(app)));
    }
    if crate::client_connect::disconnect(&path)? {
        clear_pending(app);
    }
    Ok(0)
}

/// Plain-JSON configuration files that can be purged directly when no restore
/// record exists. Paths mirror `client_connect::target` for the default profile.
fn orphan_path(app: &str) -> Option<PathBuf> {
    let home = reg::home();
    let path = match app {
        "workbuddy" => home.join(".workbuddy").join("models.json"),
        "workbuddy-intl" => home.join(".workbuddy-ai").join("models.json"),
        "qoder" => {
            let dir = std::env::var_os("QODER_CONFIG_DIR")
                .map(PathBuf::from)
                .filter(|p| p.is_absolute())
                .unwrap_or_else(|| home.join(".qoder"));
            dir.join("settings.json")
        }
        "qoder-cn" => home.join(".qoder-cn").join("settings.json"),
        _ => return None,
    };
    Some(path)
}

fn has_gateway_url(value: &Value, hosts: &[String]) -> bool {
    value
        .as_str()
        .map(|url| {
            let url = url.to_ascii_lowercase();
            hosts.iter().any(|host| url.contains(host))
        })
        .unwrap_or(false)
}

/// Drop every entry whose endpoint points at this machine's gateway. Entries
/// the user added with other endpoints stay untouched.
fn purge_value(doc: &mut Value, hosts: &[String]) -> usize {
    if let Some(arr) = doc.as_array_mut() {
        let before = arr.len();
        arr.retain(|entry| !has_gateway_url(&entry["url"], hosts));
        return before - arr.len();
    }
    if let Some(models) = doc.get_mut("models").and_then(Value::as_array_mut) {
        let before = models.len();
        models.retain(|entry| !has_gateway_url(&entry["url"], hosts));
        return before - models.len();
    }
    if let Some(providers) = doc.get_mut("providers").and_then(Value::as_object_mut) {
        let before = providers.len();
        providers.retain(|_, entry| !has_gateway_url(&entry["options"]["baseURL"], hosts));
        return before - providers.len();
    }
    0
}

/// Fallback when no restore record exists: the client rewrote its file on exit
/// after a restore consumed the record, resurrecting dead entries.
fn purge_orphans(app: &str, port: u16) -> Result<usize> {
    let Some(path) = orphan_path(app) else {
        return Ok(0);
    };
    let Some(text) = crate::client_connect::read(&path)? else {
        return Ok(0);
    };
    let mut doc: Value = serde_json::from_str(&text)
        .map_err(|_| "配置不是合法 JSON，跳过兜底清理".to_string())?;
    let hosts = gateway_hosts(port);
    let removed = purge_value(&mut doc, &hosts);
    if removed == 0 {
        return Ok(0);
    }
    if crate::client_connect::read(&path)?.as_deref() != Some(text.as_str()) {
        return Err("配置刚被修改，请重试".into());
    }
    let out = serde_json::to_string_pretty(&doc).map_err(|e| e.to_string())?;
    crate::client_connect::write(&path, &out)?;
    Ok(removed)
}

/// Trae keeps one shared cache per profile, so a sweep cleans it once no
/// matter how many Trae variants were named.
fn clean_trae_once(port: u16, limit: Duration, app: &str) -> Result<usize> {
    if trae_running(&procs::snapshot_fresh()) {
        mark_pending(app);
        return Err(format!("{} 还在运行，退出后会自动清理", label(app)));
    }
    let removed = clean_trae(port, limit)?;
    clear_pending(app);
    Ok(removed)
}

/// Clients that still hold a restore record beside their configuration.
fn attached_clients() -> Vec<String> {
    let apps: Vec<&str> = crate::client_connect::CLIENTS
        .iter()
        .map(|(app, _)| *app)
        .collect();
    let mut out: Vec<String> = crate::client_connect::connected(&apps)
        .into_iter()
        .map(|(app, _)| app)
        .collect();
    // Claude 桌面版把配置放在 Claude-3p 的资料库里，不产生 client_connect
    // 那种恢复记录，得单独问它一次有没有在管。
    if crate::claude_gateway::managed() {
        out.push("claude-desktop".into());
    }
    // 兜底清理的客户端没有恢复记录，不能等记录出现才查：客户端退出时
    // 整份写回配置，会把还原过的条目复活，所以每轮清扫都要看一眼。
    for app in ["workbuddy", "workbuddy-intl", "qoder", "qoder-cn"] {
        if !out.iter().any(|x| x == app) {
            out.push(app.into());
        }
    }
    out
}

/// Called when an Agent is disconnected or the gateway stops. `only` limits the
/// sweep to one client; `None` covers every client the gateway touched.
pub fn sweep(port: u16, only: Option<&str>) -> Value {
    sweep_within(port, only, Duration::from_secs(30))
}

/// Same sweep with an explicit time budget. Application exit uses a short one
/// so the process can still shut down; unfinished work stays pending.
pub fn sweep_within(port: u16, only: Option<&str>, limit: Duration) -> Value {
    let mut removed = 0usize;
    let mut notes = Vec::new();
    match only {
        Some(app) => match clean_client(app, port, limit) {
            Ok(n) => removed += n,
            Err(e) => notes.push(e),
        },
        None => {
            for app in attached_clients() {
                if let Err(e) = clean_client(&app, port, limit) {
                    notes.push(e);
                }
            }
            match clean_trae_once(port, limit, "traework-cn") {
                Ok(n) => removed += n,
                Err(e) => notes.push(e),
            }
        }
    }
    json!({"removed": removed, "pending": read_pending(), "notes": notes})
}

/// Startup and client-exit retry: pending entries only.
pub fn retry_pending(port: u16) -> Value {
    let pending = read_pending();
    if pending.is_empty() {
        return json!({"removed": 0, "pending": [], "notes": []});
    }
    let mut removed = 0usize;
    let mut notes = Vec::new();
    let mut trae_done = false;
    for app in pending {
        if cached_model_client(&app) {
            if trae_done {
                continue;
            }
            trae_done = true;
        }
        if procs::agent_running(&app, &procs::snapshot_fresh()) {
            notes.push(format!("{} 还在运行，退出后会自动清理", label(&app)));
            continue;
        }
        match clean_client(&app, port, Duration::from_secs(30)) {
            Ok(n) => removed += n,
            Err(e) => notes.push(e),
        }
    }
    json!({"removed": removed, "pending": read_pending(), "notes": notes})
}

/// A client that was open during a sweep rewrites its file on exit and can
/// resurrect dead entries. Poll until the pending list drains.
pub fn watch_pending(port: u16) {
    loop {
        std::thread::sleep(Duration::from_secs(20));
        for app in read_pending() {
            let _ = clean_client(&app, port, Duration::from_secs(30));
        }
    }
}

/// A client exit should retry only what was left pending, never invent work.
pub fn pending_apps() -> Vec<String> {
    read_pending()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gateway_hosts_cover_local_forms() {
        let hosts = gateway_hosts(8788);
        assert!(hosts.contains(&"127.0.0.1:8788".to_string()));
        assert!(hosts.contains(&"localhost:8788".to_string()));
    }

    #[test]
    fn pending_entries_stay_unique() {
        let apps = vec!["traework-cn".to_string(), "traework-cn".to_string()];
        let mut unique: Vec<String> = Vec::new();
        for app in &apps {
            if !unique.iter().any(|x| x == app) {
                unique.push(app.clone());
            }
        }
        assert_eq!(unique, vec!["traework-cn".to_string()]);
    }

    #[test]
    fn only_trae_keeps_a_cached_model_list() {
        assert!(cached_model_client("traework-cn"));
        assert!(cached_model_client("trae"));
        assert!(!cached_model_client("qoder-cn"));
        assert!(!cached_model_client("workbuddy"));
    }

    #[test]
    fn gateway_hosts_match_every_loopback_form() {
        for port in [8788u16, 18788] {
            let hosts = gateway_hosts(port);
            assert!(hosts.contains(&format!("127.0.0.1:{port}")));
            assert!(hosts.contains(&format!("localhost:{port}")));
            assert!(hosts.contains(&format!("[::1]:{port}")));
        }
    }

    #[test]
    fn purge_drops_only_gateway_endpoints() {
        let hosts = gateway_hosts(8788);
        let mine = json!({"id": "mine", "url": "https://api.example.com/v1"});
        let dead = json!({"id": "trae/glm-5.3", "url": "http://127.0.0.1:8788/v1/chat/completions"});
        assert!(has_gateway_url(&dead["url"], &hosts));
        assert!(!has_gateway_url(&mine["url"], &hosts));
        let mut doc = json!({"models": [mine, dead], "availableModels": ["mine"]});
        assert_eq!(purge_value(&mut doc, &hosts), 1);
        assert_eq!(doc["models"].as_array().unwrap().len(), 1);
        let mut providers = json!({"providers": {
            "cockpit_gateway": {"options": {"baseURL": "http://127.0.0.1:8788/v1"}},
            "mine": {"options": {"baseURL": "https://api.example.com/v1"}}
        }});
        assert_eq!(purge_value(&mut providers, &hosts), 1);
        assert_eq!(providers["providers"].as_object().unwrap().len(), 1);
    }
}
