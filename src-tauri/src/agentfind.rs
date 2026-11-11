//! Embedded local AgentFind session index service.
//!
//! The service source is compiled into the executable and unpacked to the data
//! root on first use. It runs on the bundled CPython runtime, so the product no
//! longer depends on a separately installed AgentFind or a system Python.
use serde_json::Value;
use std::{
    fs,
    fs::OpenOptions,
    net::{SocketAddr, TcpStream},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{Mutex, OnceLock},
    time::{Duration, Instant, SystemTime},
};

static START: Mutex<()> = Mutex::new(());
const SCRIPT: &str = include_str!("../scripts/agentfind.py");
const CREATE_NO_WINDOW: u32 = 0x08000000;

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// FNV-1a over the embedded script, matching `_script_fingerprint()` on the
/// Python side. A live service reporting a different value is another build.
fn script_fingerprint() -> &'static str {
    static FINGERPRINT: OnceLock<String> = OnceLock::new();
    FINGERPRINT.get_or_init(|| format!("{:016x}", fnv1a(SCRIPT.as_bytes())))
}

fn absolute(path: PathBuf) -> PathBuf {
    if path.is_absolute() {
        return path;
    }
    std::env::current_dir()
        .map(|dir| dir.join(&path))
        .unwrap_or(path)
}

fn data_home() -> PathBuf {
    let path = match std::env::var_os("AGENTFIND_HOME").filter(|v| !v.is_empty()) {
        Some(value) => {
            let path = PathBuf::from(value);
            if path.is_absolute() {
                path
            } else {
                crate::registry::home().join(path)
            }
        }
        None => crate::registry::home().join(".agentfind"),
    };
    absolute(path)
}

/// Write the embedded script when missing or out of date. The file path stays
/// stable because AgentFind identifies its own processes by script name.
fn script_file() -> Result<PathBuf, String> {
    script_file_at(&crate::registry::base_dir().join("agentfind-runtime"))
}

fn script_file_at(dir: &Path) -> Result<PathBuf, String> {
    if crate::registry::is_reparse_point(dir) {
        return Err("内置会话索引目录包含链接，已停止启动。".into());
    }
    fs::create_dir_all(dir).map_err(|_| "无法创建会话索引目录，请检查磁盘空间和权限。".to_string())?;
    let path = dir.join("agentfind.py");
    if crate::registry::is_reparse_point(&path) {
        return Err("内置会话索引文件包含链接，已停止启动。".into());
    }
    let expected = SCRIPT.as_bytes();
    if fs::read(&path).ok().as_deref() == Some(expected) {
        return Ok(path);
    }
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let staging = dir.join(format!(".agentfind-{}-{nonce}.py", std::process::id()));
    fs::write(&staging, expected).map_err(|_| "无法释放内置会话索引组件，请检查磁盘空间。".to_string())?;
    if fs::rename(&staging, &path).is_err() {
        let _ = fs::remove_file(&staging);
        // Another instance may have replaced it first; accept only identical bytes.
        if fs::read(&path).ok().as_deref() != Some(expected) {
            return Err("内置会话索引组件写入失败，请检查磁盘空间和权限。".into());
        }
    }
    Ok(path)
}

fn candidate_ports(dir: &Path, default: u16) -> Vec<u16> {
    // The service records its actual port; a running service has moved only if
    // that file is gone. Probing the whole fallback range on every open is
    // expensive when a firewall drops loopback SYNs.
    let recorded = fs::read_to_string(dir.join("running.port"))
        .ok()
        .and_then(|v| v.trim().parse::<u16>().ok());
    let mut ports = vec![];
    for port in recorded.into_iter().chain(std::iter::once(default)) {
        if port >= 1024 && !ports.contains(&port) {
            ports.push(port);
        }
    }
    ports
}

fn port_open(port: u16) -> bool {
    let address = SocketAddr::from(([127, 0, 0, 1], port));
    TcpStream::connect_timeout(&address, Duration::from_millis(150)).is_ok()
}

