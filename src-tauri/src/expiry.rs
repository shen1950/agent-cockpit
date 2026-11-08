//! 积分包的自定义覆盖（改名 / 改截止日 / 备注 / 手工新增一个包）。
//!
//! 为什么要有：厂商给的档名不一定是人认得的（Qoder 官网把一类资源包统称「其他」），
//! 也不总给到期日（活动奖励、赠送积分只回一个总数，界面上就永远写"到期时间 未给出"）。
//! 这两件事只有本人知道，所以给一层本机覆盖：
//!   ① 改档名：显示名换成人认得的（「其他」→「SOTA专属积分」），厂商那份读数一个字节不动；
//!   ② 补截止日：只补"厂商没给"的位置，读到真日期的不覆盖（厂商口径比人记的准）；
//!   ③ 手工加一个包：厂商压根不报的那份额度，自己登记名字、总额、剩余、截止日。
//!
//! 一律标出来（`expires_manual` / `renamed` / `custom`），界面才分得清哪些是自己填的。
//! 到期提醒走的是同一份 groups（见 state.rs），所以补完就参与临期提醒，不用另开通道。
//!
//! 落盘 `~/.agent-cockpit/expiry.json`：
//! ```json
//! { "qoder": { "SOTA": {
//!     "groups": { "g:其他": {"label": "SOTA专属积分", "date": "2026-11-08"} },
//!     "custom": [ {"id": "c1", "label": "手工包", "total": 2000, "left": 2000,
//!                  "date": "2026-12-31", "note": ""} ]
//! } } }
//! ```
//! 老格式（`{"档名": "2026-12-31"}` 扁平日期）照样认，读到就按"只有日期"理解。
use chrono::{Datelike, NaiveDate, NaiveDateTime};
use serde_json::{json, Map, Value};
use std::fs;
use std::path::PathBuf;

fn path() -> PathBuf {
    crate::registry::base_dir().join("expiry.json")
}

#[cfg(test)]
static PATH_OVERRIDE: std::sync::Mutex<Option<PathBuf>> = std::sync::Mutex::new(None);

/// 测试专用：把落盘位置指到临时目录（和 storage::set_shared_override 同一套路），
/// 绝不让单测写进用户真正的 expiry.json
#[cfg(test)]
pub fn set_path_override(p: Option<PathBuf>) {
    *PATH_OVERRIDE.lock().unwrap_or_else(|p| p.into_inner()) = p;
}

fn store_path() -> PathBuf {
    #[cfg(test)]
    {
        if let Some(p) = PATH_OVERRIDE.lock().unwrap_or_else(|p| p.into_inner()).clone() {
            return p;
        }
    }
    path()
}

fn load() -> Value {
    fs::read_to_string(store_path())
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .filter(|v| v.is_object())
        .unwrap_or_else(|| json!({}))
}

fn save(v: &Value) -> Result<(), String> {
    let p = store_path();
    if let Some(dir) = p.parent() {
        let _ = fs::create_dir_all(dir);
    }
    let body = serde_json::to_string_pretty(v).map_err(|e| e.to_string())?;
    let tmp = p.with_extension("json.tmp");
    fs::write(&tmp, body).map_err(|e| format!("没存上：{e}"))?;
    fs::rename(&tmp, &p).map_err(|e| format!("没存上：{e}"))
}

fn norm_key(k: &str) -> String {
    k.trim().to_string()
}

/// 一条覆盖：改显示名 / 补截止日 / 记一句备注，三样都可以缺。
#[derive(Clone, Default, Debug, PartialEq)]
pub struct Entry {
    pub label: Option<String>,
    pub date: Option<String>,
    pub note: Option<String>,
}

