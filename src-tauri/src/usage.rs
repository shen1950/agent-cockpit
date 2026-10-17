// 本地会话用量聚合：从 CLI 工具的会话日志汇总真实 Token 用量（对标 Cockpit Tools 的做法）
// Cockpit Tools 的做法之一就是「从本机会话日志汇总真实用量，不依赖官方配额接口」。
// 内置两家，都逐条读日志、按时钟落账，不猜：
//   · Codex：<data_dir>/sessions/**/*.jsonl 的 token_count 事件（累计值取最后一次）
//   · Claude Code：<data_dir>/projects/*.jsonl 的 assistant usage 逐条累加
// 按天序列（compute_trend）也在这里聚：驾驶舱的波形图 / 热力图只用它。
use serde_json::{json, Value};
use std::fs;
use std::path::Path;

#[derive(Default)]
struct Tokens {
    input: f64,
    output: f64,
    cached: f64,
    reasoning: f64,
    total: f64,
}

/// 按 app_id 分发到各 CLI 工具的会话日志解析器；返回可直接存进实例 meta 的 JSON。
pub fn compute(app_id: &str, data_dir: &str) -> Option<Value> {
    let dir = Path::new(data_dir);
    let mut agg = Tokens::default();
    let mut files = 0usize;
    match app_id {
        "codex" => walk_jsonl(&dir.join("sessions"), &mut |p| {
            files += 1;
            if let Some(t) = codex_file_totals(p) {
                agg.input += t.input;
                agg.output += t.output;
                agg.cached += t.cached;
                agg.reasoning += t.reasoning;
                agg.total += t.total;
            }
        }),
        // Claude Code：projects/*.jsonl 里每条 assistant 消息带一份 usage，
        // 一个文件就是一场会话，逐条累加（它不像 Codex 那样记累计值）
        "claude" => walk_jsonl(&dir.join("projects"), &mut |p| {
            files += 1;
            if let Some(t) = claude_file_totals(p) {
                agg.input += t.input;
                agg.output += t.output;
                agg.cached += t.cached;
                agg.total += t.total;
            }
        }),
        _ => return None,
    }
    if files == 0 || agg.total <= 0.0 {
        return None;
    }
    Some(json!({
        "input": agg.input,
        "output": agg.output,
        "cached": agg.cached,
        "reasoning": agg.reasoning,
        "total": agg.total,
        "files": files,
    }))
}

fn walk_jsonl(dir: &Path, f: &mut impl FnMut(&Path)) {
    let rd = match fs::read_dir(dir) {
        Ok(r) => r,
        Err(_) => return,
    };
    for e in rd.filter_map(|x| x.ok()) {
        let p = e.path();
        if let Ok(t) = e.file_type() {
            if t.is_dir() {
                walk_jsonl(&p, f);
            } else if p.extension().map(|x| x == "jsonl").unwrap_or(false) {
                f(&p);
            }
        }
    }
}

/// 单个 Codex rollout 文件里，token_count 的 total_token_usage 是累计值，取最后一次即可。
fn codex_file_totals(path: &Path) -> Option<Tokens> {
    let s = fs::read_to_string(path).ok()?;
    let mut last: Option<Tokens> = None;
    for line in s.lines() {
        if line.is_empty() {
            continue;
        }
        let v: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if v["type"].as_str() != Some("event_msg")
            || v["payload"]["type"].as_str() != Some("token_count")
        {
            continue;
        }
        let u = &v["payload"]["info"]["total_token_usage"];
        let mut t = Tokens::default();
        t.input = u["input_tokens"].as_f64().unwrap_or(0.0);
        t.output = u["output_tokens"].as_f64().unwrap_or(0.0);
        t.cached = u["cached_input_tokens"].as_f64().unwrap_or(0.0);
        t.reasoning = u["reasoning_output_tokens"].as_f64().unwrap_or(0.0);
        t.total = u["total_tokens"]
            .as_f64()
            .unwrap_or(t.input + t.output + t.cached + t.reasoning);
        last = Some(t);
    }
    last
}

