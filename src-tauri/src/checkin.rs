// 每日签到与领积分。
//
// 分工：查状态和领取的报文都在 agenthub 里（凭据解密、各家域名、UA 那些它已经解决了），
// 这一层只做三件事——把结果缓存住给界面画、把「领取」限制在明确入口、按天与时间窗
// 决定后台要不要自己动。
//
// 领取是**写操作**：只有界面上点过「立刻签到」，或者这家开了自动签到且今天还没跑过，
// 才会真发那条请求。厂商侧的当日名额领回来是没有的，所以这里宁可少跑一次。
use crate::registry;
use crate::settings;
use tauri_plugin_notification::NotificationExt;
use serde_json::{json, Value};
use std::fs;
use std::sync::Mutex;

/// 支持签到的产品目录。**只用来在"一次都还没查到过"时先把表格的行画出来**——
/// 列表（产品 / 操作 / 自动领 / 提醒）本身是纯 UI，没有任何理由等六家接口。
/// 产品名与顺序跟 agenthub.py 的 CHECKIN_STORES / CHECKIN_LABELS 对齐，
/// 新增一家时两处都要改（跟 local_quota::LOCAL_STORES 同一条规矩）。
pub const CATALOG: &[(&str, &str)] = &[
    ("workbuddy", "WorkBuddy"),
    ("workbuddy-activity", "WorkBuddy 活跃地图"),
    ("workbuddy-travel", "WorkBuddy 旅行猫"),
    ("workbuddy-intl", "WorkBuddy 国际版"),
    ("trae-cn", "Trae CN"),
    ("minimax", "MiniMax Code"),
    ("astudio", "AStudio"),
    // 每天 300 积分；当天登录过桌面客户端的那笔有效期 7 天（本机 2026-11-05 实测）。
    // 领取＝带桌面头打一条 office/v3/setting_info（agenthub 的 ci_raccoon）；
    // 服务端不发时回 need_login，界面那一步会拉起客户端登一次。
    ("raccoon", "商汤小浣熊"),
    ("qoder-cn", "Qoder CN"),
    ("qoder", "Qoder"),
    // 每天 200 额度（周一翻倍），连签 30 天约 8800。领取＝POST /alice/ug/campaign/landing/checkin，
    // 纯 cookie 鉴权、无签名，**服务端自带幂等**（重复请求回 idempotent:true），重试安全。
    // 只有累计口径（progress.total_amount），没有剩余额度接口。见 agenthub 的 ci_doubao。
    ("doubao", "豆包"),
];

/// 哪几家能**按账号各查一次**。判据是两条都成立：
///   ① 这个产品在这台机器上每个号都留着自己那份登录态（驾驶舱保管箱里的登录包）；
///   ② agenthub 的 checkin 认得出"用这个号的登录态去查"（qoder 系给 `--profile`，
///      WorkBuddy 系给 `--auth-file`）。
/// 其余几家（Trae CN / MiniMax / AStudio）的登录态是**整机一份**、写死在某个固定路径，
/// 换号必然覆盖掉前一个——硬拆只会给出一个看起来像真的、其实是当前登录号的假答案，
/// 所以宁可只给产品级一行，也不摆一排串号的假账号行。
pub const PER_ACCOUNT: &[&str] = &["qoder", "qoder-cn", "workbuddy", "workbuddy-intl"];

/// 最近一次查到的状态（整份数组）。界面每 4 秒刷新，绝不能每次都去发六条网络请求，
/// 所以只在这里存一份，靠 stale() 决定什么时候重新查。
static CACHE: Mutex<Option<Value>> = Mutex::new(None);

/// 缓存还没建起来时用的回落快照（落盘的"上次结果"或静态目录），带 15 秒 TTL
static FALLBACK: Mutex<Option<(f64, Value)>> = Mutex::new(None);
/// 上一次调 agenthub 的失败原因。界面用它显示一条全局提示，
/// 而不是让每一行都孤零零地报“读不到”。
static LAST_ERROR: Mutex<Option<String>> = Mutex::new(None);

pub fn last_error() -> Option<String> {
    LAST_ERROR.lock().ok().and_then(|g| g.clone())
}

fn set_last_error(msg: Option<String>) {
    if let Ok(mut g) = LAST_ERROR.lock() {
        *g = msg;
    }
}
static LEDGER_WRITE: Mutex<()> = Mutex::new(());
static CLAIM_WRITE: Mutex<()> = Mutex::new(());

fn now_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// 本机时刻 HH:MM:SS。日志条目的 `at` 只有日期（按天去重、按天筛都靠它），
/// 「几点几分领的」记在旁边这个 `at_time` 里——她 2026-11-06 要看精确时间；
/// 老条目没有这一栏，界面回落成只显示日期。
fn now_hms() -> String {
    chrono::Local::now().format("%H:%M:%S").to_string()
}

/// 整数就不带小数点（30 而不是 30.0），跟界面上其它积分读数同一套写法
fn num_text(v: f64) -> String {
    if v.fract().abs() < 1e-9 {
        format!("{}", v as i64)
    } else {
        format!("{v}")
    }
}

fn run(extra: &[&str]) -> Result<Value, String> {
    let mut args: Vec<&str> = vec!["checkin"];
    args.extend_from_slice(extra);
    args.push("--json");
    let out = match crate::agenthub::command(&args)
        .and_then(|mut c| c.output().map_err(|e| e.to_string())) {
        Ok(o) => o,
        Err(e) => {
            let msg = format!(
                "无法启动内置签到后端：{e}"
            );
            return Err(msg);
        }
    };
    if !out.status.success() {
        let s = String::from_utf8_lossy(&out.stderr);
        let msg = format!(
            "签到后端失败：{}",
            s.trim().chars().take(180).collect::<String>()
        );
        return Err(msg);
    }
    let parsed: Value = match serde_json::from_slice(&out.stdout) {
        Ok(v) => v,
        Err(e) => {
            let msg = format!("签到后端输出不是 JSON：{e}");
            return Err(msg);
        }
    };
    if parsed.is_array() { Ok(parsed) } else { Err("签到后端结果格式错误。".into()) }
}

fn state_path() -> std::path::PathBuf {
    registry::base_dir().join("checkin.json")
}

fn load_state() -> Value {
    let v = fs::read_to_string(state_path())
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .unwrap_or_else(|| json!({}));
    let mut o = match v.as_object() {
        Some(o) => o.clone(),
        None => serde_json::Map::new(),
    };
    for (k, d) in [("done", json!({})), ("log", json!([])), ("pending", json!({})), ("arrived", json!([])), ("expired", json!([])), ("auto_fails", json!({}))] {
        if !o.contains_key(k) {
            o.insert(k.into(), d);
        }
    }
    let mut state = Value::Object(o);
    normalize_legacy_workbuddy(&mut state);
    backfill_log_times(&mut state);
    state
}

// A product-only entry has no stable owner after login changes. Retain ambiguous
// history, but never query the newly logged-in account to settle an older claim.
fn normalize_legacy_workbuddy(st: &mut Value) {
    let pending = st["pending"].as_object().cloned().unwrap_or_default();
    for (key, entry) in pending {
        if key == "workbuddy-intl" || key.starts_with("workbuddy-intl|") {
            if !st["unassigned_pending"].is_object() { st["unassigned_pending"] = json!({}); }
            st["unassigned_pending"][&key] = entry;
            st["pending"].as_object_mut().map(|o| o.remove(&key));
        }
    }
    for (list, message) in [("log", "message"), ("arrived", "text")] {
        let rows = st[list].as_array().cloned().unwrap_or_default();
        let mut kept = Vec::new();
        for row in &rows {
            let anonymous = row["store"] == "workbuddy-intl"
                && row["account"].as_str().unwrap_or("").is_empty();
            let text = row[message].as_str().unwrap_or("");
            let arrival_body = text.split_once('」').map(|(_, tail)| tail);
            let duplicate = anonymous && text.contains("到账的 30 积分")
                && arrival_body.is_some() && rows.iter().any(|other|
                    other["store"] == row["store"] && other["at"] == row["at"]
                    && !other["account"].as_str().unwrap_or("").is_empty()
                    && other[message].as_str().and_then(|s| s.split_once('」').map(|(_, t)| t)) == arrival_body);
            if !duplicate {
                let mut entry = row.clone();
                if anonymous { entry["account"] = json!("历史记录（未记录账号）"); }
                kept.push(entry);
            }
        }
        st[list] = json!(kept);
    }
}

/// 一次性回填：2026-11-06 之前写的日志条目只有日期（`at`），精确时刻从未落盘。
/// 能救的只有三个真实来源，按可信度依次：
///   ① 回填条目自己的消息里嵌着厂商到账时间戳（「…」2026-10-30 07:10:38 到账的 30 积分）；
///   ② pending 里还留着的 since——领取发起的那一刻；
///   ③ arrived 里还留着的 at——检测到到账的那一刻（列表只留 20 条，能对上得很少）。
/// 三个来源都对不上的（绝大多数旧条目）本地只剩日期，那就继续只有日期——
/// 凭空编一个时刻比没有更糟。已有 at_time 的新条目一律不动。
fn backfill_log_times(st: &mut Value) {
    let pend = st["pending"].as_object().cloned().unwrap_or_default();
    let unassigned = st["unassigned_pending"].as_object().cloned().unwrap_or_default();
    let arrived = st["arrived"].as_array().cloned().unwrap_or_default();
    let rows = match st["log"].as_array_mut() {
        Some(r) => r,
        None => return,
    };
    for row in rows.iter_mut() {
        if row.get("at_time").is_some() { continue; }
        let store = row["store"].as_str().unwrap_or("");
        let account = row["account"].as_str().unwrap_or("");
        let at = row["at"].as_str().unwrap_or("");
        let mut hit: Option<String> = None;
        // ① 厂商时间戳：格式「{label}」{YYYY-MM-DD HH:MM:SS} 到账的 30 积分。
        //   「最近一笔于 {ts} 到账」那种消息里的时间戳是上一笔的，不是这一条的，不认。
        let msg = row["message"].as_str().unwrap_or("");
        if let Some(i) = msg.find("到账的 30 积分") {
            if let Some(ts) = i.checked_sub(20).and_then(|s| msg.get(s..i - 1)) {
                if is_stamp(ts) && msg[..i - 20].ends_with('」') {
                    hit = Some(ts[11..19].to_string());
                }
            }
        }
        // ② pending.since：领取发起的时刻。只认真正领到/在等的条目——
        //   失败的重试行不该被钉上后来成功那一笔的时刻。
        //   账号行的键是 store@账号，产品行是 store；国际版的键带 |今天，
        //   其他家不带（日期在值的 today 里）；产品级国际版的旧键被
        //   normalize_legacy_workbuddy 挪进了 unassigned_pending。
        if hit.is_none() && !at.is_empty() {
            let real = row["claimed"].as_bool().unwrap_or(false)
                || row["settling"].as_bool().unwrap_or(false)
                || row["already"].as_bool().unwrap_or(false);
            if real {
                let mut keys: Vec<String> = Vec::new();
                if account.is_empty() {
                    keys.push(format!("{store}|{at}"));
                    keys.push(store.to_string());
                } else {
                    keys.push(format!("{store}@{account}|{at}"));
                    keys.push(format!("{store}@{account}"));
                }
                for k in keys {
                    if let Some(e) = pend.get(&k).or_else(|| unassigned.get(&k)) {
                        if k.contains('|') || e["today"].as_str() == Some(at) {
                            hit = e["since"].as_f64().map(epoch_hms);
                            break;
                        }
                    }
                }
            }
        }
        // ③ arrived.at：检测到到账的那一刻
        if hit.is_none() && !at.is_empty() {
            let mut m = arrived.iter().filter(|a| {
                a["store"].as_str() == Some(store)
                    && a["account"].as_str().unwrap_or("") == account
                    && a["log_date"].as_str() == Some(at)
            });
            if let (Some(a), None) = (m.next(), m.next()) {
                hit = a["at"].as_f64().map(epoch_hms);
            }
        }
        if let Some(t) = hit { row["at_time"] = json!(t); }
    }
}

/// "YYYY-MM-DD HH:MM:SS" 的 19 位定长校验（只看骨架，不验日期真伪）
fn is_stamp(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 19
        && b[4] == b'-' && b[7] == b'-' && b[10] == b' '
        && b[13] == b':' && b[16] == b':'
        && b.iter().enumerate().all(|(i, c)| matches!(i, 4 | 7 | 10 | 13 | 16) || c.is_ascii_digit())
}

