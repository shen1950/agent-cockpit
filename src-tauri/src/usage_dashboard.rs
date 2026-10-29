//! Local usage inventory. Reading histories never changes an Agent's login or quota.
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

#[derive(Default)]
struct Cache {
    document: Value,
    profiles: HashMap<String, Value>,
    previous: BTreeMap<String, (f64, f64)>,
    sampled_at: f64,
    sampled_day: String,
    samples: usize,
    history: Vec<Value>,
    source_history: HashMap<String, Vec<Value>>,
    app_source_history: HashMap<String, Vec<Value>>,
    selected_source_history: HashMap<String, Vec<Value>>,
    previous_app: BTreeMap<String, (f64, f64)>,
    previous_selected: BTreeMap<String, (f64, f64)>,
}
static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();
static SCAN: Mutex<()> = Mutex::new(());
fn cache() -> &'static Mutex<Cache> { CACHE.get_or_init(|| Mutex::new(load_history())) }
fn now() -> f64 { chrono::Utc::now().timestamp_millis() as f64 / 1000.0 }
fn number(v: &Value) -> f64 { v.as_f64().filter(|n| n.is_finite() && *n >= 0.0).unwrap_or(0.0) }
const SAMPLE_RETENTION_SECONDS: f64 = 7.0 * 86400.0;
const SAMPLE_MAX_PER_SOURCE: usize = 10080;
// Bound both memory and disk even when many products have been monitored.
const SAMPLE_MAX_TOTAL: usize = 100_000;
const SAMPLE_MAX_FILE_BYTES: usize = 16 * 1024 * 1024;
fn history_path() -> PathBuf { crate::registry::base_dir().join("usage-monitor-history.json") }
fn clean_history(history: &mut Vec<Value>, timestamp: f64) {
    history.retain(|v| v["timestamp"].as_f64().map(|t| t >= timestamp-SAMPLE_RETENTION_SECONDS && t <= timestamp+300.0).unwrap_or(false));
    if history.len() > SAMPLE_MAX_PER_SOURCE { history.drain(..history.len() - SAMPLE_MAX_PER_SOURCE); }
}
fn load_history() -> Cache {
    let mut c = Cache::default();
    let path = history_path();
    let Ok(metadata) = std::fs::metadata(&path) else { return c; };
    if metadata.len() > SAMPLE_MAX_FILE_BYTES as u64 { return c; }
    let Ok(bytes) = std::fs::read(path) else { return c; };
    let Ok(value) = serde_json::from_slice::<Value>(&bytes) else { return c; };
    if value["version"] == 2 {
        c.history = unpack_samples(&value["history"]);
        c.source_history = unpack_map(&value["by_source"]);
        c.app_source_history = unpack_map(&value["by_app_source"]);
        c.selected_source_history = unpack_map(&value["by_selected_source"]);
    } else if value["version"] == 1 {
        c.history = serde_json::from_value(value["history"].clone()).unwrap_or_default();
        c.source_history = serde_json::from_value(value["by_source"].clone()).unwrap_or_default();
        c.app_source_history = serde_json::from_value(value["by_app_source"].clone()).unwrap_or_default();
        c.selected_source_history = serde_json::from_value(value["by_selected_source"].clone()).unwrap_or_default();
    } else { return c; }
    clean_history(&mut c.history, now());
    for histories in [&mut c.source_history, &mut c.app_source_history, &mut c.selected_source_history] {
        for history in histories.values_mut() { clean_history(history, now()); }
        histories.retain(|_, values| !values.is_empty());
    }
    c
}
fn pack_samples(samples: &[Value]) -> Value {
    json!(samples.iter().filter_map(|v| {
        Some([v["timestamp"].as_f64()?, v["exact_tokens_per_minute"].as_f64()?, v["estimated_tokens_per_minute"].as_f64()?])
    }).collect::<Vec<_>>())
}
fn unpack_samples(value: &Value) -> Vec<Value> {
    value.as_array().into_iter().flatten().filter_map(|row| {
        let a = row.as_array()?;
        if a.len() != 3 { return None; }
        let t = a[0].as_f64()?; let exact = a[1].as_f64()?; let estimated = a[2].as_f64()?;
        if !t.is_finite() || !exact.is_finite() || !estimated.is_finite() || exact < 0.0 || estimated < 0.0 { return None; }
        Some(json!({"timestamp":t,"exact_tokens_per_minute":exact,"estimated_tokens_per_minute":estimated,"tokens_per_minute":exact+estimated}))
    }).collect()
}
fn pack_map(map: &HashMap<String,Vec<Value>>) -> Value {
    Value::Object(map.iter().map(|(key, samples)| (key.clone(), pack_samples(samples))).collect())
}
fn unpack_map(value: &Value) -> HashMap<String,Vec<Value>> {
    value.as_object().into_iter().flat_map(|map| map.iter())
        .map(|(key, samples)| (key.clone(), unpack_samples(samples))).collect()
}
fn prune_history(c: &mut Cache, timestamp: f64) {
    clean_history(&mut c.history, timestamp);
    for histories in [&mut c.source_history, &mut c.app_source_history, &mut c.selected_source_history] {
        for history in histories.values_mut() { clean_history(history, timestamp); }
        // Sources not seen in this run remain available until their samples expire.
        histories.retain(|_, samples| !samples.is_empty());
    }
    loop {
        let count = c.history.len() + c.source_history.values().map(Vec::len).sum::<usize>()
            + c.app_source_history.values().map(Vec::len).sum::<usize>()
            + c.selected_source_history.values().map(Vec::len).sum::<usize>();
        if count <= SAMPLE_MAX_TOTAL { break; }
        // Keep the newest half in each series. Older points are still available
        // from timestamped event logs when those records exist.
        let trim = |samples: &mut Vec<Value>| { let n = samples.len()/2; if n > 0 { samples.drain(..n); } };
        trim(&mut c.history);
        for histories in [&mut c.source_history, &mut c.app_source_history, &mut c.selected_source_history] {
            for history in histories.values_mut() { trim(history); }
        }
        // One point per source could itself exceed the cap; drop oldest series.
        if count > SAMPLE_MAX_TOTAL && c.source_history.values().all(|h| h.len() <= 1)
            && c.app_source_history.values().all(|h| h.len() <= 1)
            && c.selected_source_history.values().all(|h| h.len() <= 1) {
            for histories in [&mut c.source_history, &mut c.app_source_history, &mut c.selected_source_history] {
                let over = count.saturating_sub(SAMPLE_MAX_TOTAL);
                let keys: Vec<_> = histories.keys().take(over).cloned().collect();
                for key in keys { histories.remove(&key); }
            }
        }
    }
}
fn persist_history(c: &Cache) -> Result<(), &'static str> {
    let path = history_path();
    if let Some(parent) = path.parent() { std::fs::create_dir_all(parent).map_err(|_| "无法创建监测历史目录")?; }
    let value = json!({"version":2,"history":pack_samples(&c.history),"by_source":pack_map(&c.source_history),
        "by_app_source":pack_map(&c.app_source_history),"by_selected_source":pack_map(&c.selected_source_history)});
    let bytes = serde_json::to_vec(&value).map_err(|_| "无法编码监测历史")?;
    if bytes.len() > SAMPLE_MAX_FILE_BYTES { return Err("监测历史超过存储上限"); }
    let tmp = path.with_extension(format!("{}.tmp", std::process::id()));
    std::fs::write(&tmp, bytes).map_err(|_| "无法写入监测历史")?;
    if std::fs::rename(&tmp, &path).is_err() {
        let _ = std::fs::remove_file(tmp);
        return Err("无法保存监测历史");
    }
    Ok(())
}
fn path_key(path: &Path) -> String {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf()).to_string_lossy()
        .replace('\\', "/").trim_start_matches("//?/").trim_end_matches('/').to_lowercase()
}
fn profile_key(app: &str, dir: &Path) -> String { format!("{app}|{}", path_key(dir)) }

