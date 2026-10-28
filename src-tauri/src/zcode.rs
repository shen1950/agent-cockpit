//! ZCode keeps conversations in one native data root. Account switching changes
//! only provider-scoped desktop OAuth credentials, never that directory.
use crate::{procs, registry as reg, secrets};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::fs;
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const APP: &str = "zcode";
static LOCK: Mutex<()> = Mutex::new(());
type Signature = (Option<SystemTime>, u64);
static IDENTITY: OnceLock<Mutex<Option<(Signature, Value)>>> = OnceLock::new();
fn now() -> u64 { SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs() }
pub fn native_root() -> PathBuf { reg::home().join(".zcode") }
fn credentials() -> PathBuf { native_root().join("v2/credentials.json") }
fn catalog_file() -> PathBuf { reg::base_dir().join("zcode-logins.json") }
fn vault_id(uid: &str) -> String { format!("zcode-login:{uid}") }
fn plain(path: &Path) -> Result<(), String> {
    if !path.is_absolute() || path.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err("ZCode 数据路径无效".into());
    }
    if path.ancestors().any(reg::is_reparse_point) { return Err("ZCode 数据目录包含旧链接，请保留资料后单独处理".into()); }
    Ok(())
}
fn helper(request: Value) -> Result<Value, String> {
    let installed = reg::home().join(".agenthub/venv/Scripts/python.exe");
    let mut command = Command::new(if installed.is_file() { installed } else { PathBuf::from("python") });
    const BOOTSTRAP: &str = "import io,json,sys; p=json.loads(sys.stdin.buffer.read().decode('utf-8')); sys.stdin=io.TextIOWrapper(io.BytesIO(json.dumps(p['request']).encode('utf-8')),encoding='utf-8'); exec(compile(p['script'],'<ZCode credentials>','exec'),{'__name__':'__main__'})";
    command.args(["-I", "-X", "utf8", "-c", BOOTSTRAP]).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    #[cfg(windows)] { use std::os::windows::process::CommandExt; command.creation_flags(0x0800_0000); }
    let mut child = command.spawn().map_err(|_| "无法启动 ZCode 登录读取组件")?;
    let input = json!({"script":include_str!("../scripts/zcode-auth.py"),"request":request}).to_string();
    child.stdin.take().ok_or("无法传递 ZCode 登录读取参数")?.write_all(input.as_bytes()).map_err(|_| "传递 ZCode 登录读取参数失败")?;
    let output = child.wait_with_output().map_err(|_| "ZCode 登录读取组件无响应")?;
    if output.stdout.len() > 8 * 1024 * 1024 { return Err("ZCode 登录数据过大".into()); }
    let result: Value = serde_json::from_slice(&output.stdout).map_err(|_| "ZCode 登录读取组件未返回有效结果")?;
    if !output.status.success() || result["ok"] != true {
        return Err(result["error"].as_str().unwrap_or("ZCode 登录信息读取失败").to_string());
    }
    Ok(result["value"].clone())
}
fn read_record() -> Result<Value, String> {
    plain(&credentials())?;
    if fs::metadata(credentials()).is_ok_and(|m|m.len()>8*1024*1024){return Err("ZCode 登录文件异常，未读取".into());}
    match fs::read(credentials()) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(|_| "ZCode 登录文件无法解析，未覆盖".into()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(json!({})),
        Err(_) => Err("无法读取 ZCode 登录文件".into()),
    }
}
fn invalidate() { if let Ok(mut cache) = IDENTITY.get_or_init(|| Mutex::new(None)).lock() { *cache = None; } crate::machine::invalidate(APP); procs::invalidate_caches(); }
pub fn current_identity() -> Result<Value, String> {
    plain(&credentials())?;
    let metadata = fs::metadata(credentials()).ok();
    let signature = (metadata.as_ref().and_then(|m| m.modified().ok()), metadata.map(|m| m.len()).unwrap_or(0));
    let cache = IDENTITY.get_or_init(|| Mutex::new(None));
    if let Ok(held) = cache.lock() { if let Some((old, value)) = held.as_ref() { if old == &signature { return Ok(value.clone()); } } }
    let value = helper(json!({"mode":"identity","root":native_root()}))?;
    if let Ok(mut held) = cache.lock() { *held = Some((signature, value.clone())); }
    Ok(value)
}
fn snapshot_current() -> Result<Value, String> { helper(json!({"mode":"snapshot","record":read_record()?})) }
pub fn identity_at(root: &Path) -> Result<Value, String> {
    plain(root)?;
    let who = if root == native_root() { current_identity()? } else { helper(json!({"mode":"identity","root":root}))? };
    if who["has_auth"] != true || who["user_id"].as_str().unwrap_or("").is_empty() { return Err("ZCode 尚未登录".into()); }
    Ok(who)
}
fn read_catalog() -> Vec<Value> { fs::read(catalog_file()).ok().and_then(|b| serde_json::from_slice::<Vec<Value>>(&b).ok()).unwrap_or_default() }
fn atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    plain(path)?;
    fs::create_dir_all(path.parent().ok_or("文件路径无父目录")?).map_err(|_| "无法创建 ZCode 记录目录")?;
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos();
    let temporary = path.with_file_name(format!(".zcode-{}-{stamp}.tmp", std::process::id()));
    fs::write(&temporary, bytes).map_err(|_| "无法写入 ZCode 临时记录")?;
    fs::rename(&temporary, path).map_err(|_| { let _ = fs::remove_file(&temporary); "无法保存 ZCode 记录".to_string() })
}
fn save_snapshot(snapshot: &Value) -> Result<(), String> {
    let who = &snapshot["identity"];
    let uid = who["user_id"].as_str().filter(|s| s.starts_with("zcode:")).ok_or("ZCode 账号身份无效")?;
    if secrets::get(&vault_id(uid)).and_then(|s|serde_json::from_str::<Value>(&s).ok()).as_ref()==Some(snapshot)
        && read_catalog().iter().any(|i|i["user_id"]==uid){return Ok(());}
    if !secrets::put(&vault_id(uid), &snapshot.to_string()) { return Err("请先解锁凭据保管箱".into()); }
    secrets::flush()?;
    let mut catalog = read_catalog();
    catalog.retain(|item| item["user_id"] != uid);
    let mut info = who.clone(); info["at"] = json!(now()); catalog.push(info);
    atomic(&catalog_file(), &serde_json::to_vec_pretty(&catalog).map_err(|_| "账号目录序列化失败")?)
}
fn snapshot_for_uid(uid: &str) -> Result<Value, String> {
    let raw = secrets::get(&vault_id(uid)).ok_or("未保存该账号登录，请重新录入")?;
    let snapshot: Value = serde_json::from_str(&raw).map_err(|_| "已保存的登录记录无效")?;
    if snapshot["identity"]["user_id"] != uid { return Err("登录记录身份不符，未切换".into()); }
    Ok(snapshot)
}
fn card(name: &str) -> Result<Value, String> {
    if name.is_empty() || reg::bad_account_name(name).is_some() { return Err("账号名称无效".into()); }
    reg::find_instance(APP, name).ok_or_else(|| "账号不存在".into())
}
fn clear_quota_cache(meta: &mut Value) {
    if let Some(fields) = meta.as_object_mut() {
        let keep = ["quota_user_id", "quota_account", "quota_url", "quota_adapter", "quota_params", "quota_auto"];
        let remove: Vec<String> = fields.keys().filter(|key| {
            matches!(key.as_str(), "credits" | "credits_left" | "credits_total" | "expires_on")
                || (key.starts_with("quota_") && !keep.contains(&key.as_str()))
        }).cloned().collect();
        for key in remove { fields.remove(&key); }
    }
}
fn bind(mut meta: Value, who: &Value) -> Value {
    let previous = meta["quota_user_id"].as_str().unwrap_or("");
    let same_identity = (previous.starts_with("zcode:bigmodel:") || previous.starts_with("zcode:zai:"))
        && who["user_id"] == previous && meta["zcode_provider"] == who["provider"];
    if !same_identity {
        // Legacy generic IDs cannot establish which account produced a cache.
        // Even a valid new login must never inherit those old balance readings.
        clear_quota_cache(&mut meta);
        meta["quota_local"] = json!(false);
        meta["quota_live_error"] = json!("尚未读取此账号额度");
    }
    meta["zcode_account_version"] = json!(1); meta["zcode_provider"] = who["provider"].clone();
    meta["quota_user_id"] = who["user_id"].clone(); meta["quota_account"] = who["account"].clone();
    meta["data_dir"] = json!(native_root()); meta["origin"] = json!("shared");
    meta.as_object_mut().map(|o| { o.remove("released"); o.remove("released_to"); o.remove("pid"); });
    meta
}
fn backup(label: &str) -> Result<PathBuf, String> {
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos();
    let directory = reg::backup_dir().join(format!("zcode-{label}-{stamp}")); plain(&directory)?;
    fs::create_dir_all(&directory).map_err(|_| "无法创建 ZCode 备份")?;
    if credentials().is_file() { fs::copy(credentials(), directory.join("credentials.json")).map_err(|_| "ZCode 登录备份失败")?; }
    if secrets::file().is_file() { fs::copy(secrets::file(), directory.join("vault.json")).map_err(|_| "ZCode 保管箱备份失败")?; }
    if reg::config_file().is_file() { fs::copy(reg::config_file(), directory.join("config.json")).map_err(|_| "ZCode 登记配置备份失败")?; }
    for meta in reg::load_instances(APP) {
        if let Some(file) = meta["_file"].as_str() { fs::copy(file, directory.join(Path::new(file).file_name().ok_or("账号文件名无效")?)).map_err(|_| "ZCode 卡片备份失败")?; }
    }
    Ok(directory)
}
fn legacy_snapshot(meta: &Value) -> Option<Value> {
    let name = meta["name"].as_str()?;
    let raw = secrets::get(&format!("authpack:{APP}/{name}"))?;
    let old: Value = serde_json::from_str(&raw).ok()?;
    let bytes = secrets::b64_decode(old["files"]["v2/credentials.json"].as_str()?).ok()?;
    let record: Value = serde_json::from_slice(&bytes).ok()?;
    let snapshot = helper(json!({"mode":"snapshot","record":record})).ok()?;
    let who = &snapshot["identity"];
    let old_id = old["user_id"].as_str().unwrap_or("");
    let matches = [who["account"].as_str(), who["email"].as_str(), who["provider_user_id"].as_str()].into_iter().flatten().any(|s| s.eq_ignore_ascii_case(name));
    if matches || (!old_id.is_empty() && who["provider_user_id"] == old_id) { Some(snapshot) } else { None }
}
/// Migration never switches or closes a client. Old native data stays in place.
pub fn migrate_registered() -> Result<(), String> {
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if !secrets::available() { return Err("请先解锁凭据保管箱".into()); }
    let accounts = reg::load_instances(APP);
    if accounts.iter().all(|m| m["zcode_account_version"] == 1) { return Ok(()); }
    let key = |value:&str|value.trim_end_matches(['\\','/']).replace('\\',"/").to_lowercase();
    if accounts.iter().any(|m|m["data_dir"].as_str().is_none_or(|p|key(p)!=key(&native_root().to_string_lossy()))) {
        return Err("ZCode 存在外部数据目录登记，请单独核对；本次未修改目录".into());
    }
    backup("accounts")?;
    let current = snapshot_current().ok();
    let mut reserved:HashSet<String>=accounts.iter().filter(|m|m["zcode_account_version"]==1)
        .filter_map(|m|m["quota_user_id"].as_str().filter(|s|!s.is_empty()).map(str::to_string)).collect();
    for meta in accounts {
        if meta["zcode_account_version"] == 1 { continue; }
        let name = meta["name"].as_str().unwrap_or("");
        let snapshot = legacy_snapshot(&meta).or_else(|| current.as_ref().filter(|s| {
            [s["identity"]["account"].as_str(), s["identity"]["email"].as_str()].into_iter().flatten().any(|a| a.eq_ignore_ascii_case(name))
        }).cloned()).filter(|s|s["identity"]["user_id"].as_str().is_some_and(|uid|reserved.insert(uid.to_string())));
        let next = if let Some(snapshot) = snapshot {
            save_snapshot(&snapshot)?; bind(meta, &snapshot["identity"])
        } else {
            let mut m = meta; m["zcode_account_version"] = json!(1); m["zcode_provider"] = json!("");
            // An ambiguous or duplicate legacy binding must not keep another
            // account's cached balance. Its original record is in the backup.
            clear_quota_cache(&mut m);
            m["quota_user_id"] = json!(""); m["quota_account"] = json!("");
            m["quota_live_error"] = json!("未保存有效登录，请重新录入");
            m["data_dir"] = json!(native_root()); m["origin"] = json!("shared");
            m.as_object_mut().map(|o| { o.remove("released"); o.remove("released_to"); }); m
        };
        reg::save_instance_checked(APP, &next)?;
    }
    let mut config = reg::load_config();
    if let Some(swaps) = config.get_mut("swaps").and_then(Value::as_object_mut) { swaps.remove(APP); }
    reg::save_config(&config);
    invalidate(); Ok(())
}
pub fn stored(name: &str) -> Option<Value> {
    let meta = card(name).ok()?; let uid = meta["quota_user_id"].as_str()?;
    let snapshot = snapshot_for_uid(uid).ok()?; let who = &snapshot["identity"];
    Some(json!({"at":read_catalog().iter().find(|i|i["user_id"]==uid).map(|i|i["at"].clone()),"account":who["account"],"user_id":uid,"provider":who["provider"],"dir":credentials(),"files":[{"name":"v2/credentials.json"}]}))
}
fn quota_state(meta: &Value) -> &'static str {
    let fetched = meta["quota_fetched_at"].as_f64().is_some_and(|at| at > 0.0);
    if fetched && meta["quota_source"] == "server" && meta["quota_plan"] == "无套餐"
        && meta["credits_left"].is_null() && meta["credits_total"].is_null()
        && meta["quota_live_error"].as_str().unwrap_or("").is_empty() {
        "no_plan"
    } else if fetched && (meta["credits_left"].as_f64().is_some() || meta["credits_total"].as_f64().is_some()) {
        "ready"
    } else {
        "unread"
    }
}
pub fn status(meta: &Value) -> Value {
    let current = current_identity().unwrap_or_else(|_| json!({}));
    let uid = meta["quota_user_id"].as_str().unwrap_or(""); let ready = !uid.is_empty() && snapshot_for_uid(uid).is_ok();
    json!({"provider":meta["zcode_provider"],"account":meta["quota_account"],"user_id":uid,"has_auth":ready,
        "current":!uid.is_empty()&&current["user_id"]==uid,"needs_login":!ready,"shared_data":true,"multi_open":false,
        "native_data_dir":native_root(),"quota_state":quota_state(meta),"auth_status":if !secrets::available(){"locked"}else if ready{"ready"}else{"needs_login"}})
}
pub fn current_card() -> Option<String> {
    let who = current_identity().ok()?; let uid = who["user_id"].as_str().filter(|s|!s.is_empty())?;
    reg::load_instances(APP).iter().find(|m|m["quota_user_id"]==uid).and_then(|m|m["name"].as_str()).map(str::to_string)
}
pub fn machine_state(mut machine: Value) -> Value {
    if let Ok(who) = current_identity() {
        if machine["value"]["user_id"] != who["user_id"] { machine["value"] = json!({}); }
        machine["known"] = json!(true); machine["value"]["ok"] = who["has_auth"].clone();
        machine["value"]["account"] = who["account"].clone(); machine["value"]["user_id"] = who["user_id"].clone();
        machine["value"]["provider"] = who["provider"].clone();
    }
    machine
}
pub fn catalog() -> Result<Value, String> {
    migrate_registered()?;
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if let Ok(snapshot) = snapshot_current() { save_snapshot(&snapshot)?; }
    let current = current_identity().unwrap_or_else(|_| json!({})); let cards = reg::load_instances(APP);
    let accounts: Vec<Value> = read_catalog().into_iter().map(|mut info| {
        let uid = info["user_id"].as_str().unwrap_or("").to_string();
        info["registered_as"] = json!(cards.iter().find(|m|m["quota_user_id"]==uid).and_then(|m|m["name"].as_str()));
        info["current"] = json!(current["user_id"]==uid); info["has_auth"] = json!(snapshot_for_uid(&uid).is_ok()); info
    }).collect();
    Ok(json!({"accounts":accounts,"native_data_dir":native_root(),"multi_open":false}))
}
pub fn capture(name: &str, selected: &str) -> Result<Value, String> {
    migrate_registered()?;
    let _guard = LOCK.lock().unwrap_or_else(|e|e.into_inner());
    if name.is_empty() || reg::bad_account_name(name).is_some() { return Err("账号名称无效".into()); }
    let snapshot = if selected.is_empty() { snapshot_current()? } else { snapshot_for_uid(selected)? };
    let who = &snapshot["identity"]; let uid = who["user_id"].as_str().ok_or("账号身份无效")?;
    if reg::load_instances(APP).iter().any(|m|m["name"]!=name && m["quota_user_id"]==uid) { return Err("该登录已登记，请使用对应账号卡片".into()); }
    let existing = reg::find_instance(APP,name);
    if let Some(m) = existing.as_ref() { if m["quota_user_id"].as_str().is_some_and(|old|!old.is_empty()&&old!=uid) { return Err("当前登录与这张卡片不符，请选择对应账号".into()); } }
    save_snapshot(&snapshot)?;
    let original = existing.unwrap_or_else(|| json!({"app":APP,"name":name,"created":chrono::Local::now().format("%Y-%m-%d").to_string(),"workdir":""}));
    let meta = bind(original, who); reg::save_instance_checked(APP,&meta)?;
    Ok(json!({"msg":"登录已录入","name":name,"zcode_auth":status(&meta)}))
}
pub fn capture_current() {
    let Ok(who) = current_identity() else { return; };
    let Some(name) = current_card() else { return; };
    let uid = who["user_id"].as_str().unwrap_or("");
    if uid.is_empty() || !secrets::available() { return; }
    let _guard = LOCK.lock().unwrap_or_else(|e|e.into_inner());
    if let Ok(snapshot) = snapshot_current() { let _=save_snapshot(&snapshot); }
    let _ = name;
}
pub fn materialize(name: &str) -> Result<PathBuf, String> {
    let meta = card(name)?; let snapshot = snapshot_for_uid(meta["quota_user_id"].as_str().unwrap_or(""))?;
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos();
    let directory = reg::base_dir().join("tmp").join(format!("zcode-read-{stamp}")); plain(&directory)?;
    let prepared=(||->Result<(),String>{
        fs::create_dir_all(directory.join("v2")).map_err(|_|"无法创建临时额度目录")?;
        fs::write(directory.join("v2/credentials.json"),serde_json::to_vec(&snapshot["record"]).map_err(|_|"登录快照无法序列化")?).map_err(|_|"无法写入额度临时凭据")?;
        let telemetry = native_root().join("v2/telemetry-state.json"); plain(&telemetry)?;
        fs::copy(telemetry,directory.join("v2/telemetry-state.json")).map_err(|_|"无法读取本机额度设备标识")?;
        Ok(())
    })();
    if let Err(error)=prepared{drop_temporary(&directory);return Err(error);}
    Ok(directory)
}
fn drop_temporary(directory:&Path) { let parent=reg::base_dir().join("tmp"); if directory.parent()==Some(parent.as_path()) && directory.file_name().is_some_and(|n|n.to_string_lossy().starts_with("zcode-read-")) && plain(directory).is_ok(){let _=fs::remove_dir_all(directory);} }
fn refresh_quota(name:&str)->Result<Value,String>{
    let meta=card(name)?;let directory=materialize(name)?;
    let result=crate::local_quota::fetch(APP,Some(&directory.to_string_lossy())); drop_temporary(&directory);
    let q=result?;if meta["quota_user_id"]!=q.user_id {return Err("额度身份与该账号不符，未更新".into());}
    let latest=card(name)?;if latest["quota_user_id"]!=q.user_id{return Err("账号登录已变化，请重新读取".into());}
    let next=crate::local_quota::apply(&latest,&q);reg::save_instance_checked(APP,&next)?;
    let state=quota_state(&next);
    Ok(json!({"msg":if state=="no_plan"{"暂无有效套餐"}else{"额度已更新"},"quota_state":state}))
}
fn pids()->Vec<u32>{procs::program_pids("zcode.exe",&procs::snapshot_fresh())}
fn stop(force:bool)->Result<bool,Value>{
    let initial=pids();if initial.is_empty(){return Ok(false);}
    if !force{return Err(json!({"ok":false,"need_force":true,"msg":"此操作需要关闭 ZCode，请先保存当前任务。"}));}
    procs::kill_pids(&initial);
    for _ in 0..16{if pids().is_empty(){return Ok(true);}std::thread::sleep(Duration::from_millis(250));}
    Err(json!({"ok":false,"msg":"ZCode 尚未退出，登录信息未修改"}))
}
fn launch_native()->Result<Value,String>{
    plain(&native_root())?;
    let exe=reg::resolve_exe(&reg::app_def(APP).ok_or("未登记 ZCode")?).ok_or("未找到 ZCode 客户端")?;
    procs::launch_plain(&exe)?;Ok(json!({"msg":"已打开 ZCode"}))
}
fn replace_login(name:&str,logout:bool,force:bool)->Result<Value,Value>{
    let _guard=LOCK.lock().unwrap_or_else(|e|e.into_inner());
    let meta=card(name).map_err(failure)?;let who=current_identity().map_err(failure)?;
    let uid=meta["quota_user_id"].as_str().unwrap_or("");
    if logout&&(uid.is_empty()||who["has_auth"]!=true||who["user_id"]!=uid){return Err(failure("该账号当前未登录"));}
    let target=if logout{None}else{Some(snapshot_for_uid(uid).map_err(failure)?)};
    // Check the target before stopping a running client.
    if let Some(snapshot)=target.as_ref(){
        let decoded=helper(json!({"mode":"snapshot","record":snapshot["record"]})).map_err(failure)?;
        if decoded["identity"]["user_id"]!=uid{return Err(failure("目标登录与账号身份不符，未切换"));}
    }
    if !logout&&who["user_id"]==uid{return launch_native().map_err(failure);}
    let restarted=stop(force)?;
    invalidate();
    if logout {
        let unchanged=current_identity().is_ok_and(|current|current["user_id"]==uid);
        if !unchanged {
            if restarted{let _=launch_native();}
            return Err(failure("当前登录已变化，已取消退出"));
        }
    }
    if let Ok(snapshot)=snapshot_current(){save_snapshot(&snapshot).map_err(failure)?;}
    let directory=backup(if logout{"logout"}else{"switch"}).map_err(failure)?;
    let original=fs::read(credentials()).ok();
    let merged=helper(json!({"mode":"merge","record":read_record().map_err(failure)?,"target":target.as_ref().map(|s|s["record"].clone()),"provider":who["provider"]})).map_err(failure)?;
    atomic(&credentials(),&serde_json::to_vec_pretty(&merged["record"]).map_err(|_|failure("登录信息序列化失败"))?).map_err(failure)?;
    invalidate();
    // Read back the written identity before launching it.
    let verified=current_identity().is_ok_and(|after|if logout{after["has_auth"]!=true}else{after["user_id"]==uid});
    if !verified{
        if let Some(bytes)=original{atomic(&credentials(),&bytes).map_err(failure)?;invalidate();}
        if restarted{let _=launch_native();}
        return Err(failure("登录核验失败，已恢复原登录"));
    }
    if restarted||!logout{launch_native().map_err(failure)?;}
    Ok(json!({"msg":if logout{"已退出当前登录"}else{"账号已切换"},"backup":directory,"zcode_auth":status(&meta)}))
}
pub fn restore(name:&str)->Result<Value,String>{
    if !pids().is_empty(){return Err("请先退出 ZCode，再恢复登录".into());}
    replace_login(name,false,false).map_err(|v|v["msg"].as_str().unwrap_or("恢复登录失败").to_string())
}
fn unregister(name:&str)->Result<Value,String>{
    let _guard=LOCK.lock().unwrap_or_else(|e|e.into_inner());let meta=card(name)?;backup("remove")?;
    let path=reg::instances_dir(APP).join(format!("{name}.json"));plain(&path)?;
    fs::remove_file(path).map_err(|_|"移除账号登记失败")?;
    let _=meta;Ok(json!({"msg":"账号已移除，登录记录与本机数据保留"}))
}
fn rename(name:&str,to:&str)->Result<Value,String>{
    if to.is_empty()||reg::bad_account_name(to).is_some(){return Err("账号名称无效".into());}
    let _guard=LOCK.lock().unwrap_or_else(|e|e.into_inner());let mut meta=card(name)?;
    if to==name{return Ok(json!({"msg":"账号名称未变化"}));}
    if reg::find_instance(APP,to).is_some(){return Err("账号名称已存在".into());}
    backup("rename")?;meta["name"]=json!(to);reg::save_instance_checked(APP,&meta)?;
    fs::remove_file(reg::instances_dir(APP).join(format!("{name}.json"))).map_err(|_|"旧账号登记移除失败")?;
    Ok(json!({"msg":"账号已重命名","name":to}))
}
fn failure(message:impl Into<String>)->Value{json!({"ok":false,"msg":message.into()})}
pub fn action(act:&str,p:&Value)->Option<Value>{
    if p["app"]!=APP{return None;}
    let supported=matches!(act,"zcode_accounts"|"zcode_capture"|"zcode_switch"|"zcode_logout"|"zcode_remove"|"zcode_status"|"zcode_quota_refresh"|"new_instance"|"launch"|"auth_switch"|"local_read"|"pull_local"|"quota_refresh"|"delete"|"swap_set"|"swap_clear"|"rehome"|"clean_cache"|"restore"|"rebind_instance"|"rename_instance"|"kill");
    if !supported{return None;}
    if let Err(e)=migrate_registered(){return Some(failure(e));}
    let name=p["name"].as_str().unwrap_or("");let force=p["force"].as_bool().unwrap_or(false);
    let result:Result<Value,Value>=match act{
        "zcode_accounts"=>catalog().map_err(failure),
        "zcode_capture"|"new_instance"=>capture(name,p["user_id"].as_str().unwrap_or("")).map_err(failure),
        "zcode_status"=>card(name).map(|m|json!({"zcode_auth":status(&m)})).map_err(failure),
        "zcode_switch"|"auth_switch"|"swap_set"=>replace_login(name,false,force),
        "zcode_logout"=>replace_login(name,true,force),
        "zcode_remove"|"delete"=>unregister(name).map_err(failure),
        "local_read"|"pull_local"|"quota_refresh"|"zcode_quota_refresh"=>refresh_quota(name).map_err(failure),
        "rename_instance"=>rename(name,p["to"].as_str().unwrap_or("")).map_err(failure),
        "kill"=>{match card(name){Ok(m)if status(&m)["current"]==true=>stop(force).map(|_|json!({"msg":"ZCode 已结束"})),Ok(_)=>Err(failure("该账号当前未运行")),Err(e)=>Err(failure(e))}},
        "launch"=>{let m=card(name);match m{Ok(m)if status(&m)["current"]==true=>launch_native().map_err(failure),Ok(_)=>replace_login(name,false,force),Err(e)=>Err(failure(e))}},
        _=>Err(failure("ZCode 共享本机数据，请使用账号登录管理")),
    };
    Some(match result{Ok(mut v)=>{v["ok"]=json!(true);v},Err(v)=>v})
}

