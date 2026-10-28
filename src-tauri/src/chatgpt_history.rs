use super::*;

pub(super) fn seed(m: &Value) -> Result<Value, String> {
    start_sync();
    if is_native(m) {
        return Ok(json!({"storage":"shared","canonical":native_home(),"preserved_existing":true}));
    }
    let target = writable(m)?;
    fs::create_dir_all(&target).map_err(|e|e.to_string())?;
    let result=run_shared(vec![native_home(),target],vec![])?;
    if result["pending"].as_array().is_some_and(|v|!v.is_empty()){
        return Err(format!("共享历史接入未完成，原记录保留：{}",result["pending"]));
    }
    // A pending account has not been registered yet. Its caller commits the
    // snapshot together with the final account metadata only after import works.
    if m["chatgpt_pending"]!=true {
        let mut saved=meta(m["name"].as_str().unwrap_or(""))?;
        saved["history_prepared"]=json!(true);saved["history_snapshot"]=result.clone();
        saved["history_mode"]=json!("shared");
        reg::save_instance_checked(APP,&saved)?;
    }
    Ok(result)
}

fn run_shared(homes:Vec<PathBuf>,busy:Vec<PathBuf>)->Result<Value,String>{
    run_python(include_str!("../scripts/chatgpt-history-shared.py"),json!({
        "canonical":native_home(),"homes":homes,"busy_homes":busy,
        "state_dir":reg::base_dir().join("history-sync"),
        "sync_script":include_str!("../scripts/chatgpt-history-sync.py"),
        "history_script":include_str!("../scripts/chatgpt-history.py")
    }))
}

fn run_script(request:Value)->Result<Value,String>{
    run_python(include_str!("../scripts/chatgpt-history.py"), request)
}

fn run_python(script:&str,request:Value)->Result<Value,String>{
    let mut cmd=Command::new("python");
    // The embedded migration script exceeds Windows' command-line size limit.
    // Transport source and request through stdin, keeping argv small and fixed.
    const BOOTSTRAP:&str="import io,json,sys; p=json.loads(sys.stdin.buffer.read().decode('utf-8')); sys.stdin=io.StringIO(json.dumps(p['request'])); exec(compile(p['script'],'<AgentCockpit history>','exec'),{'__name__':'__main__'})";
    cmd.args(["-I","-X","utf8","-c",BOOTSTRAP]).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    #[cfg(windows)] {use std::os::windows::process::CommandExt;cmd.creation_flags(0x08000000);}
    let mut child=cmd.spawn().map_err(|e|format!("无法启动本机Python历史处理组件：{e}"))?;
    let write_result=match child.stdin.take() {
        Some(mut input)=>input.write_all(json!({"script":script,"request":request}).to_string().as_bytes()).map_err(|e|e.to_string()),
        None=>Err("无法传递历史复制参数".into()),
    };
    if let Err(error)=write_result {let _=child.kill();let _=child.wait();return Err(error);}
    let started=Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_))=>break,
            Ok(None)=>{},
            Err(_)=>{let _=child.kill();let _=child.wait();return Err("无法读取历史复制状态，原对话保持原位".into());}
        }
        if started.elapsed()>Duration::from_secs(210) {let _=child.kill();let _=child.wait();return Err("历史复制等待超时，原对话保持原位，请重试".into());}
        std::thread::sleep(Duration::from_millis(100));
    }
    let output=child.wait_with_output().map_err(|e|e.to_string())?;
    if output.stdout.len()>1024*1024 {return Err("历史快照响应异常，未登记为已复制".into());}
    let result:Value=serde_json::from_slice(&output.stdout).map_err(|_|format!("历史处理组件未返回有效结果：{}",String::from_utf8_lossy(&output.stderr).chars().take(1200).collect::<String>()))?;
    if !output.status.success() || result["ok"]!=true {return Err(format!("历史复制未完成：{}",result["message"].as_str().unwrap_or("请重试")));}
    Ok(result)
}

// Start once when the account list is first read, then discover profiles anew
// each pass so later imports join without restarting the manager. Authentication
// remains per profile; only the history allowlist is replicated by the script.
fn start_sync(){
    // Synthetic unit tests must never start a worker against real accounts.
    if cfg!(test){return;}
    static STARTED:OnceLock<()>=OnceLock::new();
    STARTED.get_or_init(||{std::thread::spawn(||loop{
        {
            // Serialize with credential imports and initial history seeding.
            let _guard=IO_LOCK.lock().unwrap_or_else(|e|e.into_inner());
            let mut homes=vec![native_home()];
            let mut busy=vec![];
            let snapshot=procs::snapshot_fresh();
            for account in reg::load_instances(APP){
                let path=home(&account);
                if !instance_pids(&account,&snapshot).is_empty(){busy.push(path.clone());}
                if (path==native_home()||under(&path,&reg::base_dir().join("chatgpt-auth")))
                    && check_plain_path(&path).is_ok() && !homes.contains(&path){homes.push(path);}
            }
            if homes.len()>1{
                if let Err(error)=run_shared(homes,busy){crate::lifecycle::note(&format!("ChatGPT 共享历史：{error}"));}
            }
        }
        std::thread::sleep(Duration::from_secs(15));
    });});
}