/// Runtime histories may live separately from the Electron login profile.
/// Only the native profile maps to the shared history, never an arbitrary account copy.
fn history_dir(app: &str, profile: &Path, native: Option<&Path>) -> PathBuf {
    if native.map(path_key).as_deref() != Some(path_key(profile).as_str()) {
        return profile.to_path_buf();
    }
    let relative = match app {
        "qoder" => Some(".qoder"),
        "qoder-cn" => Some(".qoder-cn"),
        "qwenwork" => Some(".qwenworkcn"),
        "traework-cn" => Some(".trae-cn"),
        _ => None,
    };
    relative.map(|p| crate::registry::home().join(p)).unwrap_or_else(|| profile.to_path_buf())
}

fn account_history_dir(app: &str, profile: &Path, native: Option<&Path>, meta: &Value) -> PathBuf {
    if app == "codex-desktop" {
        if native.map(path_key) == Some(path_key(profile)) { return crate::registry::home().join(".codex"); }
        if let Some(home) = meta["chatgpt_home"].as_str().filter(|s| !s.is_empty()) {
            let path = PathBuf::from(home);
            let allowed = crate::registry::base_dir().join("chatgpt-auth");
            if path.is_absolute() && path.starts_with(&allowed) && path != allowed
                && !path.components().any(|c| matches!(c, std::path::Component::ParentDir)) {
                return path;
            }
        }
        return profile.to_path_buf();
    }
    history_dir(app, profile, native)
}

pub fn unavailable_reason(app: &str) -> &'static str {
    match app {
        "codex-desktop" => "此实例的会话历史没有可读取的 token 用量记录；共享对话不等于按账号划分的账单。",
        "traework-cn" => "未发现可读取、带消耗及日期的 TRAE CN 会话记录；摘要文件不能代表完整模型调用。",
        "antigravity" => "目前只读到模型配额，尚未发现可按日期统计的完整 token 记录。",
        "catpaw" => "尚未发现带日期和 token 消耗的妙手本地请求记录；第三方直连是否计入取决于客户端保存的数据。",
        _ => "没有可读取的按日期用量记录。客户端需保存 token usage 或可估算的对话文本。",
    }
}

fn decorate(mut trend: Value, app: &str, name: &str, instance: &str, id: &str) -> Value {
    trend["id"] = json!(id);
    trend["app_id"] = json!(app);
    trend["app_name"] = json!(name);
    trend["instance_id"] = json!(instance);
    trend["instance_name"] = json!(instance);
    trend["status"] = json!("ready");
    trend["count_in_total"] = json!(true);
    trend
}

const METRICS: &[&str] = &["tokens", "exact_tokens", "estimated_tokens", "input_tokens", "output_tokens",
    "cached_tokens", "requests", "failures", "timed_output_tokens", "duration_ms", "timing_samples", "ttft_ms", "ttft_samples"];