/// Returns the current service port and whether an older build (no
/// `/api/agentfind`) still owns one of the candidate ports. Closed ports are
/// rejected by TCP first: a firewall that drops loopback SYNs would otherwise
/// make each HTTP connect wait far longer than its timeout.
fn probe_ports(dir: &Path, default: u16) -> (Option<u16>, bool) {
    let http = ureq::AgentBuilder::new()
        .try_proxy_from_env(false)
        .timeout(Duration::from_millis(600))
        .redirects(0)
        .build();
    let mut legacy = false;
    for port in candidate_ports(dir, default) {
        if !port_open(port) {
            continue;
        }
        let identity = http
            .get(&format!("http://127.0.0.1:{port}/api/agentfind"))
            .call()
            .ok()
            .and_then(|r| r.into_json::<Value>().ok());
        if identity.is_some_and(|id| {
            id["service"] == "AgentFind"
                && id["sessions_ui"] == 1
                && id["script"] == script_fingerprint()
        }) {
            return (Some(port), legacy);
        }
        let meta = http
            .get(&format!("http://127.0.0.1:{port}/api/meta"))
            .call()
            .ok()
            .and_then(|r| r.into_json::<Value>().ok());
        if meta.is_some_and(|meta| meta["total"].is_number()) {
            legacy = true;
        }
    }
    (None, legacy)
}

fn running(dir: &Path, default: u16) -> Option<u16> {
    probe_ports(dir, default).0
}

/// A service we must stop before spawning: either an older build with no
/// `/api/agentfind` identity, or one whose script fingerprint is not ours. Both
/// would make a fresh child exit immediately with "already running", and the
/// latter would keep serving its own outdated page.
fn legacy_running(dir: &Path, default: u16) -> bool {
    probe_ports(dir, default).1
}

/// AgentFind writes `running.port` only after the listener is up, so a file
/// newer than the spawn is enough. Do not add a TCP/HTTP probe here: a host
/// firewall may drop loopback SYNs for the new interpreter path for many
/// seconds, while the WebView that loads the iframe can connect fine.
fn fresh_recorded_port(dir: &Path, since: SystemTime) -> Option<u16> {
    let path = dir.join("running.port");
    let modified = fs::metadata(&path).ok()?.modified().ok()?;
    if modified < since {
        return None;
    }
    let port = fs::read_to_string(&path)
        .ok()?
        .trim()
        .parse::<u16>()
        .ok()?;
    (port >= 1024).then_some(port)
}

fn python_command(python: &Path) -> Command {
    let mut command = Command::new(python);
    command.args(["-I", "-u", "-X", "utf8"]);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command
}

fn stop_legacy(python: &Path, script: &Path, dir: &Path, default: u16) {
    let mut child = match python_command(python)
        .arg(script)
        .arg("--stop")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(_) => return,
    };
    let deadline = Instant::now() + Duration::from_secs(25);
    while legacy_running(dir, default) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(250));
    }
    if child.try_wait().ok().flatten().is_none() {
        let _ = child.kill();
        let _ = child.wait();
    }
}

fn service_log(dir: &Path) -> (Stdio, Stdio) {
    let _ = fs::create_dir_all(dir);
    match OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("agentfind.log"))
    {
        Ok(file) => match file.try_clone() {
            Ok(clone) => (Stdio::from(file), Stdio::from(clone)),
            Err(_) => (Stdio::null(), Stdio::null()),
        },
        Err(_) => (Stdio::null(), Stdio::null()),
    }
}

