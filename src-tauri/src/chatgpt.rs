//! ChatGPT/Codex desktop accounts. Profiles and CODEX_HOME are separate ordinary
//! directories. Credentials and browser profiles are never linked or copied.
//! Only session subdirectories and sqlite_home use the native history store.
//! Behaviour reference: jlcodes99/cockpit-tools, acc057d (2026-10-25).
use crate::{procs, registry as reg};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
#[path = "chatgpt_official.rs"] mod official;
#[path = "chatgpt_history.rs"] mod history;
#[path = "chatgpt_quota.rs"] mod quota;
#[path = "chatgpt_windows.rs"] mod windows;

pub const APP: &str = "codex-desktop";
const MAX_IMPORT: usize = 8 * 1024 * 1024;
const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
static IO_LOCK: Mutex<()> = Mutex::new(());
struct Login { child: Arc<Mutex<Child>>, state: Arc<Mutex<Value>> }
static LOGINS: OnceLock<Mutex<HashMap<String, Login>>> = OnceLock::new();
fn logins() -> &'static Mutex<HashMap<String, Login>> { LOGINS.get_or_init(|| Mutex::new(HashMap::new())) }
fn now() -> u64 { SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs() }

fn meta(name: &str) -> Result<Value, String> {
    if name.is_empty() || reg::bad_account_name(name).is_some() { return Err("账号名称无效".into()); }
    reg::find_instance(APP, name).ok_or_else(|| "账号不存在".into())
}
fn native_profile() -> Result<PathBuf, String> { reg::profile_dir(APP).ok_or_else(|| "未找到默认资料路径".into()) }
fn native_home() -> PathBuf { reg::home().join(".codex") }
fn is_native(m: &Value) -> bool {
    reg::is_in_place(APP, Path::new(m["data_dir"].as_str().unwrap_or("")))
}
pub fn check_plain_path(path: &Path) -> Result<(), String> {
    if !path.is_absolute() || path.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err("资料路径必须是没有上级跳转的绝对路径".into());
    }
    for p in path.ancestors() {
        if reg::is_reparse_point(p) { return Err(format!("资料路径包含目录链接：{}。请先保留资料并处理旧链接。", p.display())); }
    }
    Ok(())
}
fn under(path: &Path, root: &Path) -> bool { path != root && path.starts_with(root) }
fn home(m: &Value) -> PathBuf {
    if is_native(m) { return native_home(); }
    m["chatgpt_home"].as_str().map(PathBuf::from).unwrap_or_else(|| {
        reg::base_dir().join("chatgpt-auth").join(m["name"].as_str().unwrap_or(""))
    })
}
fn writable(m: &Value) -> Result<PathBuf, String> {
    if is_native(m) { return Err("这张卡沿用本机原有资料。新登录请新建独立账号，原有登录与资料保留。".into()); }
    let p = PathBuf::from(m["data_dir"].as_str().unwrap_or(""));
    check_plain_path(&p)?;
    if !under(&p, &reg::instances_dir(APP)) { return Err("该账号使用外部资料目录，只允许沿用启动，不能覆盖其中的登录凭据。".into()); }
    let h = home(m);
    check_plain_path(&h)?;
    if !under(&h, &reg::base_dir().join("chatgpt-auth")) { return Err("独立凭据目录超出受管范围".into()); }
    if !instance_pids(m, &procs::snapshot_fresh()).is_empty() {
        return Err("请先关闭这个账号的客户端窗口，再修改登录凭据。其他账号可以继续使用。".into());
    }
    Ok(h)
}
fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    check_plain_path(path)?;
    let parent = path.parent().ok_or("文件没有父目录")?;
    fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos();
    let temp = parent.join(format!(".chatgpt-{}-{stamp}.tmp", std::process::id()));
    let previous = parent.join(format!(".chatgpt-{}-{stamp}.previous", std::process::id()));
    fs::write(&temp, bytes).map_err(|e| e.to_string())?;
    let existed = path.exists();
    if existed { fs::rename(path, &previous).map_err(|e| { let _ = fs::remove_file(&temp); e.to_string() })?; }
    if let Err(e) = fs::rename(&temp, path) {
        if existed { let _ = fs::rename(&previous, path); }
        let _ = fs::remove_file(&temp);
        return Err(format!("写入失败，已尝试恢复原文件：{e}"));
    }
    Ok(())
}
fn prepare_home(m: &Value) -> Result<PathBuf, String> {
    let h = writable(m)?;
    fs::create_dir_all(&h).map_err(|e| e.to_string())?;
    let file = h.join("config.toml");
    let mut config: toml::Value = if file.exists() {
        fs::read_to_string(&file).map_err(|e| e.to_string())?.parse().map_err(|_| "独立账号的 config.toml 无法解析，未覆盖")?
    } else { toml::Value::Table(Default::default()) };
    let table=config.as_table_mut().ok_or("独立配置必须是TOML表")?;
    table.insert("sqlite_home".into(),toml::Value::String(native_home().to_string_lossy().into_owned()));
    table.insert("cli_auth_credentials_store".into(),toml::Value::String("file".into()));
    table.entry("features").or_insert_with(||toml::Value::Table(Default::default())).as_table_mut().ok_or("features必须是配置表")?.insert("secret_auth_storage".into(),toml::Value::Boolean(false));
    atomic_write(&file, toml::to_string(&config).map_err(|e| e.to_string())?.as_bytes())?;
    let mut saved = if m["chatgpt_pending"]==true{m.clone()}else{meta(m["name"].as_str().unwrap_or(""))?}; saved["chatgpt_home"] = json!(h.to_string_lossy());
    if m["chatgpt_pending"]!=true {reg::save_instance_checked(APP, &saved)?;}
    Ok(h)
}
fn command(h: &Path) -> Result<Command, String> {
    let exe = crate::cliauth::codex_exe().ok_or("未找到 Codex 登录组件")?;
    let mut c = Command::new(exe);
    procs::scrub_env_pub(&mut c);
    for k in ["OPENAI_API_KEY", "OPENAI_BASE_URL", "OPENAI_ACCESS_TOKEN", "OPENAI_IDENTITY_TOKEN_FILE"] { c.env_remove(k); }
    c.env("CODEX_HOME", h).stdin(Stdio::null());
    #[cfg(windows)] { use std::os::windows::process::CommandExt; c.creation_flags(0x08000000); }
    Ok(c)
}
fn claims(token: &str) -> Value {
    let part = token.split('.').nth(1).unwrap_or("").replace('-', "+").replace('_', "/");
    let padded = format!("{}{}", part, "=".repeat((4 - part.len() % 4) % 4));
    crate::secrets::b64_decode(&padded).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or(Value::Null)
}
fn summary(v: &Value) -> Value {
    let access = v.pointer("/tokens/access_token").and_then(Value::as_str).unwrap_or("");
    let access = if access.is_empty(){v["personal_access_token"].as_str().unwrap_or("")}else{access};
    let id = v.pointer("/tokens/id_token").and_then(Value::as_str).filter(|s|!s.is_empty()).unwrap_or(access);
    let c = claims(id);
    let api = v["OPENAI_API_KEY"].as_str().filter(|s| !s.is_empty());
    json!({"has_auth": api.is_some() || !access.is_empty(), "mode": if api.is_some(){"apikey"}else{"chatgpt"},
        "account": c["email"], "plan": c.get("https://api.openai.com/auth").and_then(|v|v.get("chatgpt_plan_type")),
        "expires_at": claims(access)["exp"], "has_refresh": v.pointer("/tokens/refresh_token").and_then(Value::as_str).is_some_and(|s| !s.is_empty())})
}
pub fn status(m: &Value) -> Value {
    let h = home(m);
    let auth = fs::read(h.join("auth.json")).ok().filter(|b| b.len() <= MAX_IMPORT)
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok()).unwrap_or(Value::Null);
    let mut s = summary(&auth);
    s["native"] = json!(is_native(m)); s["home"] = json!(h.to_string_lossy());
    s["profile"] = m["data_dir"].clone();
    s["history_snapshot"]=m["history_snapshot"].clone();s["history_mode"]=m["history_mode"].clone();
    s["history_storage"]=json!({"mode":if is_native(m)||h.join(".agentcockpit-shared-history.json").is_file(){"shared"}else{"pending"},"canonical":native_home()});
    s["project_visibility"]=project_status(m);
    s["project_repair"]=s["project_visibility"].clone();
    s["quota"]=quota_status(m);
    if s["mode"]=="apikey" {
        if let Ok(text)=fs::read_to_string(h.join("config.toml")){if let Ok(config)=text.parse::<toml::Value>(){
            let provider=config.get("model_provider").and_then(toml::Value::as_str).unwrap_or("openai");
            s["provider"]=json!(provider);s["base_url"]=json!(config.get("model_providers").and_then(|v|v.get(provider)).and_then(|v|v.get("base_url")).and_then(toml::Value::as_str).unwrap_or("https://api.openai.com/v1"));
        }}
    }
    s["path_error"] = json!(check_plain_path(Path::new(m["data_dir"].as_str().unwrap_or(""))).err());
    if let Ok(jobs) = logins().lock() { if let Some(job) = jobs.get(m["name"].as_str().unwrap_or("")) {
        s["login"] = job.state.lock().map(|s| s.clone()).unwrap_or(Value::Null);
    }}
    s
}
pub fn quota_status(m:&Value)->Value {
    if m["chatgpt_pending"]!=true && !m["name"].as_str().unwrap_or("").is_empty(){quota::schedule(m);}
    quota::cached(m)
}
fn project_status(m:&Value)->Value{
    let mut result=history::project_status(m);
    result["pending"]=json!(result["state"]=="pending");
    result["repairable_count"]=result["legacy_count"].clone();
    result
}
fn create_meta(p: &Value) -> Result<Value, String> {
    let name = p["name"].as_str().unwrap_or("").trim();
    if name.is_empty() || reg::bad_account_name(name).is_some() { return Err("请输入有效账号名称".into()); }
    if reg::find_instance(APP, name).is_some() { return Err("同名账号已存在".into()); }
    let origin = p["origin"].as_str().unwrap_or("fresh");
    let native = origin == "adopt" || origin == "existing";
    if !native && origin != "fresh" { return Err("ChatGPT新账号只创建空白独立资料；沿用原有资料仅做原位登记。".into()); }
    let id=format!("account-{}-{}",std::process::id(),SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos());
    let path = if native { native_profile()? } else { reg::instances_dir(APP).join(&id) };
    let auth_home=reg::base_dir().join("chatgpt-auth").join(&id);
    check_plain_path(&path)?;
    if native {
        if !path.is_dir() { return Err("本机默认资料目录不存在".into()); }
        if reg::load_instances(APP).iter().any(is_native) { return Err("本机原有资料已登记，请使用已有卡片。".into()); }
    } else {
        check_plain_path(&auth_home)?;
        if auth_home.exists(){return Err("独立凭据目录已存在，未复用其中的登录，请重试。".into());}
        if path.exists() { return Err("该目录已存在，未覆盖；请更换账号名称或先登记原有目录。".into()); }
        fs::create_dir_all(&path).map_err(|e| e.to_string())?;
    }
    let m = json!({"name":name,"app":APP,"data_dir":path.to_string_lossy(),"created":now(),"origin":if native{"adopt"}else{"fresh"},
        "workdir":p["workdir"].as_str().unwrap_or(""),"chatgpt_native":native,"pid":0,
        "chatgpt_home":if native{native_home()}else{auth_home},"chatgpt_pending":true,
        "history_mode":"shared",
        "history_source_home":p["history_source_home"].as_str().map(PathBuf::from).unwrap_or_else(native_home)});
    Ok(m)
}
fn create(p: &Value) -> Result<Value, String> {
    let mut m=create_meta(p)?;
    if !is_native(&m){prepare_home(&m)?;let snapshot=history::seed(&m)?;m["history_prepared"]=json!(true);m["history_snapshot"]=snapshot;}
    m.as_object_mut().unwrap().remove("chatgpt_pending");
    reg::save_instance_checked(APP, &m)?;
    Ok(json!({"msg":"已创建账号，已有资料保持原位","name":m["name"]}))
}
fn new_account_name(p:&Value,v:&Value)->Result<String,String>{
    let requested=p["new_name"].as_str().unwrap_or("").trim();
    if !requested.is_empty(){if reg::bad_account_name(requested).is_some(){return Err("账号名称无效".into());}if meta(requested).is_ok(){return Err("同名账号已存在，请更换名称".into());}return Ok(requested.into());}
    let info=summary(v);let base=info["account"].as_str().filter(|s|reg::bad_account_name(s).is_none()).unwrap_or(if info["mode"]=="apikey"{"API 账号"}else{"ChatGPT 账号"});
    for i in 0..10000{let n=if i==0{base.into()}else{format!("{base}-{i}")};if meta(&n).is_err(){return Ok(n);}}
    Err("无法生成唯一账号名称".into())
}
fn commit_new(p:&Value,v:&Value)->Result<Value,String>{
    let name=new_account_name(p,v)?;let mut request=p.clone();request["name"]=json!(name);request["origin"]=json!("fresh");
    let mut m=create_meta(&request)?;
    import_one(&m,v)?;
    let snapshot=history::seed(&m)?;
    m["history_prepared"]=json!(true);m["history_snapshot"]=snapshot;m["official_session"]=p["official_session"].clone();
    m.as_object_mut().unwrap().remove("chatgpt_pending");reg::save_instance_checked(APP,&m)?;
    Ok(json!({"name":name,"msg":"登录凭据已保存，账号已添加","status":status(&m)}))
}
fn launch(m: &Value) -> Result<Value, String> {
    let name=m["name"].as_str().unwrap_or("");
    if logins().lock().ok().and_then(|j|j.get(name).map(|j|j.state.lock().map(|s|s["running"]==true).unwrap_or(false))).unwrap_or(false){return Err("这个账号正在授权，请完成或取消后再启动客户端。".into());}
    let p = Path::new(m["data_dir"].as_str().ok_or("账号缺少资料目录")?);
    check_plain_path(p)?;
    if !p.is_dir() { return Err("资料目录不存在，未新建替代目录".into()); }
    let existing = instance_pids(m, &procs::snapshot_fresh());
    if !existing.is_empty() {
        if windows::restore(&existing) {
            procs::invalidate_caches();
            return Ok(json!({"msg":"已打开这个账号的现有窗口","restored":true}));
        }
        // Close can destroy Electron's document window while its main process
        // remains. Activate the same profile; never run offline history repair,
        // credential preparation or provider rewrites against a resident client.
        let exe = reg::resolve_exe(&reg::app_def(APP).ok_or("未登记客户端")?).ok_or("未找到 ChatGPT 客户端")?;
        let h = home(m); check_plain_path(&h)?;
        procs::launch_pwa_isolated(&exe, p, Some(&h))?;
        return wait_for_window(m, true);
    }
    let exe = reg::resolve_exe(&reg::app_def(APP).ok_or("未登记客户端")?).ok_or("未找到 ChatGPT 客户端")?;
    if is_native(m) { procs::launch_pwa_isolated(&exe,p,Some(&native_home()))?; }
    else {
        history::apply_pending(m)?;
        history::seed(m)?;
        let mut h = home(m); check_plain_path(&h)?;
        if !under(&h, &reg::base_dir().join("chatgpt-auth")) { return Err("独立凭据目录超出受管范围".into()); }
        if !h.join("config.toml").is_file() { h=prepare_home(m)?; }
        let auth=status(m);
        if auth["has_auth"]==true&&auth["mode"]=="chatgpt"{provider_config(&h,"",name)?;}
        if auth["has_auth"]==true&&auth["mode"]=="apikey"{sync_provider_label(m)?;}
        procs::launch_pwa_isolated(&exe, p, Some(&h))?;
    }
    wait_for_window(m, false)
}
fn wait_for_window(m: &Value, restoring: bool) -> Result<Value, String> {
    let started=Instant::now();
    // The Store package can create its process long before Electron's first
    // window appears, especially while Windows is reclaiming memory. Waiting
    // only for a PID concealed this; an eight-second GUI timeout then falsely
    // reported a failed launch on a healthy but slow first initialization.
    let deadline=Duration::from_secs(if restoring {20} else {45});
    while started.elapsed()<deadline {
        let pids = instance_pids(m, &procs::snapshot_fresh());
        if windows::visible(&pids) || windows::restore(&pids) {
            procs::invalidate_caches();
            return Ok(json!({"msg":if restoring {"已重新打开这个账号的窗口"} else {"已打开这个账号的客户端窗口"},"restored":restoring,"window_wait_ms":started.elapsed().as_millis()}));
        }
        std::thread::sleep(Duration::from_millis(350));
    }
    Err("已请求打开这个账号，但尚未确认到客户端窗口。请检查官方客户端是否正在初始化或等待权限提示；已有资料和登录保持原位。".into())
}
fn profile_arg(cmd:&str)->Option<String>{
    let tail=cmd.split_once("--user-data-dir=").or_else(||cmd.split_once("--user-data-dir "))?.1.trim_start();
    let value=if let Some(quoted)=tail.strip_prefix('"'){quoted.split('"').next().unwrap_or("")}else{tail.split(" --").next().unwrap_or("")};
    Some(value.trim().trim_end_matches('"').trim_end_matches(['\\','/']).replace('\\',"/").to_lowercase())
}
pub fn instance_pids(m:&Value,snap:&HashMap<u32,procs::ProcInfo>)->Vec<u32>{
    let profile=m["data_dir"].as_str().unwrap_or("").trim_end_matches(['\\','/']).replace('\\',"/").to_lowercase();
    if profile.is_empty(){return Vec::new();}
    let native=is_native(m);
    snap.iter().filter(|(_,p)|p.name=="chatgpt.exe" && !p.cmdline.trim().is_empty() && match profile_arg(&p.cmdline){Some(path)=>path==profile,None=>native&&!p.cmdline.contains("--user-data-dir")&&!p.cmdline.contains("--type=")}).map(|(pid,_)|*pid).collect()
}
pub fn window_visible(m:&Value,snap:&HashMap<u32,procs::ProcInfo>)->bool {
    windows::visible(&instance_pids(m,snap))
}
fn kill_instance(m:&Value)->Result<Value,String>{
    let pids=instance_pids(m,&procs::snapshot_fresh());
    if pids.is_empty(){return Err("这个账号没有可确认归属的客户端进程".into());}
    procs::kill_pids(&pids);
    Ok(json!({"msg":"已结束这个账号的客户端进程，其他账号继续运行"}))
}
fn clean_output(line: &str) -> String {
    if ["access_token", "refresh_token", "id_token", "OPENAI_API_KEY", "eyJ"].iter().any(|k| line.contains(k)) { return "[凭据内容已隐藏]".into(); }
    let mut out = String::new(); let mut escape = false;
    for c in line.chars() { if c == '\u{1b}' { escape=true;continue; } if escape {if c.is_alphabetic(){escape=false;}continue;} if !c.is_control(){out.push(c);} }
    out.chars().take(1500).collect()
}
fn start_login(m: &Value, device: bool) -> Result<Value, String> {
    let name = m["name"].as_str().ok_or("账号名称为空")?.to_string();
    let mut jobs = logins().lock().map_err(|_| "登录状态被占用")?;
    if let Some(j) = jobs.get(&name) { if j.state.lock().map(|s| s["running"] == true).unwrap_or(false) {return Err("该账号已有登录流程，请先完成或取消。".into());} }
    let h=prepare_home(m)?; let mut c=command(&h)?;
    c.args(["-c", "cli_auth_credentials_store=\"file\"", "-c", "model_provider=\"openai\"", "login"]);
    if device {c.arg("--device-auth");}
    let mut child=c.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().map_err(|e| e.to_string())?;
    let out=child.stdout.take();let err=child.stderr.take();
    let child=Arc::new(Mutex::new(child));
    let state=Arc::new(Mutex::new(json!({"running":true,"phase":"waiting","method":if device{"device"}else{"browser"},"message":"等待官方登录完成","url":"","output":""})));
    for pipe in [out.map(|p|Box::new(p) as Box<dyn std::io::Read+Send>),err.map(|p|Box::new(p) as Box<dyn std::io::Read+Send>)].into_iter().flatten() {
        let state=state.clone();std::thread::spawn(move|| {
            for line in BufReader::new(pipe).lines().map_while(Result::ok) {
                let line=clean_output(&line);if let Ok(mut s)=state.lock(){
                    if let Some(url)=line.split_whitespace().find(|v|v.starts_with("https://auth.openai.com/")){s["url"]=json!(url);}
                    let old=s["output"].as_str().unwrap_or("");let combined=format!("{old}\n{line}");
                    let mut tail:Vec<char>=combined.chars().rev().take(5000).collect();tail.reverse();s["output"]=json!(tail.into_iter().collect::<String>());
                }
            }
        });
    }
    let wait_child=child.clone();let wait_state=state.clone();
    std::thread::spawn(move|| {let start=Instant::now();loop{
        let mut c=match wait_child.lock(){Ok(c)=>c,Err(_)=>break};
        match c.try_wait(){Ok(Some(code))=>{if let Ok(mut s)=wait_state.lock(){if s["phase"]!="cancelled"{s["running"]=json!(false);s["phase"]=json!(if code.success(){"completed"}else{"failed"});s["message"]=json!(if code.success(){"官方登录已完成，可启动这个账号"}else{"登录未完成，可重试或查看官方提示"});}}break;},Err(_)=>{if let Ok(mut s)=wait_state.lock(){s["running"]=json!(false);s["phase"]=json!("failed");s["message"]=json!("无法读取官方登录进程状态，请重新授权");}break;},_=>{}}
        if start.elapsed()>Duration::from_secs(600){let _=c.kill();let _=c.wait();if let Ok(mut s)=wait_state.lock(){s["running"]=json!(false);s["phase"]=json!("expired");s["message"]=json!("登录等待超时，凭据目录保留，可重新授权");}break;}
        drop(c);std::thread::sleep(Duration::from_millis(200));
    }});
    jobs.insert(name,Login{child,state});
    Ok(json!({"msg":"已启动官方授权，请在下方查看链接或设备码"}))
}
fn percent_decode(s:&str)->String {let b=s.as_bytes();let mut o=Vec::new();let mut i=0;while i<b.len(){if b[i]==b'%' && i+2<b.len(){if let Ok(n)=u8::from_str_radix(&s[i+1..i+3],16){o.push(n);i+=3;continue;}}o.push(if b[i]==b'+'{b' '}else{b[i]});i+=1;}String::from_utf8_lossy(&o).into_owned()}
fn query(url:&str,key:&str)->Option<String>{url.split_once('?')?.1.split('&').find_map(|s|{let(k,v)=s.split_once('=')?;(k==key).then(||percent_decode(v))})}
fn validate_callback(auth:&str,callback:&str)->Result<(),String>{
    let redirect=query(auth,"redirect_uri").ok_or("尚未取得本次授权链接")?;
    if !["http://localhost:1455/auth/callback","http://127.0.0.1:1455/auth/callback","http://localhost:1457/auth/callback","http://127.0.0.1:1457/auth/callback"].contains(&redirect.as_str()) {return Err("官方回调地址不在允许范围内".into());}
    if callback.split('?').next()!=Some(redirect.as_str()) || query(callback,"state")!=query(auth,"state") || query(auth,"state").is_none() || query(callback,"code").is_none(){return Err("回调地址或state与本次授权不一致".into());}
    Ok(())
}