/// 纪元秒 → 本机 HH:MM:SS。和 now_hms() 同一套写法，只是时刻从哪来不同
fn epoch_hms(secs: f64) -> String {
    chrono::DateTime::from_timestamp(secs as i64, 0)
        .map(|d| d.with_timezone(&chrono::Local).format("%H:%M:%S").to_string())
        .unwrap_or_default()
}

fn rename_ledger_account(st: &mut Value, store: &str, from: &str, to: &str) {
    let old = format!("{store}@{from}");
    let new = format!("{store}@{to}");
    for field in ["done", "pending"] {
        let map = st[field].as_object().cloned().unwrap_or_default();
        for (key, mut value) in map {
            if key == old || key.starts_with(&format!("{old}|")) {
                let new_key = format!("{new}{}", &key[old.len()..]);
                if value.is_object() {
                    if let Some(label) = value["label"].as_str() {
                        value["label"] = json!(label.replace(&format!(" · {from}"), &format!(" · {to}")));
                    }
                }
                st[field].as_object_mut().map(|o| { o.remove(&key); o.insert(new_key, value); });
            }
        }
    }
    for field in ["log", "arrived"] {
        if let Some(rows) = st[field].as_array_mut() {
            for row in rows {
                if row["store"] == store && row["account"] == from {
                    row["account"] = json!(to);
                    for key in ["label", "message", "text", "id"] {
                        if let Some(s) = row[key].as_str() {
                            row[key] = json!(s.replace(&format!(" · {from}"), &format!(" · {to}"))
                                .replace(&old, &new));
                        }
                    }
                }
            }
        }
    }
    // Cached rows carry the old action key; rebuild them after renaming.
    st.as_object_mut().map(|o| o.remove("last"));
}

pub fn rename_account(app: &str, from: &str, to: &str) {
    let Some(store) = crate::local_quota::store_for(app) else { return; };
    let _guard = LEDGER_WRITE.lock().unwrap();
    let mut st = load_state();
    rename_ledger_account(&mut st, store, from, to);
    save_state(&st);
    *CACHE.lock().unwrap() = None;
    *FALLBACK.lock().unwrap() = None;
    if let Ok(mut wants) = WANT.lock() {
        for row in wants.iter_mut() {
            if row["store"] == store && row["account"] == from { row["account"] = json!(to); }
        }
    }
}

/// 交给界面那份：`last` 是"上次查到的整份结果"，只给后端自己用来先把表格画出来，
/// 没必要跟着每 4 秒一轮的状态报文来回传。
fn public_state(st: &Value) -> Value {
    let mut s = st.clone();
    if let Some(o) = s.as_object_mut() {
        o.remove("last");
    }
    s
}

fn save_state(v: &Value) {
    let p = state_path();
    if let Some(dir) = p.parent() {
        let _ = fs::create_dir_all(dir);
    }
    // 先写临时文件再改名：签到记录被半截读走，下一轮就会以为今天还没跑过
    let tmp = p.with_extension("json.tmp");
    if let Ok(s) = serde_json::to_string_pretty(v) {
        if fs::write(&tmp, s).is_ok() {
            let _ = fs::rename(&tmp, &p);
        }
    }
}

/// 缓存还新不新。查一次要打六家接口，给 5 分钟；手动点「刷新」走 refresh() 不看这个。
pub fn stale() -> bool {
    let g = CACHE.lock().unwrap();
    match &*g {
        None => true,
        Some(v) => {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs_f64())
                .unwrap_or(0.0);
            let at = v["fetched_at"].as_f64().unwrap_or(0.0);
            now - at > 300.0
        }
    }
}

pub fn refresh() -> Value {
    match run(&[]) {
        Ok(items) => {
            set_last_error(None);
            let now = now_secs();
            // 产品行后面挂上各账号自己的行（只有"登录态按账号各存一份、且存了不止一个号"
            // 的产品才挂）。这一步是几次额外的 agenthub 调用，所以只在这一条后台路径上做，
            // 界面那 4 秒一轮读的是缓存。
            let expanded = expand_accounts(items);
            let mut items = Value::Array(filter_items(
                expanded.as_array().cloned().unwrap_or_default(),
                &store_visible,
            ));
            let _guard = LEDGER_WRITE.lock().unwrap();
            let mut st = load_state();
            apply_workbuddy_statuses(&mut items, &st);
            record_external_signins(&items, &mut st);
            // 把这次的结果落盘：下次刚开机、后台第一轮还没跑完时，界面就能拿它先把
            // 整张表画出来（产品名、备注、操作列都在里面），不用干等六家接口。
            let mut persisted = st.clone();
            persisted["last"] = json!({ "items": items, "fetched_at": now });
            save_state(&persisted);
            let v = json!({ "ok": true, "items": items, "fetched_at": now, "state": public_state(&st) });
            *CACHE.lock().unwrap() = Some(v.clone());
            v
        }
        Err(e) => {
            set_last_error(Some(e.clone()));
            json!({ "ok": false, "msg": e })
        }
    }
}

/// 国际版账单只有到账时间，没有活跃日期。今天"用过没有"有两个来源：
/// ① agenthub 读桌面端自己的会话库（按 user_id）——她在客户端自己聊过就认得出来；
/// ② 驾驶舱自己发过的那句（done 记账，或今天挂进 pending 的那条）。
/// 到账归属仍只按次日结算惯例推测；不能拿今早的到账堵住今天的活跃。
fn apply_workbuddy_status(item: &mut Value, st: &Value) {
    if item["product"].as_str() != Some("workbuddy-intl") || item.get("error").is_some() {
        return;
    }
    let today = item["today"].as_str().unwrap_or("").to_string();
    if today.is_empty() { return; }
    let account = item["ledger_account"].as_str().unwrap_or_else(||
        if item["scope"].as_str() == Some("account") { item["account"].as_str().unwrap_or("") } else { "" });
    let key = if account.is_empty() { "workbuddy-intl".to_string() }
              else { format!("workbuddy-intl@{account}") };
    // pending 里今天这一条同样说明"已经发过"：done 是明文的键值账，账号卡改名、
    // 或者文件被别的产品整段覆盖时会丢；pending 的键自带当天日期，比 done 更难丢。
    // 她 2026-11-03："点过一轮发一句对话了，还让我重新发"——丢的正是这个键。
    let pend_key = format!("{key}|{today}");
    let done_today = st["done"][&key].as_str() == Some(today.as_str())
        || st["pending"].get(&pend_key).map(|v| v.is_object()).unwrap_or(false)
        || item["signed_today"].as_bool() == Some(true);
    if !done_today { return; }
    let inferred = item["inferred_today_arrival"].as_bool() == Some(true);
    let at = item["activity_at"].as_str().unwrap_or("").to_string();
    let note = item["note"].as_str().unwrap_or("").to_string();
    item["signed_today"] = json!(true);
    item["claimable"] = json!(false);
    item["words"]["done"] = json!(if inferred { "已活跃·推测到账" } else { "今天已活跃" });
    item["words"]["done_act"] = json!("今天已用过");
    item["note"] = json!(if at.is_empty() {
        // 本地会话库里没这条（多半是驾驶舱自己用命令行发的那句），得说清是谁发的
        format!("驾驶舱今天已经替你发过一句对话，今天的积分{}；{note}",
            if inferred { "有推测属于今天的到账记录，归属未经厂商确认" }
            else { "尚未发现到账记录" })
    } else {
        note
    });
}

fn apply_workbuddy_statuses(items: &mut Value, st: &Value) {
    if let Some(arr) = items.as_array_mut() {
        for item in arr { apply_workbuddy_status(item, st); }
    }
}

/// 今天这一天（store+账号）有没有一笔"已经到手"的签到：驾驶舱自己领的
/// （claimed / already / settling）或已经记过的外部签到。外部签到的自动检测
/// 和手动补记都靠它去重，同一天同一家最多记一笔。
fn log_hit_today(st: &Value, store: &str, account: &str, today: &str) -> bool {
    st["log"].as_array().map(|rows| rows.iter().any(|x| {
        x["store"].as_str() == Some(store)
            && x["account"].as_str().unwrap_or("") == account
            && x["at"].as_str() == Some(today)
            && (x["claimed"].as_bool().unwrap_or(false)
                || x["already"].as_bool().unwrap_or(false)
                || x["settling"].as_bool().unwrap_or(false)
                || x["source"].as_str() == Some("external"))
    })).unwrap_or(false)
}

fn append_external(st: &mut Value, store: &str, account: &str, today: &str, message: &str) {
    let mut log = st["log"].as_array().cloned().unwrap_or_default();
    log.push(json!({
        "at": today, "at_time": now_hms(), "store": store, "account": account,
        "source": "external", "message": message,
    }));
    if log.len() > 2000 {
        log.drain(0..log.len() - 2000);
    }
    st["log"] = json!(log);
}

/// 补记一条「官方客户端手动签到」。她在官方客户端里自己签的（国际版是当天在
/// 客户端里用过），驾驶舱日志里没有这一笔——补上并标注来源，翻日志时才分得清
/// 哪次是驾驶舱签的、哪次是自己签的。同时把当天记成 done：名额已经在手，
/// 自动领不该再发一次。
pub fn record_external(store: &str, account: &str) -> Value {
    if store.is_empty() {
        return json!({ "ok": false, "msg": "没说要补记哪一家" });
    }
    // 按账号各记各的产品，产品行上没说是哪个号——按当前登录身份落到它名下，
    // 与 claim() 解析当前账号用同一套逻辑，键才对得上
    let resolved;
    let account = if per_account(store) && account.is_empty() {
        let app = crate::local_quota::app_for_store(store).unwrap_or(store);
        match crate::authpack::live_identity(app)
            .and_then(|(a, u)| crate::authpack::card_for_login(app, &a, &u)) {
            Some(n) => { resolved = n; resolved.as_str() }
            None => return json!({ "ok": false,
                "msg": "读不出当前登录的账号：先在客户端登它一次，或在账号那一行上补记" }),
        }
    } else { account };
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    let key = if account.is_empty() { store.to_string() } else { format!("{store}@{account}") };
    let _guard = LEDGER_WRITE.lock().unwrap();
    let mut st = load_state();
    if st["done"][&key].as_str() == Some(today.as_str())
        || log_hit_today(&st, store, account, &today) {
        return json!({ "ok": true, "already": true,
            "message": "今天已经记过了：驾驶舱领过，或外部签到已有记录" });
    }
    append_external(&mut st, store, account, &today, "官方客户端手动签到（非驾驶舱领取）");
    st["done"].as_object_mut().map(|o| { o.insert(key, json!(today)); });
    save_state(&st);
    *CACHE.lock().unwrap() = None;
    json!({ "ok": true, "message": format!("已补记 {today} 的外部签到") })
}

/// 刷新时顺带认领外部签到：状态读出来今天已签、但这天既不是驾驶舱经手的
/// （done / pending / 日志都没有），就把这笔按"官方客户端"补进日志。她在客户端
/// 里自己签的（含国际版当天在客户端里聊过）从此也有台账，当天也不会再被自动领
/// 重复处理。旅行猫的派出/领取两步都由驾驶舱管，不在此列。
fn record_external_signins(items: &Value, st: &mut Value) {
    let Some(arr) = items.as_array() else { return };
    let mut seen: Vec<String> = Vec::new();
    for it in arr {
        if it["signed_today"].as_bool() != Some(true) { continue; }
        if !it["error"].is_null() || it["mode"].as_str() == Some("travel") { continue; }
        let today = match it["today"].as_str() {
            Some(t) if !t.is_empty() => t.to_string(),
            _ => continue,
        };
        let store = it["product"].as_str().unwrap_or("").to_string();
        if store.is_empty() { continue; }
        let account = it["account"].as_str().unwrap_or("").to_string();
        let key = if account.is_empty() { store.clone() } else { format!("{store}@{account}") };
        // 产品行与它名下账号行可能报到同一个号：同一天同一账号只记一笔
        if seen.iter().any(|k| k == &key) { continue; }
        seen.push(key.clone());
        if st["done"][&key].as_str() == Some(today.as_str()) { continue; }
        let pend_key = format!("{key}|{today}");
        if st["pending"].get(&key).map(|v| v.is_object()).unwrap_or(false)
            || st["pending"].get(&pend_key).map(|v| v.is_object()).unwrap_or(false) {
            continue;
        }
        if log_hit_today(st, &store, &account, &today) { continue; }
        append_external(st, &store, &account, &today,
            "官方客户端已签到（驾驶舱读状态时检测到，非驾驶舱领取）");
        st["done"].as_object_mut().map(|o| { o.insert(key, json!(today)); });
    }
}

