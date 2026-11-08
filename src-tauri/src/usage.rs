//! Read-only usage aggregation for local Agent histories.
//! Token totals, visible-text estimates and native vendor credits stay separate.
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{Cursor, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

type CollectResult = Result<Option<Value>, String>;
static CACHE: OnceLock<Mutex<HashMap<String, (Instant, CollectResult)>>> = OnceLock::new();
static KEY_LOCKS: OnceLock<Mutex<HashMap<String, Arc<Mutex<()>>>>> = OnceLock::new();
static RUNTIME: Mutex<Option<(PathBuf, PathBuf)>> = Mutex::new(None);
const SCRIPT: &str = include_str!("../scripts/usage-local.py");
// Official CPython 3.14.10 embeddable Windows x64 package, SHA-256
// 90905e64a840557d8c1bc4f5b74db3d7ee85e3ffb217611cac1c7813acea4159.
// The original distribution includes LICENSE.txt and third-party notices.
const PYTHON_ARCHIVE: &[u8] = include_bytes!("../vendor/python-embed.zip");
const PYTHON_VERSION_DIR: &str = "python-3.14.10-90905e64a840557d";
const BOOTSTRAP: &str = "import io,json,sys; p=json.loads(sys.stdin.buffer.read().decode('utf-8')); sys.stdin=io.StringIO(json.dumps(p['request'])); exec(compile(p['script'],'<AgentCockpit usage>','exec'),{'__name__':'__main__'})";

fn supported(app: &str) -> bool {
    matches!(app, "codex" | "chatgpt" | "claude" | "claude-code" | "qoder" | "qoder-cn"
        | "qoderwork" | "qwenwork" | "qwenworkcn" | "workbuddy" | "workbuddy-intl"
        | "codebuddy" | "codebuddy-cn" | "zcode" | "opencode" | "minimax"
        | "marvis" | "gemini" | "gemini-cli" | "dsh"
        // AStudio（讯飞）：每轮计费报告落在 state.sqlite 的 turn_billing_outbox，
        // 带厂商自己写的 model_usages，比按文本估算准。
        | "astudio")
}

/// Returns canonical source identity, daily buckets, models and timing samples.
/// Empty or unsupported profiles never fall back to another account's history.
pub fn collect(app_id: &str, data_dir: &str) -> Option<Value> {
    collect_with_error(app_id, data_dir).ok().flatten()
}

/// Distinguish unavailable runtime, inaccessible data, timeout and empty history.
/// Diagnostic messages never expose paths, credentials or conversation text.
pub fn collect_with_error(app_id: &str, data_dir: &str) -> CollectResult {
    if !supported(app_id) || data_dir.trim().is_empty() { return Ok(None); }
    let path = Path::new(data_dir);
    match std::fs::metadata(path) {
        Ok(m) if m.is_dir() => (),
        Ok(_) => return Err("用量数据路径不是目录。".into()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err("无法访问本地用量目录，请检查权限。".into()),
    }
    let path = path.canonicalize().map_err(|_| "无法解析本地用量目录。".to_string())?;
    let key = format!("{}|{}", app_id, path.to_string_lossy().to_lowercase());
    cached_source(key, || run_helper(app_id, &path))
}

fn cached_source(key: String, fetch: impl FnOnce() -> CollectResult) -> CollectResult {
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    // Serialize repeated requests for the same source without blocking unrelated sources.
    let flight = {
        let locks = KEY_LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
        let mut held = locks.lock().unwrap_or_else(|e| e.into_inner());
        held.entry(key.clone()).or_insert_with(|| Arc::new(Mutex::new(()))).clone()
    };
    let _flight = flight.lock().unwrap_or_else(|e| e.into_inner());
    {
        let held = cache.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((at, value)) = held.get(&key) {
            if at.elapsed() < Duration::from_secs(15) { return value.clone(); }
        }
    }
    let value = fetch();
    cache.lock().unwrap_or_else(|e| e.into_inner()).insert(key, (Instant::now(), value.clone()));
    value
}

/// Extract only flat ordinary files from the signed-distribution archive, to a
/// fresh staging directory. Existing runtime files are verified byte-for-byte
/// against the embedded archive before they are used; never overwrite them.
fn validate_runtime(dir: &Path) -> Result<(), String> {
    if crate::registry::is_reparse_point(dir) { return Err("内置运行环境目录包含链接，已停止读取。".into()); }
    let mut archive = zip::ZipArchive::new(Cursor::new(PYTHON_ARCHIVE)).map_err(|_| "内置运行环境损坏。".to_string())?;
    for i in 0..archive.len() {
        let mut item = archive.by_index(i).map_err(|_| "内置运行环境损坏。".to_string())?;
        let entry = item.enclosed_name().ok_or("内置运行环境包含无效路径。")?.to_path_buf();
        if entry.components().count() != 1 || item.is_dir() || item.size() > 100 * 1024 * 1024 {
            return Err("内置运行环境包含无效文件。".into());
        }
        let target = dir.join(entry);
        if crate::registry::is_reparse_point(&target) { return Err("内置运行环境文件包含链接。".into()); }
        let mut file = std::fs::File::open(&target).map_err(|_| "内置运行环境不完整，请检查磁盘与权限。".to_string())?;
        if file.metadata().map_err(|_| "无法检查内置运行环境。")?.len() != item.size() {
            return Err("内置运行环境文件损坏，请检查磁盘。".into());
        }
        let mut expected = [0u8; 32768]; let mut actual = [0u8; 32768];
        loop {
            let n = item.read(&mut expected).map_err(|_| "内置运行环境校验失败。")?;
            if n == 0 { break; }
            file.read_exact(&mut actual[..n]).map_err(|_| "内置运行环境校验失败。")?;
            if expected[..n] != actual[..n] { return Err("内置运行环境文件不匹配。".into()); }
        }
    }
    if !dir.join("python.exe").is_file() || !dir.join("LICENSE.txt").is_file() {
        return Err("内置运行环境缺少必要文件。".into());
    }
    Ok(())
}

fn prepare_runtime_at(base: &Path) -> Result<PathBuf, String> {
    if !base.is_absolute() { return Err("驾驶舱数据目录无效。".into()); }
    let parent = base.join("usage-runtime");
    if crate::registry::is_reparse_point(&parent) { return Err("用量运行环境目录包含链接，已停止读取。".into()); }
    std::fs::create_dir_all(&parent).map_err(|_| "无法创建内置运行环境目录，请检查磁盘空间和权限。".to_string())?;
    if crate::registry::is_reparse_point(&parent) { return Err("用量运行环境目录包含链接，已停止读取。".into()); }
    let destination = parent.join(PYTHON_VERSION_DIR);
    if destination.exists() { validate_runtime(&destination)?; return Ok(destination.join("python.exe")); }
    let nonce = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos();
    let staging = parent.join(format!(".install-{}-{nonce}", std::process::id()));
    std::fs::create_dir(&staging).map_err(|_| "无法创建内置运行环境临时目录。".to_string())?;
    let mut archive = zip::ZipArchive::new(Cursor::new(PYTHON_ARCHIVE)).map_err(|_| "内置运行环境损坏。".to_string())?;
    for i in 0..archive.len() {
        let mut item = archive.by_index(i).map_err(|_| "内置运行环境损坏。".to_string())?;
        let entry = item.enclosed_name().ok_or("内置运行环境包含无效路径。")?.to_path_buf();
        if entry.components().count() != 1 || item.is_dir() || item.size() > 100 * 1024 * 1024 {
            return Err("内置运行环境包含无效文件。".into());
        }
        let mut out = std::fs::OpenOptions::new().write(true).create_new(true).open(staging.join(entry))
            .map_err(|_| "无法解压内置运行环境，请检查磁盘空间和权限。".to_string())?;
        std::io::copy(&mut item, &mut out).map_err(|_| "无法解压内置运行环境。".to_string())?;
    }
    validate_runtime(&staging)?;
    if std::fs::rename(&staging, &destination).is_err() {
        // Another process may have completed installation first; accept only a verified copy.
        validate_runtime(&destination)?;
    }
    Ok(destination.join("python.exe"))
}

pub(crate) fn bundled_python() -> Result<PathBuf, String> {
    bundled_python_at(&crate::registry::base_dir(), &RUNTIME)
}

fn bundled_python_at(base: &Path, runtime: &Mutex<Option<(PathBuf, PathBuf)>>) -> Result<PathBuf, String> {
    let mut held = runtime.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((cached_base, path)) = held.as_ref() {
        if cached_base == base { return Ok(path.clone()); }
    }
    let path = prepare_runtime_at(base)?;
    *held = Some((base.to_path_buf(), path.clone()));
    Ok(path)
}

fn run_helper(app_id: &str, data_dir: &Path) -> CollectResult {
    let mut command = Command::new(bundled_python()?);
    command.args(["-I", "-X", "utf8", "-c", BOOTSTRAP])
        .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
    #[cfg(windows)] {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    let mut child = command.spawn().map_err(|_| "无法启动内置用量采集环境，请检查系统安全软件与运行环境。".to_string())?;
    let mut reader = child.stdout.take().ok_or("无法读取用量采集结果。")?;
    // Drain stdout while the child runs. Daily/model output can exceed the pipe
    // buffer, so waiting for process exit before reading would deadlock.
    let output = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        reader.by_ref().take(128 * 1024 * 1024 + 1).read_to_end(&mut bytes).ok()?;
        Some(bytes)
    });
    let request = json!({"script":SCRIPT,"request":{
        "app_id":app_id,"data_dir":data_dir,
        "cache_dir":crate::registry::base_dir().join("usage-cache")
    }}).to_string();
    let written = child.stdin.take().map(|mut input| input.write_all(request.as_bytes()).is_ok()).unwrap_or(false);
    if !written { let _ = child.kill(); let _ = child.wait(); let _ = output.join(); return Err("无法向用量采集环境传递数据。".into()); }
    let started = Instant::now();
    let success = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status.success(),
            Ok(None) if started.elapsed() < Duration::from_secs(90) => std::thread::sleep(Duration::from_millis(50)),
            Ok(None) => { let _ = child.kill(); let _ = child.wait(); let _ = output.join(); return Err("读取本地用量超时，请稍后重试。".into()); }
            Err(_) => { let _ = child.kill(); let _ = child.wait(); let _ = output.join(); return Err("无法等待用量采集完成。".into()); }
        }
    };
    let bytes = output.join().map_err(|_| "读取用量采集结果失败。")?.ok_or("读取用量采集结果失败。")?;
    if !success { return Err("用量采集环境运行失败。".into()); }
    if bytes.len() > 128 * 1024 * 1024 { return Err("本地用量记录过大，暂无法汇总。".into()); }
    let value: Value = serde_json::from_slice(&bytes).map_err(|_| "用量采集结果格式错误。".to_string())?;
    if value.is_null() { return Ok(None); }
    if let Some(err) = value["error"].as_str() { return Err(err.to_string()); }
    if !value.is_object() { return Err("用量采集结果格式错误。".into()); }
    Ok(Some(value))
}