impl Entry {
    /// 老格式是裸字符串（只有日期），新格式是对象
    fn from_value(v: &Value) -> Entry {
        match v {
            Value::String(s) => Entry { label: None, date: pick(s), note: None },
            Value::Object(o) => Entry {
                label: pick(o.get("label").and_then(|x| x.as_str()).unwrap_or("")),
                date: pick(o.get("date").and_then(|x| x.as_str()).unwrap_or("")),
                note: pick(o.get("note").and_then(|x| x.as_str()).unwrap_or("")),
            },
            _ => Entry::default(),
        }
    }

    fn to_value(&self) -> Value {
        // 只有日期就写回裸字符串：文件好读，也和早先那版兼容
        if self.label.is_none() && self.note.is_none() {
            return json!(self.date.clone().unwrap_or_default());
        }
        let mut o = Map::new();
        if let Some(s) = &self.label {
            o.insert("label".into(), json!(s));
        }
        if let Some(s) = &self.date {
            o.insert("date".into(), json!(s));
        }
        if let Some(s) = &self.note {
            o.insert("note".into(), json!(s));
        }
        Value::Object(o)
    }

    fn is_empty(&self) -> bool {
        self.label.is_none() && self.date.is_none() && self.note.is_none()
    }
}

fn pick(s: &str) -> Option<String> {
    let t = s.trim();
    if t.is_empty() { None } else { Some(t.to_string()) }
}

/// 一个账号名下全部覆盖：`groups`（键是 `g:<厂商档名>` 或 `i:<厂商档名>:<笔名>`，
/// 老数据是裸档名）+ `custom`（手工加的包）。
pub struct Account {
    pub groups: Map<String, Value>,
    pub custom: Vec<Value>,
}

impl Account {
    fn read(&self, keys: &[&str]) -> Option<Entry> {
        for k in keys {
            // 裸档名也认：老文件就是「档名 → 日期」；新文件把它存在 `g:<档名>` 下，
            // 所以查「其他」时也要能落到 `g:其他`，否则两种写法各查一半、总有一边读不到
            let mut tries = vec![k.to_string()];
            if !k.starts_with("g:") && !k.starts_with("i:") {
                tries.push(format!("g:{k}"));
            }
            for key in tries {
                if let Some(v) = self.groups.get(&key) {
                    let e = Entry::from_value(v);
                    if !e.is_empty() {
                        return Some(e);
                    }
                }
            }
        }
        None
    }
}

fn account_of(all: &Value, app: &str, name: &str) -> Account {
    let node = all.get(app).and_then(|a| a.get(name)).cloned().unwrap_or(Value::Null);
    let mut groups = Map::new();
    let mut custom = Vec::new();
    if let Some(o) = node.as_object() {
        if let Some(g) = o.get("groups").and_then(|v| v.as_object()) {
            groups = g.clone();
        }
        if let Some(c) = o.get("custom").and_then(|v| v.as_array()) {
            custom = c.clone();
        }
        // 老格式：账号这一层直接就是「档名 → 日期」
        for (k, v) in o {
            if k == "groups" || k == "custom" {
                continue;
            }
            if matches!(v, Value::String(_) | Value::Object(_)) {
                groups.entry(k.clone()).or_insert_with(|| v.clone());
            }
        }
    }
    Account { groups, custom }
}

fn write_account(all: &mut Value, app: &str, name: &str, acct: &Account) -> Result<(), String> {
    let root = all.as_object_mut().ok_or("覆盖记录损坏")?;
    let app_row = root.entry(app.to_string()).or_insert_with(|| json!({}));
    let slot = app_row
        .as_object_mut()
        .ok_or("覆盖记录损坏")?
        .entry(name.to_string())
        .or_insert_with(|| json!({}));
    let o = slot.as_object_mut().ok_or("覆盖记录损坏")?;
    o.insert("groups".into(), Value::Object(acct.groups.clone()));
    if acct.custom.is_empty() {
        o.remove("custom");
    } else {
        o.insert("custom".into(), Value::Array(acct.custom.clone()));
    }
    Ok(())
}