/// 产品级结果 → 产品行 + 它的账号行。产品行标 `scope: "product"`，账号行标 `scope: "account"`，
/// 界面据此决定画不画那条缩进子行。
fn expand_accounts(items: Value) -> Value {
    let arr = match items.as_array() {
        Some(a) => a.clone(),
        None => return items,
    };
    let mut out: Vec<Value> = Vec::new();
    for it in arr {
        let store = it["product"].as_str().unwrap_or("").to_string();
        let mut prod = it.clone();
        if per_account(&store) {
            if let Some(app_id) = crate::local_quota::app_for_store(&store) {
                if let Some(card) = crate::authpack::card_for_login(app_id,
                    it["account"].as_str().unwrap_or(""),
                    it["user_id"].as_str().unwrap_or("")) {
                    prod["ledger_account"] = json!(card);
                }
            }
        }
        if let Some(o) = prod.as_object_mut() {
            o.insert("scope".into(), json!("product"));
            // 界面要 app id 才能拉起客户端（store 名跟 app id 不是一回事）
            if let Some(app_id) = crate::local_quota::app_for_store(&store) {
                o.insert("app".into(), json!(app_id));
            }
        }
        out.push(prod);
        if store.is_empty() {
            continue;
        }
        if let Some(app_id) = crate::local_quota::app_for_store(&store) {
            out.extend(account_items(app_id, &store));
        }
    }
    json!(out)
}

/// 点「刷新状态」的那一下：**立刻回快照，后台去问各家**。
///
/// 以前这里是同步跑完一整轮（`refresh()` 直接返回）。本机实测一轮 4~51 秒——
/// 51 秒那种是被 TLS 握手停等拖的（每次网络调用先白等 15 秒才降级），而这一整轮
/// 期间 service_action 全程持有全局 CMD_LOCK：她点完「刷新状态」再去点换号/读额度，
/// 那些动作全在这把锁后面排队，于是出现"10 分钟前的操作现在才反应过来"
/// （2026-11-05 她报的）。真正要等结果的只有按钮那一句提示，而结果本来就有
/// 4 秒一轮的 state 带回来。同一时间只放一轮：BUSY 挡住重复点。
pub fn refresh_async() -> Value {
    spawn_refresh();
    let mut out = snapshot();
    if let Some(o) = out.as_object_mut() {
        o.insert("refreshing".into(), json!(true));
    }
    out
}

/// 后台刷一次；同一时间只放行一个。
fn spawn_refresh() {
    use std::sync::atomic::{AtomicBool, Ordering};
    static BUSY: AtomicBool = AtomicBool::new(false);
    if BUSY.swap(true, Ordering::SeqCst) {
        return;
    }
    std::thread::spawn(move || {
        let _ = refresh();
        BUSY.store(false, Ordering::SeqCst);
    });
}

/// 界面进来时**立刻**给一份（缓存，或上次落盘的那份），过期就叫后台去刷——
/// 绝不在这儿等六家接口。以前这里是同步 refresh()：进一次页面要等十几秒到一分钟，
/// 整页只剩一句"正在问各家今天的状态…"，而列表本身根本不需要等（2026-10-15 她指出来的）。
pub fn view() -> Value {
    let mut out = snapshot();
    if stale() {
        spawn_refresh();
        if let Some(o) = out.as_object_mut() {
            o.insert("refreshing".into(), json!(true));
        }
    }
    out
}

/// 一次都没查到过时的占位行：把产品名先摆出来，状态列由界面写"查询中"。
fn catalog_items() -> Vec<Value> {
    catalog_items_for(&store_visible)
}

fn catalog_items_for(visible: &dyn Fn(&str) -> bool) -> Vec<Value> {
    CATALOG
        .iter()
        .filter(|(p, _)| visible(p))
        .map(|(p, n)| {
            json!({
                "product": p, "name": n, "pending": true, "supported": true,
                "scope": "product",
                // 占位行也要带 app id：这一行的「登录并领取」全靠它拉客户端
                "app": crate::local_quota::app_for_store(p).unwrap_or(p),
                "auto": false, "activity_on": true, "signed_today": false, "claimable": false,
            })
        })
        .collect()
}

/// 签到页只摆本机真的装了、或名下已经有账号卡的产品。
/// WorkBuddy 的活跃地图/旅行猫不是独立安装，跟着 workbuddy 走。
pub(crate) fn store_visible(store: &str) -> bool {
    let app_id = match store {
        "workbuddy-activity" | "workbuddy-travel" => "workbuddy",
        _ => match crate::local_quota::app_for_store(store) {
            Some(id) => id,
            None => return false,
        },
    };
    crate::registry::app_def(app_id)
        .map(|a| crate::registry::worth_showing(&a))
        .unwrap_or(false)
}

/// 产品行按可见性过滤；账号行跟着它的 parent 走。
pub(crate) fn filter_items(items: Vec<Value>, visible: &dyn Fn(&str) -> bool) -> Vec<Value> {
    items
        .into_iter()
        .filter(|it| {
            let store = it["product"].as_str().unwrap_or("");
            if visible(store) {
                return true;
            }
            let parent = it["parent"].as_str().unwrap_or("");
            !parent.is_empty() && visible(parent)
        })
        .collect()
}

/// 这个 store 能不能按账号各查一次（登录包登记 + agenthub 认得出这个入口）
pub fn per_account(store: &str) -> bool {
    PER_ACCOUNT.contains(&store)
        && crate::local_quota::app_for_store(store)
            .map(crate::registry::per_account_login)
            .unwrap_or(false)
}

/// 把某个账号自己的登录态铺成临时档案目录，然后让 agenthub 只拿它去查/领。
/// 返回铺好的临时目录；调用方用完必须 drop_profile。
fn stage(app_id: &str, store: &str, name: &str) -> Result<std::path::PathBuf, String> {
    let dir = crate::authpack::materialize(app_id, name)?;
    // WorkBuddy 系的登录态不在档案目录里（写死在 %LOCALAPPDATA%\CodeBuddyExtension\…），
    // 所以铺出来的是"一个文件"，得用 --auth-file 指过去；Qoder 系铺的是整个档案目录，
    // 走 --profile（那边要读 Local State 拿主密钥才解得开 auth.v1.dat）。
    let _ = store;
    Ok(dir)
}

/// 这个账号的登录包铺出来后，该用哪个命令行参数交给 agenthub
fn stage_arg(app_id: &str, store: &str, dir: &std::path::Path) -> (String, String) {
    if store.starts_with("workbuddy") {
        let leaf = crate::registry::auth_pack_files(app_id)
            .first()
            .and_then(|f| f.rsplit(['/', '\\']).next())
            .unwrap_or("auth.info");
        (
            "--auth-file".into(),
            dir.join(leaf).to_string_lossy().into_owned(),
        )
    } else {
        ("--profile".into(), dir.to_string_lossy().into_owned())
    }
}

/// 一个产品名下的账号行。**通用**：只要这个产品在驾驶舱里有 ≥2 张账号卡就展开，
/// 跟"读不读得到它自己的登录态"无关——
///   · 读得到（登录态按账号各存一份，见 PER_ACCOUNT）：这一行就是那个号自己的签到状态；
///   · 读不到（整机一份登录态，如 Trae CN / MiniMax / AStudio）：这一行只摆账号名，
///     标 `shared`，动作给「切到该号」——仍然是"通用管理"，只是不假装读到了数。
///
/// 她 2026-10-17 原话："这个下拉菜单不是只有 QoderCN 需要，如果有其他 Agent 有了其他账号
/// 也是需要的，我要的是通用管理。" 所以展开的判据从"这家能不能按账号读"改成"这家有没有多账号"。
///
/// 读得到的那几行，每个号各起一次 agenthub（一次 python 启动两三秒），所以只在后台刷新里跑。
/// 按账号查失败时摆的那一行。查询失败不许让整行消失——
/// 她看到的是"二号不见了"，真相只是这一次没读到（2026-10-20 agenthub 少参数时踩过）。
fn account_row_unreadable(app_id: &str, store: &str, name: &str, why: &str) -> Value {
    json!({
        "scope": "account", "account": name, "name": name,
        "product": store, "parent": store, "app": app_id,
        "pending": false, "unreadable": true,
        "supported": true, "auto": false, "activity_on": true,
        "signed_today": Value::Null, "claimable": false,
        "note": format!(
            "这个号自己的状态这次没读到：{why}。点这一行最右边的 ↻ 重试；一直失败就先去客户端登它一次"
        ),
    })
}

pub fn account_items(app_id: &str, store: &str) -> Vec<Value> {
    let insts = crate::registry::load_instances(app_id);
    let names: Vec<String> = insts
        .iter()
        .filter_map(|m| m["name"].as_str().map(String::from))
        .filter(|n| !n.is_empty())
        .collect();
    // 一个号的产品不展开：产品级那一行就是它，再挂一条同名子行只是重复
    if names.len() < 2 {
        return Vec::new();
    }
    let readable = per_account(store);
    let mut out = Vec::new();
    for name in names {
        // 读不到独立登录态 / 这个号还没存过登录包：摆一行占位，不再逐个起 agenthub
        if !readable || crate::authpack::stored(app_id, &name).is_none() {
            out.push(json!({
                "scope": "account", "account": name, "name": name,
                "product": store, "parent": store, "app": app_id,
                "shared": true, "pending": false,
                "supported": true, "auto": false, "activity_on": true,
                "signed_today": Value::Null, "claimable": false,
                "note": if readable {
                    "这个号还没存过登录态：先在客户端登它一次，之后就能按账号查"
                } else {
                    "这个产品的登录态是整机一份，换号会覆盖前一个；签到状态要在切到该号之后才读得到"
                },
            }));
            continue;
        }
        let dir = match stage(app_id, store, &name) {
            Ok(d) => d,
            Err(e) => {
                out.push(account_row_unreadable(app_id, store, &name, &e));
                continue;
            }
        };
        let (flag, val) = stage_arg(app_id, store, &dir);
        let r = run(&[store, flag.as_str(), val.as_str()]);
        crate::authpack::drop_profile(&dir);
        let item = match r {
            Ok(v) => v.as_array().and_then(|a| a.first().cloned()),
            Err(e) => {
                out.push(account_row_unreadable(app_id, store, &name, &e));
                continue;
            }
        };
        let mut it = match item {
            Some(i) => i,
            None => {
                out.push(account_row_unreadable(
                    app_id, store, &name, "agenthub 没返回这个号的结果",
                ));
                continue;
            }
        };
        if let Some(o) = it.as_object_mut() {
            o.insert("scope".into(), json!("account"));
            o.insert("account".into(), json!(name));
            o.insert("product".into(), json!(store));
            // 账号行的标题就是账号名（agenthub 报的 name 是产品名，那一层已经由父行说过了）
            o.insert("name".into(), json!(name));
            o.insert("parent".into(), json!(store));
            // 界面要 app id 才能拉起客户端：store 名跟 app id 不是一回事（Trae 两边叫 trae-cn / traework-cn）
            o.insert("app".into(), json!(app_id));
        }
        out.push(it);
    }
    out
}

/// 只重查一家（或一个账号），把结果并进缓存，**不动其它行**。
/// 她 2026-10-17 要的：只签到了一个的时候自己点一次刷新，不要为了一行去问六家、等半天。
pub fn refresh_one(store: &str, acct: Option<(&str, &str)>) -> Value {
    if store.is_empty() {
        return json!({ "ok": false, "msg": "没说要刷哪一家" });
    }
    let mut extra: Vec<String> = vec![store.to_string()];
    let mut staged: Option<std::path::PathBuf> = None;
    // 账号级要按它自己的登录包读；产品不支持按账号读时忽略 acct（否则会拿别人的号冒充）
    if let Some((app_id, name)) = acct {
        if per_account(store) {
            match stage(app_id, store, name) {
                Ok(d) => {
                    let (f, v) = stage_arg(app_id, store, &d);
                    extra.push(f);
                    extra.push(v);
                    staged = Some(d);
                }
                Err(e) => return json!({ "ok": false, "msg": e }),
            }
        }
    }
    let refs: Vec<&str> = extra.iter().map(|s| s.as_str()).collect();
    let r = run(&refs);
    if let Some(d) = staged {
        crate::authpack::drop_profile(&d);
    }
    let mut item = match r {
        Ok(v) => match v.as_array().and_then(|a| a.first().cloned()) {
            Some(i) => i,
            None => return json!({ "ok": false, "msg": "agenthub 没返回结果" }),
        },
        Err(e) => return json!({ "ok": false, "msg": e }),
    };
    let acct_key = acct.map(|(_, n)| n).unwrap_or("");
    let scope = if acct_key.is_empty() { "product" } else { "account" };
    if let Some(o) = item.as_object_mut() {
        o.insert("scope".into(), json!(scope));
        o.insert("product".into(), json!(store));
        if let Some((app_id, name)) = acct {
            o.insert("account".into(), json!(name));
            o.insert("name".into(), json!(name));
            o.insert("parent".into(), json!(store));
            o.insert("app".into(), json!(app_id));
        }
    }
    if acct.is_none() && per_account(store) {
        if let Some(app_id) = crate::local_quota::app_for_store(store) {
            if let Some(card) = crate::authpack::card_for_login(app_id,
                item["account"].as_str().unwrap_or(""), item["user_id"].as_str().unwrap_or("")) {
                item["ledger_account"] = json!(card);
            }
        }
    }
    apply_workbuddy_status(&mut item, &load_state());
    // 并进缓存：只换掉同一条，别的行原样留着。**不动 fetched_at**——
    // 那是"整张表最后一次查于"，动它会让别的行看起来也刚查过。
    {
        let mut g = CACHE.lock().unwrap();
        if let Some(v) = g.as_mut() {
            if let Some(arr) = v["items"].as_array_mut() {
                let hit = arr.iter().position(|x| {
                    x["product"].as_str() == Some(store)
                        && x["scope"].as_str().unwrap_or("product") == scope
                        && x["account"].as_str().unwrap_or("") == acct_key
                });
                match hit {
                    Some(i) => arr[i] = item.clone(),
                    None => arr.push(item.clone()),
                }
            }
        }
    }
    json!({ "ok": true, "item": item })
}

