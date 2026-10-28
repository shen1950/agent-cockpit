//! A disposable official-client login. Only this session's client and marked
//! directory may be stopped/removed. Permanent cards are published after import.
use super::*;
use std::sync::atomic::{AtomicBool, Ordering};

const MARKER: &str = ".agentcockpit-login.json";
struct Session { request: Value, dir: PathBuf, state: Mutex<Value>, cancel: AtomicBool }
static SESSIONS: OnceLock<Mutex<HashMap<String,Arc<Session>>>> = OnceLock::new();
// Short-lived state transitions only. Never held while waiting for a client,
// copying history, refreshing a token, or acquiring IO_LOCK.
static CONTROL: Mutex<()> = Mutex::new(());
fn sessions()-> &'static Mutex<HashMap<String,Arc<Session>>> {SESSIONS.get_or_init(||Mutex::new(HashMap::new()))}
fn root()->PathBuf {reg::base_dir().join("chatgpt-temp-login")}
fn valid_id(id:&str)->bool { !id.is_empty() && id.len()<80 && id.bytes().all(|c|c.is_ascii_hexdigit()||c==b'-') }
fn view(s:&Session)->Value {s.state.lock().unwrap_or_else(|e|e.into_inner()).clone()}
fn persist(s:&Session)->Result<(),String>{
    check_plain_path(&s.dir)?;
    atomic_write(&s.dir.join(MARKER),serde_json::to_vec(&json!({"owner":"AgentCockpit","request":s.request,"session":view(s)})).map_err(|e|e.to_string())?.as_slice())
}
fn phase(s:&Session,name:&str,message:&str){
    let _control=CONTROL.lock().unwrap_or_else(|e|e.into_inner());
    {let mut v=s.state.lock().unwrap_or_else(|e|e.into_inner());v["phase"]=json!(name);v["message"]=json!(message);}
    // Persistence failures never grant permission to clean an unmarked directory.
    let _=persist(s);
}
fn terminal(s:&Session,name:&str,message:&str,retry:bool){
    let _control=CONTROL.lock().unwrap_or_else(|e|e.into_inner());
    {let mut v=s.state.lock().unwrap_or_else(|e|e.into_inner());v["phase"]=json!(name);v["message"]=json!(message);v["running"]=json!(false);v["can_retry"]=json!(retry);}
    if s.dir.exists(){let _=persist(s);}
}
fn profile(s:&Session)->Value {json!({"name":"","data_dir":s.dir.join("browser")})}
fn read_auth(s:&Session)->Result<Value,String>{
    let p=s.dir.join("home/auth.json");check_plain_path(&p)?;
    if fs::metadata(&p).map_err(|_|"尚未读取到登录凭据")?.len()>MAX_IMPORT as u64{return Err("临时登录凭据超过大小限制".into());}
    let data=fs::read(&p).map_err(|_|"无法读取临时登录凭据")?;
    serde_json::from_slice(&data).map_err(|_|"临时登录凭据尚未写入完成".into())
}
fn ready(v:&Value)->bool {
    v["OPENAI_API_KEY"].as_str().is_some_and(|s|!s.trim().is_empty()) ||
    ["access_token","id_token","refresh_token"].iter().all(|k|v["tokens"][k].as_str().is_some_and(|s|!s.trim().is_empty()))
}
fn checked_tree(path:&Path)->Result<(),String>{
    check_plain_path(path)?;
    if path.is_dir(){for entry in fs::read_dir(path).map_err(|e|e.to_string())?{checked_tree(&entry.map_err(|e|e.to_string())?.path())?;}}
    Ok(())
}
fn owned_dir(dir:&Path,expected_root:&Path,id:&str)->Result<(),String>{
    if !valid_id(id)||dir.parent()!=Some(expected_root)||dir.file_name().and_then(|s|s.to_str())!=Some(id){return Err("临时目录不属于本次登录，已停止清理".into());}
    checked_tree(dir)?;
    let mark:Value=serde_json::from_slice(&fs::read(dir.join(MARKER)).map_err(|_|"临时目录缺少归属标记，已保留")?).map_err(|_|"临时目录标记损坏，已保留")?;
    if mark["owner"]!="AgentCockpit"||mark["session"]["id"]!=id{return Err("临时目录标记不匹配，已保留".into());}
    Ok(())
}
fn clean(s:&Session)->Result<(),String>{
    let id=view(s)["id"].as_str().unwrap_or("").to_owned();
    owned_dir(&s.dir,&root(),&id)?;
    if !instance_pids(&profile(s),&procs::snapshot_fresh()).is_empty(){return Err("临时客户端仍在运行，临时目录已保留".into());}
    // Keep the ownership marker until every other child is gone, so a file lock
    // cannot strand an unrecognisable temporary directory on the next restart.
    for entry in fs::read_dir(&s.dir).map_err(|e|e.to_string())? {
        let path=entry.map_err(|e|e.to_string())?.path();if path.file_name().and_then(|s|s.to_str())==Some(MARKER){continue;}
        let result=if path.is_dir(){fs::remove_dir_all(&path)}else{fs::remove_file(&path)};
        result.map_err(|_|"临时配置仍被占用，可重试清理".to_string())?;
    }
    fs::remove_file(s.dir.join(MARKER)).map_err(|e|e.to_string())?;
    if fs::remove_dir(&s.dir).is_err(){let _=persist(s);return Err("临时目录仍被占用，可重试清理".into());}
    Ok(())
}
#[cfg(windows)]
fn request_close(pids:&[u32]){
    #[link(name="user32")] extern "system" {
        fn EnumWindows(callback:unsafe extern "system" fn(isize,isize)->i32,param:isize)->i32;
        fn GetWindowThreadProcessId(hwnd:isize,pid:*mut u32)->u32;
        fn PostMessageW(hwnd:isize,msg:u32,wparam:usize,lparam:isize)->i32;
    }
    unsafe extern "system" fn visit(hwnd:isize,param:isize)->i32{
        let pids=&*(param as *const Vec<u32>);let mut pid=0;GetWindowThreadProcessId(hwnd,&mut pid);
        if pids.contains(&pid){PostMessageW(hwnd,0x0010,0,0);}1
    }
    let owned=pids.to_vec();unsafe {EnumWindows(visit,&owned as *const Vec<u32> as isize);}
}
#[cfg(not(windows))] fn request_close(_pids:&[u32]){}
fn close_client(s:&Session)->Result<(),String>{
    let m=profile(s);let pids=instance_pids(&m,&procs::snapshot_fresh());
    if pids.is_empty(){return Ok(());}
    request_close(&pids);
    let until=Instant::now()+Duration::from_secs(8);
    while Instant::now()<until {if instance_pids(&m,&procs::snapshot_fresh()).is_empty(){return Ok(());}std::thread::sleep(Duration::from_millis(350));}
    // Re-evaluate exact profile ownership immediately before stopping its tree.
    procs::kill_pids(&instance_pids(&m,&procs::snapshot_fresh()));
    let until=Instant::now()+Duration::from_secs(5);
    while Instant::now()<until {if instance_pids(&m,&procs::snapshot_fresh()).is_empty(){return Ok(());}std::thread::sleep(Duration::from_millis(350));}
    Err("无法确认临时客户端已经关闭，临时登录信息已保留".into())
}
// Kept separate so tests verify the irreversible ordering without opening clients.
fn final_sequence<T>(mut close:impl FnMut()->Result<(),String>,mut read:impl FnMut()->Result<T,String>,mut import:impl FnMut(T)->Result<(),String>,mut cleanup:impl FnMut()->Result<(),String>)->Result<(),String>{
    close()?;let credentials=read()?;import(credentials)?;cleanup()
}
fn finish(s:&Session)->Result<(),String>{
    final_sequence(
        ||{phase(s,"closing","正在关闭本次登录的临时客户端");close_client(s)},
        ||{
            phase(s,"importing","正在读取最终登录信息");
            if view(s)["imported"]==true{return Ok(Value::Null);}
            for _ in 0..5 {if let Ok(v)=read_auth(s){if ready(&v){return normalize_one(&v);}}std::thread::sleep(Duration::from_millis(200));}
            Err("关闭临时客户端后未能读取完整登录信息，临时配置已保留，可重试".into())
        },
        |v|{
            if view(s)["imported"]==true{return Ok(());}
            if s.cancel.load(Ordering::SeqCst){return Err("已请求取消，尚未导入".into());}
            let _guard=IO_LOCK.lock().unwrap_or_else(|e|e.into_inner());
            if s.cancel.load(Ordering::SeqCst){return Err("已请求取消，尚未导入".into());}
            let target=s.request["name"].as_str().unwrap_or("");
            let id=view(s)["id"].as_str().unwrap_or("").to_owned();
            // A published session identity makes retries idempotent, even if the
            // application exited between publishing the card and persisting the marker.
            let found=reg::load_instances(APP).into_iter().find(|m|m["official_session"]==id);
            let result=if let Some(m)=found{json!({"name":m["name"]})}else if target.is_empty(){
                let mut p=s.request.clone();p["official_session"]=json!(id);commit_new(&p,&v)?
            }else{
                let mut m=meta(target)?;
                if m["data_dir"]!=s.request["target_profile"]||home(&m).to_string_lossy()!=s.request["target_home"].as_str().unwrap_or(""){return Err("目标账号目录已变化，临时凭据已保留，请重新选择账号".into());}
                history::seed(&m)?;import_one(&m,&v)?;m=meta(target)?;m["official_session"]=json!(id);reg::save_instance_checked(APP,&m)?;json!({"name":target})
            };
            {let mut state=s.state.lock().unwrap_or_else(|e|e.into_inner());state["imported"]=json!(true);state["name"]=result["name"].clone();}
            persist(s)
        },
        ||{phase(s,"cleaning","正在清理本次临时配置");clean(s)}
    )
}
fn discard(s:&Session){
    phase(s,"closing","正在关闭本次登录的临时客户端");
    let r=close_client(s).and_then(|_|{phase(s,"cleaning","正在清理已取消的临时配置");clean(s)});
    match r{Ok(())=>terminal(s,"cancelled","已取消本次登录，原有账号保持原状",false),Err(e)=>terminal(s,"failed",&e,true)}
}
fn worker(s:Arc<Session>,retry:bool){
    let result=(||->Result<(),String>{
        if !retry {
            phase(&s,"preparing","正在准备临时配置");
            let auth_home=s.dir.join("home");let browser=s.dir.join("browser");
            check_plain_path(&auth_home)?;check_plain_path(&browser)?;
            fs::create_dir_all(&auth_home).map_err(|e|e.to_string())?;fs::create_dir_all(&browser).map_err(|e|e.to_string())?;
            atomic_write(&auth_home.join("config.toml"),b"cli_auth_credentials_store = 'file'\nmodel_provider = 'openai'\n[features]\nsecret_auth_storage = false\n")?;
            if s.cancel.load(Ordering::SeqCst){return Err("取消".into());}
            phase(&s,"launching","正在打开官方客户端");
            let exe=reg::resolve_exe(&reg::app_def(APP).ok_or("未登记客户端")?).ok_or("未找到官方客户端")?;
            procs::launch_pwa_isolated(&exe,&browser,Some(&auth_home))?;
            phase(&s,"waiting","请在临时官方客户端中完成登录");
            let start=Instant::now();let mut seen_client=false;
            loop {
                if s.cancel.load(Ordering::SeqCst){return Err("取消".into());}
                if read_auth(&s).is_ok_and(|v|ready(&v)){break;}
                let present=!instance_pids(&profile(&s),&procs::snapshot_fresh()).is_empty();seen_client|=present;
                if !present && (seen_client||start.elapsed()>Duration::from_secs(25)){return Err("临时客户端已关闭或未能启动，登录未完成，可重新开始".into());}
                if start.elapsed()>Duration::from_secs(600){return Err("登录等待已超过10分钟，临时配置已保留".into());}
                std::thread::sleep(Duration::from_millis(1000));
            }
        }
        finish(&s)
    })();
    match result {
        Ok(())=>terminal(&s,"completed","登录信息已保存，临时配置已清理",false),
        Err(_) if s.cancel.load(Ordering::SeqCst) && view(&s)["imported"]!=true=>discard(&s),
        Err(e)=>{
            let closed=close_client(&s);let msg=match closed{Ok(())=>e,Err(c)=>format!("{e}；{c}")};
            let cancelled={
                let _control=CONTROL.lock().unwrap_or_else(|e|e.into_inner());
                let mut v=s.state.lock().unwrap_or_else(|e|e.into_inner());
                if s.cancel.load(Ordering::SeqCst)&&v["imported"]!=true{true}else{
                    v["phase"]=json!("failed");v["running"]=json!(false);v["message"]=json!(msg);v["can_retry"]=json!(true);
                    drop(v);if s.dir.exists(){let _=persist(&s);}false
                }
            };
            if cancelled{discard(&s);}
        }
    }
}
fn recover(id:&str)->Result<Arc<Session>,String>{
    if !valid_id(id){return Err("登录会话编号无效".into());}
    let mut all=sessions().lock().unwrap_or_else(|e|e.into_inner());if let Some(s)=all.get(id){return Ok(s.clone());}
    let dir=root().join(id);owned_dir(&dir,&root(),id)?;
    let mark:Value=serde_json::from_slice(&fs::read(dir.join(MARKER)).map_err(|e|e.to_string())?).map_err(|_|"临时登录标记无法读取")?;
    let mut state=mark["session"].clone();state["running"]=json!(false);state["phase"]=json!("failed");state["can_retry"]=json!(true);state["message"]=json!("发现上次未完成的临时登录，可重试或取消清理");
    let s=Arc::new(Session{request:mark["request"].clone(),dir,state:Mutex::new(state),cancel:AtomicBool::new(false)});all.insert(id.into(),s.clone());Ok(s)
}
pub(super) fn busy_target(name:&str)->bool{
    !name.is_empty()&&sessions().lock().unwrap_or_else(|e|e.into_inner()).values().any(|s|s.request["name"]==name&&view(s)["running"]==true)
}
fn ensure_idle()->Result<(),String>{
    if sessions().lock().unwrap_or_else(|e|e.into_inner()).values().any(|s|view(s)["running"]==true){return Err("已有一个临时登录流程正在进行，请先完成或取消".into());}Ok(())
}
pub(super) fn start(p:&Value)->Result<Value,String>{
    let _control=CONTROL.lock().unwrap_or_else(|e|e.into_inner());
    ensure_idle()?;
    let target=p["name"].as_str().unwrap_or("");let mut request=json!({"name":target,"new_name":p["new_name"],"history_mode":p["history_mode"],"history_source_home":p["history_source_home"]});
    if !target.is_empty(){let m=meta(target)?;writable(&m)?;request["target_profile"]=m["data_dir"].clone();request["target_home"]=json!(home(&m));}
    else{new_account_name(p,&Value::Null)?;}
    check_plain_path(&root())?;fs::create_dir_all(root()).map_err(|e|e.to_string())?;
    // Offer recovery before allocating another temporary login for this target.
    for entry in fs::read_dir(root()).map_err(|e|e.to_string())?.flatten(){
        if let Some(id)=entry.file_name().to_str(){if let Ok(s)=recover(id){let v=view(&s);if v["phase"]!="cancelled"&&v["phase"]!="completed"&&s.request["name"].as_str().unwrap_or("")==target{return Ok(json!({"session":v,"msg":"请先重试或取消上次临时登录"}));}}}
    }
    let id=format!("{:x}-{:x}",std::process::id(),SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos());
    let dir=root().join(&id);fs::create_dir(&dir).map_err(|e|e.to_string())?;
    let s=Arc::new(Session{request,dir,state:Mutex::new(json!({"id":id,"phase":"preparing","running":true,"message":"正在准备临时配置","can_retry":false,"imported":false})),cancel:AtomicBool::new(false)});
    persist(&s)?;sessions().lock().unwrap_or_else(|e|e.into_inner()).insert(id,s.clone());let snapshot=view(&s);std::thread::spawn(move||worker(s,false));Ok(json!({"session":snapshot}))
}
pub(super) fn handle(action:&str,p:&Value)->Result<Value,String>{
    if action=="chatgpt_official_start"{return start(p);}
    let _control=CONTROL.lock().unwrap_or_else(|e|e.into_inner());
    let s=recover(p["session_id"].as_str().unwrap_or(""))?;
    match action {
        "chatgpt_official_status"=>{},
        "chatgpt_official_cancel"=>{
            let v=view(&s);if v["phase"]=="completed"||v["phase"]=="cancelled"{return Ok(json!({"session":v}));}
            if v["imported"]==true{return Err("账号已经导入，临时清理请使用重试".into());}
            if v["phase"]=="importing"||v["phase"]=="cleaning"{return Err("正在保存最终登录信息，请等待本步骤结束".into());}
            s.cancel.store(true,Ordering::SeqCst);
            if v["running"]!=true {s.state.lock().unwrap_or_else(|e|e.into_inner())["running"]=json!(true);let s=s.clone();std::thread::spawn(move||discard(&s));}
        },
        "chatgpt_official_retry"=>{
            ensure_idle()?;if view(&s)["phase"]!="failed"{return Err("该流程不需要重试".into());}
            s.cancel.store(false,Ordering::SeqCst);s.state.lock().unwrap_or_else(|e|e.into_inner())["running"]=json!(true);
            let have_auth=view(&s)["imported"]==true||read_auth(&s).is_ok_and(|v|ready(&v));let s=s.clone();std::thread::spawn(move||worker(s,have_auth));
        },
        _=>return Err("未知临时登录操作".into())
    }
    Ok(json!({"session":view(&s)}))
}