#[derive(Default)]
struct Bucket {
    values: BTreeMap<String, f64>,
    sessions: HashSet<String>,
    models: BTreeMap<(String, String), BTreeMap<String, f64>>,
}
impl Bucket {
    fn add(&mut self, record: &Value) {
        self.sessions.insert(record["session"].as_str().unwrap_or("").to_string());
        let model = (record["model"].as_str().unwrap_or("未记录模型").to_string(),
            record["provider"].as_str().unwrap_or("未记录供应商").to_string());
        let model_values = self.models.entry(model).or_default();
        for key in METRICS {
            let n = number(&record[*key]);
            *self.values.entry((*key).into()).or_default() += n;
            *model_values.entry((*key).into()).or_default() += n;
        }
    }
    fn finish(self) -> Value {
        let mut value = json!(self.values);
        // Empty deduplicated sources still have explicit zero fields.
        for key in METRICS { if value[*key].is_null() { value[*key] = json!(0); } }
        value["sessions"] = json!(self.sessions.len());
        value["models"] = json!(self.models.into_iter().map(|((model,provider),values)| {
            let mut v = json!(values); v["model"] = json!(model); v["provider"] = json!(provider); v
        }).collect::<Vec<_>>());
        value
    }
}

/// Recorded event timestamps allow five-minute averages, never token generation
/// speed. Events without a real timestamp remain in the daily totals only.
fn history_bucket(rows: &[&Value], now_seconds: f64) -> Vec<Value> {
    let cutoff = now_seconds - 31.0 * 86400.0;
    let mut buckets: BTreeMap<i64, (f64, f64, usize)> = BTreeMap::new();
    for row in rows {
        let Some(ts) = row["timestamp"].as_f64().filter(|t| t.is_finite() && *t >= cutoff && *t <= now_seconds + 300.0) else { continue; };
        let slot = ((ts as i64) / 300) * 300;
        let entry = buckets.entry(slot).or_default();
        entry.0 += number(&row["exact_tokens"]);
        entry.1 += number(&row["estimated_tokens"]);
        entry.2 += 1;
    }
    buckets.into_iter().map(|(timestamp, (exact, estimated, events))|
        json!({"timestamp":timestamp,"exact_tokens":exact,"estimated_tokens":estimated,"events":events})).collect()
}
fn history_coverage(rows: &[&Value]) -> Value {
    let timed = rows.iter().filter(|r| r["timestamp"].as_f64().map(|t| t.is_finite() && t > 0.0).unwrap_or(false)).count();
    json!({"timed_events":timed,"without_time_events":rows.len()-timed})
}

/// Keep narrow views independent of the global Codex/ChatGPT deduplication.
/// The per-source copy is already internally deduplicated by the collector;
/// the per-app view selects one owner per record across that app's profiles.
/// Only daily aggregates are copied, never conversation contents or the ledger.
fn prepare_narrow_views(sources: &mut [Value]) {
    let mut owners: HashMap<(String, String, String), (usize, &Value)> = HashMap::new();
    let mut counts: HashMap<usize, usize> = HashMap::new();
    let mut source_history = HashMap::new();
    let mut source_coverage = HashMap::new();
    let cutoff_now = now();
    for (index, source) in sources.iter().enumerate() {
        let Some(records) = source["_records"].as_array() else { continue; };
        counts.insert(index, records.len());
        source_history.insert(index, history_bucket(&records.iter().collect::<Vec<_>>(), cutoff_now));
        source_coverage.insert(index, history_coverage(&records.iter().collect::<Vec<_>>()));
        let app = source["app_id"].as_str().unwrap_or("");
        for record in records {
            let key = (app.to_string(), record["session"].as_str().unwrap_or("").to_string(),
                record["id"].as_str().unwrap_or("").to_string());
            let replace = owners.get(&key).map(|(_, old)| {
                (number(&record["exact_tokens"]), number(&record["tokens"])) >
                    (number(&old["exact_tokens"]), number(&old["tokens"]))
            }).unwrap_or(true);
            if replace { owners.insert(key, (index, record)); }
        }
    }
    let mut daily: HashMap<usize, BTreeMap<String, Bucket>> = HashMap::new();
    let mut retained = HashMap::<usize, usize>::new();
    let mut app_records = HashMap::<usize, Vec<&Value>>::new();
    for (_, (index, record)) in owners {
        *retained.entry(index).or_default() += 1;
        app_records.entry(index).or_default().push(record);
        let date = record["date"].as_str().unwrap_or("");
        if chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d").is_err() { continue; }
        daily.entry(index).or_default().entry(date.to_string()).or_default().add(record);
    }
    let mut finished = HashMap::new();
    for (index, buckets) in daily {
        let days: Vec<Value> = buckets.into_iter().map(|(date, bucket)| {
            let mut day = bucket.finish();
            day["date"] = json!(date);
            day["credits"] = day["tokens"].clone();
            day
        }).collect();
        finished.insert(index, days);
    }
    let mut app_history: HashMap<usize, (Vec<Value>, Value)> = app_records.into_iter()
        .map(|(index, records)| (index, (history_bucket(&records, cutoff_now),history_coverage(&records)))).collect();
    for (index, original) in counts {
        let source = &mut sources[index];
        source["source_historical_5m"] = json!(source_history.remove(&index).unwrap_or_default());
        source["source_historical_coverage"] = source_coverage.remove(&index).unwrap_or_else(|| json!({"timed_events":0,"without_time_events":0}));
        let (history, coverage) = app_history.remove(&index).unwrap_or_else(|| (Vec::new(),json!({"timed_events":0,"without_time_events":0})));
        source["app_historical_5m"] = json!(history);
        source["app_historical_coverage"] = coverage;
        source["source_days"] = source["days"].clone();
        source["source_status"] = source["status"].clone();
        source["app_days"] = json!(finished.remove(&index).unwrap_or_default());
        source["app_status"] = json!(if original > 0 && retained.get(&index).copied().unwrap_or(0) == 0 { "duplicate" } else { "ready" });
    }
}