/// 领一家。返回 agenthub 那一条结果，并把缓存作废（领完状态必然变了）。
/// `acct` 给了就按那个账号领（先用它自己的登录包铺临时档案），否则照旧领"当前登着的号"。
pub fn claim(store: &str, acct: Option<(&str, &str)>) -> Value {
    let _claim_guard = CLAIM_WRITE.lock().unwrap();
    if store.is_empty() {
        return json!({ "ok": false, "msg": "没说要给哪家签到" });
    }
    // 只有"登录态按账号各存一份"的产品才按账号领。其余几家的 acct 是界面顺手带上的
    // （产品行的 account 字段是"本机现在登着的那个号"），照着它去铺登录包会直接报
    // "还没存过登录态"——AStudio 那次点「确认领取」就是这么失败的（2026-10-17）。
    let resolved;
    let acct = if matches!(store, "workbuddy" | "workbuddy-intl") && acct.is_none() {
        let app = crate::local_quota::app_for_store(store).unwrap_or(store);
        resolved = crate::authpack::live_identity(app)
            .and_then(|(a, u)| crate::authpack::card_for_login(app, &a, &u));
        match resolved.as_deref() {
            Some(name) => Some((app, name)),
            None => return json!({"ok": false, "result": {"error": "当前登录身份没有对应的账号卡片，未签到"}}),
        }
    } else { acct.filter(|_| per_account(store)) };
    if store == "workbuddy-intl" {
        let today = chrono::Local::now().format("%Y-%m-%d").to_string();
        let key = format!("{store}@{}", acct.map(|(_, n)| n).unwrap_or(""));
        let state = load_state();
        if state["done"][&key].as_str() == Some(today.as_str()) {
            return json!({"ok": true, "result": {"already": true, "today": today,
                "message": "这个账号今天已活跃；到账进度按该账号单独追踪"}});
        }
    }
    let mut staged: Option<std::path::PathBuf> = None;
    let mut extra: Vec<String> = vec!["--claim".into(), store.to_string()];
    if let Some((app_id, name)) = acct {
        match stage(app_id, store, name) {
            Ok(d) => {
                let (flag, val) = stage_arg(app_id, store, &d);
                extra.push(flag);
                extra.push(val);
                staged = Some(d);
            }
            Err(e) => return json!({ "ok": false, "result": { "error": e } }),
        }
    }
    let refs: Vec<&str> = extra.iter().map(|s| s.as_str()).collect();
    let r = run(&refs);
    if let Some(d) = staged {
        crate::authpack::drop_profile(&d);
    }
    let item = match r {
        Ok(v) => v
            .as_array()
            .and_then(|a| a.first().cloned())
            .unwrap_or_else(|| json!({ "error": "agenthub 没返回结果" })),
        Err(e) => json!({ "error": e }),
    };
    // 记「今天跑过没有」的键：产品级用 store，账号级用 store@账号——
    // 两个号各自有各自的当日名额，共用一个键会让先领的那个把另一个顶掉
    let key = match acct {
        Some((_, n)) => format!("{store}@{n}"),
        None => store.to_string(),
    };
    let today = item["today"].as_str().unwrap_or("").to_string();
    let got = item["claimed"].as_bool().unwrap_or(false) || item["already"].as_bool().unwrap_or(false);
    // 写操作已经做完、只是服务端还没结算（现在只有 WorkBuddy 国际版那 30 积分）。
    // agenthub 用 settling_final 标出这种"今天这一步已经走完了"：
    //   · 记 done —— 否则下一轮自动领会再发一遍对话；
    //   · 挂进 pending —— 后台每几分钟替她盯一次，到账了在操作提示区说一声。
    // 旅行猫那种"先派、到点再领"的 settling 不带这个标记，今天还有第二步要走。
    let settled = item["settling"].as_bool().unwrap_or(false)
        && item["settling_final"].as_bool().unwrap_or(false);
    let _guard = LEDGER_WRITE.lock().unwrap();
    let mut st = load_state();
    // 只有真领到了（或今天这份本来就已经在手）、且这家今天没有下一步了，才记"今天跑过"。
    // 以前是无条件记，于是 MiniMax 那种"服务端拒了但被解析成成功"的一次点击，
    // 会把这家钉成今天已完成——当天再怎么重试都不会发第二次，用户看到的就是
    // "点了半天没反应，底下还一直提示领到了"（2026-10-13）。
    if marks_done(got, settled, !today.is_empty(), item["next_action"].as_str()) {
        st["done"].as_object_mut().map(|o| {
            o.insert(key.clone(), json!(today));
        });
    }
    if settled && !today.is_empty() {
        note_settling(&mut st, &key, &item, &today);
    }
    let mut log = st["log"].as_array().cloned().unwrap_or_default();
    // 记下这一笔是哪个号领的。账号级领取取传进来的账号；产品级领取（没传账号）取
    // agenthub 回包里它实际操作的账号——不取的话日志里只剩一条光秃秃的「Qoder CN」，
    // 两个号各自领没领完全分不出来（2026-11-05 她圈出来的那两条）。
    let log_acct = acct
        .map(|(_, n)| n.to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| {
            item["account"]
                .as_str()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        })
        .unwrap_or_default();
    log.push(json!({
        "at": today, "at_time": now_hms(), "store": store,
        // 账号级领取记下是哪个号领的——日志里两家都写 "qoder-cn" 就分不出是谁了
        "account": log_acct,
        "claimed": item["claimed"].as_bool().unwrap_or(false),
        "already": item["already"].as_bool().unwrap_or(false),
        // 旅行猫那种"这一步做成了、奖励要等"的，日志得跟即时提示一样按中性画——
        // 只认 claimed 的话，派猫出门会被染成红色"没领成"（2026-10-23 她圈的这条）
        "settling": item["settling"].as_bool().unwrap_or(false),
        "verified": item["verified"],
        "message": item["message"], "credit": item["credit"], "error": item["error"],
        // 活跃地图那种"发一句对话"的，把用的模型也记下来——
        // 她 2026-10-17 问过"以后能用免费模型完成对话吗"，日志里能直接看出走的哪个档
        "model": item["model"],
    }));
    // 日志留最近 2000 条。她 2026-10-17："有时候我要查看很多天前的记录，能帮我保留吗"——
    // 一天十来条，2000 条够看一年；再长就没意义了（一年的签到记录也翻不完）。
    // 界面按天筛（今天 / 近 7 天 / 近 30 天 / 全部），不在这里压。
    if log.len() > 2000 {
        log.drain(0..log.len() - 2000);
    }
    st["log"] = json!(log);
    save_state(&st);
    *CACHE.lock().unwrap() = None;
    json!({ "ok": item["error"].is_null(), "result": item })
}

/// 领到了（或今天这份本来就在手）要不要把这家记成"今天跑完"。
/// 一般家领完即结束；旅行猫领完一趟后面可能还欠一步——跨天领（昨晚到、今早领）之后
/// 往往还欠一次「派出」。agenthub 在旅行猫的领取回包里带 next_action（depart/claim
/// = 还有下一步），有下一步就不能记 done，否则当天自动领被钉死，今天的派出名额
/// 白白作废（官方 records 里 10-14 缺的那一趟就是这么丢的）。
fn marks_done(got: bool, settled: bool, today_ok: bool, next_action: Option<&str>) -> bool {
    let more = matches!(next_action, Some("depart") | Some("claim"));
    (got || settled) && today_ok && !more
}

/// 登记一家「写操作已经做完、服务端还没结算到账」的（现在只有 WorkBuddy 国际版那 30 积分）。
/// 落盘而不是只放内存：她可能过一会儿就把驾驶舱关了再开，这份"还在等"必须跟得住。
fn note_settling(st: &mut Value, key: &str, item: &Value, today: &str) {
    let now = now_secs();
    // 键是 `store` 或 `store@账号`。到账提示要能一眼看出是哪个号，
    // 所以标题写成「产品 · 账号」——账号级那行的 item.name 已经被换成账号名了，别直接用它
    let (store, acct) = match key.split_once('@') {
        Some((s, n)) => (s, Some(n)),
        None => (key, None),
    };
    let prod = item["product"]
        .as_str()
        .and_then(|p| CATALOG.iter().find(|(k, _)| *k == p).map(|(_, n)| *n))
        .unwrap_or(store);
    let label = match acct {
        Some(n) => format!("{prod} · {n}"),
        None => prod.to_string(),
    };
    let expect = item["expect"].as_str().unwrap_or("今天这份积分").to_string();
    let pend = match st["pending"].as_object_mut() {
        Some(o) => o,
        None => return,
    };
    // 国际版不同活跃日分开追踪；其他产品保留既有键和 24 小时期限定。
    let intl = store == "workbuddy-intl";
    let pending_key = if intl { format!("{key}|{today}") } else { key.to_string() };
    pend.insert(
        pending_key,
        json!({
            "since": now, "today": today, "label": label, "expect": expect,
            "until": now + if intl { 172800.0 } else { 86400.0 },
        }),
    );
}

/// 等到账到期还没来：转成一条「已到期」记录，写清欠的是哪一天、盯到什么时候收手。
/// 以前到期就是从 pending 里删掉，界面一个字不留——厂商第 3 天才发的那笔，
/// 事后连「欠过哪天」都查不出来（2026-11-06 她问的正是这条）。
fn expired_record(key: &str, p: &Value, now: f64) -> Value {
    json!({
        "key": key, "today": p["today"], "label": p["label"], "expect": p["expect"],
        "since": p["since"], "deadline": p["until"], "dropped_at": now,
    })
}

/// 把到期的几条并进 `expired`：同一个键只留一条（多实例或重复轮询不该把同一天欠账记两遍），
/// 整表留最近 60 条（一天最多几笔，够翻两个多月）。
fn note_expired(st: &mut Value, expired: &[Value]) {
    if expired.is_empty() {
        return;
    }
    let mut list = st["expired"].as_array().cloned().unwrap_or_default();
    for e in expired {
        if !list.iter().any(|x| x["key"] == e["key"]) {
            list.push(e.clone());
        }
    }
    if list.len() > 60 {
        list.drain(0..list.len() - 60);
    }
    st["expired"] = json!(list);
}

/// 现在有没有在等的到账
pub fn has_pending() -> bool {
    !load_state()["pending"].as_object().map(|o| o.is_empty()).unwrap_or(true)
}