/// Legacy summary used on account cards, derived from the same de-duplicated
/// events as the statistics page (cache reads are not added twice).
pub fn compute(app_id: &str, data_dir: &str) -> Option<Value> {
    let value = collect(app_id, data_dir)?;
    if value["total"].as_f64().unwrap_or(0.0) <= 0.0 { return None; }
    Some(json!({"input":value["input_tokens"],"output":value["output_tokens"],
        "cached":value["cached_tokens"],"reasoning":0,"total":value["total"],
        "files":value["files"],"exact_tokens":value["exact_tokens"],
        "estimated_tokens":value["estimated_tokens"],"source":value["source"],
        "coverage":value["coverage"]}))
}

pub fn compute_trend(app_id: &str, data_dir: &str) -> Option<Value> {
    let mut value = collect(app_id, data_dir)?;
    value["days"].as_array().filter(|days| !days.is_empty())?;
    value.as_object_mut()?.remove("_records");
    Some(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn independent_sources_do_not_hold_global_cache_lock() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let gate = Arc::new(std::sync::Barrier::new(2));
        let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let mut handles = vec![];
        for index in 0..2 {
            let active = active.clone(); let peak = peak.clone(); let gate = gate.clone();
            handles.push(std::thread::spawn(move || {
                gate.wait();
                cached_source(format!("test-source-{stamp}-{index}"), || {
                    let current = active.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(current, Ordering::SeqCst);
                    std::thread::sleep(Duration::from_millis(500));
                    active.fetch_sub(1, Ordering::SeqCst);
                    Ok(None)
                }).unwrap()
            }));
        }
        for handle in handles { handle.join().unwrap(); }
        assert_eq!(peak.load(Ordering::SeqCst), 2, "separate sources can read concurrently");
    }

    #[test]
    fn simultaneous_same_source_is_fetched_only_once() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let calls = Arc::new(AtomicUsize::new(0));
        let gate = Arc::new(std::sync::Barrier::new(2));
        let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let mut handles = vec![];
        for _ in 0..2 {
            let calls = calls.clone(); let gate = gate.clone();
            handles.push(std::thread::spawn(move || {
                gate.wait();
                cached_source(format!("test-same-source-{stamp}"), || {
                    calls.fetch_add(1, Ordering::SeqCst);
                    std::thread::sleep(Duration::from_millis(100));
                    Ok(Some(json!({"total": 42})))
                }).unwrap()
            }));
        }
        for handle in handles { assert_eq!(handle.join().unwrap().unwrap()["total"], 42); }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn bundled_python_runs_without_path_and_detects_corruption() {
        let temp = std::env::temp_dir();
        let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let base = temp.join(format!("cockpit-embedded-python-{}-{stamp}", std::process::id()));
        fs::create_dir(&base).expect("create isolated runtime test directory");
        let python = prepare_runtime_at(&base).expect("unpack embedded runtime");
        let run = Command::new(&python).args(["-I", "-c", "import sqlite3; from compression import zstd; print('runtime-ok')"])
            .env("PATH", "").output().expect("run bundled interpreter");
        assert!(run.status.success(), "bundled stdlib, sqlite and zstd must work with empty PATH");
        assert!(String::from_utf8_lossy(&run.stdout).contains("runtime-ok"));
        let runtime_dir = python.parent().unwrap();
        validate_runtime(runtime_dir).expect("valid archive should pass verification");
        fs::write(runtime_dir.join("python314._pth"), b"corrupt").unwrap();
        assert!(prepare_runtime_at(&base).is_err(), "corrupted runtime must never execute");
        let resolved = base.canonicalize().unwrap();
        assert!(resolved.starts_with(temp.canonicalize().unwrap()));
        fs::remove_dir_all(resolved).unwrap();
    }

    #[test]
    fn runtime_follows_data_directory_changes() {
        let temp = std::env::temp_dir();
        let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let root = temp.join(format!("cockpit-runtime-migration-{}-{stamp}", std::process::id()));
        let first = root.join("first");
        let second = root.join("second");
        fs::create_dir_all(&first).unwrap();
        fs::create_dir_all(&second).unwrap();
        let cache = Mutex::new(None);
        let old = bundled_python_at(&first, &cache).unwrap();
        let new = bundled_python_at(&second, &cache).unwrap();
        assert_ne!(old, new);
        assert!(new.starts_with(&second));
        assert_eq!(bundled_python_at(&second, &cache).unwrap(), new);
        let result = Command::new(&new).args(["-I", "-c", "import sqlite3"]).env("PATH", "").status().unwrap();
        assert!(result.success());
        let resolved = root.canonicalize().unwrap();
        assert!(resolved.starts_with(temp.canonicalize().unwrap()));
        fs::remove_dir_all(resolved).unwrap();
    }

    #[test]
    fn parse_codex_token_count() {
        let dir = std::env::temp_dir().join("cockpit-usage-test");
        let sessions = dir.join("sessions");
        let _ = fs::create_dir_all(&sessions);
        let f = sessions.join("rollout.jsonl");
        let mut lines = vec![
            r#"{"type":"session_meta","payload":{}}"#,
            r#"{"type":"event_msg","payload":{"type":"task_started"}}"#,
        ];
        // 两次 token_count：第二次是累计终值
        lines.push(
            r#"{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":100,"cached_input_tokens":10,"output_tokens":20,"reasoning_output_tokens":5,"total_tokens":120}}}}"#,
        );
        lines.push(
            r#"{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":200,"cached_input_tokens":20,"output_tokens":30,"reasoning_output_tokens":6,"total_tokens":230}}}}"#,
        );
        let _ = fs::write(&f, lines.join("\n"));

        let v = compute("codex", dir.to_string_lossy().as_ref()).expect("应能聚合出用量");
        assert_eq!(v["total"].as_f64(), Some(230.0), "应取最后一次累计值");
        assert_eq!(v["input"].as_f64(), Some(200.0));
        assert_eq!(v["output"].as_f64(), Some(30.0));
        assert_eq!(v["files"].as_i64(), Some(1));

        // 非 codex（空目录）返回 None：claude 只在 projects/ 有日志时才给得出
        assert!(compute("marvis", dir.to_string_lossy().as_ref()).is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn trend_groups_by_local_day_and_sessions() {
        let dir = std::env::temp_dir().join("cockpit-trend-test");
        let sessions = dir.join("sessions");
        let _ = fs::create_dir_all(&sessions);
        let f = sessions.join("rollout.jsonl");
        // 同一天两轮（增量 70 + 80），另一天一轮（增量 40）；
        // 2026-05-27T16:20:00Z 在北京时间已是 5-28，按本机时区归日
        let lines = [
            r#"{"type":"event_msg","timestamp":"2026-05-27T06:07:18.088Z","payload":{"type":"token_count","info":{"last_token_usage":{"total_tokens":70},"total_token_usage":{"total_tokens":70}}}}"#,
            r#"{"type":"event_msg","timestamp":"2026-05-27T07:00:00.000Z","payload":{"type":"token_count","info":{"last_token_usage":{"total_tokens":80},"total_token_usage":{"total_tokens":150}}}}"#,
            r#"{"type":"event_msg","timestamp":"2026-05-27T16:20:00.000Z","payload":{"type":"token_count","info":{"last_token_usage":{"total_tokens":40},"total_token_usage":{"total_tokens":40}}}}"#,
        ];
        let _ = fs::write(&f, lines.join("\n"));
        let t = compute_trend("codex", dir.to_string_lossy().as_ref()).expect("应能聚出按天序列");
        assert_eq!(t["unit"], "tokens");
        assert_eq!(t["total"].as_f64(), Some(190.0));
        let days = t["days"].as_array().unwrap();
        assert_eq!(days.len(), 2, "跨时区那条要单独归一天");
        assert_eq!(days[0]["date"], "2026-05-27");
        assert_eq!(days[0]["credits"].as_f64(), Some(150.0));
        assert_eq!(days[0]["sessions"].as_i64(), Some(1), "场次按会话文件数，不按事件数");
        assert_eq!(days[1]["credits"].as_f64(), Some(40.0));
        assert_eq!(t["peak"].as_f64(), Some(150.0));
        assert_eq!(t["peak_date"], "2026-05-27");
        assert_eq!(t["source"], "本机 Codex 会话日志");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn trend_reads_claude_projects_logs() {
        let dir = std::env::temp_dir().join("cockpit-trend-claude");
        let projs = dir.join("projects");
        let _ = fs::create_dir_all(&projs);
        let f = projs.join("s1.jsonl");
        let _ = fs::write(
            &f,
            concat!(
                r#"{"type":"user","timestamp":"2026-10-01T02:00:00.000Z","message":{"role":"user"}}"#,
                "\n",
                r#"{"type":"assistant","timestamp":"2026-10-01T02:00:05.000Z","message":{"role":"assistant","usage":{"input_tokens":100,"output_tokens":20,"cache_read_input_tokens":30,"cache_creation_input_tokens":5}}}"#,
            ),
        );
        let t = compute_trend("claude", dir.to_string_lossy().as_ref()).expect("应能聚出按天序列");
        assert_eq!(t["total"].as_f64(), Some(155.0), "四种 usage 全计入");
        assert_eq!(t["days"][0]["sessions"].as_i64(), Some(1));
        assert_eq!(t["source"], "本机 Claude Code 会话日志");
        // 没日志的产物：整块不出现，不摆空图
        assert!(compute_trend("claude", std::env::temp_dir().join("cockpit-trend-none").to_string_lossy().as_ref()).is_none());
        let _ = fs::remove_dir_all(&dir);
    }
}
