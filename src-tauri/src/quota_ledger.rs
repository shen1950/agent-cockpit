//! 积分包台账（Rust 侧）。
//!
//! Qoder 的逐包明细是驾驶舱用官网会话单独读回来的（`webquota::apply`），跑在
//! Python 那条通道之外——agenthub 的 quota 只能拿到「主额度 + 附加包」两个汇总数。
//! 所以 Qoder 的过期留存必须在这里做，否则官网一不给，那几笔过期的包就彻底消失。
//!
//! 文件格式与 `agenthub.py` 里的 `quota-ledger.json` 完全一致（同一份文件、同一套
//! key：`产品 \x1f 账号 \x1f 身份`，身份 = 厂商资源 ID 优先，没有才退到 到期+发放额）。
//! 两边谁先记都一样，不会长出两套。2026-11-18 前的旧键（含组名与已用）在 load 时
//! 保守迁移：同一笔的多个旧快照合并成一条，取最近读数，迁移前备份一份旧账。
//!
//! **已用不进身份**（随消费变化，会把同一笔拆成多条）；**组名不进身份**
//! （改分类只改记录上的 group 属性）。两个不同资源 ID 的同额同到期包各是一条。
//!
//! 只记录、不改判定：补回的笔带 `from_ledger: true`，界面据此标「台账」；
//! 组层汇总只按**效期内**的笔重算，台账里的旧包不会把余额撑大。
//! 过期包永久留档：不设每组条数上限，也不按年限清理（厂商不给历史的产品，
//! 台账是唯一副本）。

use chrono::{Local, NaiveDateTime};
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::path::PathBuf;

const SEP: &str = "\u{1f}";
const EXPIRY_UNKNOWN: &str = "未给出";

fn ledger_path() -> PathBuf {
    crate::registry::base_dir().join("quota-ledger.json")
}