/// 待观察的那几家回来读一次：到账了就挪进 `arrived`（界面据此在操作提示区弹一条）。
/// 只看"今天这份到没到"，走各家自己的状态接口，不带 --claim——这一步是只读的。
pub fn check_arrivals() -> Vec<Value> {
    let st = load_state();
    let pend = match st["pending"].as_object() {
        Some(o) if !o.is_empty() => o.clone(),
        _ => return Vec::new(),
    };
    let now = now_secs();
    let mut arrived: Vec<Value> = Vec::new();
    let mut expired: Vec<Value> = Vec::new();
    let mut done: Vec<String> = Vec::new();
    for (key, p) in &pend {
        if p["until"].as_f64().unwrap_or(0.0) < now {
            expired.push(expired_record(key, p, now));
            done.push(key.clone());
            continue;
        }
        // 待观察的键可能是"账号级"的（store@账号）：那个号的到没到账得用它自己的
        // 登录包去看，拿当前登着的那个号去读就是串号
        let base_key = key.split_once('|').map(|(k, _)| k).unwrap_or(key.as_str());
        let (store, acct) = match base_key.split_once('@') {
            Some((s, n)) => (s.to_string(), Some(n.to_string())),
            None => (base_key.to_string(), None),
        };
        let app_id = crate::local_quota::app_for_store(&store).unwrap_or("");
        let staged = match (&acct, app_id.is_empty()) {
            (Some(n), false) => stage(app_id, &store, n).ok(),
            _ => None,
        };
        // 指定了账号但登录包铺不开时绝不能退回当前账号，避免串号。
        if acct.is_some() && staged.is_none() { continue; }
        let mut extra: Vec<String> = vec![store.clone()];
        if let Some(d) = &staged {
            let (f, v) = stage_arg(app_id, &store, d);
            extra.push(f);
            extra.push(v);
        }
        let refs: Vec<&str> = extra.iter().map(|s| s.as_str()).collect();
        let item = match run(&refs) {
            Ok(v) => v
                .as_array()
                .and_then(|a| a.first().cloned())
                .unwrap_or_else(|| json!({})),
            Err(_) => {
                if let Some(d) = staged {
                    crate::authpack::drop_profile(&d);
                }
                continue; // 这一次没读到就下一轮再说，别把"待观察"当失败清掉
            }
        };
        if let Some(d) = staged {
            crate::authpack::drop_profile(&d);
        }
        let activity_date = p["today"].as_str().unwrap_or("");
        let inferred_arrival = if store == "workbuddy-intl" {
            item["bonus_entries"].as_array().and_then(|entries| entries.iter().find(|e|
                e["inferred_activity_date"].as_str() == Some(activity_date)))
        } else { None };
        if store == "workbuddy-intl" {
            if inferred_arrival.is_none() { continue; }
        } else if item["signed_today"].as_bool() != Some(true) { continue; }
        let label = p["label"].as_str().unwrap_or(key).to_string();
        let expect = p["expect"].as_str().unwrap_or("今天这份积分").to_string();
        let credit = item["reward"].as_f64();
        // 到账时刻：厂商流水带了完整时间戳（"2026-11-04 07:28:31"）就取它的时刻部分；
        // 没带（刚测到到账）才记现在。别拿检测时刻冒充到账时刻。
        let log_time = inferred_arrival
            .and_then(|entry| entry["arrived_at"].as_str())
            .and_then(|s| s.split_once(' ')).map(|(_, t)| t.trim().to_string())
            .filter(|t| t.contains(':') && t.len() <= 8)
            .unwrap_or_else(|| now_hms());
        arrived.push(json!({
            "id": format!("{key}-{}", p["today"].as_str().unwrap_or("")),
            "at": now, "store": store, "account": acct.clone().unwrap_or_default(),
            "log_date": inferred_arrival.and_then(|entry| entry["arrived_at"].as_str())
                .and_then(|date| date.get(..10)).unwrap_or(activity_date),
            "log_time": log_time,
            "label": label,
            "text": if let Some(entry) = inferred_arrival {
                format!("「{label}」{} 到账的 30 积分，按次日结算惯例推测属于 {activity_date} 的活跃；厂商未提供归属日期", entry["arrived_at"].as_str().unwrap_or(""))
            } else {
                format!("「{label}」的{expect}到账了{}", match credit { Some(c) => format!("（{} 积分）", num_text(c)), None => String::new() })
            },
        }));
        done.push(key.clone());
    }
    if arrived.is_empty() && done.is_empty() {
        return arrived;
    }
    let _guard = LEDGER_WRITE.lock().unwrap();
    let mut st = load_state();
    if let Some(o) = st["pending"].as_object_mut() {
        for k in &done {
            o.remove(k);
        }
    }
    let mut arr = st["arrived"].as_array().cloned().unwrap_or_default();
    arr.extend(arrived.iter().cloned());
    if arr.len() > 20 {
        arr.drain(0..arr.len() - 20);
    }
    st["arrived"] = json!(arr);
    note_expired(&mut st, &expired);
    let mut log = st["log"].as_array().cloned().unwrap_or_default();
    for entry in &arrived {
        // arrived 标记：日志筛选要分清「积分到账」和「已签未到」，两者都是 settling，
        // 靠翻消息文案猜会错；老条目没这栏，界面回落按文案认
        log.push(json!({ "at": entry["log_date"], "at_time": entry["log_time"], "store": entry["store"], "account": entry["account"],
            "settling": true, "arrived": true, "message": entry["text"] }));
    }
    if log.len() > 2000 { log.drain(0..log.len() - 2000); }
    st["log"] = json!(log);
    save_state(&st);
    arrived
}

/// 到账监测：每 4 分钟回来读一次，直到到账或超过 24 小时。
/// 单独一条线程，不挂在 auto_fetch 的间隔上——那个默认几十分钟一轮，
/// 而 30 积分凌晨三点多才到账，等到下一轮早就没意义了（2026-10-15 她报的正是这个）。
pub fn arrival_watch_loop(app: tauri::AppHandle) {
    loop {
        std::thread::sleep(std::time::Duration::from_secs(240));
        if !has_pending() {
            continue;
        }
        for n in check_arrivals() {
            let text = n["text"].as_str().unwrap_or("").to_string();
            if text.is_empty() {
                continue;
            }
            // 顺手发一条系统通知：她可能正把窗口收在托盘里
            let _ = app
                .notification()
                .builder()
                .title("Agent 驾驶舱")
                .body(text)
                .show();
        }
    }
}

/// 这一家今天该不该自动跑。判据全部摊开成一个纯函数，好测也好解释：
/// 总开关开着 → 这家单独开了 → 厂商说活动开着且今天能领 → 允许自动 →
/// 现在在时间窗内 → 今天还没为这家跑过。
pub fn should_run(
    cfg_enabled: bool,
    store_on: bool,
    item: &Value,
    hour: u32,
    from: u32,
    to: u32,
    done_today: bool,
) -> bool {
    cfg_enabled
        && store_on
        && item["auto"].as_bool().unwrap_or(false)
        && item["claimable"].as_bool().unwrap_or(false)
        && item["activity_on"].as_bool().unwrap_or(false)
        && !done_today
        && hour >= from
        && hour < to
}

/// 自动签到连续失败的封顶次数。她 2026-11-08："既然报错，能不能发几次就别发了，
/// 试个四五次就停止"——403 那种厂商硬拒的失败，5 分钟一轮的自动领每轮都撞一次，
/// 一上午就是几十条一模一样的红字，什么也换不来。连试 5 次没成就收手：
/// 当天不再自动重试（手动点不受影响），并在日志里留一条「停止重试」说明原因。
pub const AUTO_FAIL_LIMIT: u64 = 5;

/// 失败计数的键：按产品+账号+当天分开——两个号各自的失败互不拖累，
/// 隔了天自然作废（键里带日期，bump 时顺手清掉旧日期的键）。
fn auto_fail_key(store: &str, account: &str, today: &str) -> String {
    if account.is_empty() {
        format!("{store}|{today}")
    } else {
        format!("{store}@{account}|{today}")
    }
}

/// 这一轮自动领还许不许跑：当天的失败次数没用完才许
fn auto_fail_left(st: &Value, key: &str) -> bool {
    st["auto_fails"][key].as_u64().unwrap_or(0) < AUTO_FAIL_LIMIT
}

/// 记一次失败，返回累计次数。顺手把旧日期的键清掉，文件不会被攒大。
fn bump_auto_fail(st: &mut Value, key: &str, today: &str) -> u64 {
    let n = st["auto_fails"][key].as_u64().unwrap_or(0) + 1;
    if let Some(o) = st["auto_fails"].as_object_mut() {
        o.retain(|k, _| k.ends_with(&format!("|{today}")));
        o.insert(key.to_string(), json!(n));
    }
    n
}

/// 领成了就清掉当天的失败前账：偶发一次失败不该一直记着
fn clear_auto_fail(st: &mut Value, key: &str) {
    if let Some(o) = st["auto_fails"].as_object_mut() {
        o.remove(key);
    }
}

/// 这一轮领取算不算"没领成"。领到 / 已领过 / 已经推进到等服务端结算
/// （含国际版发完对话等到账）都不算——要么到手要么在等，重试没有意义。
fn claim_failed(x: &Value) -> bool {
    !(x["claimed"].as_bool().unwrap_or(false)
        || x["already"].as_bool().unwrap_or(false)
        || x["settling"].as_bool().unwrap_or(false))
}

/// 「登录并领取」的待办队列。
///
/// 为什么要有它：她点「登录并领取」之后，客户端要她自己登进去——那是几十秒到几分钟的事，
/// 一个同步动作等不起（`action` 全程拿着全局命令锁，等两分钟整个界面别的按钮全得排队）。
/// 所以点按钮只做两件事：拉起客户端、在这里登记一条待办；后台每 5 秒回来看一眼，
/// 那个号一能领就替她领掉，领完走 `arrived` 在操作提示区说一声。
///
/// 她 2026-10-17 原话："为什么我打开之后还要点一次领取，那我要你干什么"。
static WANT: Mutex<Vec<Value>> = Mutex::new(Vec::new());

/// 登记一条待办。同一个号重复点不叠加。
pub fn want_claim(app_id: &str, store: &str, account: &str) -> Value {
    if store.is_empty() {
        return json!({ "ok": false, "msg": "没说要给哪一家领" });
    }
    let mut w = WANT.lock().unwrap();
    if w.iter().any(|x| {
        x["store"].as_str() == Some(store) && x["account"].as_str().unwrap_or("") == account
    }) {
        return json!({ "ok": true, "msg": "这个号已经在等着了：登进去之后我会自动把今天这份领掉" });
    }
    w.push(json!({
        "app": app_id, "store": store, "account": account,
        "since": now_secs(),
        // 半小时还没登进来就算了：再等下去她多半是压根不想登了
        "until": now_secs() + 1800.0,
    }));
    json!({ "ok": true, "msg": "已开始等这个号登录：登进去之后我会自动把今天这份领掉，不用再点" })
}

/// 现在有几条在等
pub fn want_count() -> usize {
    WANT.lock().map(|w| w.len()).unwrap_or(0)
}

/// 把一条提示塞进 `arrived`——界面已经有"认新 id 就 toast"的通道，不用另开一条。
fn push_notice(text: &str) {
    let now = now_secs();
    let _guard = LEDGER_WRITE.lock().unwrap();
    let mut st = load_state();
    let mut arr = st["arrived"].as_array().cloned().unwrap_or_default();
    arr.push(json!({
        "id": format!("want-{}-{}", now as i64, arr.len()),
        "at": now, "store": "", "label": "", "text": text,
    }));
    if arr.len() > 20 {
        arr.drain(0..arr.len() - 20);
    }
    st["arrived"] = json!(arr);
    save_state(&st);
}

/// 后台每 5 秒调一次：把等着的那几个号挨个看一眼，能领就领掉。
/// 只读状态用 `refresh_one` 那条路（同一套按账号读的逻辑），领走才发写请求。
pub fn poll_wants() -> Vec<String> {
    let pending: Vec<Value> = match WANT.lock() {
        Ok(w) => w.clone(),
        Err(_) => return Vec::new(),
    };
    if pending.is_empty() {
        return Vec::new();
    }
    let now = now_secs();
    let mut msgs: Vec<String> = Vec::new();
    for want in &pending {
        let store = want["store"].as_str().unwrap_or("");
        let account = want["account"].as_str().unwrap_or("");
        let app_id = want["app"].as_str().unwrap_or("");
        let label = match account {
            "" => store.to_string(),
            n => format!("{store} · {n}"),
        };
        if want["until"].as_f64().unwrap_or(0.0) < now {
            remove_want(store, account);
            let m = format!("「{label}」等了半小时还没登录上，先不等了：登进去之后点这一行的 ↻ 就行");
            push_notice(&m);
            msgs.push(m);
            continue;
        }
        let pair = if account.is_empty() || app_id.is_empty() {
            None
        } else {
            Some((app_id, account))
        };
        // 只看状态：refresh_one 不动别的行，也不发写请求
        let st = refresh_one(store, pair);
        if st["ok"].as_bool() != Some(true) {
            continue; // 这一次没读到就下一轮再说
        }
        let item = &st["item"];
        if item["signed_today"].as_bool() == Some(true) {
            remove_want(store, account);
            let m = format!("「{label}」今天这份已经在账上了");
            push_notice(&m);
            msgs.push(m);
            continue;
        }
        if item["claimable"].as_bool() != Some(true) {
            continue; // 还没到能领的时候，继续等
        }
        let r = claim(store, pair);
        let x = &r["result"];
        let ok = x["claimed"].as_bool().unwrap_or(false);
        let credit = x["credit"].as_f64();
        let m = if ok {
            format!(
                "「{label}」已自动领到{}",
                match credit {
                    Some(c) => format!(" {} 积分", num_text(c)),
                    None => "今天这份".to_string(),
                }
            )
        } else if x["already"].as_bool() == Some(true) {
            format!("「{label}」今天这份已经领过了")
        } else if x["settling"].as_bool() == Some(true) {
            format!("「{label}」已发出，等服务端结算")
        } else {
            format!(
                "「{label}」自动领取没成：{}",
                x["message"]
                    .as_str()
                    .or(x["error"].as_str())
                    .unwrap_or("厂商没回原因")
            )
        };
        remove_want(store, account);
        push_notice(&m);
        msgs.push(m);
    }
    msgs
}

