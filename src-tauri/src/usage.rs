// 本地会话用量聚合：从 CLI 工具的会话日志汇总真实 Token 用量（对标 Cockpit Tools 的做法）
// Cockpit Tools 的做法之一就是「从本机会话日志汇总真实用量，不依赖官方配额接口」。
// 目前内置 Codex（<data_dir>/sessions/**/*.jsonl 的 token_count 事件）；Claude Code 的
// projects/*.jsonl 结构不同，留待后续按需补。
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

        // 非 codex 返回 None
        assert!(compute("claude", dir.to_string_lossy().as_ref()).is_none());
        let _ = fs::remove_dir_all(&dir);
    }
}