pub(crate) fn saved_records() -> Value {
    std::fs::read(ledger_path()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or(Value::Null)
}

pub(crate) fn saved_grant(ledger: &Value, app: &str, account: &str, it: &Value, allow_estimate: bool) -> Option<(Value, Value)> {
    let key = format!("{}{}{}{}{}", app, SEP, account, SEP, ident(it));
    let rec = ledger["packs"].get(&key)?;
    if it["resource_id"].as_str().unwrap_or("").is_empty() && ledger["packs"].get(format!("{key}{SEP}2")).is_some() {
        return None;
    }
    let estimated = rec["granted_est"] == true || rec["granted_src"] == "estimate";
    if (allow_estimate || !estimated) && rec["granted_on"].as_str().is_some_and(|s| s.starts_with(|c: char| c.is_ascii_digit())) {
        let src = rec["granted_src"].as_str().unwrap_or(if estimated { "estimate" } else { "vendor" });
        return Some((rec["granted_on"].clone(), Value::from(src)));
    }
    Some((Value::from(rec["first_seen"].as_str().filter(|s| !s.is_empty())?), Value::from("first_seen")))
}

fn transaction_lock() -> std::io::Result<std::fs::File> {
    let path = ledger_path().with_extension("lock");
    std::fs::create_dir_all(path.parent().unwrap())?;
    let lock = std::fs::OpenOptions::new().create(true).truncate(false).read(true).write(true).open(path)?;
    lock.lock()?;
    Ok(lock)
}

fn num(v: Option<&Value>) -> Option<f64> {
    match v {
        Some(Value::Number(n)) => n.as_f64(),
        Some(Value::String(s)) => s.trim().replace(',', "").parse::<f64>().ok(),
        _ => None,
    }
}

fn exp_past(s: &str, now: &NaiveDateTime) -> bool {
    if s.trim().is_empty() || s == EXPIRY_UNKNOWN || s == "无限期" || s == "—" {
        return false;
    }
    crate::quota::expiry_time(s).is_some_and(|end| end < *now)
}

/// 保守匹配用的签名：到期（截到分钟，新旧精度归一）+ 发放额。都是不可变字段。
/// 用字符串做键（f64 不能 Hash/Eq）；总额没给写空串，与 Python 侧一致。
fn sig(it: &Value) -> String {
    let exp = it.get("expires_on").and_then(|v| v.as_str()).unwrap_or("");
    let exp = exp.chars().take(16).collect::<String>();
    let total = num(it.get("total"))
        .map(|v| format!("{:.6}", (v * 1e6).round() / 1e6))
        .unwrap_or_default();
    format!("{}{}{}", exp, SEP, total)
}

fn sig_key(product: &str, account: &str, s: &str) -> String {
    format!("{}{}{}{}s{}{}", product, SEP, account, SEP, SEP, s)
}

/// 一笔包的稳定身份：厂商资源 ID（resource_id）优先，没有才退到 到期+发放额。
/// 已用随消费变化、组名随分类变化，都不进身份。
fn ident(it: &Value) -> String {
    let rid = it.get("resource_id").and_then(|v| v.as_str()).unwrap_or("").trim();
    if !rid.is_empty() {
        return format!("r{}{}", SEP, rid);
    }
    format!("s{}{}", SEP, sig(it))
}

/// 旧键（产品␟账号␟组名␟到期␟总额␟已用）→ 新键（产品␟账号␟身份）。
/// 同一笔的多个旧快照合并：last_seen 最新的读数优先，first_seen 取最早。
/// 返回是否有改动。
fn migrate(packs: &mut Map<String, Value>) -> bool {
    let mut changed = false;
    let mut merged: Map<String, Value> = Map::new();
    let keys: Vec<String> = packs.keys().cloned().collect();
    for key in keys {
        let parts: Vec<&str> = key.split(SEP).collect();
        if parts.len() >= 3 && (parts[2] == "r" || parts[2] == "s") {
            continue; // 已是新键
        }
        let Some(rec) = packs.remove(&key) else { continue };
        changed = true;
        if parts.len() < 6 {
            continue; // 认不出的残键
        }
        let (product, acct, exp, total) = (parts[0], parts[1], parts[3], parts[4]);
        let s = format!(
            "{}{}{}",
            exp.chars().take(16).collect::<String>(),
            SEP,
            total.parse::<f64>().ok().map(|v| format!("{:.6}", (v * 1e6).round() / 1e6)).unwrap_or_default()
        );
        let nk = sig_key(product, acct, &s);
        let Some(old) = merged.remove(&nk) else {
            merged.insert(nk, rec);
            continue;
        };
        let last_new = rec.get("last_seen").and_then(|v| v.as_str()).unwrap_or("");
        let last_old = old.get("last_seen").and_then(|v| v.as_str()).unwrap_or("");
        let (mut keep, drop) = if last_new >= last_old { (rec, old) } else { (old, rec) };
        let fs: Vec<&str> = [&keep, &drop]
            .iter()
            .filter_map(|x| x.get("first_seen").and_then(|v| v.as_str()))
            .filter(|s| !s.is_empty())
            .collect();
        if let Some(min) = fs.iter().min() {
            keep["first_seen"] = Value::String((*min).to_string());
        }
        for fld in ["granted_on", "granted_est", "granted_src", "resource_id", "unit"] {
            if keep.get(fld).map_or(true, |v| v.is_null() || v.as_str() == Some("")) {
                if let Some(v) = drop.get(fld).filter(|v| !v.is_null()) {
                    keep[fld] = v.clone();
                }
            }
        }
        merged.insert(nk, keep);
    }
    for (k, v) in merged {
        packs.insert(k, v);
    }
    changed
}

fn load() -> Map<String, Value> {
    let p = ledger_path();
    let Ok(s) = std::fs::read_to_string(&p) else {
        return Map::new();
    };
    let Ok(v) = serde_json::from_str::<Value>(&s) else {
        // 台账坏了不许把额度页带崩：当空的处理，下一轮重建
        return Map::new();
    };
    let mut packs = v
        .get("packs")
        .and_then(|x| x.as_object())
        .cloned()
        .unwrap_or_default();
    if migrate(&mut packs) {
        // 迁移前备份一份旧账（只备一次），改坏了还能找回来
        let bak = p.with_file_name("quota-ledger.json.bak-identity");
        if !bak.exists() {
            let _ = std::fs::write(&bak, &s);
        }
    }
    packs
}

fn save(packs: &Map<String, Value>) {
    let p = ledger_path();
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let body = serde_json::json!({ "version": 2, "packs": packs });
    let tmp = p.with_file_name("quota-ledger.json.tmp");
    if std::fs::write(&tmp, body.to_string()).is_ok() {
        let _ = std::fs::rename(&tmp, &p);
    }
}

fn item_of(rec: &Value, gname: &str, unit: Option<&str>) -> Value {
    let mut it = serde_json::json!({
        "label": rec.get("label").and_then(|v| v.as_str()).unwrap_or(gname),
        "total": rec.get("total").cloned().unwrap_or(Value::Null),
        "used": rec.get("used").cloned().unwrap_or(Value::Null),
        "left": rec.get("left").cloned().unwrap_or(Value::Null),
        "voided": rec.get("voided"),
        "unit": rec.get("unit").and_then(|v| v.as_str()).or(unit),
        "expires_on": rec.get("expires_on").cloned().unwrap_or(Value::Null),
        "detail": "台账记录 · 厂商已不再返回这一笔",
        "granted_on": rec.get("granted_on").cloned().unwrap_or(Value::Null),
        // 到账时间的来源：vendor=厂商给的 / estimate=规则推算 / first_seen=我们首次读到
        "granted_src": rec.get("granted_src").cloned().unwrap_or(Value::Null),
        "granted_est": rec.get("granted_est").cloned().unwrap_or(Value::Bool(false)),
        "expired": true,
        "from_ledger": true,
        "last_seen": rec.get("last_seen").cloned().unwrap_or(Value::Null),
    });
    if it["granted_on"].as_str().unwrap_or("").is_empty() && rec["first_seen"].is_string() {
        it["granted_on"] = rec["first_seen"].clone();
        it["granted_src"] = Value::from("first_seen");
    }
    if let Some(rid) = rec.get("resource_id").filter(|v| v.as_str().map_or(false, |s| !s.is_empty())) {
        it["resource_id"] = rid.clone();
    }
    it
}

/// 台账回填：把**这次读到的**逐笔明细记进台账。
///
/// 厂商一旦把某一笔收起来（ZCode 套餐结束连 balance 桶都不返回、Qoder 到期即下架），
/// 就再也读不回来了。这次读到时就得记下来——只"补"不"记"的话，台账永远是空的，
/// 过期那一栏也就永远补不出东西（2026-11-16 实测：Qoder 103 笔全靠这条进了台账）。
///
/// 幂等：已经记过的一律跳过（不改 last_seen，免得把"最后见到"刷成今天）。
pub fn backfill(app: &str, account: &str, groups: &Value) {
    if app.is_empty() || account.is_empty() {
        return;
    }
    let Some(gs) = groups.as_array() else { return };
    if gs.is_empty() {
        return;
    }
    let Ok(_lock) = transaction_lock() else { return };
    let mut packs = load();
    let mut added = false;
    let stamp = Local::now().naive_local().format("%Y-%m-%d %H:%M:%S").to_string();
    for g in gs {
        let gname = g.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
        if gname.is_empty() {
            continue;
        }
        for it in g.get("items").and_then(|v| v.as_array()).cloned().unwrap_or_default() {
            // 全 0 的占位条目（官网偶尔给一条 total/left/used 都是 0 的「周期刷新」）
            // 记进来只会在「已过期」里多一行没意义的 0，直接不记。
            let t = num(it.get("total")).unwrap_or(0.0);
            let l = num(it.get("left")).unwrap_or(0.0);
            let u = num(it.get("used")).unwrap_or(0.0);
            if it["from_ledger"] == true || it["aggregate"] == true || it["spec"] == true
                || (t <= 0.0 && l <= 0.0 && u <= 0.0 && num(it.get("voided")).unwrap_or_default() <= 0.0) {
                continue;
            }
            let key = format!("{}{}{}{}{}", app, SEP, account, SEP, ident(&it));
            if packs.contains_key(&key) {
                continue;
            }
            packs.insert(
                key,
                serde_json::json!({
                    "product": app, "account": account, "group": gname,
                    "label": it.get("label").cloned().unwrap_or(Value::Null),
                    "unit": it.get("unit").cloned().unwrap_or(Value::Null),
                    "expires_on": it.get("expires_on").cloned().unwrap_or(Value::Null),
                    "total": it.get("total").cloned().unwrap_or(Value::Null),
                    "used": it.get("used").cloned().unwrap_or(Value::Null),
                    "left": it.get("left").cloned().unwrap_or(Value::Null),
                    "granted_on": it.get("granted_on").cloned().unwrap_or(Value::Null),
                    "granted_src": it.get("granted_src").cloned().unwrap_or(Value::Null),
                    "granted_est": it["granted_est"], "expired": it["expired"], "voided": it["voided"],
                    "resource_id": it.get("resource_id").cloned().unwrap_or(Value::Null),
                    "first_seen": stamp, "last_seen": stamp, "backfilled": true,
                }),
            );
            added = true;
        }
    }
    if added {
        save(&packs);
    }
}

/// 把这次读数并进台账，并把台账里这次没返回、且已过期的笔补回 groups。
///
/// 身份认人：资源 ID 优先，没有才按 到期+发放额 保守匹配（收养）。
/// 匹配不上的旧记录（活笔过期后从带 ID 的接口转到不带 ID 的过期接口）
/// 按签名认回同一条，不另立一条、不补出重影。
pub fn merge(app: &str, account: &str, groups: &Value) -> Value {
    let now = Local::now().naive_local();
    let stamp = now.format("%Y-%m-%d %H:%M:%S").to_string();
    let Ok(_lock) = transaction_lock() else { return groups.clone() };
    let mut packs = load();
    let out = merge_at(app, account, groups, &now, &stamp, &mut packs);
    save(&packs);
    out
}

fn merge_at(app: &str, account: &str, groups: &Value, now: &NaiveDateTime, stamp: &str, packs: &mut Map<String, Value>) -> Value {
    let mine = format!("{}{}{}{}", app, SEP, account, SEP);
    // 妙手旧解析曾把「失效积分」流水当成独立积分包记进台账
    // （resource_id = expire_inv_*，没有发放额）。新版已按 invId 并回原包；
    // 这些幽灵记录必须清掉，否则每次刷新都会在「已过期」里多出几条重复项。
    // 只清本产品本账号，其它 Agent 的台账不动。
    if app == "catpaw" {
        let ghosts: Vec<String> = packs
            .iter()
            .filter(|(_, rec)| {
                rec.get("product").and_then(|v| v.as_str()) == Some(app)
                    && rec.get("account").and_then(|v| v.as_str()) == Some(account)
                    && rec
                        .get("resource_id")
                        .and_then(|v| v.as_str())
                        .is_some_and(|s| s.starts_with("expire_inv_"))
            })
            .map(|(k, _)| k.clone())
            .collect();
        for k in ghosts {
            packs.remove(&k);
        }
    }
    // Trae 旧组名迁移：键不再含组名，只改记录上的 group 属性
    if app == "trae-cn" || app == "traework-cn" {
        for rec in packs.values_mut() {
            if rec.get("product").and_then(|v| v.as_str()) != Some(app)
                || rec.get("account").and_then(|v| v.as_str()) != Some(account)
            {
                continue;
            }
            let old = rec.get("group").and_then(|v| v.as_str()).unwrap_or("").to_string();
            if let Some(new) = crate::webquota::trae_group_alias(&old) {
                rec["group"] = serde_json::json!(new);
            }
        }
    }
    // 本产品的记录按签名建索引，供收养匹配用
    let mut by_sig: HashMap<String, Vec<String>> = HashMap::new();
    // 二级索引：到账时刻（截分钟）+ 总额。到期日口径变过时签名对不上，靠它认回同一笔
    let mut by_grant: HashMap<(String, String), Vec<String>> = HashMap::new();
    for (k, r) in packs.iter() {
        if k.starts_with(&mine) {
            by_sig.entry(sig(r)).or_default().push(k.clone());
            let g: String = r.get("granted_on").and_then(|v| v.as_str()).unwrap_or("")
                .chars().take(16).collect();
            if !g.is_empty() {
                let total = num(r.get("total"))
                    .map(|v| format!("{:.6}", (v * 1e6).round() / 1e6))
                    .unwrap_or_default();
                by_grant.entry((g, total)).or_default().push(k.clone());
            }
        }
    }
    let mut out: Vec<Value> = vec![];
    let mut seen_groups: Vec<String> = vec![];
    let empty: Vec<Value> = vec![];

    // 第一阶段：**所有组**的返回笔先全量并进台账（matched 是全局的）。
    // 按组"边匹配边补回"会让旧分类的记录先被补回、又被新分类收养，
    // 同一笔在两个档各出现一次（2026-11-18 真机 92/90 的根因）。
    let mut matched: Vec<String> = vec![];
    let incoming_ids: std::collections::HashSet<&str> = groups.as_array().into_iter().flatten()
        .flat_map(|g| g["items"].as_array().into_iter().flatten())
        .filter_map(|it| it["resource_id"].as_str()).collect();
    let mut prepared: Vec<(Value, String, Vec<Value>)> = vec![];
    for g in groups.as_array().unwrap_or(&empty) {
        let mut g = g.clone();
        let gname = g.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
        seen_groups.push(gname.clone());
        let mut items: Vec<Value> = g
            .get("items")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        for it in items.iter_mut() {
            let rid = it.get("resource_id").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
            let exact = format!("{}{}", mine, ident(it));
            let key = if packs.contains_key(&exact) && !matched.contains(&exact) {
                exact.clone()
            } else {
                // 收养：同签名的未匹配记录，无 ID 的优先、同组的优先、最近见到的优先
                let mut cands: Vec<String> = by_sig
                    .get(&sig(it))
                    .map(|ks| {
                        ks.iter()
                            .filter(|k| packs.contains_key(*k) && !matched.contains(*k) && **k != exact)
                            .cloned()
                            .collect()
                    })
                    .unwrap_or_default();
                if cands.is_empty() {
                    // 二级收养：到期日口径变过（周期末→真实过期日、有→无）时按 到账+总额 认
                    let g: String = it.get("granted_on").and_then(|v| v.as_str()).unwrap_or("")
                        .chars().take(16).collect();
                    if !g.is_empty() {
                        let total = num(it.get("total"))
                            .map(|v| format!("{:.6}", (v * 1e6).round() / 1e6))
                            .unwrap_or_default();
                        cands = by_grant
                            .get(&(g, total))
                            .map(|ks| {
                                ks.iter()
                                    .filter(|k| packs.contains_key(*k) && !matched.contains(*k) && **k != exact)
                                    .cloned()
                                    .collect()
                            })
                            .unwrap_or_default();
                    }
                }
                cands.retain(|k| {
                    let old_id = packs[k]["resource_id"].as_str().unwrap_or("");
                    old_id.is_empty() || (rid.is_empty() && !incoming_ids.contains(old_id))
                });
                cands.sort_by(|a, b| {
                    let ra = packs.get(a).unwrap();
                    let rb = packs.get(b).unwrap();
                    let la = ra.get("last_seen").and_then(|v| v.as_str()).unwrap_or("");
                    let lb = rb.get("last_seen").and_then(|v| v.as_str()).unwrap_or("");
                    lb.cmp(la)
                });
                cands.sort_by_key(|k| {
                    let r = packs.get(k).unwrap();
                    (
                        r.get("resource_id").and_then(|v| v.as_str()).map_or(false, |s| !s.is_empty()),
                        r.get("group").and_then(|v| v.as_str()).unwrap_or("") != gname,
                    )
                });
                match cands.first() {
                    Some(old) if !rid.is_empty() => {
                        // 这笔带 ID：把旧记录搬到 ID 键下（历史跟着搬）
                        let rec = packs.remove(old).unwrap();
                        packs.insert(exact.clone(), rec);
                        exact.clone()
                    }
                    Some(old) => old.clone(),
                    None if rid.is_empty() && matched.contains(&exact) => {
                        (2..).map(|n| format!("{exact}{SEP}{n}")).find(|k| !packs.contains_key(k)).unwrap()
                    }
                    None => exact.clone(),
                }
            };
            let mut rec = packs
                .get(&key)
                .and_then(|v| v.as_object())
                .cloned()
                .unwrap_or_default();
            // 「已用」取**最近一次读数**，不是历史最大值（max(旧,新) 会把
            // 任何一轮的小读数永久冻住）。读数变粗由 local_quota::stored_rank 挡。
            match num(it.get("used")) {
                Some(u) => {
                    rec.insert("used".into(), serde_json::json!(u));
                }
                None => {
                    if let Some(v) = rec.get("used").cloned() {
                        it["used"] = v;
                    }
                }
            }
            match num(it.get("left")) {
                Some(l) => {
                    rec.insert("left".into(), serde_json::json!(l));
                }
                None => {
                    if let Some(v) = rec.get("left").cloned() {
                        it["left"] = v;
                    }
                }
            }
            if it.get("total").map_or(true, |v| v.is_null()) {
                if let Some(v) = rec.get("total").cloned() {
                    it["total"] = v;
                }
            }
            if !rid.is_empty() {
                rec.insert("resource_id".into(), serde_json::json!(rid));
            } else if let Some(v) = rec.get("resource_id").filter(|v| v.as_str().map_or(false, |s| !s.is_empty())) {
                it["resource_id"] = v.clone();
            }
            rec.insert("product".into(), serde_json::json!(app));
            rec.insert("account".into(), serde_json::json!(account));
            rec.insert("group".into(), serde_json::json!(gname));
            rec.insert("label".into(), it.get("label").cloned().unwrap_or(Value::Null));
            rec.insert("unit".into(), it.get("unit").cloned().unwrap_or(Value::Null));
            rec.insert(
                "expires_on".into(),
                it.get("expires_on").cloned().unwrap_or(Value::Null),
            );
            rec.insert(
                "total".into(),
                it.get("total").cloned().unwrap_or(Value::Null),
            );
            rec.insert("last_seen".into(), serde_json::json!(stamp));
            rec.entry("first_seen").or_insert(serde_json::json!(stamp));
            rec.insert("expired".into(), Value::from(it["expired"] == true));
            if !it["voided"].is_null() { rec.insert("voided".into(), it["voided"].clone()); }
            let rank = |src: &str| match src { "first_seen" => 0, "estimate" => 1, "effective" => 2, _ => 3 };
            let source = it["granted_src"].as_str().filter(|s| !s.is_empty()).unwrap_or(
                if it["granted_est"] == true { "estimate" } else { "vendor" }).to_owned();
            let stored = rec.get("granted_src").and_then(Value::as_str).filter(|s| !s.is_empty()).unwrap_or(
                if rec.get("granted_est").and_then(Value::as_bool) == Some(true) { "estimate" } else { "vendor" });
            let old_date = rec.get("granted_on").filter(|v| v.as_str().is_some_and(|s| !s.is_empty())).cloned();
            if let Some(v) = it.get("granted_on").filter(|v| v.as_str().is_some_and(|s| !s.is_empty()) && (old_date.is_none() || rank(&source) >= rank(stored))) {
                rec.insert("granted_on".into(), v.clone());
                rec.insert("granted_src".into(), Value::from(source.clone()));
                rec.insert("granted_est".into(), Value::from(source == "estimate"));
                it["granted_src"] = Value::from(source);
                it["granted_est"] = rec["granted_est"].clone();
            } else if let Some(v) = old_date {
                it["granted_on"] = v;
                if let Some(src) = rec.get("granted_src").cloned() {
                    it["granted_src"] = src;
                }
                if let Some(est) = rec.get("granted_est").cloned() {
                    it["granted_est"] = est;
                }
            } else {
                // 厂商不给到账时间：用台账第一次见到它的时刻兜底，标 first_seen，
                // 界面写「首次记录」——不能标成「约某日到账」冒充发放时刻。
                if let Some(fs) = rec.get("first_seen").and_then(|v| v.as_str()) {
                    if !fs.is_empty() {
                        it["granted_on"] = serde_json::json!(fs);
                        it["granted_src"] = serde_json::json!("first_seen");
                        rec.insert("granted_on".into(), serde_json::json!(fs));
                        rec.insert("granted_src".into(), serde_json::json!("first_seen"));
                    }
                }
            }
            packs.insert(key.clone(), Value::Object(rec.clone()));
            matched.push(key.clone());
        }
        prepared.push((g, gname, items));
    }

    // 第二阶段：逐组补回——只补全局都没匹配上的记录，按记录**当前**的 group
    // 属性归组（第一阶段已把收养到的记录改成新分类）。
    for (mut g, gname, mut items) in prepared {
        // 补回：台账里这一档、这次没给、且确实已经过期的笔。
        // 身份匹配已经把"这次返回了的"都认走了，剩下的未匹配记录就是真正消失的笔。
        let keys: Vec<String> = packs.keys().cloned().collect();
        for key in keys {
            if !key.starts_with(&mine) || matched.contains(&key) {
                continue;
            }
            let rec = packs.get(&key).unwrap();
            if rec.get("group").and_then(|v| v.as_str()).unwrap_or("") != gname {
                continue;
            }
            let exp = rec.get("expires_on").and_then(|v| v.as_str()).unwrap_or("");
            if rec["expired"] != true && !exp_past(exp, &now) {
                continue;
            }
            if num(rec.get("total")).unwrap_or(0.0) <= 0.0
                && num(rec.get("left")).unwrap_or(0.0) <= 0.0
                && num(rec.get("used")).unwrap_or(0.0) <= 0.0
                && num(rec.get("voided")).unwrap_or(0.0) <= 0.0
            {
                continue; // 全 0 的占位条目不值得占一行
            }
            items.push(item_of(rec, &gname, g.get("unit").and_then(|v| v.as_str())));
        }
        for it in items.iter_mut() {
            it["expired"] = Value::from(it["expired"] == true || exp_past(it["expires_on"].as_str().unwrap_or(""), &now));
        }
        // 组层汇总按效期内的笔重算，过期笔的额度已经作废，不能继续算在余额里
        let total_n = items.len();
        let live_count;
        {
            let live: Vec<&Value> = items
                .iter()
                .filter(|i| !i.get("expired").and_then(|v| v.as_bool()).unwrap_or(false))
                .collect();
            live_count = live.len();
            if live.is_empty() {
                // 整档全部过期：可用余额必须是 0（厂商档级汇总里的剩余已作废）
                g["left"] = serde_json::json!(0);
                g["live_left"] = serde_json::json!(0);
            }
            let full = !live.is_empty()
                && live
                    .iter()
                    .all(|i| num(i.get("total")).is_some() && num(i.get("left")).is_some());
            if full {
                let t: f64 = live.iter().filter_map(|i| num(i.get("total"))).sum();
                let l: f64 = live.iter().filter_map(|i| num(i.get("left"))).sum();
                g["total"] = serde_json::json!((t * 1e4).round() / 1e4);
                g["left"] = serde_json::json!((l * 1e4).round() / 1e4);
                g["used"] = serde_json::json!(((t - l).max(0.0) * 1e4).round() / 1e4);
                g["used_known"] = serde_json::json!(true);
            }
        }
        g["items"] = Value::Array(items);
        g["live_count"] = serde_json::json!(live_count);
        g["expired_count"] = serde_json::json!(total_n - live_count);
        out.push(g);
    }

    // 整档都没了的（官网把收起来的空桶连名字都不给）：单独立一档「已全部过期」
    let mut gone: HashMap<String, Vec<Value>> = HashMap::new();
    for (key, rec) in packs.iter() {
        if !key.starts_with(&mine) {
            continue;
        }
        let gname = rec.get("group").and_then(|v| v.as_str()).unwrap_or("").to_string();
        if gname.is_empty() || seen_groups.contains(&gname) {
            continue;
        }
        let exp = rec.get("expires_on").and_then(|v| v.as_str()).unwrap_or("");
        if rec["expired"] != true && !exp_past(exp, &now) {
            continue;
        }
        if num(rec.get("total")).unwrap_or(0.0) <= 0.0
            && num(rec.get("left")).unwrap_or(0.0) <= 0.0
            && num(rec.get("used")).unwrap_or(0.0) <= 0.0
            && num(rec.get("voided")).unwrap_or(0.0) <= 0.0
        {
            continue;
        }
        gone.entry(gname.clone()).or_default().push(item_of(rec, &gname, None));
    }
    for (gname, mut items) in gone {
        items.sort_by(|a, b| {
            b["expires_on"]
                .as_str()
                .unwrap_or("")
                .cmp(a["expires_on"].as_str().unwrap_or(""))
        });
        let n = items.len();
        let unit = items
            .first()
            .and_then(|i| i.get("unit"))
            .and_then(|v| v.as_str())
            .unwrap_or("credits")
            .to_string();
        out.push(serde_json::json!({
            "name": gname, "unit": unit, "total": 0, "left": 0, "used": 0,
            "expires": EXPIRY_UNKNOWN, "days_left": Value::Null,
            "live_count": 0, "expired_count": n, "usable_count": 0, "live_left": 0,
            "items": items, "all_expired": true,
            "note": "这一档的额度已经全部过期；厂商接口不再返回它，记录来自台账",
        }));
    }

    Value::Array(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 没给到期日的笔不许补回来：补回来就是永远不消失的幽灵。
    #[test]
    fn pack_without_expiry_is_never_backfilled() {
        let now = Local::now().naive_local();
        assert!(!exp_past(EXPIRY_UNKNOWN, &now));
        assert!(!exp_past("", &now));
        assert!(!exp_past("无限期", &now));
        assert!(!exp_past("—", &now));
        assert!(exp_past("2020-01-01 00:00", &now));
        assert!(exp_past("2020-01-01", &now));
    }

    /// 身份只认不可变字段：改名、改组、已用变化都不许换键；
    /// 资源 ID 不同的两笔即使同额同到期也是两个键。
    #[test]
    fn ident_is_stable_under_rename_and_usage_change() {
        let a = json!({"label": "签到奖励", "expires_on": "2026-11-17 15:12",
                       "total": 100.0, "used": 39.078});
        let b = json!({"label": "每日签到", "expires_on": "2026-11-17 15:12",
                       "total": 100.0, "used": 50.0});
        assert_eq!(ident(&a), ident(&b), "改名/已用变化不该换键");
        // 到期精度归一：分钟与秒级字符串认同一笔
        let c = json!({"expires_on": "2026-11-17 15:12:59", "total": 100.0});
        assert_eq!(ident(&a), ident(&c));
        // 资源 ID 优先
        let d = json!({"resource_id": "res-1", "expires_on": "2026-11-17 15:12", "total": 100.0});
        let e = json!({"resource_id": "res-2", "expires_on": "2026-11-17 15:12", "total": 100.0});
        assert_ne!(ident(&d), ident(&e), "不同资源 ID 是同额同到期也是两笔");
        assert_ne!(ident(&d), ident(&a));
        assert!(ident(&d).starts_with("r"));
        assert!(ident(&a).starts_with("s"));
    }

    /// 旧键迁移：同一笔的多个旧快照（已用不同）合并成一条，
    /// 取最近读数、最早 first_seen；新键不受影响。
    #[test]
    fn migrate_collapses_old_snapshots() {
        let mut packs = Map::new();
        let k1 = format!("doubao{0}acct{0}平台奖励积分{0}2026-11-16 23:59{0}100.000000{0}0.000000", SEP);
        let k2 = format!("doubao{0}acct{0}平台奖励积分{0}2026-11-16 23:59{0}100.000000{0}300.000000", SEP);
        packs.insert(k1, json!({"product": "doubao", "account": "acct", "group": "平台奖励积分",
            "expires_on": "2026-11-16 23:59", "total": 100.0, "used": 0.0, "left": 100.0,
            "first_seen": "2026-11-10 09:00", "last_seen": "2026-11-15 09:00"}));
        packs.insert(k2, json!({"product": "doubao", "account": "acct", "group": "平台奖励积分",
            "expires_on": "2026-11-16 23:59", "total": 100.0, "used": 300.0, "left": 700.0,
            "first_seen": "2026-11-15 09:00", "last_seen": "2026-11-16 09:00"}));
        assert!(migrate(&mut packs));
        assert_eq!(packs.len(), 1);
        let rec = packs.values().next().unwrap();
        assert_eq!(rec["used"], json!(300.0));
        assert_eq!(rec["first_seen"], json!("2026-11-10 09:00"));
        // 再跑一次不变（幂等）
        assert!(!migrate(&mut packs));
        assert_eq!(packs.len(), 1);
    }

    /// 补回的笔带台账标记与来源标注，资源 ID 跟着带回来（下一轮靠它认人）。
    #[test]
    fn ledger_item_carries_identity_and_grant_source() {
        let rec = json!({"label": "平台赠送", "total": 100.0, "used": 40.0, "left": 60.0,
            "expires_on": "2026-11-01 00:00", "resource_id": "res-9",
            "granted_on": "2026-10-01 08:00", "granted_src": "first_seen",
            "last_seen": "2026-11-02 09:00"});
        let it = item_of(&rec, "平台奖励积分", Some("credits"));
        assert_eq!(it["expired"], json!(true));
        assert_eq!(it["from_ledger"], json!(true));
        assert_eq!(it["resource_id"], json!("res-9"));
        assert_eq!(it["granted_src"], json!("first_seen"));
        assert_eq!(it["used"], json!(40.0));
    }

    #[test]
    fn refresh_through_expiry_preserves_distinct_packs_and_official_dates() {
        let mut ledger = Map::new();
        let now = crate::quota::expiry_time("2026-11-18 08:00:01").unwrap();
        let apply = |rows: Value, ledger: &mut Map<String, Value>| merge_at("qoder", "acct",
            &json!([{"name":"积分","items":rows}]), &now, "2026-11-18 08:00:01", ledger);
        let mut row = json!({"resource_id":"A","total":100,"left":80,"expires_on":"2026-11-18 07:59:59","granted_on":null});
        let first = apply(json!([row]), &mut ledger);
        assert_eq!(first[0]["items"][0]["granted_src"], "first_seen");
        row["granted_on"] = json!("2026-10-18 10:00:04");
        apply(json!([row]), &mut ledger);
        row["resource_id"] = json!("B");
        let after = apply(json!([row]), &mut ledger);
        assert_eq!(after[0]["expired_count"], 2);
        assert_eq!(after[0]["left"], 0);
        assert_eq!(after[0]["items"][1]["granted_src"], "vendor");
        let official = json!({"resource_id":"C","voided":12,"expired":true});
        apply(json!([official]), &mut ledger);
        let gone = apply(json!([]), &mut ledger);
        assert_eq!(gone[0]["expired_count"], 3);
        assert!(gone[0]["items"].as_array().unwrap().iter().any(|i| i["voided"] == 12));
    }

    /// 妙手旧版把「失效积分」流水当成独立积分包写进台账
    /// （resource_id = expire_inv_*）。新解析已按 invId 并回原包，
    /// 这些幽灵记录必须在合并时清掉，否则每次刷新都会多出重复的已过期项。
    #[test]
    fn catpaw_legacy_expire_pseudo_packs_are_pruned_on_merge() {
        let mut ledger = Map::new();
        let now = crate::quota::expiry_time("2026-11-18 08:00:01").unwrap();
        ledger.insert(
            format!("catpaw{SEP}acct{SEP}r{SEP}expire_inv_1250371"),
            json!({
                "product": "catpaw", "account": "acct", "group": "平台奖励积分",
                "resource_id": "expire_inv_1250371", "expires_on": "2026-11-01 00:09:07",
                "total": null, "expired": true, "last_seen": "2026-11-18 17:28:00"
            }),
        );
        // 别的产品的同前缀记录不许被误删
        ledger.insert(
            format!("qoder{SEP}acct{SEP}r{SEP}expire_inv_keep"),
            json!({
                "product": "qoder", "account": "acct", "group": "平台奖励积分",
                "resource_id": "expire_inv_keep", "expires_on": "2026-11-01 00:00",
                "total": 10.0, "expired": true, "last_seen": "2026-11-18 17:28:00"
            }),
        );
        let incoming = json!([{
            "name": "套餐基础积分",
            "items": [{
                "resource_id": "F2026114138865630", "label": "赠送基础积分",
                "total": 200.0, "left": null, "used": null,
                "expires_on": "2026-11-30 23:59:59", "granted_on": "2026-11-01 00:00:30",
                "expired": false
            }]
        }]);
        let out = merge_at("catpaw", "acct", &incoming, &now, "2026-11-18 18:30:00", &mut ledger);
        let items = out[0]["items"].as_array().unwrap();
        assert!(
            !items.iter().any(|i| i["resource_id"] == "expire_inv_1250371"),
            "幽灵失效包不能补回：{items:?}"
        );
        assert!(!ledger.keys().any(|k| k.contains("expire_inv_1250371")));
        assert!(ledger.keys().any(|k| k.contains("expire_inv_keep")), "别的产品不许误删");
    }
}