#[cfg(test)] mod tests{
    use super::*;use std::cell::RefCell;
    #[test] fn final_read_follows_close_and_cleanup_follows_import(){let events=RefCell::new(Vec::new());
        final_sequence(||{events.borrow_mut().push("close");Ok(())},||{events.borrow_mut().push("read");Ok(7)},|v|{assert_eq!(v,7);events.borrow_mut().push("import");Ok(())},||{events.borrow_mut().push("cleanup");Ok(())}).unwrap();
        assert_eq!(*events.borrow(),vec!["close","read","import","cleanup"]);
    }
    #[test] fn failed_import_preserves_temporary_credentials(){let cleaned=std::cell::Cell::new(false);assert!(final_sequence(||Ok(()),||Ok(()),|_|Err("failure".into()),||{cleaned.set(true);Ok(())}).is_err());assert!(!cleaned.get());}
    #[test] fn failed_close_never_reads_or_imports(){assert!(final_sequence(||Err("busy".into()),||->Result<(),String>{panic!("read after failed close")},|_|panic!("import"),||panic!("cleanup")).is_err());}
    #[test] fn only_complete_credentials_finish_login(){assert!(!ready(&json!({"tokens":{"access_token":"partial"}})));assert!(ready(&json!({"tokens":{"access_token":"a","id_token":"i","refresh_token":"r"}})));assert!(ready(&json!({"OPENAI_API_KEY":"key"})));}
    #[test] fn cleanup_requires_exact_owned_child(){let r=std::env::temp_dir().join("login-tests-root");assert!(owned_dir(&r,&r,"abc").is_err());assert!(owned_dir(&r.join("../outside"),&r,"../outside").is_err());assert!(owned_dir(&r.join("abc"),&r,"def").is_err());}
    #[test]
    #[ignore = "opens a disposable official desktop client; run explicitly on the local Windows host"]
    fn official_client_cancel_smoke(){
        let original_auth=fs::read(native_home().join("auth.json")).unwrap();
        // Users normally run isolated accounts, not the default profile. Protect
        // every existing official main process, regardless of its profile path.
        let exe=reg::resolve_exe(&reg::app_def(APP).unwrap()).unwrap();
        let exe_name=exe.file_name().unwrap().to_string_lossy().to_lowercase();
        let original:Vec<u32>=procs::snapshot_fresh().iter()
            .filter(|(_,p)|p.name==exe_name&&!p.cmdline.contains("--type="))
            .map(|(pid,_)|*pid).collect();
        assert!(!original.is_empty(),"keep an existing official account open to verify isolation");
        if let Ok(id)=std::env::var("AGENTCOCKPIT_CLEANUP_TEST_SESSION"){
            let previous=recover(&id).unwrap();
            assert!(previous.request.as_object().is_some_and(|v|v.is_empty()),"only an abandoned synthetic session may be cleared by this smoke check");
            assert!(read_auth(&previous).is_err(),"a session with login credentials must be kept");
            discard(&previous);assert_eq!(view(&previous)["phase"],"cancelled");assert!(!previous.dir.exists());
        }
        let id=format!("{:x}-{:x}",std::process::id(),SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos());
        let dir=root().join(&id);check_plain_path(&dir).unwrap();fs::create_dir_all(&dir).unwrap();
        let s=Arc::new(Session{request:json!({}),dir:dir.clone(),state:Mutex::new(json!({"id":id,"phase":"preparing","running":true,"imported":false})),cancel:AtomicBool::new(false)});persist(&s).unwrap();
        sessions().lock().unwrap().insert(id.clone(),s.clone());
        let child=s.clone();let worker_handle=std::thread::spawn(move||worker(child,false));
        let deadline=Instant::now()+Duration::from_secs(45);let mut launched=false;
        while Instant::now()<deadline {if !instance_pids(&profile(&s),&procs::snapshot_fresh()).is_empty(){launched=true;break;}if view(&s)["running"]!=true{break;}std::thread::sleep(Duration::from_millis(500));}
        // Exercise the same cancellation/status path as the UI, not just the
        // worker's atomic flag. Cancellation acknowledgement must be immediate.
        let started=Instant::now();
        let acknowledged=handle("chatgpt_official_cancel",&json!({"session_id":id})).unwrap();
        assert_eq!(acknowledged["session"]["id"],id);
        assert!(started.elapsed()<Duration::from_secs(2),"cancel blocked on the login worker");
        worker_handle.join().unwrap();
        let status=handle("chatgpt_official_status",&json!({"session_id":id})).unwrap();
        assert_eq!(status["session"]["phase"],"cancelled");
        sessions().lock().unwrap().remove(&id);
        assert!(launched,"temporary client startup failed: {}",view(&s));assert_eq!(view(&s)["phase"],"cancelled");assert!(!dir.exists());
        assert_eq!(fs::read(native_home().join("auth.json")).unwrap(),original_auth);
        let current=procs::snapshot_fresh();assert!(original.iter().all(|p|current.contains_key(p)));assert!(!reg::is_reparse_point(&native_profile().unwrap()));
    }
}