fn remove_want(store: &str, account: &str) {
    if let Ok(mut w) = WANT.lock() {
        w.retain(|x| {
            !(x["store"].as_str() == Some(store)
                && x["account"].as_str().unwrap_or("") == account)
        });
    }
}

/// 给 4 秒一轮的状态快照用：**绝不发网络请求**。
/// 缓存还没有（刚开机、后台第一轮没跑完）就把**上次查到的那份**交出去，一张表先画出来；
/// 一次都没查过才退回静态目录占位（界面把状态列写成"查询中"）。
/// 不许在这里顺手去查一次——那会把六家接口挂到界面刷新上。
///
/// 回落那份带 15 秒短缓存：状态构建每 4 秒要为二十来个程序各问一次 for_app，
/// 不能每次都去读一遍文件。
pub fn snapshot() -> Value {
    if let Some(v) = CACHE.lock().unwrap().clone() {
        return with_backend_error(v, last_error());
    }
    let now = now_secs();
    if let Ok(g) = FALLBACK.lock() {
        if let Some((at, v)) = g.as_ref() {
            if now - at < 15.0 {
                return with_backend_error(v.clone(), last_error());
            }
        }
    }
    let st = load_state();
    let last = &st["last"];
    let raw = last["items"]
        .as_array()
        .filter(|a| !a.is_empty())
        .cloned()
        .unwrap_or_else(catalog_items);
    let items = filter_items(raw, &store_visible);
    let v = json!({
        "ok": true, "items": items,
        // 有几条「登录并领取」正在等（界面据此在那一行显示"等待登录"）
        "want": want_count(),
        "fetched_at": last["fetched_at"].as_f64().unwrap_or(0.0),
        "state": public_state(&st),
        "stale": true, "refreshing": true,
        "placeholder": last["items"].as_array().map(|a| a.is_empty()).unwrap_or(true),
    });
    if let Ok(mut g) = FALLBACK.lock() {
        *g = Some((now, v.clone()));
    }
    with_backend_error(v, last_error())
}

fn with_backend_error(mut cached: Value, error: Option<String>) -> Value {
    cached["agenthub_error"] = json!(error);
    cached
}

/// 领取日志导出：把界面拼好的 CSV 落成文件，返回完整路径给界面标出来。
/// 以前走 WebView2 的匿名下载（blob + a.click），文件进了系统下载目录，
/// 界面只说一句「已导出 82 条」，她根本不知道文件在哪（2026-11-08 她报的）。
/// 现在后端来写：优先「下载」文件夹（USERPROFILE\Downloads），被重定向或写不进
/// 就退回驾驶舱数据目录——路径照实回报，是哪一个就说哪一个。
pub fn export_csv(csv: &str) -> Value {
    let name = format!("领取日志-{}.csv", chrono::Local::now().format("%Y-%m-%d"));
    // 带 BOM：Excel 打开中文不乱码
    let body = format!("\u{feff}{csv}");
    let home = std::env::var_os("USERPROFILE")
        .filter(|p| !p.is_empty())
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| registry::base_dir());
    let mut last_err = String::new();
    for dir in [home.join("Downloads"), registry::base_dir()] {
        if let Err(e) = fs::create_dir_all(&dir) {
            last_err = format!("{}（{}）", dir.display(), e);
            continue;
        }
        let p = dir.join(&name);
        // 同名就覆盖：文件名带日期，今天最新的一份永远是这一个，不用翻
        match fs::write(&p, &body) {
            Ok(_) => {
                return json!({
                    "ok": true,
                    "msg": format!("已导出到 {}", p.display()),
                    "path": p.to_string_lossy(),
                    "dir": dir.to_string_lossy(),
                })
            }
            Err(e) => last_err = format!("{}（{}）", p.display(), e),
        }
    }
    json!({ "ok": false, "msg": format!("导出失败：{last_err}") })
}

/// 卡片上那枚签到角标的数据：**只读缓存，不发网络请求**。
/// app id 与 agenthub 的 store 名不同名的情况（Trae 两边分别叫 traework-cn / trae-cn）
/// 走 `local_quota::store_for` 那一份真值，别在这里再抄一张映射表。
/// 缓存里没这家（不支持签到的产品）就回 Null，界面据此不画角标。
pub fn for_app(app_id: &str) -> Value {
    for_instance(app_id, "")
}

pub fn for_instance(app_id: &str, name: &str) -> Value {
    let store = match crate::local_quota::store_for(app_id) {
        Some(s) => s,
        None => return Value::Null,
    };
    // 走 snapshot()：缓存没建起来时它给的是"上次落盘那份"，所以卡片角标不用等后台
    // 第一轮查完才出现（一次都没查过则是占位行，会被下面判成 unknown，不画角标）
    let snap = snapshot();
    let items = match snap["items"].as_array() {
        Some(a) => a,
        None => return Value::Null,
    };
    let has_accounts = items.iter().any(|x| x["product"] == store && x["scope"] == "account");
    let it = match items.iter().find(|x| x["product"].as_str() == Some(store)
        && (name.is_empty() || !has_accounts || (x["scope"] == "account" && x["account"] == name))) {
        Some(x) => x,
        None => return Value::Null,
    };
    if it["pending"].as_bool() == Some(true) {
        return Value::Null;
    }
    if it.get("error").is_some() {
        return json!({ "state": "error", "msg": it["error"] });
    }
    json!({
        // 可领要排在"已领"前面：旅行猫跨天领时两个字段会同时为真——今天领过昨晚那趟
        // （signed_today=true），眼前这趟又到了（claimable=true）；先判 done 会把
        // 待领的盖成已完成（2026-10-25 漏领 5 积分那次）。其余各家 claimable 与
        // signed_today 互斥，这个换序只影响旅行猫。
        "state": if it["claimable"].as_bool() == Some(true) { "claimable" }
            else if it["signed_today"].as_bool() == Some(true) { "done" }
            // 这次没读到（空列表 / 没拿到设备令牌）：卡片角标要写"没读到"，
            // 不能落到下面的 unknown——那是"厂商没给"，跟"我们没问出来"是两件事
            else if it["unreadable"].as_bool() == Some(true) { "unreadable" }
            else if it["activity_on"].as_bool() == Some(false) { "off" }
            else { "unknown" },
        "reward": it["reward"], "streak": it["streak"],
        "auto": it["auto"], "name": it["name"],
    })
}

/// "10:05" → 605。解不开就退回默认 600，别让一个填错的提醒时间把提醒整个关掉。
pub fn hhmm_to_min(s: &str) -> u32 {
    let parts: Vec<&str> = s.split(':').collect();
    if parts.len() == 2 {
        if let (Ok(h), Ok(m)) = (parts[0].trim().parse::<u32>(), parts[1].trim().parse::<u32>()) {
            if h < 24 && m < 60 {
                return h * 60 + m;
            }
        }
    }
    600
}

/// 今天该不该弹这条提醒：到点了、今天还没弹过。
pub fn remind_due(now_min: u32, at_min: u32, already_today: bool) -> bool {
    !already_today && now_min >= at_min
}

/// 后台每轮自动取数之后调一次。领取要总开关 + 逐家开；提醒是只读的，默认就开着。
pub fn tick(app: &tauri::AppHandle) {
    let st = settings::get();
    let c = &st["checkin"];
    let auto_on = c["enabled"].as_bool().unwrap_or(false);
    let remind_on = c["remind"].as_bool().unwrap_or(true);
    if !auto_on && !remind_on {
        return;
    }
    let v = view();
    let items = match v["items"].as_array() {
        Some(a) => a.clone(),
        None => return,
    };
    let first = match items.first() {
        Some(x) => x,
        None => return,
    };
    let today = first["today"].as_str().unwrap_or("").to_string();
    let now_min = first["hour"].as_u64().unwrap_or(0) as u32 * 60
        + first["minute"].as_u64().unwrap_or(0) as u32;
    let mut state = load_state();

    if auto_on {
        let from = c["window_start"].as_u64().unwrap_or(6) as u32;
        let to = c["window_end"].as_u64().unwrap_or(12) as u32;
        // 按账号各存登录态的那几家（qoder 系 / WorkBuddy 系）：每个号各有各的当日名额。
        // 只走产品级那一行的话，自动签到永远只落在"当前登着的那个号"头上——
        // 她 2026-11-08："登录了哪个 WorkBuddy 国际版的账号，读取哪个积分到了，
        // 另一个根本读取不了"。名下有账号行的就账号行各领各的，产品行不再重复。
        let acct_stores: Vec<&str> = items
            .iter()
            .filter(|it| {
                it["scope"].as_str() == Some("account")
                    && it["product"]
                        .as_str()
                        .map(|s| per_account(s))
                        .unwrap_or(false)
                    && !it["account"].as_str().unwrap_or("").is_empty()
            })
            .filter_map(|it| it["product"].as_str())
            .collect();
        // (键, 产品, 账号, 最后一次败因)：失败计数与「停止重试」在循环后一次落盘
        let mut failures: Vec<(String, String, String, String)> = Vec::new();
        let mut succeeded: Vec<String> = Vec::new();
        for it in &items {
            let store = match it["product"].as_str() {
                Some(s) => s,
                None => continue,
            };
            let scope = it["scope"].as_str().unwrap_or("product");
            let per_acct_row = acct_stores.contains(&store);
            if scope != "account" && per_acct_row {
                // 名下有账号行时让位——但仅当当前登录号自己就有账号行（有它自己的登录包）。
                // 当前登录的那个号还没存登录包时，产品级那一行仍是它唯一的自动通道，
                // 关掉它等于这个号永远没人领。
                let cur = it["ledger_account"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| it["account"].as_str().unwrap_or(""));
                let covered = !cur.is_empty()
                    && items.iter().any(|a| {
                        a["product"].as_str() == Some(store)
                            && a["scope"].as_str() == Some("account")
                            && a["account"].as_str() == Some(cur)
                            && a["shared"].as_bool() != Some(true)
                            && a["unreadable"].as_bool() != Some(true)
                    });
                if covered {
                    continue;
                }
            }
            if scope == "account" && !per_acct_row {
                // 整机一份登录态的家（Trae CN / MiniMax…）挂出来的账号行只是"切到该号"的入口，
                // 不是各存各的名额——自动领仍走产品级那一行
                continue;
            }
            let account = if scope == "account" { it["account"].as_str().unwrap_or("") } else { "" };
            let already = if account.is_empty() {
                state["done"][store].as_str() == Some(today.as_str())
            } else {
                state["done"][&format!("{store}@{account}")].as_str() == Some(today.as_str())
            };
            if !should_run(
                true,
                c["auto"][store].as_bool().unwrap_or(false),
                it,
                now_min / 60,
                from,
                to,
                already,
            ) {
                continue;
            }
            let fail_key = auto_fail_key(store, account, &today);
            if !auto_fail_left(&state, &fail_key) {
                continue;
            }
            let app_id = it["app"].as_str().unwrap_or("");
            let r = if account.is_empty() {
                claim(store, None)
            } else {
                claim(store, Some((app_id, account)))
            };
            let x = &r["result"];
            if claim_failed(x) {
                let why = x["error"]
                    .as_str()
                    .or(x["message"].as_str())
                    .unwrap_or("厂商没回原因")
                    .to_string();
                failures.push((fail_key, store.to_string(), account.to_string(), why));
            } else {
                succeeded.push(fail_key);
            }
        }
        if !failures.is_empty() || !succeeded.is_empty() {
            let mut stops: Vec<String> = Vec::new();
            {
                let _guard = LEDGER_WRITE.lock().unwrap();
                let mut st = load_state();
                for key in &succeeded {
                    clear_auto_fail(&mut st, key);
                }
                let mut log = st["log"].as_array().cloned().unwrap_or_default();
                for (key, store, account, why) in &failures {
                    let n = bump_auto_fail(&mut st, key, &today);
                    if n != AUTO_FAIL_LIMIT {
                        continue;
                    }
                    // 到顶了：日志里留一条「停止重试」，把败因和出路说清
                    let label = CATALOG
                        .iter()
                        .find(|(k, _)| *k == store.as_str())
                        .map(|(_, n)| *n)
                        .unwrap_or(store.as_str());
                    let who = if account.is_empty() {
                        label.to_string()
                    } else {
                        format!("{label} · {account}")
                    };
                    let msg = format!(
                        "自动签到连试 {AUTO_FAIL_LIMIT} 次都没领成，今天不再自动重试；\
                         最后一次败在：{why}。想领的话手动点这一行的按钮，或去客户端自己签一次"
                    );
                    log.push(json!({
                        "at": today, "at_time": now_hms(), "store": store,
                        "account": account, "source": "auto-stop", "message": msg,
                    }));
                    stops.push(format!("「{who}」自动签到已停止重试：{why}"));
                }
                if log.len() > 2000 {
                    log.drain(0..log.len() - 2000);
                }
                st["log"] = json!(log);
                save_state(&st);
            }
            for m in stops {
                // 收手这件事要当场说一声：她可能正把窗口收在托盘里
                let _ = app.notification().builder().title("Agent 驾驶舱").body(m).show();
            }
        }
        state = load_state();
    }

    // 到点提醒：把"今天还有几家没领"报一次，一天只报一次，不催第二遍
    if !remind_on || today.is_empty() {
        return;
    }
    let at = hhmm_to_min(c["remind_at"].as_str().unwrap_or("10:05"));
    if !remind_due(now_min, at, state["reminded"].as_str() == Some(today.as_str())) {
        return;
    }
    let mute = &c["remind_mute"];
    let pending: Vec<String> = items
        .iter()
        .filter(|it| {
            it["claimable"].as_bool().unwrap_or(false)
                && !mute.get(it["product"].as_str().unwrap_or("")).and_then(|v| v.as_bool()).unwrap_or(false)
        })
        .map(|it| {
            let n = it["name"].as_str().unwrap_or("");
            match it["reward"].as_u64() {
                Some(v) => format!("{}（{}）", if n.is_empty() { "?" } else { n }, v),
                None => n.to_string(),
            }
        })
        .collect();
    // 不管有没有可领的，今天都算"报过"了：没可领的更不该晚上再弹一次
    {
        let _guard = LEDGER_WRITE.lock().unwrap();
        let mut latest = load_state();
        latest["reminded"] = json!(today);
        save_state(&latest);
    }
    if pending.is_empty() {
        return;
    }
    let body = format!("今天还没领：{}。点开「每日签到」看一眼。", pending.join("、"));
    let _ = app
        .notification()
        .builder()
        .title("Agent 驾驶舱")
        .body(body)
        .show();
}