/// 一次状态快照里要问很多次（每张卡 × 每一档 × 每一笔），所以读一次盘、之后只在内存里查：
/// 4 秒一轮的界面刷新不该把同一个 json 反复读几十遍。
pub struct Book(Value);

impl Book {
    pub fn load() -> Book {
        Book(load())
    }

    pub fn account(&self, app: &str, name: &str) -> Account {
        account_of(&self.0, app, name)
    }

    /// 这一笔（或这一档）自己填的到期日：先按逐笔键，再按档名键，最后认老格式的裸档名
    pub fn for_portion(
        &self, app: &str, name: &str, label: &str, category: &str, pid: &str,
    ) -> Option<String> {
        let acct = self.account(app, name);
        let pid = pid.trim();
        if !pid.is_empty() {
            if let Some(e) = acct.read(&[&format!("i:{pid}")]) {
                return e.date;
            }
        }
        acct.read(&[
            &format!("i:{category}:{label}"),
            &format!("g:{category}"),
            label,
            category,
        ])
        .and_then(|e| e.date)
    }

    /// 这个账号这一档（或这一笔）自己填的到期日
    pub fn get(&self, app: &str, name: &str, key: &str) -> Option<String> {
        let k = norm_key(key);
        if k.is_empty() {
            return None;
        }
        self.account(app, name).read(&[&k]).and_then(|e| e.date)
    }

    /// 把覆盖铺进 groups（就地改显示名 / 补日期 / 追加手工包），每档带上 `gid` 与 `orig`，
    /// 界面据此把编辑结果写回同一格。
    pub fn apply(&self, app: &str, name: &str, groups: &mut Vec<Value>) {
        let acct = self.account(app, name);
        for g in groups.iter_mut() {
            let orig = g["name"].as_str().unwrap_or("").trim().to_string();
            let gid = format!("g:{orig}");
            g["orig"] = json!(orig);
            g["gid"] = json!(gid);
            let ov = acct.read(&[&gid, &orig]);
            if let Some(e) = ov.as_ref() {
                if let Some(l) = &e.label {
                    g["name"] = json!(l);
                    g["renamed"] = json!(true);
                }
                if let Some(n) = &e.note {
                    g["note"] = json!(n);
                }
            }
            let gdate = ov.as_ref().and_then(|e| e.date.clone());
            let mut manual = false;
            if let Some(items) = g["items"].as_array_mut() {
                for it in items.iter_mut() {
                    // 逐笔的身份键：同一档里 30 多笔常常全叫「平台赠送」，用名字当键会把
                    // 改名/改到期改到别人头上。所以优先用厂商给的 pid，没有才退回「档名:笔名」。
                    let pid = it["pid"].as_str().unwrap_or("").trim().to_string();
                    let label = it["label"].as_str().unwrap_or("").trim().to_string();
                    let orig_label = label.clone();
                    let keys: Vec<String> = if pid.is_empty() {
                        vec![format!("i:{orig}:{label}"), format!("i:{label}"), label.clone()]
                    } else {
                        vec![format!("i:{pid}")]
                    };
                    it["pid"] = json!(pid.clone());
                    it["orig"] = json!(orig_label.clone());
                    it["iid"] = json!(keys.first().cloned().unwrap_or_default());
                    let refs: Vec<&str> = keys.iter().map(String::as_str).collect();
                    let per = acct.read(&refs);
                    if let Some(e) = per.as_ref() {
                        // 改笔名：和档名一样，只换显示名，厂商那份一个字节不动
                        if let Some(l) = &e.label {
                            it["label"] = json!(l);
                            it["renamed"] = json!(true);
                        }
                        if let Some(n) = &e.note {
                            it["detail"] = json!(n);
                        }
                    }
                    // 先把厂商那个日期取成 owned 字符串：下面要写回 it，借用不能跨过去
                    let cur = it["expires_on"].as_str().unwrap_or("").to_string();
                    let manual_date = per.as_ref().and_then(|e| e.date.clone());
                    if has_real(&cur) {
                        // 厂商给了真日期：只有她自己显式填了这一笔才覆盖（档级日期不盖）
                        if let Some(d) = manual_date {
                            it["expires_on"] = json!(d);
                            it["expires_manual"] = json!(true);
                            it["expires_vendor"] = json!(cur);
                            manual = true;
                        }
                        continue;
                    }
                    if let Some(d) = manual_date.or(gdate.clone()) {
                        it["expires_on"] = json!(d);
                        it["expires_manual"] = json!(true);
                        manual = true;
                    }
                }
            }
            if manual {
                g["expires_manual"] = json!(true);
            }
            // 档级日期：不管有没有逐笔，都交给调用方当这一档的到期日。
            // 逐笔全都有厂商日期时它才是唯一生效的那一个——用户点「编辑」填了日期，
            // 就该照她填的显示，不能因为厂商逐笔给了日期就把这一格悄悄吃掉。
            if let Some(d) = gdate {
                g["fallback_expires"] = json!(d);
            }
        }
        // 手工加的包排在最后：它不属于厂商任何一档，混在中间会让人以为是厂商读数
        for c in acct.custom.iter() {
            let id = c["id"].as_str().unwrap_or("").to_string();
            if id.is_empty() {
                continue;
            }
            let label = c["label"].as_str().unwrap_or("").trim().to_string();
            if label.is_empty() {
                continue;
            }
            groups.push(json!({
                "name": label,
                "orig": label,
                "gid": format!("c:{id}"),
                "custom": true,
                "custom_id": id,
                "unit": "积分",
                "left": c["left"].as_f64(),
                "total": c["total"].as_f64(),
                "used": match (c["total"].as_f64(), c["left"].as_f64()) {
                    (Some(t), Some(l)) => json!((t - l).max(0.0)),
                    _ => Value::Null,
                },
                "items": [],
                "expires_manual": true,
                "fallback_expires": c["date"].as_str().unwrap_or(""),
                "note": c["note"].as_str().unwrap_or(""),
            }));
        }
    }
}

