//! Read-only usage aggregation for local Agent histories.
//! Token totals, visible-text estimates and native vendor credits stay separate.
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

static CACHE: OnceLock<Mutex<HashMap<String, (Instant, Option<Value>)>>> = OnceLock::new();
const SCRIPT: &str = include_str!("../scripts/usage-local.py");
const BOOTSTRAP: &str = "import io,json,sys; p=json.loads(sys.stdin.buffer.read().decode('utf-8')); sys.stdin=io.StringIO(json.dumps(p['request'])); exec(compile(p['script'],'<AgentCockpit usage>','exec'),{'__name__':'__main__'})";

fn supported(app: &str) -> bool {
    matches!(app, "codex" | "chatgpt" | "claude" | "claude-code" | "qoder" | "qoder-cn"
        | "qoderwork" | "qwenwork" | "qwenworkcn" | "workbuddy" | "workbuddy-intl"
        | "codebuddy" | "codebuddy-cn" | "zcode" | "opencode" | "minimax"
        | "marvis" | "gemini" | "gemini-cli" | "dsh")
}

/// Returns canonical source identity, daily buckets, models and timing samples.
/// Empty or unsupported profiles never fall back to another account's history.
pub fn collect(app_id: &str, data_dir: &str) -> Option<Value> {
    if !supported(app_id) || data_dir.trim().is_empty() || !Path::new(data_dir).is_dir() {
        return None;
    }
    let path = Path::new(data_dir).canonicalize().ok()?;
    let key = format!("{}|{}", app_id, path.to_string_lossy().to_lowercase());
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let mut held = cache.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((at, value)) = held.get(&key) {
        if at.elapsed() < Duration::from_secs(15) { return value.clone(); }
    }
    let value = run_helper(app_id, &path);
    held.insert(key, (Instant::now(), value.clone()));
    value
}

fn run_helper(app_id: &str, data_dir: &Path) -> Option<Value> {
    let installed = crate::registry::home().join(".agenthub/venv/Scripts/python.exe");
    let mut command = Command::new(if installed.is_file() { installed } else { PathBuf::from("python") });
    command.args(["-I", "-X", "utf8", "-c", BOOTSTRAP])
        .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
    #[cfg(windows)] {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    let mut child = command.spawn().ok()?;
    let mut reader = child.stdout.take()?;
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
    if !written { let _ = child.kill(); let _ = child.wait(); let _ = output.join(); return None; }
    let started = Instant::now();
    let success = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status.success(),
            Ok(None) if started.elapsed() < Duration::from_secs(90) => std::thread::sleep(Duration::from_millis(50)),
            _ => { let _ = child.kill(); let _ = child.wait(); break false; }
        }
    };
    let bytes = output.join().ok()??;
    if !success || bytes.len() > 128 * 1024 * 1024 { return None; }
    let value: Value = serde_json::from_slice(&bytes).ok()?;
    value.is_object().then_some(value)
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
