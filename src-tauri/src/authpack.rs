// 一键换号：把"某个号正登录着"存成一份文件包，要用的时候原样放回去。
//
// 包里就是产品自己的登录文件（Qoder 系是 auth.v1.dat），我们不解它也不改它——
// 那份文件本来就是 os_crypt/DPAPI 封着的，只对这台机器的这个 Windows 账户有效，
// 所以收进保管箱（secrets.rs：DPAPI + 可选主密码）之后防护等级一点没降。
//
// 为什么必须让客户端重启一次：它只在启动时读这份文件，运行中写回去会被它退出时
// 整个覆盖掉——所以顺序是"收当前号 → 结束客户端 → 放目标号 → 再拉起来"。
use crate::registry::{self, auth_pack_files};
use crate::secrets;
use serde_json::{json, Value};
use std::fs;
use std::path::PathBuf;

fn vid(app: &str, name: &str) -> String {
    format!("authpack:{}/{name}", app)
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 把档案里那几个登录文件读成一份包（base64），不写任何东西
fn read_pack(app_id: &str) -> Result<Value, String> {
    let mut files = serde_json::Map::new();
    let mut first = String::new();
    for f in auth_pack_files(app_id) {
        let p = registry::auth_pack_path(app_id, f)
            .ok_or_else(|| format!("{} 的登录包路径解析不出来", f))?;
        if first.is_empty() {
            first = p.to_string_lossy().into_owned();
        }
        let bytes = fs::read(&p).map_err(|e| format!("读 {} 失败：{e}", p.display()))?;
        if bytes.is_empty() {
            return Err(format!("{} 是空文件，这份登录态不能用", p.display()));
        }
        files.insert(f.to_string(), json!(secrets::b64_encode(&bytes)));
    }
    if files.is_empty() {
        return Err("这个产品没登记登录态文件".into());
    }
    Ok(json!({ "at": now_secs(), "dir": first, "files": files }))
}

/// 包里那份登录态**到底是谁**：能解就直接从密文里读出 user —— 这是唯一不靠猜的凭据。
/// 只有 Qoder 系（`auth.v1.dat` 自带 `user.id/name`）能这么认；其余产品解不了，
/// 返回 None，仍按调用方传进来的账号名记账。
fn pack_identity(app_id: &str, files: &serde_json::Map<String, Value>) -> Option<(String, String)> {
    if matches!(app_id, "workbuddy" | "workbuddy-intl") {
        let entry = auth_pack_files(app_id).first()?;
        let bytes = secrets::b64_decode(files.get(*entry)?.as_str()?).ok()?;
        let v: Value = serde_json::from_slice(&bytes).ok()?;
        let uid = v["account"]["uid"].as_str()?.trim();
        if uid.is_empty() { return None; }
        // uid is plaintext in the vendor envelope; nickname is encrypted and mutable.
        return Some((String::new(), uid.to_string()));
    }
    if !matches!(app_id, "qoder" | "qoder-cn") {
        return None;
    }
    let b64 = files.get("auth.v1.dat")?.as_str()?;
    let bytes = secrets::b64_decode(b64).ok()?;
    let prof = registry::profile_dir(app_id)?;
    let c = crate::qproxy::read_auth_blob_with_key(&prof, &bytes).ok()?;
    if c.user_id.trim().is_empty() {
        return None;
    }
    Some((c.user_name, c.user_id))
}

/// 收下当前登录的号。account / user_id 由调用方从登录态里现读——包本身不解密，认不出是谁。
/// **认得出就一律以包体为准**（Qoder 系能解）：调用方给的账号名可能是"被改绑污染过的卡片身份"，
/// 拿它记账就等于把 A 的登录态记到 B 名下，之后点「切换到此账号」写回去的还是原来那个号。
pub fn capture(app_id: &str, name: &str, account: &str, user_id: &str) -> Result<Value, String> {
    if app_id == crate::zcode::APP { return crate::zcode::capture(name, ""); }
    let mut pack = read_pack(app_id)?;
    let files = pack["files"]
        .as_object()
        .cloned()
        .unwrap_or_default();
    let (account, user_id) = match pack_identity(app_id, &files) {
        Some((a, u)) => {
            if !user_id.trim().is_empty() && u != user_id.trim() {
                return Err("读取期间登录账号发生变化，未保存这份登录态，请重试".into());
            }
            (if a.is_empty() { account.to_string() } else { a }, u)
        },
        None => (account.to_string(), user_id.to_string()),
    };
    let (account, user_id) = (account.as_str(), user_id.as_str());
    // 一张卡只装一个号的登录态。包里的 userId 是这张卡"是谁"的唯一硬凭据，
    // 而卡片上的 quota_user_id 会被一次误读/改绑覆盖——放过去的话，两个号会挤在同一张卡里，
    // 最后谁都被当成"存过登录态"，点切换写回去的却还是当前这个号的文件（表现成"切了没反应"）。
    if let Some(prev) = stored(app_id, name) {
        let prev_uid = prev["user_id"].as_str().unwrap_or("").trim().to_string();
        if !prev_uid.is_empty() && !user_id.trim().is_empty() && prev_uid != user_id.trim() {
            return Err(format!(
                "「{name}」这张卡里存的是「{}」的登录态，不能覆盖成「{account}」。\
                 一个号一张卡：先给它建一张新卡，再回来收这份登录态。",
                prev["account"].as_str().unwrap_or("另一个号")
            ));
        }
    }
    pack["account"] = json!(account);
    pack["user_id"] = json!(user_id);
    pack["card"] = json!(name);
    if !secrets::put(&vid(app_id, name), &pack.to_string()) {
        return Err("保管箱此刻锁着，登录态存不进去（先解锁再换号）".into());
    }
    // 落盘失败必须说出来：只在内存里存着，程序一重启就没了，换号时会报"没存过"
    secrets::flush()?;
    Ok(pack)
}

/// 这个号存过登录态没有。返回的是摘要，包体不外传给界面。
pub fn stored(app_id: &str, name: &str) -> Option<Value> {
    if app_id == crate::zcode::APP { return crate::zcode::stored(name); }
    let raw = secrets::get(&vid(app_id, name))?;
    let pack: Value = serde_json::from_str(&raw).ok()?;
    let mut files = Vec::new();
    if let Some(o) = pack["files"].as_object() {
        for (k, v) in o {
            let bytes = v.as_str().and_then(|s| secrets::b64_decode(s).ok()).unwrap_or_default();
            files.push(json!({ "name": k, "bytes": bytes.len() }));
        }
    }
    Some(json!({
        "at": pack["at"],
        "account": pack["account"].as_str().unwrap_or(""),
        "user_id": pack["user_id"].as_str().unwrap_or(""),
        "dir": pack["dir"].as_str().unwrap_or(""),
        "files": files,
    }))
}

/// 备份目录里用扁平名字存，条目带子目录或占位符时不至于要照着建一层树
fn slot_name(entry: &str) -> String {
    entry.replace('/', "+").replace('\\', "+").replace(':', "+")
}

/// 此刻档案里真正登的是谁（只对 Qoder 系这类"登录文件自带 user"的产品有效）。
/// 换号写完必须拿它核一遍：写进去的和真正登进去的不是同一个号，就是"切了没反应"。
pub fn live_identity(app_id: &str) -> Option<(String, String)> {
    let f = auth_pack_files(app_id).first()?;
    let p = registry::auth_pack_path(app_id, f)?;
    let bytes = fs::read(p).ok()?;
    let mut files = serde_json::Map::new();
    files.insert(f.to_string(), json!(secrets::b64_encode(&bytes)));
    pack_identity(app_id, &files)
}

/// 从备份候选里挑"这个卡的登录态"：先按**卡片名字**挑，其次才按卡片上绑的 uid。
///
/// 顺序不能反：这张卡出事的时候，卡片上的 `quota_user_id` 恰恰已经被顶成别人的了
/// （Qoder CN 的另一张卡曾记着仓库作者的 uid），照 uid 挑只会把别人的包再收一遍；
/// 而卡名是人自己命名的、不会被自动流程改写，它是这张卡剩下的唯一可信标识。
/// 候选已按备份目录名（含时间戳）升序排好，越靠后越新，所以从后往前找。
fn pick_backup(
    cands: &[(String, PathBuf, (String, String))],
    name: &str,
    want_uid: &str,
) -> Option<(String, PathBuf, (String, String))> {
    cands
        .iter()
        .rev()
        .find(|(_, _, (acct, _))| acct == name)
        .or_else(|| {
            cands
                .iter()
                .rev()
                .find(|(_, _, (_, uid))| !want_uid.is_empty() && uid == want_uid)
        })
        .cloned()
}

/// 从"换号前自动留的备份"里找回某个号的登录态。
///
/// 为什么需要这条路：共用一份档案的产品，两个号挤在同一份 auth.v1.dat 上，
/// 一次误收包就能把 A 的登录态顶成 B 的——A 那张卡从此"存的其实是别人"，
/// 点切换写回去还是 B，看起来就是切不动。但每次换号写回之前我们都留了备份
/// （`backups/auth/<产品>-<时间戳>/`），A 的真身还在里面，解一下就认得出来。
///
/// 只认**包体自己写着是这个号**的备份（Qoder 系能解）；解不出来的一律不猜。
pub fn recover_from_backups(app_id: &str, name: &str) -> Result<Value, String> {
    if app_id == crate::zcode::APP {
        return Err("ZCode 的登录态不走这条路".into());
    }
    let want_uid = registry::find_instance(app_id, name)
        .map(|m| m["quota_user_id"].as_str().unwrap_or("").trim().to_string())
        .unwrap_or_default();
    let dir = registry::base_dir().join("backups").join("auth");
    let mut cands: Vec<(String, PathBuf, (String, String))> = Vec::new();
    if let Ok(rd) = fs::read_dir(&dir) {
        for e in rd.filter_map(|x| x.ok()) {
            let p = e.path();
            let base = e.file_name().to_string_lossy().to_string();
            if !p.is_dir() || !base.starts_with(&format!("{app_id}-")) {
                continue;
            }
            let mut files = serde_json::Map::new();
            let mut ok = true;
            for f in auth_pack_files(app_id) {
                let slot = p.join(slot_name(f));
                match fs::read(&slot) {
                    Ok(b) if !b.is_empty() => {
                        files.insert(f.to_string(), json!(secrets::b64_encode(&b)));
                    }
                    _ => {
                        ok = false;
                        break;
                    }
                }
            }
            if !ok {
                continue;
            }
            if let Some(id) = pack_identity(app_id, &files) {
                // 目录名是 <产品>-<时间戳>，时间戳越大越新
                cands.push((base, p, id));
            }
        }
    }
    cands.sort_by(|a, b| a.0.cmp(&b.0));
    let hit = pick_backup(&cands, name, &want_uid);
    let (label, path, (account, user_id)) = match hit {
        Some(v) => v,
        None => {
            return Err(format!(
                "换号备份里没有「{name}」的登录态（{} 份备份都不是它）。\n\
                 只能去客户端里退出登录、登它一次——登进来驾驶舱会自动收下。",
                cands.len()
            ))
        }
    };
    let mut files = serde_json::Map::new();
    for f in auth_pack_files(app_id) {
        let b = fs::read(path.join(slot_name(f))).map_err(|e| format!("读 {f} 失败：{e}"))?;
        files.insert(f.to_string(), json!(secrets::b64_encode(&b)));
    }
    // 已经被别人的包占着时，先把它挪到"被覆盖前"的备份名下再写——不静默丢别人的登录态
    if let Some(prev) = stored(app_id, name) {
        let pu = prev["user_id"].as_str().unwrap_or("").trim();
        if !pu.is_empty() && pu != user_id {
            let keep = vid(app_id, &format!("{name}#被覆盖的{}", prev["account"].as_str().unwrap_or("旧号")));
            if let Some(raw) = secrets::get(&vid(app_id, name)) {
                secrets::put(&keep, &raw);
            }
        }
    }
    let pack = json!({
        "at": now_secs(),
        "dir": registry::auth_pack_path(app_id, auth_pack_files(app_id)[0])
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default(),
        "files": files,
        "account": account,
        "user_id": user_id,
        "card": name,
        "recovered_from": label,
    });
    if !secrets::put(&vid(app_id, name), &pack.to_string()) {
        return Err("保管箱此刻锁着，读回来的登录态存不进去（先解锁）".into());
    }
    secrets::flush()?;
    // 顺手把卡片上被污染的绑定纠正过来：这张卡从今往后的身份以包里写的为准，
    // 不然下一次"按 userId 认人"还会把它认成别人。
    if let Some(mut m) = registry::find_instance(app_id, name) {
        let changed = m["quota_user_id"].as_str().unwrap_or("").trim() != user_id
            || m["quota_account"].as_str().unwrap_or("") != account;
        if changed {
            m["quota_user_id"] = json!(user_id);
            m["quota_account"] = json!(account);
            registry::save_instance(app_id, &m);
        }
    }
    Ok(json!({
        "account": account, "user_id": user_id, "from": label,
        "msg": format!("已从换号备份「{label}」里找回「{account}」的登录态"),
    }))
}

/// 这个产品当前的登录文件指纹：收包时比一下，就知道存的包是不是已经旧了
pub fn current_mtime(app_id: &str) -> Option<u64> {
    let first = auth_pack_files(app_id).first()?;
    let p = registry::auth_pack_path(app_id, first)?;
    fs::metadata(p)
        .ok()?
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .ok()
}

/// 把某个号存过的登录态放回原位。先备份现在这份，再临时文件 + 改名原子覆盖，
/// 中途失败不会留下半截文件。客户端还在跑就直接拒绝——那样写回去等于白写。
pub fn restore(app_id: &str, name: &str) -> Result<Value, String> {
    if app_id == crate::zcode::APP { return crate::zcode::restore(name); }
    let raw = secrets::get(&vid(app_id, name)).ok_or(format!(
        "「{name}」还没存过登录态：先在客户端里退出登录、登它一次，\
         登进来驾驶舱会自动收下这份登录态，之后就能一键切回来了"
    ))?;
    let pack: Value = serde_json::from_str(&raw).map_err(|e| format!("存的登录包读坏了：{e}"))?;
    let app = registry::app_def(app_id).ok_or("未知 Agent")?;
    let snap = crate::procs::snapshot_fresh();
    let live = crate::procs::program_pids(&registry::exe_name(&app).to_lowercase(), &snap);
    if !live.is_empty() {
        return Err(format!(
            "{} 还在跑（{} 个进程），现在写回去会被它退出时覆盖。先结束它再切。",
            app.name,
            live.len()
        ));
    }
    let stamp = now_secs();
    let backup = registry::base_dir()
        .join("backups")
        .join("auth")
        .join(format!("{app_id}-{stamp}"));
    let mut written: Vec<(String, PathBuf)> = Vec::new();
    let mut failed: Option<String> = None;
    if let Some(o) = pack["files"].as_object() {
        for (f, b64) in o {
            let bytes = match b64.as_str().map(secrets::b64_decode) {
                Some(Ok(b)) => b,
                _ => {
                    failed = Some(format!("{f} 的包体解不开"));
                    break;
                }
            };
            let target = match registry::auth_pack_path(app_id, f) {
                Some(p) => p,
                None => {
                    failed = Some(format!("{f} 的路径解析不出来"));
                    break;
                }
            };
            if target.is_file() {
                let _ = fs::create_dir_all(&backup);
                let _ = fs::copy(&target, backup.join(slot_name(f)));
            }
            let tmp = PathBuf::from(format!("{}.cockpit-tmp", target.display()));
            if let Err(e) = fs::write(&tmp, &bytes) {
                failed = Some(format!("写临时文件失败：{e}"));
                break;
            }
            if let Err(e) = fs::rename(&tmp, &target) {
                failed = Some(format!("放回 {} 失败：{e}", target.display()));
                break;
            }
            written.push((f.clone(), target));
        }
    }
    if let Some(why) = failed {
        // 已经改写了几个文件就按备份退回去，别留半套
        for (f, target) in &written {
            let bak = backup.join(slot_name(f));
            if bak.is_file() {
                let _ = fs::copy(&bak, target);
            }
        }
        return Err(format!("换号失败：{why}（已写过的部分按备份退回原样）"));
    }
    Ok(json!({
        "written": written,
        "backup": backup.to_string_lossy(),
        "at": stamp,
        "account": pack["account"].as_str().unwrap_or(""),
        "user_id": pack["user_id"].as_str().unwrap_or(""),
    }))
}

/// 后台刚认出"现在登的是谁"，顺手把这份登录态收进保管箱——她手动登录那一次就是唯一一次，
/// 之后切回来都是一键。只在没存过、或者登录文件又写过（比上次收的更新）时才收。
/// 收包只是读文件 + 写保管箱，不打扰界面；失败也不报（她没在等这个动作，下一轮还会再试）。
pub fn capture_current(app_id: &str, account: &str, user_id: &str) {
    if app_id == crate::zcode::APP { crate::zcode::capture_current(); return; }
    if auth_pack_files(app_id).is_empty() || (account.is_empty() && user_id.is_empty()) {
        return;
    }
    let name = match card_for_login(app_id, account, user_id) {
        Some(n) => n,
        None => return,
    };
    // 这个号重新登进来了，它就不再是"已让位给别的卡"的历史卡——让位标记留着的话，
    // 界面上这张卡会被当成历史号（不算"本机登录中"），人也切不回它。
    // 这里只是改驾驶舱自己那条记录，不动任何档案文件。
    if let Some(mut m) = registry::find_instance(app_id, &name) {
        if m["released"].as_bool() == Some(true) {
            if let Some(o) = m.as_object_mut() {
                o.remove("released");
                o.remove("released_to");
            }
            registry::save_instance(app_id, &m);
        }
    }
    if let Some(p) = stored(app_id, &name) {
        // 这张卡里已经装的是**这个号**的包：只有登录文件被重写过才重新收。
        // 装的是别人的包时不能因为"文件没变"就跳过——那份包压根不是这个号的。
        let same_owner = {
            let pu = p["user_id"].as_str().unwrap_or("").trim();
            !pu.is_empty() && pu == user_id.trim()
        };
        let at = p["at"].as_u64().unwrap_or(0);
        let fresh = match current_mtime(app_id) {
            Some(m) => m <= at,
            None => true,
        };
        if same_owner && fresh {
            return;
        }
        if !same_owner {
            // 包里是别人的号：不许静默覆盖（capture 自己会拒绝），直接放弃，
            // 让界面把"这个号还没有卡"如实说出来，别把别人的登录态顶替掉
            return;
        }
    }
    let _ = capture(app_id, &name, account, user_id);
}

/// 这张卡里存的那份登录态属于谁（userId）。没存过、或者包读坏了给空串。
/// **包里的 userId 才是这张卡"是谁"的唯一硬凭据**：卡片上的 quota_user_id 会被
/// 一次误读、一次「改绑到当前登录」覆盖掉，包体却不会被顺手改。
pub fn pack_uid(app_id: &str, name: &str) -> String {
    stored(app_id, name)
        .and_then(|p| p["user_id"].as_str().map(|s| s.trim().to_string()))
        .unwrap_or_default()
}

/// Keep the original slot as a rollback copy. Rename must never orphan a login.
pub fn copy_for_rename(app_id: &str, from: &str, to: &str) -> Result<(), String> {
    if auth_pack_files(app_id).is_empty() { return Ok(()); }
    if !secrets::available() { return Err("请先解锁保管箱，改名需要保留该账号的登录态".into()); }
    let Some(raw) = secrets::get(&vid(app_id, from)) else { return Ok(()); };
    let mut pack: Value = serde_json::from_str(&raw).map_err(|_| "登录包损坏，未改名")?;
    if let Some(other) = stored(app_id, to) {
        if other["user_id"] != pack["user_id"] {
            return Err("新名字已有另一个账号的登录态，未改名".into());
        }
    }
    pack["card"] = json!(to);
    if !secrets::put(&vid(app_id, to), &pack.to_string()) { return Err("登录态保存失败，未改名".into()); }
    secrets::flush()
}

/// 这份登录态该收进哪张卡：
/// ① 已经存过这个号登录态的那张卡（按包里的 userId 认，最可靠）；
/// ② 名字／绑定账号就是这个号、且这张卡没存着别人的包；
/// ③ 卡片上绑的 userId 就是这个号、且没存着别人的包。
///
/// **让位过的卡（released）一律不选**：它的包有意留成上一个号，正是为了「切回去」能用；
/// 拿它当当前号去收包、去标「本机登录中」，就是 2026-11-03 看到的那一幕——两张共用一份
/// 档案的卡同时被标成"正在登录"，而其中一张其实早换了人。
/// 都对不上说明这是个没建过卡的号，不替她瞎建。
pub fn card_for_login(app_id: &str, account: &str, user_id: &str) -> Option<String> {
    let insts = registry::load_instances(app_id);
    let live = user_id.trim();
    let name_of = |m: &Value| m["name"].as_str().unwrap_or("").to_string();
    let released = |m: &Value| m["released"].as_bool() == Some(true);
    let holds_other = |m: &Value| {
        let u = pack_uid(app_id, &name_of(m));
        !u.is_empty() && !live.is_empty() && u != live
    };
    // 未让位的卡优先：让位卡只在"包确实是这个号"时才被认领回来
    // （它重新登进来了，capture_current 会顺手把让位标记摘掉）。
    let ordered: Vec<&Value> = insts
        .iter()
        .filter(|m| !released(m))
        .chain(insts.iter().filter(|m| released(m)))
        .collect();
    if !live.is_empty() {
        if let Some(m) = ordered
            .iter()
            .find(|m| pack_uid(app_id, &name_of(m)) == live)
        {
            return Some(name_of(m));
        }
    }
    let by_name = |m: &Value| {
        !account.is_empty()
            && (m["name"].as_str() == Some(account) || m["quota_account"].as_str() == Some(account))
    };
    if let Some(m) = ordered.iter().find(|m| !holds_other(m) && by_name(m)) {
        return Some(name_of(m));
    }
    if !live.is_empty() {
        if let Some(m) = ordered
            .iter()
            .find(|m| !holds_other(m) && m["quota_user_id"].as_str() == Some(live))
        {
            return Some(name_of(m));
        }
    }
    None
}

/// 这个号存过的登录态，铺成"一个只有它自己的档案目录"，供按账号读额度/签到用。
///
/// 为什么必须铺而不是直接指原目录：同一个产品在这台机器上只有**一份**默认档案，
/// 里面躺着的是"现在登着的那个号"。想读另一个号，就得把那个号的包放到别处再指过去，
/// 否则读到的永远是当前号的状态——那正是她 2026-10-16 说的
/// "不能我切换另一个账号才显示他的签到"。
///
/// 铺的时候连**辅助文件**一起带（`Local State` 那份 DPAPI 主密钥）：Qoder 的
/// auth.v1.dat 是 os_crypt v10 信封，没有它解不开。辅助文件是从**当前**档案目录借的，
/// 同一台机器同一个 Windows 账户下主密钥是稳定的，所以借过来的密钥解得出任何一个号的包。
///
/// 返回临时目录；调用方用完删（`drop_profile`）。目录里放的是一份可解的登录密文，
/// 但它只对这台机器这个 Windows 账户有效——和它原本的防护等级一致，没有降低。
pub fn materialize(app_id: &str, name: &str) -> Result<PathBuf, String> {
    if app_id == crate::zcode::APP { return crate::zcode::materialize(name); }
    let raw = secrets::get(&vid(app_id, name)).ok_or_else(|| {
        format!("「{name}」还没存过登录态，读不了它自己的签到/额度（先在客户端里登它一次）")
    })?;
    let pack: Value = serde_json::from_str(&raw).map_err(|e| format!("存的登录包读坏了：{e}"))?;
    let files = pack["files"]
        .as_object()
        .ok_or("登录包里没有文件")?
        .clone();
    if files.is_empty() {
        return Err("登录包里一个文件都没有".into());
    }
    let dir = registry::base_dir()
        .join("tmp")
        .join(format!("read-{app_id}-{}-{}", std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default().as_nanos()));
    fs::create_dir_all(&dir).map_err(|e| format!("建临时目录失败：{e}"))?;
    // 先借辅助文件（失败只记一笔，不阻断——有些产品的包本来就不需要它）
    for f in registry::auth_pack_support_files(app_id) {
        let live = match registry::auth_pack_path(app_id, f) {
            Some(p) => p,
            None => continue,
        };
        if live.is_file() {
            let _ = fs::copy(&live, dir.join(f));
        }
    }
    for (f, b64) in &files {
        let bytes = b64
            .as_str()
            .and_then(|s| secrets::b64_decode(s).ok())
            .ok_or_else(|| format!("{f} 的包体解不开"))?;
        // 条目带子目录（ZCode 的 v2/credentials.json）时按名字平铺，不建树——
        // 这条路上只认"目录里有这么个文件"，没有产品会去数子目录层级
        let leaf = f.rsplit(['/', '\\']).next().unwrap_or(f);
        fs::write(dir.join(leaf), bytes).map_err(|e| format!("写 {leaf} 失败：{e}"))?;
    }
    Ok(dir)
}

/// 用完就把临时档案目录删掉。删不掉不算错误（下次开机由 sweep 收），
/// 但绝不把异常往上抛——那会把一次成功的读数变成一次失败。
pub fn drop_profile(dir: &std::path::Path) {
    let _ = fs::remove_dir_all(dir);
}

#[cfg(test)]
mod tests {
    #[test]
    fn workbuddy_envelope_identity_is_uid_not_the_mutable_nickname() {
        use super::*;
        let mut files = serde_json::Map::new();
        files.insert(auth_pack_files("workbuddy-intl")[0].into(),
            json!(secrets::b64_encode(br#"{"account":{"uid":"stable-account","nickname":{"encrypted":true}}}"#)));
        let (_, uid) = pack_identity("workbuddy-intl", &files).unwrap();
        assert_eq!(uid, "stable-account");
    }
    use super::*;
    use crate::registry::auth_switch_ok;

    /// 备份里挑"哪份是这个号的"：卡名优先于 uid。
    /// 顺序反了就会把别人的包再收一遍——Qoder CN 的另一张卡偏偏记着 shen 的 uid，
    /// 照 uid 挑回来的是 shen 的登录态，切过去还是 shen，人看到的就是"切不动"。
    #[test]
    fn backup_pick_prefers_the_card_name_over_a_polluted_uid() {
        let p = PathBuf::from("x");
        let cands = vec![
            // 老备份：card-b 的真身
            ("qoder-cn-1793502071".to_string(), p.clone(), ("card-b".to_string(), "u-nick".to_string())),
            // 新备份：shen 的，而这张卡的 uid 已被污染成 shen 的
            ("qoder-cn-1793630614".to_string(), p.clone(), ("shen1950".to_string(), "u-shen".to_string())),
        ];
        let hit = pick_backup(&cands, "card-b", "u-shen").expect("该按卡名挑中 card-b 那份");
        assert_eq!(hit.2 .0, "card-b");
        assert_eq!(hit.0, "qoder-cn-1793502071");
    }

    /// 卡名没命中时退回按 uid 挑；两个都没命中就是不猜
    #[test]
    fn backup_pick_falls_back_to_uid_then_gives_up() {
        let p = PathBuf::from("x");
        let cands = vec![(
            "qoder-cn-1793502071".to_string(),
            p.clone(),
            ("someone-else".to_string(), "u-1".to_string()),
        )];
        let hit = pick_backup(&cands, "card-b", "u-1").expect("uid 命中该挑中");
        assert_eq!(hit.2 .1, "u-1");
        assert!(pick_backup(&cands, "card-b", "u-2").is_none(), "都对不上就不该猜");
    }

    /// 表本身不许漂：没登记的产品绝不能被当成"能一键换号"
    #[test]
    fn only_registered_products_can_switch() {
        assert_eq!(auth_pack_files("qoder-cn"), &["auth.v1.dat"]);
        assert_eq!(auth_pack_files("qoder"), &["auth.v1.dat"]);
        assert_eq!(auth_pack_files("zcode"), &["v2/credentials.json"]);
        assert!(auth_pack_files("workbuddy")[0].ends_with("workbuddy-desktop.info"));
        assert!(auth_pack_files("workbuddy-intl")[0].ends_with("workbuddy-desktop-ai.info"));
        // TRAE 的登录混在 868 KB 的 state.vscdb 里，整份换会把对话与设置一起换走，故意不做
        assert!(auth_pack_files("traework-cn").is_empty());
        assert!(auth_pack_files("codex").is_empty());
        assert!(!auth_switch_ok("no-such-app"));
    }

    /// 条目两种写法都要能解析：相对档案目录的，和带占位符的绝对路径
    #[test]
    fn pack_entries_resolve_to_absolute_paths() {
        let rel = registry::auth_pack_path("qoder-cn", "auth.v1.dat").expect("相对条目要能解析");
        assert!(rel.is_absolute());
        assert!(rel.to_string_lossy().ends_with("auth.v1.dat"));
        let abs = registry::auth_pack_path(
            "workbuddy",
            "{LOCALAPPDATA}/CodeBuddyExtension/Data/Public/auth/workbuddy-desktop.info",
        )
        .expect("带占位符的条目要能解析");
        assert!(abs.is_absolute());
        assert!(abs.to_string_lossy().contains("CodeBuddyExtension"));
        assert!(!abs.to_string_lossy().contains('{'));
        assert!(registry::auth_pack_path("qoder-cn", "  ").is_none());
    }

    /// 备份用的扁平名不许带路径分隔符，否则条目带子目录时会往备份目录里建树
    #[test]
    fn backup_names_are_flat() {
        assert_eq!(slot_name("v2/credentials.json"), "v2+credentials.json");
        assert_eq!(slot_name("C:\\x\\auth.v1.dat").contains('\\'), false);
    }

    /// 机器身份绝不能进包：换了它等于让服务端看见一台新设备
    #[test]
    fn pack_never_carries_the_machine_identity() {
        for app in ["qoder", "qoder-cn", "zcode", "workbuddy", "workbuddy-intl"] {
            for f in auth_pack_files(app) {
                assert!(
                    !f.to_lowercase().contains("machine"),
                    "{app} 的登录包里混进了机器身份文件：{f}"
                );
            }
        }
    }

    /// 真机：把每个存过登录包的账号铺一遍，看铺出来的目录里到底有什么。
    /// 只在装了驾驶舱数据、且这些号登过的机器上有意义，所以默认 ignore。
    /// 跑：cargo test -- --ignored --nocapture materialize_real
    #[test]
    #[ignore]
    fn materialize_real_accounts() {
        // 测试进程里保管箱默认没加载（那是 main.rs 启动时做的），不加载就全是"没存过"
        crate::secrets::load();
        println!("保管箱可用={} 条目数={}", crate::secrets::available(), crate::secrets::count());
        for app in ["qoder-cn", "qoder", "workbuddy", "workbuddy-intl"] {
            let insts = crate::registry::load_instances(app);
            for m in insts {
                let name = m["name"].as_str().unwrap_or("");
                if name.is_empty() {
                    continue;
                }
                let has = stored(app, name).is_some();
                match materialize(app, name) {
                    Ok(d) => {
                        let mut got: Vec<String> = fs::read_dir(&d)
                            .map(|rd| {
                                rd.filter_map(|e| e.ok())
                                    .map(|e| {
                                        format!(
                                            "{}:{}B",
                                            e.file_name().to_string_lossy(),
                                            e.metadata().map(|m| m.len()).unwrap_or(0)
                                        )
                                    })
                                    .collect()
                            })
                            .unwrap_or_default();
                        got.sort();
                        println!("{app}/{name} 存过包={has} 铺出 -> {}", got.join("  "));
                        // 真读一次：铺出来的档案目录交给 agenthub，看它报的是不是**这个号**。
                        // 这一条才是"按账号查"成立的证据——只铺得出来不等于解得开。
                        if app.starts_with("qoder") {
                            let out = std::process::Command::new("cmd.exe")
                                .args([
                                    "/d", "/c", "agenthub", "checkin", app,
                                    "--profile", &d.to_string_lossy(), "--json",
                                ])
                                .output();
                            match out {
                                Ok(o) => {
                                    let s = String::from_utf8_lossy(&o.stdout);
                                    let v: Value = serde_json::from_str(&s).unwrap_or(Value::Null);
                                    println!(
                                        "    读回来 -> account={:?} signed_today={:?} err={:?}",
                                        v[0]["account"], v[0]["signed_today"], v[0]["error"]
                                    );
                                }
                                Err(e) => println!("    调 agenthub 失败：{e}"),
                            }
                        }
                        drop_profile(&d);
                    }
                    Err(e) => println!("{app}/{name} 存过包={has} 铺不出：{e}"),
                }
            }
        }
    }
}