#[cfg(test)]mod tests{
    use super::*;
    #[test]fn server_no_plan_is_distinct_from_unread_or_failed_quota(){
        let empty=json!({"quota_source":"server","quota_plan":"无套餐","quota_fetched_at":123.0,"credits_left":null,"credits_total":null,"quota_live_error":""});
        assert_eq!(quota_state(&empty),"no_plan");
        for change in [json!({"quota_fetched_at":0}),json!({"quota_source":"local-cache"}),json!({"quota_live_error":"HTTP 401"}),json!({"quota_plan":""})]{
            let mut next=empty.clone();for(key,value)in change.as_object().unwrap(){next[key]=value.clone();}assert_eq!(quota_state(&next),"unread");
        }
        let mut zero=empty;zero["credits_left"]=json!(0.0);zero["credits_total"]=json!(8.0);assert_eq!(quota_state(&zero),"ready");
        assert_eq!(quota_state(&json!({})),"unread");
    }
    #[test]fn binding_clears_unverified_or_other_account_quota_but_keeps_the_same_identity(){
        let who=json!({"provider":"zai","user_id":"zcode:zai:one","account":"Example"});
        let fixture=json!({"name":"Example","zcode_provider":"zai","quota_user_id":"zcode:zai:one",
            "credits_left":12,"credits_total":20,"credits":[{"left":12}],"expires_on":"2026-11-01",
            "quota_groups":[{"left":12}],"quota_trend":{"days":[1]},"quota_fetched_at":123,"quota_detail_at":120,
            "quota_source":"server","quota_detail_source":"server","quota_local":true,"quota_live_error":"",
            "quota_url":"https://configured.invalid/usage","workdir":"C:/Work/Keep"});
        let unchanged=bind(fixture.clone(),&who);
        for key in ["credits_left","credits_total","credits","expires_on","quota_groups","quota_trend","quota_fetched_at","quota_detail_at","quota_source","quota_detail_source","quota_local","quota_live_error"]{assert_eq!(unchanged[key],fixture[key],"same account must keep {key}");}
        for (old_uid,old_provider) in [("",""),("legacy-id","zai"),("zcode:zai:other","zai"),("zcode:bigmodel:one","bigmodel")]{
            let mut previous=fixture.clone();previous["quota_user_id"]=json!(old_uid);previous["zcode_provider"]=json!(old_provider);
            let next=bind(previous,&who);
            for key in ["credits_left","credits_total","credits","expires_on","quota_groups","quota_trend","quota_fetched_at","quota_detail_at","quota_source","quota_detail_source"]{assert!(next.get(key).is_none(),"changed identity must clear {key}");}
            assert_eq!(next["quota_user_id"],"zcode:zai:one");assert_eq!(next["quota_local"],false);
            assert_eq!(next["quota_live_error"],"尚未读取此账号额度");assert_eq!(next["quota_url"],fixture["quota_url"]);assert_eq!(next["workdir"],fixture["workdir"]);
        }
    }
    fn record(provider:&str,id:&str)->Value{let mut value=json!({"oauth:active_provider":provider});value[format!("oauth:{provider}:access_token")]=json!("synthetic-token");value[format!("oauth:{provider}:user_info")]=json!(json!({"user_id":id,"name":"Example"}).to_string());value}
    #[test]fn provider_identity_does_not_merge_the_same_user_id(){let a=helper(json!({"record":record("bigmodel","same")})).unwrap();let b=helper(json!({"record":record("zai","same")})).unwrap();assert_ne!(a["user_id"],b["user_id"]);assert_eq!(b["account"],"Example");}
    #[test]fn restoring_oauth_preserves_other_provider_bot_and_project_credentials(){let mut current=record("zai","one");current["bot:private:credential"]=json!("unchanged-bot");current["project-key-location:v1:example"]=json!("unchanged-api");let target=record("bigmodel","two");let merged=helper(json!({"mode":"merge","record":current,"target":target})).unwrap();assert_eq!(merged["record"]["bot:private:credential"],"unchanged-bot");assert_eq!(merged["record"]["project-key-location:v1:example"],"unchanged-api");assert_eq!(merged["record"]["oauth:zai:access_token"],current["oauth:zai:access_token"]);assert_eq!(merged["record"]["oauth:zai:user_info"],current["oauth:zai:user_info"]);let who=helper(json!({"record":merged["record"]})).unwrap();assert_eq!(who["user_id"],"zcode:bigmodel:two");}
    #[test]fn logout_preserves_other_provider_and_does_not_activate_it(){
        let mut current=record("zai","one");let other=record("bigmodel","two");
        for(k,v)in other.as_object().unwrap(){if k!="oauth:active_provider"{current[k]=v.clone();}}
        current["oauth:bigmodel:refresh_token"]=json!("other-refresh-byte-for-byte");current["project-key-location:v1:example"]=json!("keep");
        let merged=helper(json!({"mode":"merge","record":current,"provider":"zai"})).unwrap();
        for key in ["oauth:bigmodel:access_token","oauth:bigmodel:refresh_token","oauth:bigmodel:user_info","project-key-location:v1:example"]{assert_eq!(merged["record"][key],current[key]);}
        assert!(merged["record"].get("oauth:zai:access_token").is_none());assert!(merged["record"].get("oauth:zai:user_info").is_none());
        assert_eq!(merged["record"]["oauth:active_provider"],"zai");assert!(merged["record"].get("zcodejwttoken").is_none());
        let who=helper(json!({"record":merged["record"]})).unwrap();assert_eq!(who["has_auth"],false);assert_eq!(who["provider"],"zai");
    }
    #[test]fn logout_rejects_a_provider_changed_since_the_request(){assert!(helper(json!({"mode":"merge","record":record("bigmodel","two"),"provider":"zai"})).is_err());}
    #[test]fn snapshot_has_only_one_providers_oauth_fields(){let mut current=record("zai","one");current["oauth:bigmodel:access_token"]=json!("other");current["bot:private:credential"]=json!("keep");let snap=helper(json!({"mode":"snapshot","record":current})).unwrap();assert!(snap["record"].get("bot:private:credential").is_none());assert!(snap["record"].get("oauth:bigmodel:access_token").is_none());}
    #[test]#[ignore="explicit native ZCode account migration and read-only quota checks; never switches a client"]
    fn registered_zcode_account_migration_smoke(){
        if std::env::var("AGENTCOCKPIT_ZCODE_SMOKE").as_deref()!=Ok("1"){return;}
        secrets::load();let before=fs::read(credentials()).unwrap();let before_pids=pids();let count=reg::load_instances(APP).len();
        migrate_registered().unwrap();let accounts=catalog().unwrap();assert!(!accounts["accounts"].as_array().unwrap().is_empty());
        let mut identities=std::collections::HashSet::new();for m in reg::load_instances(APP){
            let s=status(&m);assert_eq!(s["has_auth"],true);assert!(identities.insert(s["user_id"].as_str().unwrap().to_string()));
            let refreshed=refresh_quota(m["name"].as_str().unwrap()).unwrap();let latest=card(m["name"].as_str().unwrap()).unwrap();
            let numeric=latest["credits_left"].as_f64().is_some()||latest["credits_total"].as_f64().is_some();
            assert!(numeric||refreshed["quota_state"]=="no_plan","server response has neither a numeric quota nor a verified no-plan state");
            println!("zcode_account={} provider={} ready=true quota_state={} numeric_quota={} shared_native_data=true",m["name"],s["provider"],refreshed["quota_state"],numeric);
        }
        assert_eq!(fs::read(credentials()).unwrap(),before);assert_eq!(reg::load_instances(APP).len(),count);let after=procs::snapshot_fresh();assert!(before_pids.iter().all(|pid|after.contains_key(pid)));println!("zcode_credentials_unchanged=true original_processes_preserved=true");
    }
}