/// 日期只收 `2026-12-31` 或 `2026-12-31 23:59` 两种写法（界面上那个「选日期」回填的是前者）。
/// 返回归一化后的字符串：给了时刻就带时刻，没给就只到日。
pub fn norm_date(s: &str) -> Option<String> {
    let t = s.trim();
    if t.is_empty() {
        return None;
    }
    if let Ok(dt) = NaiveDateTime::parse_from_str(t, "%Y-%m-%d %H:%M") {
        return in_range(dt.date()).then(|| dt.format("%Y-%m-%d %H:%M").to_string());
    }
    let d = NaiveDate::parse_from_str(t, "%Y-%m-%d").ok()?;
    in_range(d).then(|| d.format("%Y-%m-%d").to_string())
}

/// 明显录错的年份挡在这里：2100 之后、2000 之前都不是"积分到期日"该有的值
fn in_range(d: NaiveDate) -> bool {
    (2000..=2100).contains(&d.year())
}

fn valid_date(s: &str) -> Option<String> {
    norm_date(s)
}

fn need(app: &str, name: &str) -> Result<(), String> {
    if app.trim().is_empty() || name.trim().is_empty() {
        return Err("要指名哪个账号，才记得住这些改动".into());
    }
    Ok(())
}

/// 写一条覆盖（档或笔）。三项都空 = 删掉这条覆盖，回到厂商原样。
pub fn upsert(
    app: &str, name: &str, key: &str, label: &str, date: &str, note: &str,
) -> Result<(), String> {
    need(app, name)?;
    let key = norm_key(key);
    if key.is_empty() {
        return Err("没指明改哪一档".into());
    }
    let entry = Entry {
        label: pick(label),
        date: match pick(date) {
            Some(d) => Some(valid_date(&d).ok_or("截止日期要写成 2026-12-31 或 2026-12-31 23:59")?),
            None => None,
        },
        note: pick(note),
    };
    let mut all = load();
    let mut acct = account_of(&all, app, name);
    if entry.is_empty() {
        acct.groups.remove(&key);
    } else {
        acct.groups.insert(key, entry.to_value());
    }
    write_account(&mut all, app, name, &acct)?;
    save(&all)
}