#[cfg(test)]
mod tests {
    #[test]
    fn backend_error_overlays_cached_rows_and_clears_after_recovery() {
        let previous = json!({"ok": true, "items": [{"product": "qoder"}]});
        let failed = with_backend_error(previous.clone(), Some("offline".into()));
        assert_eq!(failed["items"], previous["items"]);
        assert_eq!(failed["agenthub_error"], "offline");
        let recovered = with_backend_error(failed, None);
        assert!(recovered["agenthub_error"].is_null());
    }
    /// 没装的产品和它名下的账号行都不该出现在签到页。
    #[test]
    fn hidden_products_and_their_account_rows_are_filtered() {
        use super::*;
        let items = json!([
            {"product":"workbuddy","scope":"product"},
            {"product":"workbuddy","scope":"account","parent":"workbuddy"},
            {"product":"qoder-cn","scope":"product"},
            {"product":"qoder-cn","scope":"account","parent":"qoder-cn"}
        ])
        .as_array()
        .unwrap()
        .clone();
        let out = filter_items(items, &|s| s == "workbuddy");
        assert_eq!(out.len(), 2);
        assert!(out.iter().all(|x| x["product"] == "workbuddy"));
    }

    /// 占位行也不能把本机没装的产品全铺出来。
    #[test]
    fn catalog_placeholders_follow_visibility() {
        use super::*;
        let rows = catalog_items_for(&|s| {
            matches!(s, "workbuddy" | "workbuddy-activity" | "workbuddy-travel")
        });
        let names: Vec<&str> = rows.iter().filter_map(|x| x["product"].as_str()).collect();
        assert_eq!(names, vec!["workbuddy", "workbuddy-activity", "workbuddy-travel"]);
    }

    #[test]
    fn workbuddy_rename_keeps_each_accounts_history_and_pending() {
        use super::*;
        let mut st = json!({"done":{"workbuddy-intl@old@example.com":"2026-11-04","workbuddy-intl@previous":"2026-11-03"},
            "pending":{"workbuddy-intl@old@example.com|2026-11-04":{"today":"2026-11-04","label":"WorkBuddy 国际版 · old@example.com"}},
            "log":[{"store":"workbuddy-intl","account":"old@example.com","message":"已活跃"}],"arrived":[]});
        rename_ledger_account(&mut st, "workbuddy-intl", "old@example.com", "Friendly Name");
        assert_eq!(st["done"]["workbuddy-intl@Friendly Name"], "2026-11-04");
        assert_eq!(st["done"]["workbuddy-intl@previous"], "2026-11-03");
        assert!(st["done"].get("workbuddy-intl@old@example.com").is_none());
        assert_eq!(st["pending"]["workbuddy-intl@Friendly Name|2026-11-04"]["label"], "WorkBuddy 国际版 · Friendly Name");
        assert_eq!(st["log"][0]["account"], "Friendly Name");
    }

    #[test]
    fn workbuddy_legacy_duplicate_arrival_has_one_owner() {
        use super::*;
        let mut st = json!({"pending":{"workbuddy-intl|2026-11-04":{"today":"2026-11-04"},
            "workbuddy-intl@new|2026-11-04":{"today":"2026-11-04"}},
            "log":[{"store":"workbuddy-intl","account":"","at":"2026-11-04","message":"「WorkBuddy 国际版」2026-11-04 07:28:31 到账的 30 积分"},
                {"store":"workbuddy-intl","account":"new","at":"2026-11-04","message":"「WorkBuddy 国际版 · new」2026-11-04 07:28:31 到账的 30 积分"},
                {"store":"workbuddy-intl","account":"previous","at":"2026-11-04","message":"「WorkBuddy 国际版 · previous」2026-11-04 01:13:44 到账的 30 积分"}],"arrived":[]});
        normalize_legacy_workbuddy(&mut st);
        assert_eq!(st["log"].as_array().unwrap().len(), 2);
        assert_eq!(st["pending"].as_object().unwrap().len(), 1);
        assert!(st["unassigned_pending"].get("workbuddy-intl|2026-11-04").is_some());
        let before = st.clone();
        normalize_legacy_workbuddy(&mut st);
        assert_eq!(before, st);
    }

    #[test]
    fn workbuddy_product_completion_follows_its_current_account() {
        use super::*;
        let mut it = json!({"product":"workbuddy-intl","scope":"product","ledger_account":"new",
            "today":"2026-11-04","signed_today":false,"claimable":true});
        apply_workbuddy_status(&mut it, &json!({"done":{"workbuddy-intl":"2026-11-04","workbuddy-intl@old":"2026-11-04"}}));
        assert_eq!(it["claimable"], true);
        apply_workbuddy_status(&mut it, &json!({"done":{"workbuddy-intl@new":"2026-11-04"}}));
        assert_eq!(it["signed_today"], true);
    }
    #[test]
    fn workbuddy_intl_completion_is_separate_from_older_arrival() {
        use super::*;
        let base = json!({ "product": "workbuddy-intl", "today": "2026-10-29",
            "scope": "product", "signed_today": false, "claimable": true,
            "inferred_today_arrival": false, "note": "今早到账推测属于昨天", "words": {} });
        let mut before = base.clone();
        apply_workbuddy_status(&mut before, &json!({"done": {}}));
        assert_eq!(before["claimable"], true);
        let mut after = base.clone();
        apply_workbuddy_status(&mut after, &json!({"done": {"workbuddy-intl": "2026-10-29"}}));
        assert_eq!(after["claimable"], false);
        assert_eq!(after["words"]["done"], "今天已活跃");
        let mut other = base.clone();
        other["scope"] = json!("account"); other["account"] = json!("B");
        apply_workbuddy_status(&mut other, &json!({"done": {"workbuddy-intl@A": "2026-10-29"}}));
        assert_eq!(other["claimable"], true);
    }

    /// 今天发过的那一句只剩在 pending 里（done 的键丢了）时，也必须认「今天已用过」，
    /// 否则她又会被催着重发一次——她 2026-11-03 报的就是这个。
    #[test]
    fn workbuddy_intl_pending_today_counts_as_done() {
        let base = json!({ "product": "workbuddy-intl", "today": "2026-10-29",
            "scope": "account", "account": "A", "signed_today": false, "claimable": true,
            "inferred_today_arrival": false, "note": "", "words": {} });
        let mut it = base.clone();
        apply_workbuddy_status(&mut it, &json!({"done": {},
            "pending": {"workbuddy-intl@A|2026-10-29": {"since": 1.0}}}));
        assert_eq!(it["claimable"], false);
        assert_eq!(it["words"]["done_act"], "今天已用过");
        // 别的日子挂着的那条不算今天
        let mut old = base.clone();
        apply_workbuddy_status(&mut old, &json!({"done": {},
            "pending": {"workbuddy-intl@A|2026-10-28": {"since": 1.0}}}));
        assert_eq!(old["claimable"], true);
    }

    /// 本地会话库认出来的活跃（agenthub 已把 signed_today 置真）不许被改回"可签到"，
    /// 也不许说成"驾驶舱替你发过"——那次是她自己在客户端聊的。
    #[test]
    fn workbuddy_intl_local_evidence_keeps_client_word() {
        let mut it = json!({ "product": "workbuddy-intl", "today": "2026-10-29",
            "scope": "account", "account": "A", "signed_today": true, "claimable": false,
            "inferred_today_arrival": false, "activity_at": "13:54",
            "note": "本地会话记录显示这个号今天 13:54 还发过对话（1 条会话）", "words": {} });
        apply_workbuddy_status(&mut it, &json!({"done": {}, "pending": {}}));
        assert_eq!(it["words"]["done"], "今天已活跃");
        assert_eq!(it["words"]["done_act"], "今天已用过");
        assert_eq!(it["note"], "本地会话记录显示这个号今天 13:54 还发过对话（1 条会话）",
            "她自己发的那次不能说成驾驶舱替她发的");
    }

    use super::*;

    fn item(auto: bool, claimable: bool, active: bool) -> Value {
        json!({ "auto": auto, "claimable": claimable, "activity_on": active })
    }

    /// 六道闸门少一道都会变成"没让她开她却自己签了"，所以每条单独钉
    #[test]
    fn auto_claim_needs_every_gate_open() {
        let ok = item(true, true, true);
        assert!(should_run(true, true, &ok, 9, 6, 12, false));
        assert!(!should_run(false, true, &ok, 9, 6, 12, false), "总开关关着不能跑");
        assert!(!should_run(true, false, &ok, 9, 6, 12, false), "这家没单独开不能跑");
        assert!(!should_run(true, true, &item(false, true, true), 9, 6, 12, false),
                "厂商要验证码/只能网页点的不能自动");
        assert!(!should_run(true, true, &item(true, false, true), 9, 6, 12, false),
                "今天没得领不能跑");
        assert!(!should_run(true, true, &item(true, true, false), 9, 6, 12, false),
                "活动没开不能跑");
        assert!(!should_run(true, true, &ok, 9, 6, 12, true), "今天已经跑过不能再跑");
        assert!(!should_run(true, true, &ok, 5, 6, 12, false), "窗口之前不跑");
        assert!(!should_run(true, true, &ok, 13, 6, 12, false), "窗口之后不跑");
    }

    /// 旅行猫跨天领：领完昨晚那趟，回包 next_action=depart 说明还欠一次派出，
    /// 不能记 done（记了当天自动领就被钉死）；领完最后一趟（next_action=done）才记
    #[test]
    fn travel_claim_with_more_steps_does_not_mark_done() {
        assert!(marks_done(true, false, true, Some("done")));
        assert!(marks_done(true, false, true, None), "其余几家没有 next_action，行为不变");
        assert!(!marks_done(true, false, true, Some("depart")));
        assert!(!marks_done(false, false, true, Some("done")), "没领到不记");
        assert!(!marks_done(true, false, false, Some("done")), "读不到今天日期不记");
    }