fn repair_paths(m:&Value)->Result<(PathBuf,PathBuf),String>{
    if is_native(m){return Err("原有默认实例保持原状，只能修复复制创建的独立实例".into());}
    let target=home(m);let browser=PathBuf::from(m["data_dir"].as_str().unwrap_or(""));
    check_plain_path(&target)?;check_plain_path(&browser)?;
    if !under(&target,&reg::base_dir().join("chatgpt-auth")) || !under(&browser,&reg::instances_dir(APP)) {
        return Err("该账号不属于独立实例资料范围，未修改项目状态".into());
    }
    let source=m["history_source_home"].as_str().map(PathBuf::from).unwrap_or_else(native_home);
    check_plain_path(&source)?;
    if source!=native_home()&&!reg::load_instances(APP).iter().any(|i|home(i)==source){return Err("项目修复来源未登记".into());}
    Ok((source,target))
}

/// Queue only metadata while a client is active. Applying a queue never closes it.
pub(super) fn queue_project_repair(m:&Value)->Result<Value,String>{
    if home(m).join(".agentcockpit-shared-history.json").is_file(){
        return Ok(json!({"eligible":false,"pending":false,"message":"对话和项目已经使用统一历史，无需复制修复"}));
    }
    let (source,target)=repair_paths(m)?;
    let plan=run_script(json!({"mode":"repair_projects","dry_run":true,"source":source,"target":target}))?;
    if plan["eligible"]!=true{return Ok(plan);}
    if plan["legacy_count"].as_u64()==Some(0)&&plan["project_count"].as_u64()==Some(0){
        // A repeated inspection of an already repaired instance is a no-op.
        // Clear stale queue metadata without touching an active client's files.
        let mut saved=meta(m["name"].as_str().unwrap_or(""))?;
        if saved.as_object_mut().and_then(|o|o.remove("pending_project_repair")).is_some(){reg::save_instance_checked(APP,&saved)?;}
        let mut result=plan;result["pending"]=json!(false);
        result["message"]=json!(if result["uncertain"].as_u64().unwrap_or(0)>0{"没有可确认需要修正的旧项目，已保留现有登记"}else{"项目列表无需修正"});
        return Ok(result);
    }
    let mut saved=meta(m["name"].as_str().unwrap_or(""))?;
    saved["pending_project_repair"]=json!({"plan":plan,"queued_at":now()});
    reg::save_instance_checked(APP,&saved)?;
    let mut result=plan;result["pending"]=json!(true);
    result["message"]=json!("项目列表修复已排队，请关闭此账号窗口后再启动；对话和项目文件保留。");
    Ok(result)
}

pub(super) fn apply_pending(m:&Value)->Result<Value,String>{
    if home(m).join(".agentcockpit-shared-history.json").is_file(){return Ok(json!({"pending":false,"skipped":true,"storage":"shared"}));}
    let mut current=meta(m["name"].as_str().unwrap_or(""))?;
    let queued=current["pending_project_repair"].clone();
    if !queued.is_object(){return Ok(json!({"pending":false,"skipped":true}));}
    let (source,target)=repair_paths(&current)?;
    // Exact profile ownership is checked by writable(); no kill or directory
    // replacement is used to make a running client fit the repair workflow.
    writable(&current)?;
    let result=run_script(json!({"mode":"repair_projects","dry_run":false,"source":source,"target":target,"expected_plan":queued["plan"]}))?;
    if result["applied"]==true {
        current=meta(m["name"].as_str().unwrap_or(""))?;
        current.as_object_mut().map(|v|v.remove("pending_project_repair"));
        current["project_visibility_repair"]=result.clone();
        reg::save_instance_checked(APP,&current)?;
    }
    Ok(result)
}

pub(super) fn project_status(m:&Value)->Value{
    start_sync();
    if m["pending_project_repair"].is_object(){
        let plan=&m["pending_project_repair"]["plan"];
        return json!({"state":"pending","legacy_count":plan["legacy_count"],"project_count":plan["project_count"],"uncertain":plan["uncertain"],"message":"已排队，关闭此账号窗口后下次启动自动修复项目列表"});
    }
    if m["project_visibility_repair"]["applied"]==true{
        return json!({"state":"applied","legacy_count":m["project_visibility_repair"]["legacy_count"],"project_count":m["project_visibility_repair"]["project_count"],"uncertain":m["project_visibility_repair"]["uncertain"],"message":"已修复重复出现的旧项目登记，对话和项目文件保留"});
    }
    json!({"state":if m["history_snapshot"]["visibility_version"].as_u64().unwrap_or(0)>=2{"current"}else{"unchecked"}})
}

#[cfg(test)] mod tests{
    use super::*;
    #[test] fn embedded_history_script_uses_stdin_instead_of_long_windows_argv(){
        let base=std::env::temp_dir();
        let directory=base.join(format!("agentcockpit-script-test-{}",SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()));
        fs::create_dir_all(directory.join("source")).unwrap();fs::create_dir_all(directory.join("target")).unwrap();
        let result=run_script(json!({"source":directory.join("source"),"target":directory.join("target")}));
        assert!(directory.starts_with(&base)&&directory.file_name().unwrap().to_string_lossy().starts_with("agentcockpit-script-test-"));
        fs::remove_dir_all(&directory).unwrap();
        assert!(result.is_ok(),"the embedded Python script must start successfully: {:?}",result.err());
    }
}