/// 撤掉一条覆盖
pub fn clear(app: &str, name: &str, key: &str) -> Result<bool, String> {
    let key = norm_key(key);
    let mut all = load();
    let mut acct = account_of(&all, app, name);
    let removed = acct.groups.remove(&key).is_some();
    if removed {
        write_account(&mut all, app, name, &acct)?;
        save(&all)?;
    }
    Ok(removed)
}

/// 手工加一个包 / 改一个包 / 删一个包。`id` 为空就是新增，返回它的 id。
pub fn custom(
    app: &str, name: &str, id: &str, label: &str, total: &str, left: &str, date: &str, note: &str,
    delete: bool,
) -> Result<String, String> {
    need(app, name)?;
    let mut all = load();
    let mut acct = account_of(&all, app, name);
    if delete {
        let id = norm_key(id);
        if id.is_empty() {
            return Err("没指明删哪个包".into());
        }
        acct.custom.retain(|c| c["id"].as_str() != Some(id.as_str()));
        write_account(&mut all, app, name, &acct)?;
        save(&all)?;
        return Ok(id);
    }
    let label = pick(label).ok_or("名称不能为空")?;
    let num = |s: &str, what: &str| -> Result<Option<f64>, String> {
        match pick(s) {
            None => Ok(None),
            Some(t) => t
                .replace([',', '，'], "")
                .parse::<f64>()
                .map(Some)
                .map_err(|_| format!("{what}要填数字（可留空）")),
        }
    };
    let t = num(total, "总额")?;
    let l = num(left, "剩余")?;
    if let (Some(t), Some(l)) = (t, l) {
        if l > t {
            return Err("剩余不能比总额还多".into());
        }
    }
    let d = match pick(date) {
        Some(d) => Some(valid_date(&d).ok_or("截止日期要写成 2026-12-31 或 2026-12-31 23:59")?),
        None => None,
    };
    let id = if norm_key(id).is_empty() {
        format!("c{}", chrono::Local::now().timestamp_millis())
    } else {
        norm_key(id)
    };
    let row = json!({
        "id": id, "label": label,
        "total": t, "left": l,
        "date": d, "note": pick(note),
    });
    match acct.custom.iter_mut().find(|c| c["id"].as_str() == Some(id.as_str())) {
        Some(slot) => *slot = row,
        None => acct.custom.push(row),
    }
    write_account(&mut all, app, name, &acct)?;
    save(&all)?;
    Ok(id)
}