fn token_value(v:&Value,keys:&[&str])->String {keys.iter().find_map(|k|v.get(*k).and_then(Value::as_str)).unwrap_or("").trim().to_string()}
fn normalize_one(v:&Value)->Result<Value,String>{
    if let Some(s)=v.as_str(){
        let s=s.trim();
        if s.starts_with("sk-"){return normalize_one(&json!({"OPENAI_API_KEY":s}));}
        if s.split('.').count()==3{return normalize_one(&json!({"access_token":s}));}
        if s.starts_with("rt_"){return normalize_one(&json!({"refresh_token":s}));}
        return Err("无法识别令牌，请粘贴 access_token、rt_ 开头的 refresh token，或完整 auth.json。".into());
    }
    if !v.is_object(){return Err("凭据必须是对象或令牌文本".into());}
    for k in ["auth","credentials","session"] {if v.get(k).is_some_and(Value::is_object){return normalize_one(&v[k]);}}
    let key=token_value(v,&["OPENAI_API_KEY","api_key","apiKey"]);
    if !key.is_empty(){return Ok(json!({"auth_mode":"apikey","OPENAI_API_KEY":key,"base_url":token_value(v,&["base_url","api_base_url","baseUrl"])}));}
    let t=if v["tokens"].is_object(){&v["tokens"]}else{v};
    let access=token_value(t,&["access_token","accessToken","personal_access_token"]);
    let refresh=token_value(t,&["refresh_token","refreshToken"]);
    if access.is_empty() && refresh.is_empty(){return Err("JSON中没有可导入的登录凭据".into());}
    if access.chars().chain(refresh.chars()).any(|c|c.is_control()||c.is_whitespace()){return Err("令牌中含有空白或控制字符".into());}
    let id=token_value(t,&["id_token","idToken"]);
    let c=claims(if id.is_empty(){&access}else{&id});let a=claims(&access);
    let account_id=token_value(t,&["account_id","accountId"]);
    let account_id=if account_id.is_empty(){a.get("https://api.openai.com/auth").and_then(|x|x.get("chatgpt_account_id")).and_then(Value::as_str).unwrap_or("").to_string()}else{account_id};
    Ok(json!({"auth_mode":"chatgpt","OPENAI_API_KEY":null,"tokens":{"access_token":access,"refresh_token":refresh,"id_token":id,"account_id":if account_id.is_empty(){Value::Null}else{json!(account_id)}},"last_refresh":chrono::Utc::now().to_rfc3339(),"email_hint":c["email"]}))
}
fn normalize_many(text:&str)->Result<Vec<Value>,String>{
    if text.len()>MAX_IMPORT{return Err("导入内容超过8MB".into());}
    let text=text.trim_start_matches('\u{feff}').trim();
    if text.is_empty(){return Err("导入内容为空".into());}
    let raw:Value=if text.starts_with('{')||text.starts_with('['){serde_json::from_str(text).map_err(|_|"JSON格式无效，未写入任何凭据")?}
        else{json!(text.lines().map(str::trim).filter(|s|!s.is_empty()).collect::<Vec<_>>())};
    let list=raw.as_array().or_else(||raw.get("accounts").and_then(Value::as_array));
    let values=if let Some(list)=list{if list.len()>100{return Err("一次最多导入100个账号".into());}list.iter().map(normalize_one).collect::<Result<Vec<_>,_>>()?}else{vec![normalize_one(&raw)?]};
    if values.is_empty(){return Err("没有可导入账号".into());}Ok(values)
}
fn with_import_base(mut value:Value,base:&str)->Result<Value,String>{
    if !base.trim().is_empty(){if value["auth_mode"]!="apikey"{return Err("自定义 Base URL 只适用于 API Key，请为 ChatGPT 账号令牌留空该字段。".into());}value["base_url"]=json!(base.trim());}
    Ok(value)
}
fn refresh_if_needed(v:&Value)->Result<Value,String>{
    if v["auth_mode"]=="apikey"{return Ok(v.clone());}
    let access=v["tokens"]["access_token"].as_str().unwrap_or("");
    let expired=claims(access)["exp"].as_u64().is_some_and(|e|e<=now()+30);
    let needs_identity=v["tokens"]["id_token"].as_str().unwrap_or("").is_empty()&&!v["tokens"]["refresh_token"].as_str().unwrap_or("").is_empty();
    if !access.is_empty() && !expired && !needs_identity{return Ok(v.clone());}
    let rt=v["tokens"]["refresh_token"].as_str().filter(|s|!s.is_empty()).ok_or("访问令牌已过期或缺失，请重新授权或提供refresh token")?;
    let reply:Value=ureq::AgentBuilder::new().timeout(Duration::from_secs(30)).build()
        .post("https://auth.openai.com/oauth/token")
        .send_json(json!({"grant_type":"refresh_token","client_id":CLIENT_ID,"refresh_token":rt}))
        .map_err(|e|format!("官方令牌续期失败：{e}"))?.into_json().map_err(|_|"官方返回的凭据格式无效")?;
    let mut reply=reply; if reply["refresh_token"].as_str().is_none(){reply["refresh_token"]=json!(rt);}
    for key in ["id_token","account_id"]{if reply[key].as_str().filter(|s|!s.is_empty()).is_none(){reply[key]=v["tokens"][key].clone();}}
    let out=normalize_one(&reply)?;
    if out["tokens"]["access_token"].as_str().unwrap_or("").is_empty(){return Err("续期未返回访问令牌".into());}Ok(out)
}
fn provider_config(h:&Path,base:&str,label:&str)->Result<(),String>{
    let file=h.join("config.toml");
    let mut cfg:toml::Value=fs::read_to_string(&file).map_err(|e|e.to_string())?.parse().map_err(|_|"账号配置无法解析")?;
    if base.trim().is_empty(){
        if let Some(url)=cfg.get("model_providers").and_then(|p|p.get("openai")).and_then(|p|p.get("base_url")).and_then(toml::Value::as_str){
            if url.trim_end_matches('/')!="https://api.openai.com/v1"{return Err("此账号把官方供应商地址改成了其他地址，未向该地址配置ChatGPT凭据。".into());}
        }
        cfg.as_table_mut().ok_or("账号配置必须是TOML表")?.insert("model_provider".into(),toml::Value::String("openai".into()));
        return atomic_write(&file,toml::to_string(&cfg).map_err(|e|e.to_string())?.as_bytes());
    }
    let base=base.trim().trim_end_matches('/');
    if !(base.starts_with("https://")||base.starts_with("http://127.0.0.1:")||base.starts_with("http://localhost:")) || base.contains(['\n','\r','"','@','?','#']) {return Err("Base URL需要HTTPS地址或本机HTTP端口，且不能包含凭据和查询参数".into());}
    let table=cfg.as_table_mut().ok_or("账号配置必须是TOML表")?;
    table.insert("model_provider".into(),toml::Value::String("agentcockpit".into()));
    let providers=table.entry("model_providers".to_string()).or_insert_with(||toml::Value::Table(Default::default())).as_table_mut().ok_or("model_providers必须是配置表")?;
    let mut provider=toml::map::Map::new();
    for(k,v)in[("name",toml::Value::String(if label.trim().is_empty(){"API 账号".into()}else{label.into()})),("base_url",toml::Value::String(base.into())),("wire_api",toml::Value::String("responses".into())),("requires_openai_auth",toml::Value::Boolean(true))]{provider.insert(k.into(),v);}
    providers.insert("agentcockpit".into(),toml::Value::Table(provider));
    atomic_write(&file,toml::to_string(&cfg).map_err(|e|e.to_string())?.as_bytes())
}
fn sync_provider_label(m:&Value)->Result<bool,String>{
    if is_native(m){return Ok(false);}
    let h=home(m);check_plain_path(&h)?;
    if !under(&h,&reg::base_dir().join("chatgpt-auth")){return Err("账号运行目录超出受管范围".into());}
    let auth:Value=fs::read(h.join("auth.json")).ok().filter(|b|b.len()<=MAX_IMPORT).and_then(|b|serde_json::from_slice(&b).ok()).unwrap_or(Value::Null);
    if !auth["OPENAI_API_KEY"].as_str().is_some_and(|s|!s.is_empty()){return Ok(false);}
    let file=h.join("config.toml");check_plain_path(&file)?;
    let mut cfg:toml::Value=fs::read_to_string(&file).map_err(|e|e.to_string())?.parse().map_err(|_|"账号配置无法解析")?;
    let id=cfg.get("model_provider").and_then(toml::Value::as_str).unwrap_or("openai").to_string();
    let label=m["name"].as_str().ok_or("账号名称缺失")?;
    if id=="openai" {
        provider_config(&h,"https://api.openai.com/v1",label)?;return Ok(true);
    }
    let provider=cfg.get_mut("model_providers").and_then(|p|p.get_mut(&id)).and_then(toml::Value::as_table_mut).ok_or("账号供应商配置不存在")?;
    if provider.get("name").and_then(toml::Value::as_str)==Some(label){return Ok(false);}
    provider.insert("name".into(),toml::Value::String(label.into()));
    atomic_write(&file,toml::to_string(&cfg).map_err(|e|e.to_string())?.as_bytes())?;Ok(true)
}
fn install_stdin(h:&Path,flag:&str,secret:&str)->Result<(),String>{
    let mut c=command(h)?;
    let mut child=c.args(["-c","cli_auth_credentials_store=\"file\"","login",flag])
        .stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null()).spawn().map_err(|e|e.to_string())?;
    if let Some(mut input)=child.stdin.take(){if input.write_all(secret.as_bytes()).is_err(){let _=child.kill();let _=child.wait();return Err("登录组件未能接收凭据".into());}}
    let start=Instant::now();loop{
        match child.try_wait().map_err(|e|e.to_string())?{
            Some(code)=>return if code.success(){Ok(())}else{Err("官方登录组件未接受这份凭据，请检查格式或重新授权。".into())},
            None=>{if start.elapsed()>Duration::from_secs(30){let _=child.kill();let _=child.wait();return Err("官方登录组件处理超时，已停止本次操作。".into());}}
        }std::thread::sleep(Duration::from_millis(100));
    }
}
fn import_one(m:&Value,value:&Value)->Result<Value,String>{
    let name=m["name"].as_str().unwrap_or("");
    if logins().lock().ok().and_then(|j|j.get(name).map(|j|j.state.lock().map(|s|s["running"]==true).unwrap_or(false))).unwrap_or(false){return Err("请先完成或取消当前授权，再导入凭据。".into());}
    let h=prepare_home(m)?;let value=refresh_if_needed(value)?;
    let auth_path=h.join("auth.json");let config_path=h.join("config.toml");
    let old_auth=fs::read(&auth_path).ok();let old_config=fs::read(&config_path).map_err(|e|e.to_string())?;
    let result=(||->Result<(),String>{if value["auth_mode"]=="apikey"{
        let base=value["base_url"].as_str().filter(|s|!s.trim().is_empty()).unwrap_or("https://api.openai.com/v1");
        provider_config(&h,base,name)?;
        install_stdin(&h,"--with-api-key",value["OPENAI_API_KEY"].as_str().ok_or("API Key为空")?)?;
    }else{
        provider_config(&h,"",name)?;
        if value["tokens"]["id_token"].as_str().unwrap_or("").is_empty() && value["tokens"]["refresh_token"].as_str().unwrap_or("").is_empty(){
            install_stdin(&h,"--with-access-token",value["tokens"]["access_token"].as_str().ok_or("访问令牌为空")?)?;
        }else{
            let mut stored=value.clone();stored.as_object_mut().map(|o|o.remove("email_hint"));
            atomic_write(&h.join("auth.json"),serde_json::to_vec_pretty(&stored).map_err(|e|e.to_string())?.as_slice())?;
        }
    }
    Ok(())})();
    if let Err(error)=result{
        let restore_config=atomic_write(&config_path,&old_config);
        let restore_auth=if let Some(bytes)=old_auth{atomic_write(&auth_path,&bytes)}else if auth_path.exists(){fs::remove_file(&auth_path).map_err(|e|e.to_string())}else{Ok(())};
        if restore_config.is_err()||restore_auth.is_err(){return Err(format!("{error}；凭据恢复未完成，请保留目录并检查。"));}
        return Err(error);
    }
    if m["chatgpt_pending"]!=true {let mut fresh=meta(name)?;fresh.as_object_mut().unwrap().remove("chatgpt_quota");reg::save_instance_checked(APP,&fresh)?;quota::invalidate(&fresh);}
    let fresh=if m["chatgpt_pending"]==true{m.clone()}else{meta(name)?};
    Ok(json!({"msg":"凭据已保存到这个账号的独立目录，实际可用性由官方客户端验证","status":status(&fresh)}))
}
fn local_credentials(p:&Value)->Result<Value,String>{
    let source=p["source"].as_str().filter(|s|!s.trim().is_empty()).map(PathBuf::from).unwrap_or_else(native_home);
    let source=if source.is_dir(){source.join("auth.json")}else{source};
    check_plain_path(&source)?;
    if !source.is_file(){return Err("所选目录没有auth.json。使用系统凭据库的账号，请改用独立客户端登录或浏览器授权。".into());}
    if fs::metadata(&source).map_err(|e|e.to_string())?.len()>MAX_IMPORT as u64{return Err("凭据文件超过8MB".into());}
    let text=fs::read_to_string(&source).map_err(|_|"凭据文件无法读取")?;
    let values=normalize_many(&text)?;
    if values.len()!=1{return Err("本机文件含有多个账号，请在Token／JSON页使用批量创建独立账号。".into());}
    Ok(values[0].clone())
}
fn local_import(m:&Value,p:&Value)->Result<Value,String>{
    import_one(m,&local_credentials(p)?)
}
fn batch_names(prefix:&str,count:usize)->Result<Vec<String>,String>{
    let prefix=prefix.trim();
    if prefix.is_empty()||reg::bad_account_name(prefix).is_some(){return Err("请填写有效的批量账号名前缀".into());}
    let names:Vec<_>=(1..=count).map(|n|format!("{prefix}-{n:02}")).collect();
    if names.iter().any(|n|reg::bad_account_name(n).is_some()){return Err("批量账号名称过长或无效，请缩短前缀".into());}
    Ok(names)
}
fn import_many(p:&Value)->Result<Value,String>{
    let values=normalize_many(p["text"].as_str().unwrap_or(""))?;
    let names=batch_names(p["prefix"].as_str().unwrap_or(""),values.len())?;
    // Preflight every destination before writing. Existing accounts are never reused.
    for name in &names {
        if reg::find_instance(APP,name).is_some(){return Err(format!("账号「{name}」已存在，请更换批量前缀。"));}
    }
    let mut rows=Vec::new();
    for (name,value) in names.iter().zip(&values) {
        let result=commit_new(&json!({"new_name":name,"history_mode":p["history_mode"]}),value);
        rows.push(match result{Ok(_)=>json!({"name":name,"ok":true,"msg":"凭据已保存"}),Err(e)=>json!({"name":name,"ok":false,"msg":e})});
    }
    let done=rows.iter().filter(|r|r["ok"]==true).count();
    Ok(json!({"msg":format!("批量导入完成：{}个成功，{}个未完成。失败项可以单独重新导入。",done,rows.len()-done),"rows":rows,"all_ok":done==values.len()}))
}
fn cancel(name:&str)->Result<Value,String>{
    let jobs=logins().lock().map_err(|_|"登录状态被占用")?;
    if let Some(j)=jobs.get(name){
        if let Ok(mut c)=j.child.lock(){if c.try_wait().map_err(|e|e.to_string())?.is_none(){c.kill().map_err(|e|e.to_string())?;let _=c.wait();}}
        if let Ok(mut s)=j.state.lock(){s["running"]=json!(false);s["phase"]=json!("cancelled");s["message"]=json!("已取消授权，已有资料与凭据保留");}
    }Ok(json!({"msg":"已取消这个账号的授权流程"}))
}
fn login_url(name:&str)->Result<String,String>{
    let jobs=logins().lock().map_err(|_|"登录状态被占用")?;let job=jobs.get(name).ok_or("没有进行中的授权")?;
    let s=job.state.lock().map_err(|_|"登录状态被占用")?;
    if s["running"]!=true{return Err("授权流程已结束，请重新开始。".into());}
    s["url"].as_str().filter(|s|s.starts_with("https://auth.openai.com/")).map(str::to_owned).ok_or_else(||"尚未取得官方授权地址".into())
}
fn open_authorization(name:&str,private:bool)->Result<Value,String>{
    let url=login_url(name)?;
    if private {
        let pf86=PathBuf::from(std::env::var_os("ProgramFiles(x86)").unwrap_or_default());
        let pf=PathBuf::from(std::env::var_os("ProgramFiles").unwrap_or_default());
        for (exe,arg) in [(pf86.join("Microsoft/Edge/Application/msedge.exe"),"--inprivate"),(pf.join("Google/Chrome/Application/chrome.exe"),"--incognito")] {
            if exe.is_file(){Command::new(exe).args([arg,&url]).spawn().map_err(|e|e.to_string())?;return Ok(json!({"msg":"已打开独立授权窗口"}));}
        }
        return Err("未找到支持无痕窗口的Edge或Chrome，可复制授权链接到其他浏览器。".into());
    }
    Command::new("rundll32").args(["url.dll,FileProtocolHandler",&url]).spawn().map_err(|e|e.to_string())?;
    Ok(json!({"msg":"已打开官方授权页"}))
}
fn unregister(name:&str)->Result<Value,String>{
    let _=meta(name)?;cancel(name)?;
    let file=reg::instances_dir(APP).join(format!("{name}.json"));
    fs::remove_file(&file).map_err(|e|e.to_string())?;
    let mut cfg=reg::load_config();
    if reg::swaps_of(&cfg).get(APP).and_then(|v|v["instance"].as_str())==Some(name){reg::set_swap(&mut cfg,APP,None);reg::save_config(&cfg);}
    Ok(json!({"msg":"已移除账号登记，资料目录和登录凭据均保留"}))
}
fn rename(name:&str,new:&str)->Result<Value,String>{
    if new.is_empty()||reg::bad_account_name(new).is_some(){return Err("新账号名称无效".into());}
    if reg::find_instance(APP,new).is_some(){return Err("同名账号已存在".into());}
    let mut m=meta(name)?;cancel(name)?;
    // Keep the existing physical paths; changing a label never moves a profile.
    let h=home(&m);if !is_native(&m){m["chatgpt_home"]=json!(h.to_string_lossy());}
    m["name"]=json!(new);reg::save_instance_checked(APP,&m)?;
    let old=reg::instances_dir(APP).join(format!("{name}.json"));
    if let Err(e)=fs::remove_file(old){return Err(format!("新登记已保存，旧登记未移除，请核对：{e}"));}
    let label_pending=sync_provider_label(&m).is_err();
    let mut cfg=reg::load_config();if reg::swaps_of(&cfg).get(APP).and_then(|v|v["instance"].as_str())==Some(name){reg::set_swap(&mut cfg,APP,Some(json!({"instance":new,"in_place":true})));reg::save_config(&cfg);}
    Ok(json!({"msg":if label_pending{"已修改卡片名称，客户端显示名将在配置修复后更新"}else{"已修改账号名称，客户端显示名与卡片一致"},"name":new}))
}
fn handle(act:&str,p:&Value)->Result<Value,String>{
    let name=p["name"].as_str().unwrap_or("").trim();
    if act.starts_with("chatgpt_official_"){return official::handle(act,p);}
    if act=="chatgpt_add_import"{let values=normalize_many(p["text"].as_str().unwrap_or(""))?;if values.len()!=1{return Err("请选择一份凭据；批量导入请使用批量创建入口".into());}let v=with_import_base(values[0].clone(),p["base_url"].as_str().unwrap_or(""))?;return commit_new(p,&v);}
    if act=="chatgpt_add_apikey"{return commit_new(p,&normalize_one(&json!({"OPENAI_API_KEY":p["key"],"base_url":p["base_url"]}))?);}
    if act=="chatgpt_add_local"{return commit_new(p,&local_credentials(p)?);}
    if act=="chatgpt_add_oauth"{let n=new_account_name(p,&Value::Null)?;let mut request=p.clone();request["name"]=json!(n);request["origin"]=json!("fresh");create(&request)?;let mut result=start_login(&meta(&n)?,p["method"]=="device")?;result["name"]=json!(n);return Ok(result);}
    if !matches!(act,"chatgpt_status"|"chatgpt_cancel"|"chatgpt_open_home") && official::busy_target(name){return Err("这个账号正在完成临时官方登录，请等待完成或先取消该流程。".into());}
    if act=="new_instance"||act=="chatgpt_create"{return create(p);}
    if act=="chatgpt_import_many"{return import_many(p);}
    if act=="launch_app"{
        let profile=native_profile()?;check_plain_path(&profile)?;
        return launch(&json!({"name":"","data_dir":profile.to_string_lossy()}));
    }
    if ["swap_set","swap_clear","rehome","restore","clean_cache","auth_switch","rebind_instance","codex_login","codex_logout"].contains(&act){return Err("ChatGPT资料目录受保护：请使用专用登录页和独立账号启动，旧换号、搬移和凭据覆盖接口已停用。".into());}
    if act=="delete"{return unregister(name);}
    if act=="rename_instance"{return rename(name,p["to"].as_str().unwrap_or(""));}
    let m=meta(name)?;
    match act {
        "launch"|"chatgpt_show_window"=>launch(&m),
        "kill"=>kill_instance(&m),
        "chatgpt_status"=>Ok(json!({"status":status(&m)})),
        "chatgpt_quota_refresh"=>quota::refresh(&m),
        "chatgpt_project_repair"=>{
            let queued=history::queue_project_repair(&m)?;
            if instance_pids(&m,&procs::snapshot_fresh()).is_empty(){history::apply_pending(&m)?;}
            let state=project_status(&meta(name)?);
            Ok(json!({"msg":state["message"].as_str().or_else(||queued["message"].as_str()).unwrap_or("项目列表已检查"),"project_repair":state}))
        },
        "chatgpt_login"=>if p["method"]=="client"{official::start(p)}else{start_login(&m,p["method"]=="device")},
        "chatgpt_cancel"=>cancel(name),
        "chatgpt_open_authorization"=>open_authorization(name,p["private"].as_bool().unwrap_or(false)),
        "chatgpt_callback"=>{
            let auth=login_url(name)?;let callback=p["url"].as_str().unwrap_or("").trim();validate_callback(&auth,callback)?;
            ureq::AgentBuilder::new().timeout(Duration::from_secs(8)).build().get(callback).call().map_err(|_|"回调未提交成功，请检查官方登录进程是否仍在运行")?;
            Ok(json!({"msg":"回调已提交给本次官方登录进程"}))
        },
        "chatgpt_import"=>{let values=normalize_many(p["text"].as_str().unwrap_or(""))?;if values.len()!=1{return Err("这里需要一份账号凭据。多份JSON或令牌请逐个导入到独立账号。".into());}import_one(&m,&values[0])},
        "chatgpt_local_import"=>local_import(&m,p),
        "chatgpt_open_home"=>{let h=home(&m);check_plain_path(&h)?;let(ok,msg)=crate::space::open_dir(&h.to_string_lossy());if ok{Ok(json!({"msg":msg}))}else{Err(msg)}},
        "chatgpt_apikey"=>import_one(&m,&normalize_one(&json!({"OPENAI_API_KEY":p["key"],"base_url":p["base_url"]}))?),
        _=>Err("未知ChatGPT操作".into()),
    }
}
pub fn action(act:&str,p:&Value)->Option<Value>{
    let scoped=p["app"]==APP;
    let generic=["new_instance","launch","launch_app","kill","delete","rename_instance","swap_set","swap_clear","rehome","restore","clean_cache","auth_switch","rebind_instance","codex_login","codex_logout"];
    if !act.starts_with("chatgpt_") && !(scoped&&generic.contains(&act)){return None;}
    if act.starts_with("chatgpt_") && p["app"].as_str().is_some_and(|a|a!=APP){return Some(json!({"ok":false,"msg":"该登录操作只适用于ChatGPT桌面账号"}));}
    // Read-only polling and cancellation remain available while an import waits on the network.
    let _guard=if matches!(act,"chatgpt_status"|"chatgpt_cancel"|"chatgpt_quota_refresh"|"chatgpt_official_status"|"chatgpt_official_cancel"|"chatgpt_official_list"){None}else{Some(IO_LOCK.lock().unwrap_or_else(|e|e.into_inner()))};
    Some(match handle(act,p){Ok(mut v)=>{v["ok"]=json!(true);v},Err(e)=>json!({"ok":false,"msg":e})})
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test] fn import_formats_and_arrays(){
        let cases=[r#"{"tokens":{"access_token":"abc","refresh_token":"rt_test","account_id":"acct"}}"#,r#"{"accessToken":"abc"}"#,r#"{"session":{"accessToken":"abc"}}"#,r#"{"accounts":[{"access_token":"abc"}]}"#];
        for input in cases{let out=normalize_many(input).unwrap();assert_eq!(out.len(),1);assert_eq!(out[0]["tokens"]["access_token"],"abc");}
        assert!(normalize_many(r#"[{"access_token":"abc"},{}]"#).is_err());
        assert!(normalize_many(r#"{"tokens":{"access_token":"abc\ndef"}}"#).is_err());
    }
    #[test] fn key_is_not_returned_in_summary(){let s=summary(&normalize_one(&json!({"OPENAI_API_KEY":"sk-test-secret"})).unwrap());assert_eq!(s["has_auth"],true);assert!(!s.to_string().contains("sk-test"));}
    #[test] fn callback_binds_to_current_flow_and_local_listener(){
        let a="https://auth.openai.com/oauth/authorize?state=abc&redirect_uri=http%3A%2F%2Flocalhost%3A1455%2Fauth%2Fcallback";
        assert!(validate_callback(a,"http://localhost:1455/auth/callback?code=x&state=abc").is_ok());
        for bad in ["http://localhost:1455/auth/callback?code=x&state=wrong","https://evil.example/auth/callback?code=x&state=abc","http://localhost:8888/auth/callback?code=x&state=abc"]{assert!(validate_callback(a,bad).is_err());}
    }
    #[test] fn profile_paths_reject_parent_traversal(){assert!(check_plain_path(Path::new("relative/path")).is_err());assert!(check_plain_path(&reg::home().join("abc/../def")).is_err());}
    #[test] fn login_output_hides_secrets(){assert_eq!(clean_output("refresh_token: do-not-print"),"[凭据内容已隐藏]");assert_eq!(clean_output("\u{1b}[32mhello\u{1b}[0m"),"hello");}
    #[test] fn old_swap_apis_are_blocked_before_reading_user_data(){for act in ["swap_set","swap_clear","rehome","restore","clean_cache","auth_switch","rebind_instance","codex_login","codex_logout"]{let r=action(act,&json!({"app":APP,"name":"missing"})).unwrap();assert_eq!(r["ok"],false);assert!(r["msg"].as_str().unwrap().contains("受保护"));}}
    #[test] fn batch_destinations_are_distinct_and_cannot_escape_their_root(){assert_eq!(batch_names("import",2).unwrap(),vec!["import-01","import-02"]);assert!(batch_names("../outside",2).is_err());assert!(batch_names("",1).is_err());}
    #[test] fn instance_matching_does_not_claim_neighbor_or_default_accounts(){
        let m=json!({"data_dir":"C:/managed/account"});
        let snap=HashMap::from([(1,procs::ProcInfo{name:"chatgpt.exe".into(),cmdline:"chatgpt.exe --user-data-dir=C:\\managed\\account".into()}),(2,procs::ProcInfo{name:"chatgpt.exe".into(),cmdline:"chatgpt.exe --user-data-dir=C:\\managed\\account two".into()}),(3,procs::ProcInfo{name:"chatgpt.exe".into(),cmdline:"chatgpt.exe".into()}),(4,procs::ProcInfo{name:"chatgpt.exe".into(),cmdline:String::new()})]);
        assert_eq!(instance_pids(&m,&snap),vec![1]);
        assert_eq!(profile_arg("chatgpt.exe --user-data-dir=\"C:\\has spaces\\a\" --type=renderer").as_deref(),Some("c:/has spaces/a"));
        let native=json!({"data_dir":native_profile().unwrap().to_string_lossy()});assert_eq!(instance_pids(&native,&snap),vec![3]);
        assert_eq!(profile_arg("chatgpt.exe --user-data-dir \"C:\\has spaces\\a\" --type=renderer").as_deref(),Some("c:/has spaces/a"));
        let unknown=HashMap::from([(5,procs::ProcInfo{name:"chatgpt.exe".into(),cmdline:"chatgpt.exe --user-data-dir".into()})]);
        assert!(instance_pids(&native,&unknown).is_empty());
    }
    fn temp_dir()->PathBuf{let p=std::env::temp_dir().join(format!("agentcockpit-chatgpt-test-{}",SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()));fs::create_dir_all(&p).unwrap();p}
    fn clean_temp(p:&Path){assert!(p.file_name().unwrap().to_string_lossy().starts_with("agentcockpit-chatgpt-test-"));for f in fs::read_dir(p).unwrap(){fs::remove_file(f.unwrap().path()).unwrap();}fs::remove_dir(p).unwrap();}
    #[test] fn atomic_credentials_keep_previous_and_remove_temporary_files(){
        let dir=temp_dir();let file=dir.join("auth.json");atomic_write(&file,b"old").unwrap();atomic_write(&file,b"new").unwrap();
        assert_eq!(fs::read(&file).unwrap(),b"new");let files:Vec<_>=fs::read_dir(&dir).unwrap().map(|e|e.unwrap().path()).collect();
        assert!(files.iter().any(|p|p.extension().is_some_and(|s|s=="previous")&&fs::read(p).unwrap()==b"old"));
        assert!(!files.iter().any(|p|p.extension().is_some_and(|s|s=="tmp")));clean_temp(&dir);
    }
    #[test] fn oauth_configuration_cannot_reuse_custom_provider(){
        let dir=temp_dir();let file=dir.join("config.toml");fs::write(&file,"model = 'kept'\ncli_auth_credentials_store = 'file'\n").unwrap();
        provider_config(&dir,"http://127.0.0.1:8788/v1","My API Account").unwrap();
        let cfg:toml::Value=fs::read_to_string(&file).unwrap().parse().unwrap();assert_eq!(cfg["model_provider"].as_str(),Some("agentcockpit"));
        assert_eq!(cfg["model_providers"]["agentcockpit"]["name"].as_str(),Some("My API Account"));
        provider_config(&dir,"","OAuth Account").unwrap();let cfg:toml::Value=fs::read_to_string(&file).unwrap().parse().unwrap();assert_eq!(cfg["model_provider"].as_str(),Some("openai"));assert_eq!(cfg["model"].as_str(),Some("kept"));
        assert!(provider_config(&dir,"https://user:secret@invalid.example","Account").is_err());clean_temp(&dir);
    }
    #[test] fn copying_or_swapping_native_profile_is_never_attempted(){
        assert!(!crate::swap::swap_set(APP,"missing",true).ok);assert!(!crate::swap::swap_clear(APP).0);assert!(!crate::swap::rehome(APP,"missing",true).0);
        let p=std::env::temp_dir().join("chatgpt-copy-must-not-exist");assert!(crate::space::seed_instance(APP,&p,"copy",Some(&native_home())).is_err());
    }
    #[test] fn import_preserves_oidc_fields_and_uses_timestamp_string(){let v=normalize_one(&json!({"tokens":{"access_token":"a","id_token":"i","refresh_token":"r","account_id":"x"}})).unwrap();assert_eq!(v["tokens"]["id_token"],"i");assert!(v["last_refresh"].is_string());}
    #[test] fn custom_import_base_is_preserved_only_for_api_keys(){let key=normalize_one(&json!({"OPENAI_API_KEY":"sk-test"})).unwrap();assert_eq!(with_import_base(key,"https://relay.example/v1").unwrap()["base_url"],"https://relay.example/v1");let token=normalize_one(&json!({"access_token":"abc"})).unwrap();assert!(with_import_base(token,"https://relay.example/v1").is_err());}
    #[test] #[ignore="explicit maintenance of registered account labels and queued project visibility repairs"]
    fn maintain_registered_chatgpt_cards(){
        if std::env::var("AGENTCOCKPIT_CARD_MAINTENANCE").as_deref()!=Ok("1"){return;}
        let _guard=IO_LOCK.lock().unwrap_or_else(|e|e.into_inner());
        let accounts=reg::load_instances(APP);let count=accounts.len();
        let mut labels=0;let mut pending=0;let mut applied=0;
        for m in accounts{
            if is_native(&m){continue;}
            let auth=fs::read(home(&m).join("auth.json")).ok();
            if sync_provider_label(&m).unwrap(){labels+=1;}
            let _=history::queue_project_repair(&m).unwrap();
            if instance_pids(&m,&procs::snapshot_fresh()).is_empty(){let result=history::apply_pending(&m).unwrap();if result["applied"]==true{applied+=1;}}
            let current=meta(m["name"].as_str().unwrap()).unwrap();
            if history::project_status(&current)["state"]=="pending"{pending+=1;}
            assert!(fs::read(home(&m).join("auth.json")).ok()==auth,"maintenance must preserve account credentials");
        }
        assert_eq!(reg::load_instances(APP).len(),count);
        println!("account_count={count} provider_labels_updated={labels} project_repairs_pending={pending} applied={applied}");
    }
    #[cfg(windows)]
    #[test] #[ignore="explicit real API window Close and Restore smoke; leaves selected API window open"]
    fn registered_api_window_restore_smoke(){
        if std::env::var("AGENTCOCKPIT_WINDOW_SMOKE").as_deref()!=Ok("1"){return;}
        let _guard=IO_LOCK.lock().unwrap_or_else(|e|e.into_inner());
        let initial=procs::snapshot_fresh();
        let before_main:Vec<u32>=initial.iter().filter(|(_,p)|p.name=="chatgpt.exe"&&!p.cmdline.trim().is_empty()&&!p.cmdline.contains("--type=")).map(|(pid,_)|*pid).collect();
        let host=std::env::var_os("CODEX_HOME").map(PathBuf::from);
        let accounts=reg::load_instances(APP);
        let mut eligible:Vec<Value>=accounts.iter().filter(|m|{
            let h=home(m);
            if is_native(m)||host.as_ref()==Some(&h)||h.to_string_lossy().contains("1793065077430657200")||!instance_pids(m,&initial).is_empty(){return false;}
            fs::read(h.join("auth.json")).ok().and_then(|b|serde_json::from_slice::<Value>(&b).ok()).is_some_and(|v|summary(&v)["mode"]=="apikey"&&summary(&v)["has_auth"]==true)
        }).cloned().collect();
        eligible.sort_by(|a,b|a["name"].as_str().cmp(&b["name"].as_str()));
        let m=eligible.into_iter().next().expect("没有完全未运行的已登记API账号；未关闭任何现有窗口，请根任务决定后续验证。");
        let name=m["name"].as_str().unwrap();let original_home=home(&m);let original_profile=m["data_dir"].clone();
        // Keep raw bytes only in memory. Do not print credentials, hashes or paths.
        let mut auth_files=std::collections::BTreeSet::new();
        auth_files.insert(native_home().join("auth.json"));
        if let Some(h)=host{auth_files.insert(h.join("auth.json"));}
        for account in &accounts{auth_files.insert(home(account).join("auth.json"));}
        if let Ok(entries)=fs::read_dir(reg::base_dir().join("chatgpt-auth")){
            for entry in entries{let entry=entry.unwrap();if entry.file_type().unwrap().is_dir(){auth_files.insert(entry.path().join("auth.json"));}}
        }
        let auth_before:Vec<_>=auth_files.into_iter().filter(|p|p.is_file()).map(|p|{let b=fs::read(&p).expect("读取凭据保护快照失败");(p,b)}).collect();
        assert!(instance_pids(&m,&procs::snapshot_fresh()).is_empty(),"所选API刚被其他操作启动；未关闭窗口。");
        launch(&m).expect("API窗口首次启动失败；未关闭其他窗口");
        let opened=procs::snapshot_fresh();let first_pids=instance_pids(&m,&opened);
        assert!(!first_pids.is_empty()&&windows::visible(&first_pids),"未确认所选API的真实窗口");
        assert!(first_pids.iter().all(|pid|!before_main.contains(pid)),"所选窗口不是本测试新启动的，未关闭。");
        assert!(windows::close_for_smoke(&first_pids),"没有向本测试窗口发送关闭指令");
        let closed_at=Instant::now();
        while closed_at.elapsed()<Duration::from_secs(8)&&window_visible(&m,&procs::snapshot_fresh()){
            std::thread::sleep(Duration::from_millis(100));
        }
        assert!(!window_visible(&m,&procs::snapshot_fresh()),"所选API未进入关闭或隐藏状态；未操作任何其他窗口");
        let result=handle("chatgpt_show_window",&json!({"app":APP,"name":name})).expect("所选API窗口恢复失败");
        let after=procs::snapshot_fresh();let final_pids=instance_pids(&m,&after);
        assert!(windows::visible(&final_pids),"恢复操作返回后所选API窗口不可见");
        assert!(before_main.iter().all(|pid|after.get(pid).is_some_and(|p|p.name=="chatgpt.exe")),"原有客户端主进程状态变化，请核查；本测试未向它们发送关闭操作");
        for(path,bytes)in auth_before{assert!(fs::read(path).ok().as_deref()==Some(bytes.as_slice()),"测试前后的凭据字节不同");}
        let current=meta(name).unwrap();assert_eq!(home(&current),original_home);assert_eq!(current["data_dir"],original_profile);
        println!("window_restore_smoke name={name} visible=true profile_unchanged=true auth_unchanged=true other_main_processes={} first_pids={first_pids:?} restored_pids={final_pids:?} result={}",before_main.len(),result["msg"].as_str().unwrap_or("opened"));
    }
}