/// Account copies can contain the same histories under different real paths.
/// Compare record IDs across all profiles, keeping the fullest recorded revision.
/// The internal numeric ledger is removed before anything reaches the WebView.
fn deduplicate_sources(sources: &mut [Value]) {
    let mut owners: HashMap<(String, String, String), (usize, Value)> = HashMap::new();
    let mut counts = HashMap::new();
    for (index, source) in sources.iter_mut().enumerate() {
        let records = source.as_object_mut().and_then(|s| s.remove("_records"));
        let Some(Value::Array(records)) = records else { continue; };
        counts.insert(index, records.len());
        let app = source["app_id"].as_str().unwrap_or("");
        // Desktop histories are synchronized from the Codex store. A matching
        // event is the same usage regardless of which product exposes it.
        let family = if matches!(app, "codex" | "codex-desktop") { "codex-family" } else { app };
        for record in records {
            let identity = (family.to_string(), record["session"].as_str().unwrap_or("").to_string(),
                record["id"].as_str().unwrap_or("").to_string());
            let replace = owners.get(&identity).map(|(_, old)| {
                (number(&record["exact_tokens"]), number(&record["tokens"])) >
                    (number(&old["exact_tokens"]), number(&old["tokens"]))
            }).unwrap_or(true);
            if replace { owners.insert(identity, (index, record)); }
        }
    }
    let mut retained: HashMap<usize, Vec<Value>> = HashMap::new();
    for (_, (index, record)) in owners { retained.entry(index).or_default().push(record); }
    for (index, original_count) in counts {
        let source = &mut sources[index];
        let records = retained.remove(&index).unwrap_or_default();
        source["historical_5m"] = json!(history_bucket(&records.iter().collect::<Vec<_>>(), now()));
        source["historical_coverage"] = history_coverage(&records.iter().collect::<Vec<_>>());
        let duplicates = original_count - records.len();
        let mut total = Bucket::default();
        let mut daily: BTreeMap<String, Bucket> = BTreeMap::new();
        let mut undated = 0usize;
        for record in &records {
            let date = record["date"].as_str().unwrap_or("");
            if chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d").is_err() { undated += 1; continue; }
            total.add(record);
            daily.entry(date.to_string()).or_default().add(record);
        }
        let summary = total.finish();
        for (key, value) in summary.as_object().unwrap() { source[key] = value.clone(); }
        source["total"] = summary["tokens"].clone();
        let mut peak = 0.0;
        let mut peak_day = String::new();
        source["days"] = json!(daily.into_iter().map(|(date, bucket)| {
            let mut day = bucket.finish();
            let tokens = number(&day["tokens"]);
            if tokens > peak { peak = tokens; peak_day = date.clone(); }
            day["date"] = json!(date); day["credits"] = json!(tokens); day
        }).collect::<Vec<_>>());
        source["peak"] = json!(peak);
        source["peak_date"] = json!(peak_day);
        source["duplicate_records"] = json!(duplicates);
        source["undated_records"] = json!(undated);
        if duplicates > 0 {
            let coverage = source["coverage"].as_str().unwrap_or("");
            source["coverage"] = json!(format!("{coverage} 已排除 {duplicates} 条与其他档案重复的模型用量记录。"));
        }
        if records.is_empty() && original_count > 0 {
            source["status"] = json!("duplicate");
            source["count_in_total"] = json!(false);
            source["reason"] = json!("此档案的模型用量记录已由其他档案或 Codex 共享历史计入，汇总不重复计算。账号详情仍可查看共享历史。");
        }
    }
}