fn stop(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

#[tauri::command]
pub async fn agentfind_port() -> Result<u16, String> {
    tauri::async_runtime::spawn_blocking(|| {
        let _lock = START.lock().map_err(|_| "会话索引启动状态不可用")?;
        let dir = data_home();
        let default = std::env::var("AGENTFIND_PORT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(8790);
        if let Some(port) = running(&dir, default) {
            return Ok(port);
        }
        let script = script_file()?;
        let python = crate::usage::bundled_python()
            .map_err(|e| format!("内置运行环境不可用：{e}"))?;
        if legacy_running(&dir, default) {
            stop_legacy(&python, &script, &dir, default);
            if let Some(port) = running(&dir, default) {
                return Ok(port);
            }
            if legacy_running(&dir, default) {
                return Err("检测到旧版 AgentFind 仍在运行，请先关闭它再重新连接。".into());
            }
        }
        let (stdout, stderr) = service_log(&dir);
        let started = SystemTime::now();
        let mut cmd = python_command(&python);
        let mut child = cmd
            .arg(&script)
            .arg("--no-open")
            .env("AGENTFIND_HOME", &dir)
            .stdin(Stdio::null())
            .stdout(stdout)
            .stderr(stderr)
            .spawn()
            .map_err(|e| format!("无法启动内置会话索引服务：{e}"))?;
        let deadline = Instant::now() + Duration::from_secs(180);
        loop {
            if let Some(port) = fresh_recorded_port(&dir, started)
                .or_else(|| running(&dir, default))
            {
                return Ok(port);
            }
            match child.try_wait() {
                Ok(Some(_)) => {
                    // A concurrent instance may have won the race; otherwise the
                    // child refused to start (or hit an old service).
                    if let Some(port) = fresh_recorded_port(&dir, started)
                        .or_else(|| running(&dir, default))
                    {
                        return Ok(port);
                    }
                    return Err(if legacy_running(&dir, default) {
                        "检测到旧版 AgentFind 仍在运行，请先关闭它再重新连接。".into()
                    } else {
                        "内置会话索引服务没有启动，请查看 ~/.agentfind/agentfind.log。".into()
                    });
                }
                Ok(None) => {}
                Err(e) => {
                    stop(&mut child);
                    return Err(format!("无法等待会话索引启动：{e}"));
                }
            }
            if Instant::now() >= deadline {
                // A live child may still be building the first index; leaving it
                // running lets the next retry reuse it instead of starting over.
                if child.try_wait().ok().flatten().is_some() {
                    return Err("会话索引服务没有启动，请查看 ~/.agentfind/agentfind.log。".into());
                }
                return Err("会话索引仍在首次整理，请稍后再点重新连接。".into());
            }
            std::thread::sleep(Duration::from_millis(400));
        }
    })
    .await
    .map_err(|_| "会话索引探测任务中断")?
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir(PathBuf);
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn embedded_script_is_the_session_service() {
        assert!(SCRIPT.len() > 100_000, "AgentFind source must be compiled in");
        assert!(SCRIPT.contains("\"service\": \"AgentFind\""));
        assert!(SCRIPT.contains("\"sessions_ui\": 1"));
    }

    #[test]
    fn fingerprint_uses_standard_fnv1a_so_python_can_reproduce_it() {
        assert_eq!(fnv1a(b""), 0xcbf29ce484222325);
        assert_eq!(fnv1a(b"a"), 0xaf63dc4c8601ec8c);
        assert_eq!(fnv1a(b"foobar"), 0x85944171f73967e8);
        assert_eq!(script_fingerprint().len(), 16);
    }

    #[test]
    fn service_identity_carries_the_script_fingerprint() {
        assert!(SCRIPT.contains("0xcbf29ce484222325"));
        assert!(SCRIPT.contains("0x100000001b3"));
        assert!(SCRIPT.contains("\"script\": SCRIPT_FINGERPRINT"));
    }

    #[test]
    fn embedded_script_is_written_once_and_repaired() {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let temp = TempDir(std::env::temp_dir().join(format!(
            "cockpit-agentfind-{}-{stamp}",
            std::process::id()
        )));
        let first = script_file_at(&temp.0).expect("unpack embedded AgentFind");
        assert_eq!(fs::read(&first).unwrap(), SCRIPT.as_bytes());
        assert_eq!(script_file_at(&temp.0).unwrap(), first);
        fs::write(&first, b"stale").unwrap();
        let repaired = script_file_at(&temp.0).expect("repair embedded AgentFind");
        assert_eq!(fs::read(&repaired).unwrap(), SCRIPT.as_bytes());
    }

    #[test]
    fn fresh_port_file_requires_recent_timestamp_and_open_port() {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let temp = TempDir(std::env::temp_dir().join(format!(
            "cockpit-agentfind-port-{}-{stamp}",
            std::process::id()
        )));
        fs::create_dir_all(&temp.0).unwrap();
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        fs::write(temp.0.join("running.port"), port.to_string()).unwrap();
        let since = SystemTime::now() - Duration::from_secs(1);
        assert_eq!(fresh_recorded_port(&temp.0, since), Some(port));
        let future = SystemTime::now() + Duration::from_secs(60);
        assert_eq!(fresh_recorded_port(&temp.0, future), None);
    }

    #[test]
    fn relative_data_paths_become_absolute() {
        let path = absolute(PathBuf::from(".agentfind"));
        assert!(path.is_absolute());
        assert!(path.ends_with(".agentfind"));
    }
}
