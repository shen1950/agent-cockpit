//! 临时公网中继。仅在桌面端明确开启时运行，入口由 Cloudflare 邮箱验证码保护。
//! 中继传输经过 Cloudflare，Quick Tunnel 的域名和可用性不保证长期稳定。
use serde_json::{json, Value};
use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

#[cfg(windows)]
use std::os::windows::process::CommandExt;
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x08000000;

struct Runtime {
    generation: u64,
    child: Option<Child>,
    job: Option<Job>,
    phase: &'static str,
    message: &'static str,
    url: Option<String>,
    protected: bool,
    connected: bool,
}

static TUNNEL: OnceLock<Mutex<Runtime>> = OnceLock::new();
fn runtime() -> &'static Mutex<Runtime> {
    TUNNEL.get_or_init(|| Mutex::new(Runtime {
        generation: 0, child: None, job: None, phase: "stopped",
        message: "公网远控未开启", url: None, protected: false, connected: false,
    }))
}

/// 校验单个邮箱，禁止通配符、多个收件人和控制字符。
fn valid_email(value: &str) -> bool {
    if value.len() > 254 || value.len() < 3 || !value.is_ascii() { return false; }
    let mut p = value.split('@');
    let (local, domain) = match (p.next(), p.next(), p.next()) {
        (Some(l), Some(d), None) if !l.is_empty() && !d.is_empty() => (l, d),
        _ => return false,
    };
    local.len() <= 64 && !local.starts_with('.') && !local.ends_with('.')
        && !local.contains("..")
        && local.bytes().all(|c| c.is_ascii_alphanumeric() || b".!#$%&'+-/=?^_`{|}~".contains(&c))
        && domain.len() <= 253 && domain.contains('.')
        && domain.split('.').all(|label| !label.is_empty() && label.len() <= 63
            && !label.starts_with('-') && !label.ends_with('-')
            && label.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-'))
}

fn tunnel_url(line: &str) -> Option<String> {
    for word in line.split_whitespace() {
        let candidate = word.trim_matches(|c: char| c == '|' || c == '"');
        let host = match candidate.strip_prefix("https://") { Some(h) => h, None => continue };
        let prefix = match host.strip_suffix(".trycloudflare.com") { Some(p) => p, None => continue };
        if !prefix.is_empty() && prefix.len() <= 63 && !prefix.starts_with('-') && !prefix.ends_with('-')
            && prefix.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-') {
            return Some(candidate.to_string());
        }
    }
    None
}

fn executable() -> Result<std::path::PathBuf, String> {
    let path = std::env::current_exe().map_err(|_| "找不到驾驶舱程序".to_string())?
        .parent().ok_or_else(|| "找不到驾驶舱程序目录".to_string())?.join("cloudflared.exe");
    if !path.is_file() { return Err("缺少 cloudflared，请先安装远程连接组件".into()); }
    verify_signature(&path)?;
    Ok(path)
}

#[cfg(windows)]
fn verify_signature(path: &std::path::Path) -> Result<(), String> {
    // 不拼接 shell 命令，路径经子进程环境变量传递；只接受有效的 Cloudflare 签名。
    let script = "$s=Get-AuthenticodeSignature -LiteralPath $env:AC_REMOTE_TUNNEL_EXE; if ($s.Status -eq 'Valid' -and $s.SignerCertificate.GetNameInfo([System.Security.Cryptography.X509Certificates.X509NameType]::SimpleName, $false) -eq 'Cloudflare, Inc.') { exit 0 } else { exit 1 }";
    let status = Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .env("AC_REMOTE_TUNNEL_EXE", path)
        .creation_flags(CREATE_NO_WINDOW).stdout(Stdio::null()).stderr(Stdio::null())
        .status().map_err(|_| "无法验证远程连接组件的签名".to_string())?;
    if status.success() { Ok(()) } else { Err("远程连接组件的签名无效或发行者不匹配".into()) }
}
#[cfg(not(windows))]
fn verify_signature(_: &std::path::Path) -> Result<(), String> { Err("当前系统暂不支持公网远控".into()) }

/// 不在日志中保存隧道输出，避免将链接、邮箱和其他连接细节留在日志。
fn observe_line(generation: u64, line: &str) {
    let mut state = runtime().lock().unwrap_or_else(|p| p.into_inner());
    if state.generation != generation || state.child.is_none() { return; }
    if line.contains("Your protected quick Tunnel has been created!") { state.protected = true; }
    if let Some(url) = tunnel_url(line) { state.url = Some(url); }
    if line.contains("Registered tunnel connection") { state.connected = true; }
    if line.contains("Retrying connection") || line.contains("connection terminated") {
        state.connected = false;
        state.phase = "reconnecting";
        state.message = "中继连接中断，正在重连";
    }
    if state.protected && state.connected && state.url.is_some() {
        state.phase = "ready";
        state.message = "公网远控已连接，手机访问需要邮箱验证码";
    }
}

