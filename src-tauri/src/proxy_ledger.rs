//! 反代用量账本：把网关自己记下的每笔调用折成「按 Agent、按日」的 token 用量。
//!
//! 为什么要有它：TRAE CN / 妙手这类客户端**自己不保存带 token 的会话记录**
//! （TRAE 的 `ModularData/ai-agent/database.db` 是加密库，妙手日志里没有用量），
//! 但当它们经驾驶舱反代出模型时，`gateway_cost::record` 已经把上游返回的 usage
//! 逐笔写进 `gateway-usage.jsonl`，并且 `model` 字段是 `owner/裸模型名`——
//! 前缀就是上游归属。也就是说这些调用**本来就有账，只是没进用量统计页**。
//!
//! 边界要说清楚：账本只覆盖"经驾驶舱反代"的调用，不覆盖 Agent 客户端直连厂商
//! 的那部分。所以它是本地日志之外的**补充来源**，有本地记录的 Agent 上不重复计入。
use chrono::TimeZone;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader};

/// 上游组名 → 驾驶舱 app id。Trae CN 的上游组叫 `trae`，与 app id 不同名；
/// 其余（astudio / catpaw / qwenwork / zcode / qoder…）两边一致。
fn app_for_owner(owner: &str) -> &str {
    match owner {
        "trae" => "traework-cn",
        other => other,
    }
}

pub fn ledger_path() -> std::path::PathBuf {
    crate::registry::base_dir().join("gateway-usage.jsonl")
}

fn number(v: &Value) -> f64 {
    v.as_f64().filter(|n| n.is_finite() && *n >= 0.0).unwrap_or(0.0)
}

/// 每条调用一行记录，形状与 usage-local.py 聚合出的事件一致，
/// 这样 `usage_dashboard` 的去重/按日分桶逻辑可以原样复用。
pub fn records() -> Vec<Value> {
    let Ok(file) = fs::File::open(ledger_path()) else { return Vec::new(); };
    let mut out = Vec::new();
    for (index, line) in BufReader::new(file).lines().map_while(Result::ok).enumerate() {
        // 账本是追加写；进程被杀可能留下半行，坏行跳过即可，不能让整本失效。
        let Ok(call) = serde_json::from_str::<Value>(&line) else { continue; };
        let model = call["model"].as_str().unwrap_or("");
        let Some((owner, bare)) = model.split_once('/') else { continue; };
        let owner = owner.trim();
        if owner.is_empty() || bare.trim().is_empty() {
            continue;
        }
        let Some(usage) = call["usage"].as_object() else { continue; };
        let input = number(&usage["input"]);
        let output = number(&usage["output"]);
        let cache_read = number(&usage["cache_read"]);
        let cache_write = number(&usage["cache_write"]);
        let tokens = input + output + cache_read + cache_write;
        if tokens <= 0.0 {
            continue;
        }
        let ts = call["ts"].as_i64().unwrap_or(0);
        let Some(stamp) = chrono::Local.timestamp_opt(ts, 0).single() else { continue; };
        out.push(json!({
            "id": format!("gateway-{index}"),
            "session": format!("gateway-{}", stamp.format("%Y-%m-%d")),
            "date": stamp.format("%Y-%m-%d").to_string(),
            "timestamp": ts as f64,
            "model": bare.trim(),
            "provider": owner,
            "app_id": app_for_owner(owner),
            "tokens": tokens,
            // 反代拿到的是上游自己回报的 usage，不是按文本估的。
            "exact_tokens": tokens,
            "estimated_tokens": 0.0,
            "input_tokens": input,
            "output_tokens": output,
            "cached_tokens": cache_read + cache_write,
            "requests": 1.0,
        }));
    }
    out
}

/// 按 app 分组的原始记录。没有账目的 app 不出现在结果里。
pub fn by_app() -> BTreeMap<String, Vec<Value>> {
    let mut out: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    for record in records() {
        let app = record["app_id"].as_str().unwrap_or("").to_string();
        if app.is_empty() {
            continue;
        }
        out.entry(app).or_default().push(record);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test] fn trae_owner_maps_to_cn_app_id() {
        assert_eq!(app_for_owner("trae"), "traework-cn");
        assert_eq!(app_for_owner("astudio"), "astudio");
        assert_eq!(app_for_owner("catpaw"), "catpaw");
    }
}