fn has_real(date: &str) -> bool {
    crate::quota::expiry(date).1.is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 落盘位置是进程级单例，测试并发会把彼此的文件指来指去——用一把锁串起来
    static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn tmpdir(tag: &str) -> std::sync::MutexGuard<'static, ()> {
        let g = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let d = std::env::temp_dir().join(format!("cockpit-expiry-test-{tag}"));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        set_path_override(Some(d.join("expiry.json")));
        g
    }

    #[test]
    fn upsert_clear_round_trip_and_legacy_shape_still_reads() {
        let _d = tmpdir("roundtrip");
        upsert("qoder", "号1", "g:其他", "SOTA专属积分", "2026-11-08", "只喂 SOTA 模型")
            .unwrap();
        let acct = Book::load().account("qoder", "号1");
        let e = acct.read(&["g:其他"]).unwrap();
        assert_eq!(e.label.as_deref(), Some("SOTA专属积分"));
        assert_eq!(e.date.as_deref(), Some("2026-11-08"));
        assert_eq!(e.note.as_deref(), Some("只喂 SOTA 模型"));
        // 老格式（扁平日期）照样认
        assert_eq!(Book::load().get("qoder", "号1", "其他").as_deref(), Some("2026-11-08"));
        // 三项全空 = 删掉这条覆盖
        upsert("qoder", "号1", "g:其他", "", "", "").unwrap();
        assert!(Book::load().account("qoder", "号1").read(&["g:其他"]).is_none());
        assert!(!clear("qoder", "号1", "g:其他").unwrap(), "删不存在的项不算删到");
        set_path_override(None);
    }

    #[test]
    fn dates_accept_day_or_minute_and_reject_junk() {
        let _d = tmpdir("dates");
        assert_eq!(norm_date("2026-12-31").as_deref(), Some("2026-12-31"));
        assert_eq!(norm_date("2026-12-31 23:59").as_deref(), Some("2026-12-31 23:59"));
        for bad in ["2026-13-01", "12-31", "1999-01-01", "2026/12/31", ""] {
            assert!(norm_date(bad).is_none(), "{bad} 不该被当成日期");
        }
        assert!(upsert("a", "b", "g:档", "", "2026-13-01", "").is_err());
        assert!(upsert("a", "b", "", "", "2026-12-31", "").is_err(), "没指明哪一档要报错");
        set_path_override(None);
    }

    #[test]
    fn rename_and_date_land_on_the_right_group_and_items() {
        let _d = tmpdir("apply");
        upsert("qoder", "号1", "g:其他", "SOTA专属积分", "2026-11-08", "").unwrap();
        upsert("qoder", "号1", "g:套餐基础积分", "", "2026-12-31", "").unwrap();
        upsert("qoder", "号1", "i:平台奖励积分:某笔", "", "2026-11-20", "").unwrap();
        let mut groups = vec![
            json!({"name": "套餐基础积分", "items": [
                {"label": "某笔", "expires_on": "2026-11-10"},
            ]}),
            json!({"name": "平台奖励积分", "items": [
                {"label": "某笔", "expires_on": ""},
                {"label": "另一笔", "expires_on": ""},
            ]}),
            json!({"name": "其他", "items": [
                {"label": "SOTA", "expires_on": ""},
            ]}),
        ];
        Book::load().apply("qoder", "号1", &mut groups);
        // 改名只改显示名，orig 留着，界面据此写回同一格
        assert_eq!(groups[2]["name"], json!("SOTA专属积分"));
        assert_eq!(groups[2]["orig"], json!("其他"));
        assert_eq!(groups[2]["gid"], json!("g:其他"));
        // 厂商给了真日期的那一笔不许被覆盖（档级日期只补缺的那一笔）
        assert_eq!(groups[1]["items"][0]["expires_on"], json!("2026-11-20"));
        assert_eq!(groups[1]["items"][0]["expires_manual"], json!(true));
        assert_eq!(groups[1]["items"][1]["expires_on"], json!(""));
        // 只有汇总数的档：日期挂到档自己身上
        assert_eq!(groups[0]["fallback_expires"], json!("2026-12-31"));
        assert_eq!(groups[0]["items"][0]["expires_on"], json!("2026-11-10"));
        set_path_override(None);
    }

    #[test]
    fn inner_packs_with_the_same_name_are_told_apart_by_pid() {
        let _d = tmpdir("pid");
        // 同一档里三笔全叫「平台赠送」：只改 b2 的名字与日期，另外两笔一个字都不许动
        upsert("qoder", "号1", "i:b2", "SOTA 赠送 A", "2026-11-30", "SOTA 那笔").unwrap();
        let mut groups = vec![json!({"name": "平台奖励积分", "items": [
            {"label": "平台赠送", "pid": "b1", "expires_on": "2026-11-18 21:19"},
            {"label": "平台赠送", "pid": "b2", "expires_on": "2026-11-19 20:24"},
            {"label": "平台赠送", "pid": "b3", "expires_on": ""},
        ]})];
        Book::load().apply("qoder", "号1", &mut groups);
        let its = groups[0]["items"].as_array().unwrap();
        assert_eq!(its[0]["label"], json!("平台赠送"), "同名的那笔不该被改名");
        assert_eq!(its[0]["expires_on"], json!("2026-11-18 21:19"), "它的日期也不许被改");
        assert_eq!(its[1]["label"], json!("SOTA 赠送 A"));
        assert_eq!(its[1]["renamed"], json!(true));
        assert_eq!(its[1]["orig"], json!("平台赠送"), "原名要留着，好悬停看出改过什么");
        assert_eq!(its[1]["expires_on"], json!("2026-11-30"));
        assert_eq!(its[1]["expires_manual"], json!(true));
        assert_eq!(its[1]["expires_vendor"], json!("2026-11-19 20:24"), "厂商原日期留着才能改回去");
        assert_eq!(its[1]["iid"], json!("i:b2"), "界面靠这个键写回同一笔");
        assert_eq!(its[2]["expires_on"], json!(""), "没碰的那笔仍然是空（界面显示未给出）");
        set_path_override(None);
    }

    #[test]
    fn clearing_an_inner_pack_falls_back_to_the_vendor_date() {
        let _d = tmpdir("pidclear");
        upsert("qoder", "号1", "i:b2", "", "2026-11-30", "").unwrap();
        upsert("qoder", "号1", "i:b2", "", "", "").unwrap(); // 三项全空 = 撤掉这条覆盖
        let mut groups = vec![json!({"name": "平台奖励积分", "items": [
            {"label": "平台赠送", "pid": "b2", "expires_on": "2026-11-19 20:24"},
        ]})];
        Book::load().apply("qoder", "号1", &mut groups);
        assert_eq!(groups[0]["items"][0]["expires_on"], json!("2026-11-19 20:24"));
        assert!(groups[0]["items"][0].get("expires_manual").is_none(), "撤掉之后不该再标自填");
        set_path_override(None);
    }

    #[test]
    fn custom_pack_is_appended_and_counted() {
        let _d = tmpdir("custom");
        let id = custom("qoder", "号1", "", "手工包", "2000", "2000", "2026-12-31", "自己记的", false)
            .unwrap();
        assert!(id.starts_with('c'));
        let mut groups = vec![json!({"name": "套餐基础积分", "left": 0, "total": 0})];
        Book::load().apply("qoder", "号1", &mut groups);
        assert_eq!(groups.len(), 2);
        let c = &groups[1];
        assert_eq!(c["name"], json!("手工包"));
        assert_eq!(c["left"], json!(2000.0));
        assert_eq!(c["custom"], json!(true));
        assert_eq!(c["fallback_expires"], json!("2026-12-31"));
        // 改名字与数额
        custom("qoder", "号1", &id, "改名后的包", "3000", "2500", "", "", false).unwrap();
        let mut again = vec![];
        Book::load().apply("qoder", "号1", &mut again);
        assert_eq!(again[0]["name"], json!("改名后的包"));
        assert_eq!(again[0]["total"], json!(3000.0));
        // 剩余不许超过总额
        assert!(custom("qoder", "号1", &id, "x", "100", "200", "", "", false).is_err());
        // 删掉
        custom("qoder", "号1", &id, "", "", "", "", "", true).unwrap();
        let mut empty = vec![];
        Book::load().apply("qoder", "号1", &mut empty);
        assert!(empty.is_empty());
        set_path_override(None);
    }

    #[test]
    fn no_override_changes_nothing() {
        let _d = tmpdir("empty");
        let mut groups = vec![json!({"name": "积分", "items": [{"label": "x", "expires_on": ""}]})];
        Book::load().apply("nobody", "nothing", &mut groups);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0]["name"], json!("积分"));
        assert_eq!(groups[0]["items"][0]["expires_on"], json!(""));
        assert!(groups[0].get("expires_manual").is_none());
        set_path_override(None);
    }
}