/// 调用者需要先开启仅供本机连接的远控服务；该方法不会自动开启公网。
pub fn start(email: &str) -> Result<(), String> {
    let email = email.trim();
    if !valid_email(email) { return Err("请输入一个有效的验证码邮箱地址，不支持通配符".into()); }
    let exe = executable()?;
    start_command(&exe, email)
}

fn start_command(exe: &std::path::Path, email: &str) -> Result<(), String> {
    let mut command = Command::new(exe);
    command.args(["tunnel", "--no-autoupdate", "--protocol", "http2", "--url", "http://127.0.0.1:8790", "--allowed-mail"])
        .arg(email);
    launch_command(command)
}

fn launch_command(mut command: Command) -> Result<(), String> {
    let mut state = runtime().lock().unwrap_or_else(|p| p.into_inner());
    if state.child.is_some() { return Err("公网远控已启动或正在连接".into()); }
    command.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped());
    #[cfg(windows)]
    command.creation_flags(CREATE_NO_WINDOW);
    let mut child = command.spawn().map_err(|_| "远程连接组件启动失败".to_string())?;
    let job = match Job::attach(&child) {
        Ok(j) => j,
        Err(e) => { let _ = child.kill(); let _ = child.wait(); return Err(e); }
    };
    let stderr = match child.stderr.take() {
        Some(s) => s,
        None => { let _ = child.kill(); let _ = child.wait(); return Err("无法读取远程连接状态".into()); }
    };
    state.generation = state.generation.wrapping_add(1);
    let generation = state.generation;
    state.child = Some(child);
    state.job = Some(job);
    state.phase = "starting";
    state.message = "正在建立临时公网连接";
    state.url = None; state.protected = false; state.connected = false;
    drop(state);
    std::thread::spawn(move || {
        let mut reader = stderr;
        let mut pending = Vec::new();
        let mut oversized = false;
        let mut buffer = [0u8; 2048];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(n) => for byte in &buffer[..n] {
                    if *byte == b'\n' {
                        if !oversized { observe_line(generation, &String::from_utf8_lossy(&pending)); }
                        pending.clear(); oversized = false;
                    } else if pending.len() < 8192 && !oversized {
                        pending.push(*byte);
                    } else { oversized = true; }
                }
            }
        }
    });
    std::thread::spawn(move || monitor(generation));
    Ok(())
}

fn monitor(generation: u64) {
    let deadline = Instant::now() + Duration::from_secs(75);
    let mut ever_ready = false;
    loop {
        std::thread::sleep(Duration::from_millis(500));
        let mut state = runtime().lock().unwrap_or_else(|p| p.into_inner());
        if state.generation != generation { break; }
        let exited = match state.child.as_mut() {
            Some(c) => c.try_wait().map(|s| s.is_some()).unwrap_or(true),
            None => break,
        };
        if exited {
            state.child.take(); state.job.take(); state.url = None;
            state.connected = false; state.phase = "failed";
            state.message = "公网中继已退出，请检查网络后重新开启";
            break;
        }
        if state.phase == "ready" { ever_ready = true; }
        if !ever_ready && Instant::now() >= deadline {
            if let Some(mut child) = state.child.take() { let _ = child.kill(); let _ = child.wait(); }
            state.job.take(); state.url = None; state.connected = false; state.phase = "failed";
            state.message = "公网中继连接超时，请检查网络后重新开启";
            break;
        }
    }
}

pub fn stop() -> Result<(), String> {
    let mut state = runtime().lock().unwrap_or_else(|p| p.into_inner());
    state.generation = state.generation.wrapping_add(1);
    if let Some(mut child) = state.child.take() {
        let _ = child.kill();
        let _ = child.wait();
    }
    state.job.take(); state.url = None; state.connected = false; state.protected = false;
    state.phase = "stopped"; state.message = "公网远控已关闭";
    Ok(())
}

pub fn view() -> Value {
    let state = runtime().lock().unwrap_or_else(|p| p.into_inner());
    json!({ "state": state.phase, "message": state.message, "temporary": true,
        "url": if state.phase == "ready" { state.url.as_deref() } else { None } })
}