    /// 真机：整条 refresh() 跑一遍（含按账号展开），把结果打出来。
    /// 跑：cargo test -- --ignored --nocapture refresh_real
    #[test]
    #[ignore]
    fn refresh_real_machine() {
        crate::secrets::load();
        let v = refresh();
        println!("ok={:?} err={:?}", v["ok"], v["msg"]);
        if let Some(a) = v["items"].as_array() {
            for x in a {
                println!(
                    "  {:<20} scope={:<8} account={:<20} signed={:?} claimable={:?} err={:?}",
                    x["product"].as_str().unwrap_or("?"),
                    x["scope"].as_str().unwrap_or("-"),
                    x["account"].as_str().unwrap_or("-"),
                    x["signed_today"], x["claimable"],
                    x["error"].as_str().map(|s| s.chars().take(40).collect::<String>()),
                );
            }
        }
    }

    /// 按账号查失败也要留一行：「这次没读到」不是一个消失的账号
    #[test]
    fn failed_account_query_still_yields_a_row() {
        let row = account_row_unreadable("qoder-cn", "qoder-cn", "账号2", "unrecognized arguments");
        assert_eq!(row["scope"], "account");
        assert_eq!(row["account"], "账号2");
        assert_eq!(row["unreadable"], true);
        assert!(row["note"].as_str().unwrap().contains("unrecognized arguments"));
    }

    /// 提醒时间解析：填错要退回默认，不能把提醒整个关掉
    #[test]
    fn remind_time_parses_and_falls_back() {
        assert_eq!(hhmm_to_min("10:05"), 605);
        assert_eq!(hhmm_to_min("0:00"), 0);
        assert_eq!(hhmm_to_min("23:59"), 1439);
        assert_eq!(hhmm_to_min("乱填"), 600);
        assert_eq!(hhmm_to_min("25:00"), 600);
        assert_eq!(hhmm_to_min("10:75"), 600);
        assert!(remind_due(605, 600, false), "到点且今天没弹过");
        assert!(!remind_due(599, 600, false), "没到点");
        assert!(!remind_due(900, 600, true), "今天已经弹过就不再弹");
    }

    #[test]
    fn external_hit_dedupes_by_day_and_account() {
        let st = json!({"log": [
            {"at": "2026-11-04", "store": "qoder-cn", "account": "", "claimed": true},
            {"at": "2026-11-03", "store": "qoder-cn", "account": "", "source": "external"}
        ]});
        assert!(log_hit_today(&st, "qoder-cn", "", "2026-11-04"));
        assert!(log_hit_today(&st, "qoder-cn", "", "2026-11-03"));
        assert!(!log_hit_today(&st, "qoder-cn", "", "2026-11-02"));
        assert!(!log_hit_today(&st, "qoder", "", "2026-11-04"));
    }

    #[test]
    fn backfill_recovers_old_rows_from_real_sources_only() {
        let mut st = json!({
            "done": {}, "pending": {}, "unassigned_pending": {}, "arrived": [],
            "log": [
                {"at": "2026-10-30", "store": "workbuddy-intl", "account": "", "settling": true,
                 "message": "「WorkBuddy 国际版」2026-10-30 07:10:38 到账的 30 积分，按次日结算惯例推测属于 2026-10-29 的活跃；厂商未提供归属日期"},
                {"at": "2026-10-29", "store": "workbuddy-intl", "account": "", "settling": true,
                 "message": "今天的对话已完成（6114 token，hy3），今天的积分尚待到账；最近一笔于 2026-10-29 07:52:43 到账，按次日结算惯例推测属于 2026-10-28 的活跃。"},
                {"at": "2026-11-05", "store": "workbuddy-intl", "account": "Gerald", "settling": true,
                 "message": "今天的对话已完成（6108 token，hy3），今天的积分尚待到账"},
                {"at": "2026-10-13", "store": "qoder-cn", "account": "", "claimed": true, "message": "领取成功"},
                {"at": "2026-11-06", "at_time": "20:21:11", "store": "qoder-cn", "account": "", "claimed": true, "message": "CLAIMED"}
            ]
        });
        st["pending"]["workbuddy-intl@Gerald|2026-11-05"] =
            json!({"since": 1793848074.08, "today": "2026-11-05"});
        st["arrived"] = json!([{"store": "qoder-cn", "account": "", "log_date": "2026-10-13", "at": 1791855600.0}]);
        backfill_log_times(&mut st);
        // ① 厂商时间戳嵌在消息里的，精确到秒
        assert_eq!(st["log"][0]["at_time"], "07:10:38");
        // 「最近一笔于 …」里的时间戳是上一笔的，不是这一条的，不认
        assert!(st["log"][1]["at_time"].is_null());
        // ② pending.since（具体几点随时区，只看形状）
        let t = st["log"][2]["at_time"].as_str().unwrap_or("");
        assert!(t.len() == 8 && t.as_bytes()[2] == b':' && t.as_bytes()[5] == b':', "实际 {t:?}");
        // ③ arrived.at
        let t = st["log"][3]["at_time"].as_str().unwrap_or("");
        assert!(t.len() == 8 && t.as_bytes()[2] == b':' && t.as_bytes()[5] == b':', "实际 {t:?}");
        // 已有 at_time 的新条目不动
        assert_eq!(st["log"][4]["at_time"], "20:21:11");
    }

    #[test]
    fn external_signins_recorded_once_and_skip_cockpit_owned_days() {
        // 同一天同一个号只记一笔（产品行 + 账号行报到同一个号时去重）
        let mut st = json!({"done": {}, "pending": {}, "log": []});
        record_external_signins(&json!([
            {"product": "qoder-cn", "account": "", "today": "2026-11-04", "signed_today": true},
            {"product": "qoder-cn", "account": "", "today": "2026-11-04", "signed_today": true},
            {"product": "workbuddy-intl", "account": "Gerald", "today": "2026-11-04",
             "signed_today": true, "mode": "activity"}
        ]), &mut st);
        assert_eq!(st["log"].as_array().unwrap().len(), 2);
        assert_eq!(st["log"][0]["source"], "external");
        // 新条目要带精确时刻（at 只有日期）；老条目没这栏，界面回落成只显示日期
        let t = st["log"][0]["at_time"].as_str().unwrap_or("");
        assert!(t.len() == 8 && t.as_bytes()[2] == b':' && t.as_bytes()[5] == b':',
            "at_time 该是 HH:MM:SS，实际是 {t:?}");
        assert_eq!(st["done"]["qoder-cn"], "2026-11-04");
        assert_eq!(st["done"]["workbuddy-intl@Gerald"], "2026-11-04");
        // 驾驶舱自己领过的（日志有 claimed）不再记外部
        let mut st2 = json!({"done": {}, "pending": {}, "log": [
            {"at": "2026-11-04", "store": "qoder-cn", "account": "", "claimed": true}]});
        record_external_signins(&json!([
            {"product": "qoder-cn", "account": "", "today": "2026-11-04", "signed_today": true}
        ]), &mut st2);
        assert_eq!(st2["log"].as_array().unwrap().len(), 1);
        // 驾驶舱在等到账的（pending，含国际版的 key|today 键）也不记
        let mut st3 = json!({"done": {}, "pending": {
            "workbuddy-intl@Gerald|2026-11-04": {"since": 1}}, "log": []});
        record_external_signins(&json!([
            {"product": "workbuddy-intl", "account": "Gerald", "today": "2026-11-04",
             "signed_today": true}
        ]), &mut st3);
        assert_eq!(st3["log"].as_array().unwrap().len(), 0);
        // 旅行猫由驾驶舱管全程，不认外部
        let mut st4 = json!({"done": {}, "pending": {}, "log": []});
        record_external_signins(&json!([
            {"product": "workbuddy-travel", "account": "", "today": "2026-11-04",
             "signed_today": true, "mode": "travel"}
        ]), &mut st4);
        assert_eq!(st4["log"].as_array().unwrap().len(), 0);
    }

    /// 到期那一条必须把「欠的是哪天、盯到什么时候」一起带走，否则界面只剩一句空话
    #[test]
    fn expired_pending_keeps_which_day_was_owed() {
        let p = json!({"since": 100.0, "today": "2026-11-05",
            "label": "WorkBuddy 国际版 · Gerald Fabinhu", "expect": "今天这份积分", "until": 200.0});
        let e = expired_record("workbuddy-intl@Gerald Fabinhu|2026-11-05", &p, 260.0);
        assert_eq!(e["today"], json!("2026-11-05"));
        assert_eq!(e["label"], json!("WorkBuddy 国际版 · Gerald Fabinhu"));
        assert_eq!(e["expect"], json!("今天这份积分"));
        assert_eq!(e["deadline"], json!(200.0));
        assert_eq!(e["dropped_at"], json!(260.0));
    }

    #[test]
    fn note_expired_dedupes_by_key_and_bounds_the_list() {
        let one = |day: &str| json!({"key": format!("workbuddy-intl@A|{day}"), "today": day});
        let mut st = json!({"expired": []});
        note_expired(&mut st, &[one("2026-11-05")]);
        // 同一天的欠账重复轮询不许记两遍
        note_expired(&mut st, &[one("2026-11-05"), one("2026-11-06")]);
        assert_eq!(st["expired"].as_array().unwrap().len(), 2);
        let many: Vec<Value> = (0..70).map(|i| one(&format!("2026-{i:02}"))).collect();
        let mut st2 = json!({"expired": []});
        note_expired(&mut st2, &many);
        let list = st2["expired"].as_array().unwrap();
        assert_eq!(list.len(), 60);
        assert_eq!(list[59]["today"], json!("2026-69"), "留下的是最近那批，不是最老的");
    }

    /// 自动领的失败封顶：产品级与账号级各记各的账，隔天作废，领成清前账。
    /// 她 2026-11-08：403 撞了一上午没人喊停——不能再来一次。
    #[test]
    fn auto_fail_cap_counts_per_store_account_and_day() {
        let mut st = json!({"auto_fails": {}});
        let k = auto_fail_key("workbuddy-intl", "Gerald Fabinhu", "2026-11-08");
        assert_eq!(k, "workbuddy-intl@Gerald Fabinhu|2026-11-08");
        let kp = auto_fail_key("minimax", "", "2026-11-08");
        assert_eq!(kp, "minimax|2026-11-08");
        for _ in 0..4 {
            bump_auto_fail(&mut st, &k, "2026-11-08");
        }
        assert!(auto_fail_left(&st, &k), "4 次还在限额内");
        assert_eq!(bump_auto_fail(&mut st, &k, "2026-11-08"), 5);
        assert!(!auto_fail_left(&st, &k), "到 5 次就收手");
        // 另一个号 / 另一家 / 隔一天，各是各的账
        assert!(auto_fail_left(&st, &auto_fail_key("workbuddy-intl", "shen1950", "2026-11-08")));
        assert!(auto_fail_left(&st, &auto_fail_key("minimax", "", "2026-11-08")));
        assert!(auto_fail_left(&st, &auto_fail_key("workbuddy-intl", "Gerald Fabinhu", "2026-11-09")));
        // 新一天的第一次失败把旧日期的键清掉，文件不被攒大
        bump_auto_fail(&mut st, &auto_fail_key("workbuddy-intl", "Gerald Fabinhu", "2026-11-09"), "2026-11-09");
        assert!(st["auto_fails"]
            .as_object()
            .unwrap()
            .keys()
            .all(|x| x.ends_with("|2026-11-09")));
        // 领成了清前账
        clear_auto_fail(&mut st, &auto_fail_key("workbuddy-intl", "Gerald Fabinhu", "2026-11-09"));
        assert!(auto_fail_left(&st, &auto_fail_key("workbuddy-intl", "Gerald Fabinhu", "2026-11-09")));
    }

    /// 什么算"没领成"：领到 / 已领过 / 已经在等结算都不算，重试没有意义
    #[test]
    fn claim_failed_only_counts_real_failures() {
        assert!(claim_failed(&json!({"error": "这一句没跑成：403 request illegal"})));
        assert!(claim_failed(&json!({"claimed": false, "message": "服务端拒了"})));
        assert!(!claim_failed(&json!({"claimed": true})));
        assert!(!claim_failed(&json!({"already": true})));
        assert!(!claim_failed(&json!({"settling": true, "settling_final": true})),
            "国际版发完对话等到账，不是失败");
        assert!(!claim_failed(&json!({"claimed": false, "already": true})));
    }
}
