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
use std::process::Command;
use std::sync::Mutex;

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

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
    ("qoder-cn", "Qoder CN"),
    ("qoder", "Qoder"),
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

fn now_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
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
    // agenthub 是 .cmd 包装，CreateProcess 直接执行不了，必须经 cmd.exe 转一手
    let mut c = Command::new("cmd.exe");
    let mut args: Vec<&str> = vec!["/d", "/c", "agenthub", "checkin"];
    args.extend_from_slice(extra);
    args.push("--json");
    c.args(&args);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(CREATE_NO_WINDOW);
    }
    let out = c
        .output()
        .map_err(|e| format!("调不到 agenthub（{e}）：本机需要先装 agent-memory-suite"))?;
    if !out.status.success() {
        let s = String::from_utf8_lossy(&out.stderr);
        return Err(format!(
            "agenthub checkin 失败：{}",
            s.trim().chars().take(180).collect::<String>()
        ));
    }
    let parsed: Value = serde_json::from_slice(&out.stdout)
        .map_err(|e| format!("agenthub 输出不是 JSON：{e}"))?;
    Ok(parsed)
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
    for (k, d) in [("done", json!({})), ("log", json!([])), ("pending", json!({})), ("arrived", json!([]))] {
        if !o.contains_key(k) {
            o.insert(k.into(), d);
        }
    }
    Value::Object(o)
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
            let now = now_secs();
            // 产品行后面挂上各账号自己的行（只有"登录态按账号各存一份、且存了不止一个号"
            // 的产品才挂）。这一步是几次额外的 agenthub 调用，所以只在这一条后台路径上做，
            // 界面那 4 秒一轮读的是缓存。
            let mut items = expand_accounts(items);
            let st = load_state();
            apply_workbuddy_statuses(&mut items, &st);
            // 把这次的结果落盘：下次刚开机、后台第一轮还没跑完时，界面就能拿它先把
            // 整张表画出来（产品名、备注、操作列都在里面），不用干等六家接口。
            let mut persisted = st.clone();
            persisted["last"] = json!({ "items": items, "fetched_at": now });
            save_state(&persisted);
            let v = json!({ "ok": true, "items": items, "fetched_at": now, "state": public_state(&st) });
            *CACHE.lock().unwrap() = Some(v.clone());
            v
        }
        Err(e) => json!({ "ok": false, "msg": e }),
    }
}

/// 国际版账单只有到账时间，没有活跃日期。当天是否完成对话以本地记录为准，
/// 到账归属仅按次日结算惯例推测；不能用今早的到账堵住今天的签到。
fn apply_workbuddy_status(item: &mut Value, st: &Value) {
    if item["product"].as_str() != Some("workbuddy-intl") || item.get("error").is_some() {
        return;
    }
    let today = item["today"].as_str().unwrap_or("").to_string();
    if today.is_empty() { return; }
    let account = if item["scope"].as_str() == Some("account") {
        item["account"].as_str().unwrap_or("")
    } else { "" };
    let key = if account.is_empty() { "workbuddy-intl".to_string() }
              else { format!("workbuddy-intl@{account}") };
    if st["done"][&key].as_str() != Some(today.as_str()) { return; }
    let inferred = item["inferred_today_arrival"].as_bool() == Some(true);
    let note = item["note"].as_str().unwrap_or("").to_string();
    item["signed_today"] = json!(true);
    item["claimable"] = json!(false);
    item["words"]["done"] = json!(if inferred { "已签到·推测到账" } else { "已签到·积分未到账" });
    item["words"]["done_act"] = json!("今天已签到");
    item["note"] = json!(format!("今天的对话已完成，{}；{}",
        if inferred { "有推测属于今天的到账记录，归属未经厂商确认" } else { "今天的积分尚未发现到账记录" }, note));
}

fn apply_workbuddy_statuses(items: &mut Value, st: &Value) {
    if let Some(arr) = items.as_array_mut() {
        for item in arr { apply_workbuddy_status(item, st); }
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
    CATALOG
        .iter()
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
    if store.is_empty() {
        return json!({ "ok": false, "msg": "没说要给哪家签到" });
    }
    // 只有"登录态按账号各存一份"的产品才按账号领。其余几家的 acct 是界面顺手带上的
    // （产品行的 account 字段是"本机现在登着的那个号"），照着它去铺登录包会直接报
    // "还没存过登录态"——AStudio 那次点「确认领取」就是这么失败的（2026-10-17）。
    let acct = acct.filter(|_| per_account(store));
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
    log.push(json!({
        "at": today, "store": store,
        // 账号级领取记下是哪个号领的——日志里两家都写 "qoder-cn" 就分不出是谁了
        "account": acct.map(|(_, n)| n).unwrap_or(""),
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
    let mut done: Vec<String> = Vec::new();
    for (key, p) in &pend {
        if p["until"].as_f64().unwrap_or(0.0) < now {
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
        arrived.push(json!({
            "id": format!("{key}-{}", p["today"].as_str().unwrap_or("")),
            "at": now, "store": store, "account": acct.clone().unwrap_or_default(),
            "log_date": inferred_arrival.and_then(|entry| entry["arrived_at"].as_str())
                .and_then(|date| date.get(..10)).unwrap_or(activity_date),
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
    let mut log = st["log"].as_array().cloned().unwrap_or_default();
    for entry in &arrived {
        log.push(json!({ "at": entry["log_date"], "store": entry["store"], "account": entry["account"],
            "settling": true, "message": entry["text"] }));
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
        return v;
    }
    let now = now_secs();
    if let Ok(g) = FALLBACK.lock() {
        if let Some((at, v)) = g.as_ref() {
            if now - at < 15.0 {
                return v.clone();
            }
        }
    }
    let st = load_state();
    let last = &st["last"];
    let items = last["items"]
        .as_array()
        .filter(|a| !a.is_empty())
        .cloned()
        .unwrap_or_else(catalog_items);
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
    v
}

/// 卡片上那枚签到角标的数据：**只读缓存，不发网络请求**。
/// app id 与 agenthub 的 store 名不同名的情况（Trae 两边分别叫 traework-cn / trae-cn）
/// 走 `local_quota::store_for` 那一份真值，别在这里再抄一张映射表。
/// 缓存里没这家（不支持签到的产品）就回 Null，界面据此不画角标。
pub fn for_app(app_id: &str) -> Value {
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
    let it = match items.iter().find(|x| x["product"].as_str() == Some(store)) {
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
        for it in &items {
            // 自动领只走产品级那一行：账号行是"同一件事的另一个视角"，
            // 照它们各领一次会把同一家的当日名额重复消耗
            if it["scope"].as_str() == Some("account") {
                continue;
            }
            let store = match it["product"].as_str() {
                Some(s) => s,
                None => continue,
            };
            let already = state["done"][store].as_str() == Some(today.as_str());
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
            claim(store, None);
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
    state["reminded"] = json!(today);
    save_state(&state);
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
        assert_eq!(after["words"]["done"], "已签到·积分未到账");
        let mut other = base.clone();
        other["scope"] = json!("account"); other["account"] = json!("B");
        apply_workbuddy_status(&mut other, &json!({"done": {"workbuddy-intl@A": "2026-10-29"}}));
        assert_eq!(other["claimable"], true);
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
}