// 独立 Job Object：关闭驾驶舱或意外崩溃时，内核结束 cloudflared 和其子进程。
#[cfg(windows)]
struct Job(isize);
#[cfg(windows)]
impl Job {
    fn attach(child: &Child) -> Result<Self, String> {
        use std::os::windows::io::AsRawHandle;
        use std::ffi::c_void;
        #[repr(C)]
        #[derive(Default)]
        struct Basic { per_process_time: i64, per_job_time: i64, flags: u32, min_ws: usize,
            max_ws: usize, active_limit: u32, affinity: usize, priority: u32, scheduling: u32 }
        #[repr(C)]
        #[derive(Default)]
        struct Io { read_ops: u64, write_ops: u64, other_ops: u64, read_bytes: u64, write_bytes: u64, other_bytes: u64 }
        #[repr(C)]
        #[derive(Default)]
        struct Extended { basic: Basic, io: Io, process_mem: usize, job_mem: usize, peak_process_mem: usize, peak_job_mem: usize }
        #[link(name = "kernel32")]
        extern "system" {
            fn CreateJobObjectW(attributes: *const c_void, name: *const u16) -> *mut c_void;
            fn SetInformationJobObject(job: *mut c_void, class: i32, info: *const c_void, len: u32) -> i32;
            fn AssignProcessToJobObject(job: *mut c_void, process: *mut c_void) -> i32;
            fn CloseHandle(handle: *mut c_void) -> i32;
        }
        unsafe {
            let handle = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if handle.is_null() { return Err("无法创建中继进程保护".into()); }
            let mut info = Extended::default();
            info.basic.flags = 0x00002000; // JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
            if SetInformationJobObject(handle, 9, &info as *const _ as *const c_void, std::mem::size_of::<Extended>() as u32) == 0
                || AssignProcessToJobObject(handle, child.as_raw_handle() as *mut c_void) == 0 {
                CloseHandle(handle);
                return Err("无法绑定中继进程保护，已停止启动".into());
            }
            Ok(Job(handle as isize))
        }
    }
}
#[cfg(windows)]
impl Drop for Job {
    fn drop(&mut self) {
        #[link(name = "kernel32")]
        extern "system" { fn CloseHandle(handle: *mut std::ffi::c_void) -> i32; }
        unsafe { CloseHandle(self.0 as *mut std::ffi::c_void); }
    }
}
#[cfg(not(windows))]
struct Job;
#[cfg(not(windows))]
impl Job { fn attach(_: &Child) -> Result<Self, String> { Err("当前系统暂不支持公网远控".into()) } }

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn email_rejects_wildcards_multiple_addresses_and_flags() {
        assert!(valid_email("user.name+tag@example.com"));
        for value in ["*@example.com", "a@example.com,b@example.com", "x\n@example.com", "x@example", "x@-example.com", "x@example.com --url"] {
            assert!(!valid_email(value), "should reject {value:?}");
        }
    }
    #[test]
    fn only_exact_https_quick_tunnel_addresses_are_accepted() {
        assert_eq!(tunnel_url("INF | https://abcd-123.trycloudflare.com |"), Some("https://abcd-123.trycloudflare.com".into()));
        for line in ["https://evil.test", "http://abc.trycloudflare.com", "https://abc.trycloudflare.com.attacker.test", "https://abc.trycloudflare.com/path"] {
            assert_eq!(tunnel_url(line), None);
        }
    }

    #[cfg(windows)]
    #[test]
    fn fake_process_requires_protected_registration_and_stops_cleanly() {
        // 仅产生虚构日志，无网络、无真实中继；运行后通过 Job 清理进程。
        let mut cmd = Command::new("powershell.exe");
        cmd.args(["-NoProfile", "-NonInteractive", "-Command",
            "[Console]::Error.WriteLine('Your protected quick Tunnel has been created!'); [Console]::Error.WriteLine('| https://test-123.trycloudflare.com |'); [Console]::Error.WriteLine('Registered tunnel connection'); Start-Sleep -Seconds 30"]);
        launch_command(cmd).expect("fake process must start");
        let deadline = Instant::now() + Duration::from_secs(10);
        while view()["state"] != "ready" && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        let current = view();
        let stopped = stop();
        assert_eq!(current["state"], "ready");
        assert_eq!(current["url"], "https://test-123.trycloudflare.com");
        assert!(stopped.is_ok());
        assert_eq!(view()["state"], "stopped");
        assert!(view()["url"].is_null());
    }
}