fn collect_sources() -> (Vec<Value>, HashMap<String, Value>) {
    let mut sources = Vec::new();
    let mut profiles = HashMap::new();
    let mut seen = HashSet::new();
    let mut billing_seen = HashSet::new();
    for app in crate::registry::all_apps() {
        let native = crate::registry::profile_dir(&app.id);
        let mut accounts = crate::registry::load_instances(&app.id);
        // The native profile is also useful before it has been registered as an account.
        if let Some(dir) = native.as_ref().filter(|p| p.is_dir()) {
            if !accounts.iter().any(|m| m["data_dir"].as_str().map(|s| path_key(Path::new(s))) == Some(path_key(dir))) {
                accounts.push(json!({"name":"本机共用记录", "data_dir":dir.to_string_lossy()}));
            }
        }
        accounts.sort_by_key(|m| {
            native.as_deref().map(path_key) != m["data_dir"].as_str().map(|s| path_key(Path::new(s)))
        });
        let start = sources.len();
        for meta in accounts {
            let instance = meta["name"].as_str().unwrap_or("本机记录");
            let raw_dir = meta["data_dir"].as_str().unwrap_or("");
            if raw_dir.trim().is_empty() { continue; }
            let profile = Path::new(raw_dir);
            let dir = account_history_dir(&app.id, profile, native.as_deref(), &meta);
            let key = profile_key(&app.id, &dir);
            // Same canonical directory, including junction aliases, is counted once.
            let local = if let Some(t) = profiles.get(&key).cloned() { Some(t) }
                else { crate::usage::collect(if app.id == "codex-desktop" { "chatgpt" } else { &app.id }, &dir.to_string_lossy()) };
            let has_tokens = local.as_ref().map(|v| v["days"].as_array().map(|d| !d.is_empty()).unwrap_or(false)).unwrap_or(false);
            if let Some(mut t) = local.filter(|_| has_tokens) {
                if app.id == "codex-desktop" || native.as_deref().map(path_key) == Some(path_key(profile)) {
                    t["scope"] = json!("device");
                    let coverage = t["coverage"].as_str().unwrap_or("");
                    t["coverage"] = json!(format!("本机共用记录，未按登录账号拆分。{coverage}"));
                }
                let source_key = t["source_key"].as_str().unwrap_or(&key).to_string();
                let profile_value = Value::Object(t.as_object().unwrap().iter()
                    .filter(|(key, _)| *key != "_records")
                    .map(|(key, value)| (key.clone(), value.clone())).collect());
                profiles.insert(key.clone(), profile_value.clone());
                profiles.insert(profile_key(&app.id, profile), profile_value);
                if seen.insert(format!("{}|{}", app.id, source_key)) {
                    let mut s = decorate(t, &app.id, &app.name, instance, &format!("{}|{}", app.id, source_key));
                    let errors = s["unreadable_files"].as_u64().unwrap_or(0);
                    if errors > 0 {
                        let coverage = s["coverage"].as_str().unwrap_or("");
                        s["coverage"] = json!(format!("{coverage} 有 {errors} 个文件未能读取，结果不完整。"));
                    }
                    // A shared device history cannot reliably separate switched accounts.
                    if app.id == "codex-desktop" || native.as_deref().map(path_key) == Some(path_key(profile)) {
                        s["instance_name"] = json!("本机共用记录");
                        s["scope"] = json!("device");
                    }
                    sources.push(s);
                }
            }
            // Keep vendor credits visible, but never add their conversion on top of
            // token logs for the same source. Their billing scope may overlap.
            let trend = &meta["quota_trend"];
            if let Some(days) = trend["days"].as_array().filter(|d| !d.is_empty()) {
                let unit = trend["unit"].as_str().unwrap_or("credits");
                if has_tokens && matches!(unit, "token" | "tokens") { continue; }
                let account = meta["quota_user_id"].as_str().filter(|s| !s.is_empty()).unwrap_or(instance);
                let id = format!("{}|billing|{}", app.id, account);
                if !billing_seen.insert(id.clone()) { continue; }
                let mut s = decorate(trend.clone(), &app.id, &app.name, instance, &id);
                s["status"] = json!("cached");
                s["fetched_at"] = meta["quota_fetched_at"].clone();
                s["count_in_total"] = json!(!has_tokens);
                s["days"] = json!(days);
                if has_tokens {
                    s["reason"] = json!("已有本地 token 记录，官方积分保留查看，不再重复折算加入总量。");
                } else {
                    s["reason"] = json!("最近一次额度刷新保存的历史；原始单位保留，未设置换算系数的积分不计入 token 总量。");
                }
                sources.push(s);
            }
            if !has_tokens && trend["days"].as_array().map(|d| d.is_empty()).unwrap_or(true) {
                sources.push(json!({"id":format!("{}|unavailable|{}",app.id,instance), "app_id":app.id,
                    "app_name":app.name,"instance_id":instance,"instance_name":instance,"unit":"tokens",
                    "days":[],"status":"unavailable","count_in_total":false,"reason":unavailable_reason(&app.id)}));
            }
        }
        if sources.len() == start {
            sources.push(json!({"id":format!("{}|unavailable", app.id), "app_id":app.id,
                "app_name":app.name, "instance_id":"", "instance_name":"", "unit":"tokens",
                "days":[], "status":"unavailable", "count_in_total":false,
                "reason":unavailable_reason(&app.id)}));
        }
        // Shared histories may include previously logged-in accounts. Their vendor
        // bills must not be converted a second time under another account card.
        if sources[start..].iter().any(|s| s["status"] == "ready") {
            for s in &mut sources[start..] {
                if s["status"] == "cached" {
                    s["count_in_total"] = json!(false);
                    s["reason"] = json!("此 Agent 已有本地 token 记录；账号账单可能与共用历史重叠，原始用量保留查看，不再重复折算加入总量。");
                }
            }
        }
    }
    prepare_narrow_views(&mut sources);
    deduplicate_sources(&mut sources);
    for (profile, trend) in profiles.iter_mut() {
        // Per-instance history remains visible; dashboard alone deduplicates the
        // shared ledger. Keeping the original also avoids empty duplicate cards.
        let _ = profile;
        if let Some(obj) = trend.as_object_mut() { obj.remove("_records"); }
    }
    (sources, profiles)
}