/// 按天聚合的用量序列——驾驶舱「消耗趋势」（波形图 + 热力图）的数据源。
/// 判据很硬：只有本机日志逐笔记了**时间**与**消耗**的产品才给得出。目前两家：
///   · Codex：<data_dir>/sessions/**/*.jsonl 里带 timestamp 的 token_count 事件
///   · Claude Code：<data_dir>/projects/*.jsonl 里带 timestamp 的 assistant usage
/// 读不到就整块不出现——摆一张空图占位等于骗人（同"官方没给总额不许当 0"）。
/// 单位统一 tokens：驾驶舱不把本地 token 数折算成别家的积分，那才是编数。
pub fn compute_trend(app_id: &str, data_dir: &str) -> Option<Value> {
    let dir = Path::new(data_dir);
    let mut days: std::collections::BTreeMap<String, DayAgg> = std::collections::BTreeMap::new();
    match app_id {
        "codex" => {
            // sessions 按"当天有几个会话文件"数，不是事件数——热力图脚注写的是"场会话"
            let mut seen: std::collections::HashMap<String, std::collections::HashSet<String>> =
                std::collections::HashMap::new();
            walk_jsonl(&dir.join("sessions"), &mut |p| {
                let s = match fs::read_to_string(p) {
                    Ok(v) => v,
                    Err(_) => return,
                };
                let fid = p.to_string_lossy().to_string();
                // 同一个会话文件内，total_token_usage 是累计值：按天切的时候要么用
                // last_token_usage（这一轮的增量），要么跟上一次的累计值做差
                let mut prev_by_day: std::collections::HashMap<String, f64> = std::collections::HashMap::new();
                for line in s.lines() {
                    if line.is_empty() {
                        continue;
                    }
                    let v: Value = match serde_json::from_str(line) {
                        Ok(v) => v,
                        Err(_) => continue,
                    };
                    if v["type"].as_str() != Some("event_msg")
                        || v["payload"]["type"].as_str() != Some("token_count")
                    {
                        continue;
                    }
                    let day = local_day(v["timestamp"].as_str().unwrap_or(""));
                    let inc = v["payload"]["info"]["last_token_usage"]["total_tokens"]
                        .as_f64()
                        .or_else(|| {
                            let cur = v["payload"]["info"]["total_token_usage"]["total_tokens"]
                                .as_f64()
                                .unwrap_or(0.0);
                            let prev = prev_by_day.insert(day.clone(), cur).unwrap_or(0.0);
                            Some((cur - prev).max(0.0))
                        })
                        .unwrap_or(0.0);
                    let e = days.entry(day.clone()).or_default();
                    e.tokens += inc;
                    seen.entry(day).or_default().insert(fid.clone());
                }
            });
            // 场次数落实到每天
            for (d, files) in seen {
                if let Some(e) = days.get_mut(&d) {
                    e.sessions = files.len();
                }
            }
        }
        "claude" => {
            let mut seen: std::collections::HashMap<String, std::collections::HashSet<String>> =
                std::collections::HashMap::new();
            walk_jsonl(&dir.join("projects"), &mut |p| {
                let s = match fs::read_to_string(p) {
                    Ok(v) => v,
                    Err(_) => return,
                };
                let fid = p.to_string_lossy().to_string();
                for line in s.lines() {
                    if line.is_empty() {
                        continue;
                    }
                    let v: Value = match serde_json::from_str(line) {
                        Ok(v) => v,
                        Err(_) => continue,
                    };
                    if v["type"].as_str() != Some("assistant") {
                        continue;
                    }
                    let day = local_day(v["timestamp"].as_str().unwrap_or(""));
                    let u = &v["message"]["usage"];
                    let inc = u["input_tokens"].as_f64().unwrap_or(0.0)
                        + u["output_tokens"].as_f64().unwrap_or(0.0)
                        + u["cache_read_input_tokens"].as_f64().unwrap_or(0.0)
                        + u["cache_creation_input_tokens"].as_f64().unwrap_or(0.0);
                    if inc <= 0.0 {
                        continue;
                    }
                    let e = days.entry(day.clone()).or_default();
                    e.tokens += inc;
                    seen.entry(day).or_default().insert(fid.clone());
                }
            });
            for (d, files) in seen {
                if let Some(e) = days.get_mut(&d) {
                    e.sessions = files.len();
                }
            }
        }
        _ => return None,
    }
    let mut out: Vec<Value> = Vec::new();
    let mut peak = 0.0f64;
    let mut peak_day = String::new();
    for (d, a) in &days {
        if a.tokens <= 0.0 {
            continue;
        }
        if a.tokens > peak {
            peak = a.tokens;
            peak_day = d.clone();
        }
        out.push(json!({ "date": d, "credits": a.tokens, "sessions": a.sessions }));
    }
    if out.is_empty() {
        return None;
    }
    let total: f64 = out.iter().map(|d| d["credits"].as_f64().unwrap_or(0.0)).sum();
    let src = if app_id == "codex" { "本机 Codex 会话日志" } else { "本机 Claude Code 会话日志" };
    Some(json!({
        "unit": "tokens",
        "days": out,
        "total": total,
        "peak": peak,
        "peak_date": peak_day,
        "source": src,
    }))
}

#[derive(Default)]
struct DayAgg {
    tokens: f64,
    sessions: usize,
}

/// UTC 时间戳（RFC3339，如 2026-05-27T06:07:18.088Z）→ 本机日期 YYYY-MM-DD。
/// 驾驶舱所有时间都按本机显示，"昨晚跑的"不能算到第二天头上。
fn local_day(ts: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(ts)
        .map(|d| d.with_timezone(&chrono::Local).format("%Y-%m-%d").to_string())
        .unwrap_or_else(|_| ts.get(..10).unwrap_or("").to_string())
}

/// 单个 Claude Code 会话文件：assistant 消息的 usage 逐条累加。
/// cache_read / cache_creation 也计入——它们照样是花掉的 token。
fn claude_file_totals(path: &Path) -> Option<Tokens> {
    let s = fs::read_to_string(path).ok()?;
    let mut t = Tokens::default();
    let mut any = false;
    for line in s.lines() {
        if line.is_empty() {
            continue;
        }
        let v: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if v["type"].as_str() != Some("assistant") {
            continue;
        }
        let u = &v["message"]["usage"];
        t.input += u["input_tokens"].as_f64().unwrap_or(0.0);
        t.output += u["output_tokens"].as_f64().unwrap_or(0.0);
        t.cached += u["cache_read_input_tokens"].as_f64().unwrap_or(0.0)
            + u["cache_creation_input_tokens"].as_f64().unwrap_or(0.0);
        any = true;
    }
    if !any {
        return None;
    }
    t.total = t.input + t.output + t.cached;
    Some(t)
}

#[cfg(test)]
mod tests {
    use super::*;


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
