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

/// 收下当前登录的号。account / user_id 由调用方从登录态里现读——包本身不解密，认不出是谁。
pub fn capture(app_id: &str, name: &str, account: &str, user_id: &str) -> Result<Value, String> {
    if app_id == crate::zcode::APP { return crate::zcode::capture(name, ""); }
    let mut pack = read_pack(app_id)?;
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
    if let Some(p) = stored(app_id, &name) {
        let at = p["at"].as_u64().unwrap_or(0);
        // 收过的时间不比文件旧，说明包就是现在这份登录态，不用再收
        let fresh = match current_mtime(app_id) {
            Some(m) => m <= at,
            None => true,
        };
        if fresh {
            return;
        }
    }
    let _ = capture(app_id, &name, account, user_id);
}

/// 这份登录态属于哪张卡：先按绑过的 userId 认，认不到再按账号名。
/// 都对不上说明这是个没建过卡的号，不替她瞎建。
pub fn card_for_login(app_id: &str, account: &str, user_id: &str) -> Option<String> {
    let insts = registry::load_instances(app_id);
    let by_uid = |m: &Value| !user_id.is_empty() && m["quota_user_id"].as_str() == Some(user_id);
    let by_name = |m: &Value| {
        !account.is_empty()
            && (m["name"].as_str() == Some(account) || m["quota_account"].as_str() == Some(account))
    };
    insts
        .iter()
        .find(|m| by_uid(m))
        .or_else(|| insts.iter().find(|m| by_name(m)))
        .and_then(|m| m["name"].as_str())
        .map(str::to_string)
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
        .join(format!("read-{app_id}-{name}-{}", now_secs()));
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
    use super::*;
    use crate::registry::auth_switch_ok;

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