fn totals_by_source(sources: &[Value], today: &str) -> BTreeMap<String, (f64, f64)> {
    sources.iter().filter(|s| s["status"] == "ready" && s["count_in_total"] != false)
        .map(|s| {
            let (mut exact, mut estimated) = (0.0, 0.0);
            if let Some(days) = s["days"].as_array() {
                for d in days.iter().filter(|d| d["date"].as_str() == Some(today)) {
                    exact += number(&d["exact_tokens"]); estimated += number(&d["estimated_tokens"]);
                }
            }
            (s["id"].as_str().unwrap_or("").to_string(), (exact, estimated))
        }).collect()
}

fn totals_in_view(sources: &[Value], today: &str, days_key: &str, status_key: &str) -> BTreeMap<String, (f64, f64)> {
    sources.iter().filter(|s| s[status_key] == "ready" && s[days_key].is_array())
        .map(|s| {
            let mut total = (0.0, 0.0);
            if let Some(days) = s[days_key].as_array() {
                for d in days.iter().filter(|d| d["date"].as_str() == Some(today)) {
                    total.0 += number(&d["exact_tokens"]); total.1 += number(&d["estimated_tokens"]);
                }
            }
            (s["id"].as_str().unwrap_or("").to_string(), total)
        }).collect()
}

fn source_monitor(sources: &[Value], totals: &BTreeMap<String,(f64,f64)>,
    previous: &BTreeMap<String,(f64,f64)>, histories: &mut HashMap<String,Vec<Value>>,
    timestamp: f64, elapsed: f64, comparable: bool, days_key: &str) -> serde_json::Map<String,Value> {
    let mut out = serde_json::Map::new();
    for source in sources {
        let id = source["id"].as_str().unwrap_or("");
        if !totals.contains_key(id) { continue; }
        let rate = if comparable { source_rate(previous.get(id), totals.get(id), elapsed) } else { None };
        let history = histories.entry(id.to_string()).or_default();
        if let Some((a,b)) = rate {
            history.push(json!({"timestamp":timestamp,"tokens_per_minute":a+b,
                "exact_tokens_per_minute":a,"estimated_tokens_per_minute":b}));
        }
        clean_history(history, timestamp);
        let field = |name: &str| -> f64 {
            if days_key == "days" { return number(&source[name]); }
            source[days_key].as_array().map(|days| days.iter().map(|d| number(&d[name])).sum()).unwrap_or(0.0)
        };
        let duration = field("duration_ms");
        let first_count = field("ttft_samples");
        let output = field("timed_output_tokens");
        let ttft = field("ttft_ms");
        out.insert(id.to_string(), json!({"app_id":source["app_id"],"instance_id":source["instance_id"],
            "tokens_per_minute":rate.map(|(a,b)|a+b),"exact_tokens_per_minute":rate.map(|(a,_)|a),
            "estimated_tokens_per_minute":rate.map(|(_,b)|b),"history":history,
            "timed_output_tokens":output,"duration_ms":duration,
            "timing_samples":field("timing_samples"),"ttft_ms":ttft,"ttft_samples":first_count,
            "output_tokens_per_second":if duration > 0.0 {Some(output*1000.0/duration)} else {None},
            "average_first_token_ms":if first_count > 0.0 {Some(ttft/first_count)} else {None}}));
    }
    out
}

/// New/removed sources and counter resets establish a baseline rather than a spike.
fn rates(previous: &BTreeMap<String, (f64, f64)>, current: &BTreeMap<String, (f64, f64)>, seconds: f64) -> Option<(f64, f64)> {
    if previous.is_empty() || previous.keys().ne(current.keys()) || seconds < 1.0 || seconds > 300.0 { return None; }
    let (mut exact, mut estimated) = (0.0, 0.0);
    for (id, (a, b)) in current {
        let (x, y) = previous[id];
        if *a < x || *b < y { return None; }
        exact += a - x;
        estimated += b - y;
    }
    Some((exact * 60.0 / seconds, estimated * 60.0 / seconds))
}

fn source_rate(previous: Option<&(f64, f64)>, current: Option<&(f64, f64)>, seconds: f64) -> Option<(f64, f64)> {
    let ((a,b),(x,y)) = previous.zip(current)?;
    if !(1.0..=300.0).contains(&seconds) || x < a || y < b { return None; }
    Some(((x-a)*60.0/seconds, (y-b)*60.0/seconds))
}

pub fn snapshot(force: bool) -> Value {
    let _guard = match SCAN.try_lock() {
        Ok(g) => g,
        Err(_) => {
            let mut value = cache().lock().unwrap().document.clone();
            if !value.is_object() { value = json!({"sources":[], "monitor":{}}); }
            value["updating"] = json!(true);
            return value;
        }
    };
    {
        let c = cache().lock().unwrap();
        if !force && now() - c.sampled_at < 15.0 { return c.document.clone(); }
    }
    let (sources, profiles) = collect_sources();
    let timestamp = now();
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    let totals = totals_by_source(&sources, &today);
    let app_totals = totals_in_view(&sources, &today, "app_days", "app_status");
    let selected_totals = totals_in_view(&sources, &today, "source_days", "source_status");
    let mut c = cache().lock().unwrap();
    let elapsed = timestamp - c.sampled_at;
    let rate = if c.sampled_day == today { rates(&c.previous, &totals, elapsed) } else { None };
    c.samples += 1;
    if let Some((a, b)) = rate {
        c.history.push(json!({"timestamp":timestamp,"tokens_per_minute":a+b,
            "exact_tokens_per_minute":a,"estimated_tokens_per_minute":b}));
        clean_history(&mut c.history, timestamp);
    }
    let comparable = c.sampled_day == today && elapsed >= 1.0 && elapsed <= 300.0;
    let previous = c.previous.clone();
    let previous_app = c.previous_app.clone();
    let previous_selected = c.previous_selected.clone();
    let by_source = source_monitor(&sources,&totals,&previous,&mut c.source_history,timestamp,elapsed,comparable,"days");
    let by_app_source = source_monitor(&sources,&app_totals,&previous_app,&mut c.app_source_history,timestamp,elapsed,comparable,"app_days");
    let by_selected_source = source_monitor(&sources,&selected_totals,&previous_selected,&mut c.selected_source_history,timestamp,elapsed,comparable,"source_days");
    let metric = |field: &str| -> f64 {
        sources.iter().filter(|s| s["status"] == "ready").map(|s| number(&s[field])).sum()
    };
    let duration = metric("duration_ms");
    let ttft_samples = metric("ttft_samples");
    prune_history(&mut c, timestamp);
    c.document = json!({"generated_at":timestamp,"updating":false,"sources":sources,
        "monitor":{"samples":c.samples,"interval_seconds":if c.sampled_at > 0.0 {Some(elapsed)} else {None},
            "tokens_per_minute":rate.map(|(a,b)|a+b),"exact_tokens_per_minute":rate.map(|(a,_)|a),
            "estimated_tokens_per_minute":rate.map(|(_,b)|b),
            "output_tokens_per_second":if duration > 0.0 {Some(metric("timed_output_tokens")*1000.0/duration)} else {None},
            "timing_samples":metric("timing_samples"),
            "average_first_token_ms":if ttft_samples > 0.0 {Some(metric("ttft_ms")/ttft_samples)} else {None},
            "history":c.history,"by_source":by_source,"by_app_source":by_app_source,"by_selected_source":by_selected_source,
            "sample_retention_seconds":SAMPLE_RETENTION_SECONDS,"sample_max_per_source":SAMPLE_MAX_PER_SOURCE,
            "note":"消耗速率来自两次本地日志采样的增量，可能受延迟落盘影响。回复平均吞吐只统计有耗时的请求，包含首字等待。"}});
    c.profiles = profiles;
    c.previous = totals;
    c.previous_app = app_totals;
    c.previous_selected = selected_totals;
    c.sampled_at = timestamp;
    c.sampled_day = today;
    c.document["monitor"]["history_persistence"] = match persist_history(&c) {
        Ok(()) => json!({"ok":true}),
        Err(reason) => json!({"ok":false,"reason":reason}),
    };
    c.document.clone()
}

pub fn trend_for(app: &str, dir: &str) -> Option<Value> {
    if dir.trim().is_empty() { return None; }
    cache().lock().ok()?.profiles.get(&profile_key(app, Path::new(dir))).cloned()
}

/// The desktop continues observing local histories while its window is in the tray.
pub fn watch() {
    std::thread::sleep(std::time::Duration::from_secs(12));
    loop {
        snapshot(false);
        std::thread::sleep(std::time::Duration::from_secs(60));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test] fn rates_require_comparable_counters() {
        let a = BTreeMap::from([("a".to_string(), (100.0, 20.0))]);
        let b = BTreeMap::from([("a".to_string(), (160.0, 30.0))]);
        assert_eq!(rates(&a,&b,30.0),Some((120.0,20.0)));
        assert!(rates(&BTreeMap::new(),&b,30.0).is_none());
        assert!(rates(&b,&a,30.0).is_none());
        assert!(rates(&a,&b,3600.0).is_none());
        let mut c = b.clone(); c.insert("new".into(),(1000000.0,0.0));
        assert!(rates(&a,&c,30.0).is_none());
    }
    #[test] fn each_source_rate_uses_its_own_baseline() {
        assert_eq!(source_rate(Some(&(100.0, 40.0)),Some(&(130.0, 50.0)),30.0),Some((60.0,20.0)));
        assert_eq!(source_rate(Some(&(200.0, 0.0)),Some(&(200.0, 0.0)),30.0),Some((0.0,0.0)));
        assert!(source_rate(None,Some(&(100.0,0.0)),30.0).is_none());
        assert!(source_rate(Some(&(100.0,0.0)),Some(&(90.0,0.0)),30.0).is_none());
        assert!(source_rate(Some(&(100.0,0.0)),Some(&(110.0,0.0)),400.0).is_none());
    }
    #[test] fn isolated_profile_does_not_read_device_history() {
        let profile = Path::new("D:/isolated/account");
        let native = Path::new("D:/native/profile");
        assert_eq!(history_dir("qoder",profile,Some(native)),profile);
    }
    #[test] fn billing_is_not_observed_as_new_tokens() {
        let sources = vec![json!({"id":"native","status":"ready","days":[{"date":"2026-10-29","exact_tokens":42,"estimated_tokens":8}, {"date":"2026-10-28","exact_tokens":999999,"estimated_tokens":0}]}),
            json!({"id":"credits","status":"cached","days":[{"credits":1000}]}),
            json!({"id":"duplicate","status":"ready","count_in_total":false,"days":[{"exact_tokens":50}]})];
        assert_eq!(totals_by_source(&sources,"2026-10-29"), BTreeMap::from([("native".into(),(42.0,8.0))]));
    }
    #[test] fn copied_histories_count_the_fullest_message_once() {
        let record = |n| json!({"session":"same-session","id":"same-message","date":"2026-10-29",
            "model":"m","provider":"p","tokens":n,"exact_tokens":n,"requests":1});
        let mut sources = vec![json!({"app_id":"claude","status":"ready","_records":[record(100)]}),
            json!({"app_id":"claude","status":"ready","_records":[record(120)]}),
            json!({"app_id":"other","status":"ready","_records":[record(50)]})];
        deduplicate_sources(&mut sources);
        assert_eq!(sources.iter().map(|s| number(&s["total"])).sum::<f64>(),170.0);
        assert_eq!(sources[0]["status"],"duplicate");
        assert_eq!(sources[1]["days"][0]["models"][0]["tokens"],120.0);
        assert!(sources.iter().all(|s| s.get("_records").is_none()));
    }
    #[test] fn desktop_and_codex_shared_events_only_count_once() {
        let record = json!({"session":"same","id":"one","date":"2026-10-29",
            "model":"m","provider":"p","tokens":123,"exact_tokens":123});
        let mut sources = vec![json!({"app_id":"codex","status":"ready","days":[{"date":"2026-10-29","tokens":123}],"_records":[record.clone()]}),
            json!({"app_id":"codex-desktop","status":"ready","days":[{"date":"2026-10-29","tokens":123}],"_records":[record.clone()]}),
            json!({"app_id":"codex-desktop","status":"ready","days":[{"date":"2026-10-29","tokens":123}],"_records":[record]})];
        prepare_narrow_views(&mut sources);
        deduplicate_sources(&mut sources);
        assert_eq!(sources.iter().map(|s| number(&s["total"])).sum::<f64>(),123.0);
        assert_eq!(sources[1]["status"],"duplicate");
        assert_eq!(sources[1]["app_days"][0]["tokens"],123.0);
        assert_eq!(sources[2]["app_status"],"duplicate");
        assert_eq!(sources[2]["app_days"].as_array().unwrap().len(),0);
        assert_eq!(sources[2]["source_days"][0]["tokens"],123);
    }
    #[test] fn historical_buckets_skip_unknown_time_and_average_has_fixed_window() {
        let n = 2_000_000_000.0;
        let first = json!({"timestamp": n-60.0, "exact_tokens":300, "estimated_tokens":20});
        let other = json!({"timestamp": n-40.0, "exact_tokens":100, "estimated_tokens":0});
        let unknown = json!({"date":"2033-05-18", "exact_tokens":900});
        let old = json!({"timestamp":n-32.0*86400.0, "exact_tokens":1000});
        let result = history_bucket(&[&first,&other,&unknown,&old],n);
        assert_eq!(result.len(),1);
        assert_eq!(result[0]["exact_tokens"],400.0);
        assert_eq!(result[0]["estimated_tokens"],20.0);
        assert_eq!(result[0]["events"],2);
        assert_eq!(history_coverage(&[&first,&unknown])["without_time_events"],1);
    }
    #[test] fn narrow_source_and_app_rates_retain_shared_codex_history() {
        let timestamp = now()-30.0;
        let record = json!({"session":"shared","id":"r","date":chrono::Local::now().format("%Y-%m-%d").to_string(),
            "timestamp":timestamp,"tokens":100,"exact_tokens":100});
        let mut sources = vec![json!({"id":"codex","app_id":"codex","status":"ready","days":[],"_records":[record.clone()]}),
            json!({"id":"desktop","app_id":"codex-desktop","status":"ready","days":[],"_records":[record.clone()]}),
            json!({"id":"copy","app_id":"codex-desktop","status":"ready","days":[],"_records":[record]})];
        prepare_narrow_views(&mut sources);
        deduplicate_sources(&mut sources);
        assert_eq!(sources.iter().map(|s| s["historical_5m"].as_array().unwrap().len()).sum::<usize>(),1);
        assert_eq!(sources[1]["app_historical_5m"][0]["exact_tokens"],100.0);
        assert_eq!(sources[2]["app_historical_5m"].as_array().unwrap().len(),0);
        assert_eq!(sources[2]["source_historical_5m"][0]["exact_tokens"],100.0);
        let today = chrono::Local::now().format("%Y-%m-%d").to_string();
        let app = totals_in_view(&sources,&today,"app_days","app_status");
        assert_eq!(app["desktop"],(100.0,0.0));
        assert!(!app.contains_key("copy"));
        let direct = totals_in_view(&sources,&today,"source_days","source_status");
        // source_days in this fixture is empty but still represents a valid source baseline.
        assert_eq!(direct["copy"],(0.0,0.0));
    }
    #[test] fn persisted_samples_roundtrip_and_prune_old_values() {
        let t = now();
        let samples = vec![json!({"timestamp":t-10.0,"exact_tokens_per_minute":12.0,"estimated_tokens_per_minute":3.0}),
            json!({"timestamp":t-SAMPLE_RETENTION_SECONDS-1.0,"exact_tokens_per_minute":50.0,"estimated_tokens_per_minute":0.0})];
        let mut restored = unpack_samples(&pack_samples(&samples));
        clean_history(&mut restored,t);
        assert_eq!(restored.len(),1);
        assert_eq!(restored[0]["tokens_per_minute"],15.0);
        assert!(unpack_samples(&json!([[1,-1,2],[1,2],["bad",2,3]])).is_empty());
    }
}
