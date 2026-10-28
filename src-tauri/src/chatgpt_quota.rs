//! Read-only account usage. OAuth credentials only go to fixed OpenAI endpoints;
//! API keys only go to paths on the user-configured provider origin.
use super::*;
use std::hash::{Hash, Hasher};
use std::io::Read;

const USAGE_URL:&str="https://chatgpt.com/backend-api/wham/usage";
const MAX_RESPONSE:u64=2*1024*1024;
const REFRESH_TTL:u64=300;
#[derive(Clone,Default)]
struct Job { running:bool, attempted:u64, identity:String }
static JOBS:OnceLock<Mutex<HashMap<String,Job>>>=OnceLock::new();
fn jobs()->&'static Mutex<HashMap<String,Job>> {JOBS.get_or_init(||Mutex::new(HashMap::new()))}
fn job_key(m:&Value)->String {home(m).to_string_lossy().to_lowercase()}

fn read_auth(m:&Value)->Result<Value,String>{
    let path=home(m).join("auth.json");check_plain_path(&path)?;
    if fs::metadata(&path).map_err(|_|"请先完成这个账号的登录")?.len()>MAX_IMPORT as u64{return Err("登录文件超过大小限制".into());}
    serde_json::from_slice(&fs::read(path).map_err(|_|"无法读取这个账号的登录文件")?).map_err(|_|"登录文件格式无法识别".into())
}
fn provider_base(m:&Value)->Result<String,String>{
    let path=home(m).join("config.toml");check_plain_path(&path)?;
    let cfg:toml::Value=fs::read_to_string(path).map_err(|_|"无法读取账号供应商配置")?.parse().map_err(|_|"账号供应商配置格式无效")?;
    let selected=cfg.get("model_provider").and_then(toml::Value::as_str).unwrap_or("openai");
    Ok(cfg.get("model_providers").and_then(|v|v.get(selected)).and_then(|v|v.get("base_url")).and_then(toml::Value::as_str).unwrap_or("https://api.openai.com/v1").trim_end_matches('/').to_string())
}
fn is_api(auth:&Value)->bool {auth["OPENAI_API_KEY"].as_str().is_some_and(|s|!s.trim().is_empty())}
fn account_id(auth:&Value)->String{
    if let Some(id)=auth.pointer("/tokens/account_id").and_then(Value::as_str).filter(|s|!s.is_empty()){return id.into();}
    for field in ["access_token","id_token"]{
        let c=claims(auth["tokens"][field].as_str().unwrap_or(""));
        if let Some(id)=c.pointer("/https:~1~1api.openai.com~1auth/chatgpt_account_id").and_then(Value::as_str){return id.into();}
    }String::new()
}
fn identity(auth:&Value,base:&str)->String{
    let mut hash=std::collections::hash_map::DefaultHasher::new();
    if is_api(auth){"apikey".hash(&mut hash);base.hash(&mut hash);auth["OPENAI_API_KEY"].as_str().unwrap_or("").hash(&mut hash);}
    else{let id=account_id(auth);"chatgpt".hash(&mut hash);id.hash(&mut hash);
        let token=auth["tokens"]["access_token"].as_str().unwrap_or("");let c=claims(token);
        c["sub"].as_str().unwrap_or(if id.is_empty(){token}else{""}).hash(&mut hash);
    }format!("{:016x}",hash.finish())
}
fn current_identity(m:&Value)->Result<String,String>{let auth=read_auth(m)?;let base=if is_api(&auth){provider_base(m)?}else{String::new()};Ok(identity(&auth,&base))}
fn empty(mode:&str,message:&str)->Value{json!({"state":"loading","mode":mode,"fetched_at":null,"windows":[],"balance":null,"message":message,"provider":null})}
fn has_values(v:&Value)->bool{v["windows"].as_array().is_some_and(|a|!a.is_empty())||v["balance"].is_object()}
pub(super) fn cached(m:&Value)->Value{
    let value=&m["chatgpt_quota"];
    let actual=current_identity(m);
    let same=value["identity"].as_str().is_some_and(|expected|actual.as_ref().is_ok_and(|identity|identity==expected));
    let credential_error=actual.is_err()&&!has_values(value)&&matches!(value["state"].as_str(),Some("error"|"unsupported"));
    if value.is_object() && (same||credential_error){
        let mut out=value.clone();out.as_object_mut().map(|v|v.remove("identity"));return out;
    }
    let mode=read_auth(m).ok().map(|a|if is_api(&a){"apikey"}else{"chatgpt"}).unwrap_or("unknown");
    empty(mode,"正在读取这个账号的实时额度")
}
pub(super) fn invalidate(m:&Value){
    if let Some(job)=jobs().lock().unwrap_or_else(|e|e.into_inner()).get_mut(&job_key(m)){
        // Preserve an in-flight claim until its worker finishes. Its snapshot
        // will fail the credential identity comparison if the login changed.
        job.attempted=0;job.identity.clear();
    }
}
fn claim(m:&Value,force:bool)->bool{
    let key=job_key(m);let expected=current_identity(m).unwrap_or_default();
    let mut all=jobs().lock().unwrap_or_else(|e|e.into_inner());let job=all.entry(key).or_default();
    if job.running{return false;}
    let fetched=m["chatgpt_quota"]["fetched_at"].as_u64().unwrap_or(0);
    let latest=job.attempted.max(fetched);
    if !force&&job.identity==expected&&now().saturating_sub(latest)<REFRESH_TTL{return false;}
    if !force&&job.attempted==0&&m["chatgpt_quota"]["identity"]==expected&&now().saturating_sub(fetched)<REFRESH_TTL{return false;}
    *job=Job{running:true,attempted:now(),identity:expected};true
}
fn release(m:&Value){if let Some(job)=jobs().lock().unwrap_or_else(|e|e.into_inner()).get_mut(&job_key(m)){job.running=false;}}
pub(super) fn schedule(m:&Value){
    if m["chatgpt_pending"]==true||!claim(m,false){return;}
    let account=m.clone();std::thread::spawn(move||{let _=perform_refresh(&account);release(&account);});
}
pub(super) fn refresh(m:&Value)->Result<Value,String>{
    if !claim(m,true){let mut value=cached(m);value["refreshing"]=json!(true);return Ok(json!({"quota":value,"msg":"正在读取这个账号的额度"}));}
    let result=perform_refresh(m);release(m);result
}

#[cfg(windows)]
fn system_proxy()->Option<String>{
    use std::os::windows::process::CommandExt;
    // A fixed command reads the same Windows proxy used by the user's browser.
    // Proxy values never appear in logs or error messages.
    let script=r#"$p=Get-ItemProperty -LiteralPath 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Internet Settings';if($p.ProxyEnable -eq 1){[Console]::Write([string]$p.ProxyServer)}"#;
    let output=Command::new("powershell.exe").args(["-NoProfile","-NonInteractive","-WindowStyle","Hidden","-Command",script]).creation_flags(0x08000000).output().ok()?;
    if !output.status.success(){return None;}proxy_address(String::from_utf8(output.stdout).ok()?.trim())
}
#[cfg(not(windows))] fn system_proxy()->Option<String>{None}
fn proxy_address(value:&str)->Option<String>{
    let value=value.trim();if value.is_empty()||value.chars().any(char::is_whitespace){return None;}
    let server=if value.contains('='){value.split(';').find_map(|part|part.strip_prefix("https=")).or_else(||value.split(';').find_map(|part|part.strip_prefix("http=")))?}else{value};
    if server.is_empty(){None}else{Some(if server.contains("://"){server.into()}else{format!("http://{server}")})}
}
fn agent()->ureq::Agent{
    let mut builder=ureq::AgentBuilder::new().timeout(Duration::from_secs(18)).timeout_connect(Duration::from_secs(7)).redirects(0).try_proxy_from_env(true);
    let environment_proxy=["HTTPS_PROXY","https_proxy","HTTP_PROXY","http_proxy","ALL_PROXY","all_proxy"].iter().any(|k|std::env::var(k).is_ok_and(|v|!v.trim().is_empty()));
    if !environment_proxy{if let Some(address)=system_proxy(){if let Ok(proxy)=ureq::Proxy::new(address){builder=builder.proxy(proxy);}}}
    builder.build()
}
#[derive(Clone,Debug)] struct ProbeError{status:Option<u16>,message:String}
impl ProbeError{fn local(message:&str)->Self{Self{status:None,message:message.into()}}}
fn response_json(response:ureq::Response)->Result<Value,ProbeError>{
    let mut bytes=Vec::new();response.into_reader().take(MAX_RESPONSE+1).read_to_end(&mut bytes).map_err(|_|ProbeError::local("额度响应读取失败"))?;
    if bytes.len() as u64>MAX_RESPONSE{return Err(ProbeError::local("额度响应超过大小限制"));}
    serde_json::from_slice(&bytes).map_err(|_|ProbeError::local("服务端返回的额度数据不是 JSON"))
}
fn http_error(error:ureq::Error)->ProbeError{match error{ureq::Error::Status(code,_)=>ProbeError{status:Some(code),message:format!("额度接口返回 HTTP {code}")},ureq::Error::Transport(_)=>ProbeError::local("无法连接额度服务，请检查网络或系统代理")}}
fn trace_http(_url:&str,_status:u16,_content_type:&str,_body:Option<&Value>){
    #[cfg(test)]{
        if std::env::var("AGENTCOCKPIT_QUOTA_SMOKE").as_deref()==Ok("1"){
            let tail=_url.split_once("://").map(|(_,tail)|tail).unwrap_or("");
            let path=tail.find('/').map(|at|&tail[at..]).unwrap_or("/").split('?').next().unwrap_or("/");
            let content_type:String=_content_type.split(';').next().unwrap_or("").chars().filter(|c|c.is_ascii_alphanumeric()||matches!(c,'/'|'+'|'-'|'.')).take(80).collect();
            let keys:Vec<String>=_body.and_then(Value::as_object).map(|v|v.keys().take(24).map(|k|if k.contains("sk-")||k.contains("eyJ"){"[redacted]".into()}else{k.chars().take(64).collect()}).collect()).unwrap_or_default();
            println!("quota_probe={}",json!({"path":path,"status":_status,"content_type":content_type,"json_keys":keys}));
        }
    }
}
fn get_json(agent:&ureq::Agent,url:&str,key:&str,oauth:bool,id:&str)->Result<Value,ProbeError>{
    if key.is_empty()||key.chars().any(|c|c.is_control()||c.is_whitespace()){return Err(ProbeError::local("账号登录凭据无效，请重新登录"));}
    let mut req=agent.get(url).set("Accept","application/json").set("Authorization",&format!("Bearer {key}"));
    if oauth{
        if url!=USAGE_URL{return Err(ProbeError::local("官方额度地址校验失败"));}
        req=req.set("originator","Codex Desktop").set("User-Agent","Codex Desktop/0.163.0 (Windows; x86_64)");
        if !id.is_empty(){if id.chars().any(|c|c.is_control()||c.is_whitespace()){return Err(ProbeError::local("账号标识无效"));}req=req.set("ChatGPT-Account-Id",id);}
    }
    match req.call(){
        Ok(response)=>{
            let status=response.status();let content_type=response.header("Content-Type").unwrap_or("").to_string();
            let result=response_json(response);trace_http(url,status,&content_type,result.as_ref().ok());result
        },
        Err(ureq::Error::Status(code,response))=>{trace_http(url,code,response.header("Content-Type").unwrap_or(""),None);Err(ProbeError{status:Some(code),message:format!("额度接口返回 HTTP {code}")})},
        Err(error)=>Err(http_error(error)),
    }
}
fn number(value:&Value)->Option<f64>{value.as_f64().or_else(||value.as_str()?.trim().parse().ok()).filter(|n|n.is_finite())}
fn named_number(value:&Value,keys:&[&str])->Option<f64>{keys.iter().find_map(|k|number(&value[*k]))}
fn parse_usage(body:&Value,expected_id:&str)->Result<Value,ProbeError>{
    if let Some(actual)=body["account_id"].as_str(){if !expected_id.is_empty()&&actual!=expected_id{return Err(ProbeError::local("额度响应的账号标识不匹配，未保存该数据"));}}
    let mut windows=Vec::new();
    for (key,id,label) in [("primary_window","5h","5 小时"),("secondary_window","weekly","每周")]{
        let item=&body["rate_limit"][key];if item.is_null(){continue;}
        let used=number(&item["used_percent"]).filter(|v|(0.0..=100.0).contains(v)).ok_or_else(||ProbeError::local("额度窗口缺少有效的使用百分比"))?;
        let reset=number(&item["reset_at"]).map(|n|n as i64).or_else(||number(&item["reset_after_seconds"]).map(|n|now() as i64+n.max(0.0) as i64));
        windows.push(json!({"id":id,"label":label,"used_percent":used,"remaining_percent":100.0-used,"reset_at":reset,"limit_window_seconds":item["limit_window_seconds"]}));
    }
    if windows.is_empty(){return Err(ProbeError::local("官方接口未返回这个账号的额度窗口"));}
    Ok(json!({"state":"live","mode":"chatgpt","fetched_at":now(),"windows":windows,"balance":null,"plan":body["plan_type"],"provider":"chatgpt.com","message":"已从官方服务读取实时额度"}))
}
fn refresh_auth(m:&Value,old:&Value,agent:&ureq::Agent)->Result<Value,ProbeError>{
    // Let an active official client own token rotation. A manager rotating its
    // refresh token concurrently could invalidate the user's live session.
    if is_native(m)||!instance_pids(m,&procs::snapshot_fresh()).is_empty(){return Err(ProbeError::local("登录令牌已失效，请在这个账号的官方客户端刷新登录后重试"));}
    let _guard=IO_LOCK.lock().unwrap_or_else(|e|e.into_inner());
    let current=read_auth(m).map_err(|s|ProbeError::local(&s))?;
    if current["tokens"]!=old["tokens"]{return Ok(current);}
    let refresh=current["tokens"]["refresh_token"].as_str().filter(|s|!s.is_empty()).ok_or_else(||ProbeError::local("缺少续期凭据，请重新登录"))?;
    let response=agent.post("https://auth.openai.com/oauth/token").send_json(json!({"grant_type":"refresh_token","client_id":CLIENT_ID,"refresh_token":refresh})).map_err(http_error)?;
    let reply=response_json(response)?;
    let access=reply["access_token"].as_str().filter(|s|!s.is_empty()).ok_or_else(||ProbeError::local("官方续期响应缺少访问令牌"))?;
    let latest=read_auth(m).map_err(|s|ProbeError::local(&s))?;
    if latest["tokens"]!=current["tokens"]{return Ok(latest);}
    let mut updated=current.clone();updated["tokens"]["access_token"]=json!(access);
    for field in ["refresh_token","id_token"]{if reply[field].as_str().is_some_and(|s|!s.is_empty()){updated["tokens"][field]=reply[field].clone();}}
    updated["last_refresh"]=json!(chrono::Utc::now().to_rfc3339());
    let h=home(m);check_plain_path(&h).map_err(|s|ProbeError::local(&s))?;
    if !under(&h,&reg::base_dir().join("chatgpt-auth")){return Err(ProbeError::local("凭据目录超出受管范围，未写入续期结果"));}
    atomic_write(&h.join("auth.json"),&serde_json::to_vec_pretty(&updated).map_err(|_|ProbeError::local("续期凭据序列化失败"))?).map_err(|s|ProbeError::local(&s))?;
    Ok(updated)
}
fn fetch_oauth(m:&Value,auth:&Value,agent:&ureq::Agent)->Result<Value,ProbeError>{
    let access=auth["tokens"]["access_token"].as_str().unwrap_or("");let id=account_id(auth);
    let reply=get_json(agent,USAGE_URL,access,true,&id);
    match reply{
        Ok(body)=>parse_usage(&body,&id),
        Err(e) if e.status==Some(401)=>{let updated=refresh_auth(m,auth,agent)?;let id=account_id(&updated);let body=get_json(agent,USAGE_URL,updated["tokens"]["access_token"].as_str().unwrap_or(""),true,&id)?;parse_usage(&body,&id)},
        Err(e)=>Err(e),
    }
}
#[derive(Clone,Debug)]struct Provider{base:String,root:String,host:String}
fn provider(base:&str)->Result<Provider,ProbeError>{
    let base=base.trim().trim_end_matches('/');
    if base.chars().any(|c|c.is_control()||c.is_whitespace())||base.contains(['@','?','#','\\','"']){return Err(ProbeError::local("供应商 Base URL 无效，未发送凭据"));}
    let (scheme,tail)=base.split_once("://").ok_or_else(||ProbeError::local("供应商 Base URL 必须包含协议"))?;
    let host=tail.split('/').next().unwrap_or("");
    let local=host=="localhost"||host.starts_with("localhost:")||host=="127.0.0.1"||host.starts_with("127.0.0.1:")||host.starts_with("[::1]:");
    if host.is_empty()||!(scheme=="https"||scheme=="http"&&local)||tail.split('/').any(|p|p==".."||p=="."){return Err(ProbeError::local("供应商 Base URL 需要 HTTPS 或本机 HTTP 地址"));}
    let root=base.strip_suffix("/v1").unwrap_or(base).trim_end_matches('/');
    Ok(Provider{base:base.into(),root:root.into(),host:host.to_string()})
}
fn balance(remaining:Option<f64>,total:Option<f64>,used:Option<f64>,unit:&str,unlimited:bool)->Option<Value>{
    if remaining.is_none()&&total.is_none()&&used.is_none()&&!unlimited{return None;}
    Some(json!({"remaining":remaining,"total":total,"used":used,"currency":if ["USD","CNY","EUR","GBP","JPY"].contains(&unit){json!(unit)}else{Value::Null},"unit":unit,"unlimited":unlimited}))
}
fn token_balance(body:&Value)->Option<Value>{
    if body["success"]==false{return None;}
    let data=body.get("data").unwrap_or(body);let display=&data["display"];
    let unit=display["unit"].as_str().unwrap_or("").trim().to_uppercase();
    if unit.is_empty(){return None;}
    let mut total=number(&display["total"]);let mut used=number(&display["used"]);let mut remaining=number(&display["remaining"]);
    total=total.or_else(||used.zip(remaining).map(|(u,r)|u+r));used=used.or_else(||total.zip(remaining).map(|(t,r)|(t-r).max(0.0)));remaining=remaining.or_else(||total.zip(used).map(|(t,u)|(t-u).max(0.0)));
    balance(remaining,total,used,&unit,data["unlimited_quota"]==true)
}
fn billing_balance(subscription:&Value,usage:&Value)->Option<Value>{
    let total=named_number(subscription,&["hard_limit_usd","soft_limit_usd","system_hard_limit_usd"]);
    let unlimited=["hard_limit_usd","soft_limit_usd","system_hard_limit_usd"].iter().all(|k|number(&subscription[*k])==Some(100_000_000.0));
    let total=total.filter(|_|!unlimited);
    let used=number(&usage["total_usage"]).map(|n|n/100.0);
    // *_usd is a compatibility field name; some relays return CNY. Use an
    // explicit site-provided unit only and never guess a quota conversion.
    let unit=subscription["currency"].as_str().or_else(||usage["currency"].as_str()).unwrap_or("").trim().to_uppercase();
    balance(total.zip(used).map(|(t,u)|(t-u).max(0.0)),total,used,&unit,unlimited)
}
fn sub2api_balance(body:&Value)->Option<Value>{
    if body["success"]==false{return None;}
    let body=body.get("data").unwrap_or(body);let quota=&body["quota"];
    let quota_unit=quota["unit"].as_str().unwrap_or("").trim().to_uppercase();
    if !quota_unit.is_empty(){
        let total=number(&quota["limit"]);let used=number(&quota["used"]);
        let remaining=number(&quota["remaining"]).or_else(||total.zip(used).map(|(t,u)|(t-u).max(0.0)));
        return balance(remaining,total,used,&quota_unit,quota["unlimited"]==true);
    }
    let remaining=named_number(body,&["remaining","balance"]).or_else(||number(&quota["remaining"]));
    let unit=body["unit"].as_str().or_else(||body["currency"].as_str()).unwrap_or("").trim().to_uppercase();
    balance(remaining,number(&quota["limit"]),number(&quota["used"]),&unit,quota["unlimited"]==true)
}
fn api_live(p:&Provider,value:Value,message:&str)->Value{json!({"state":"live","mode":"apikey","fetched_at":now(),"windows":[],"balance":value,"provider":p.host,"message":message})}
fn billing_roots(p:&Provider)->Vec<String>{
    // New API serves compatibility billing routes under /v1 even when the
    // user's inference Base URL was entered as a bare origin.
    let mut roots=Vec::new();for candidate in [p.base.clone(),p.root.clone(),format!("{}/v1",p.root)]{if !roots.contains(&candidate){roots.push(candidate);}}roots
}
fn fetch_api(auth:&Value,base:&str,agent:&ureq::Agent)->Result<Value,ProbeError>{
    let p=provider(base)?;let key=auth["OPENAI_API_KEY"].as_str().unwrap_or("");
    let mut token_result=None;let mut failures=Vec::new();
    for suffix in ["/api/usage/token/","/api/usage/token"]{
        match get_json(agent,&format!("{}{suffix}",p.root),key,false,""){
            Ok(body)=>{if let Some(value)=token_balance(&body){return Ok(api_live(&p,value,"已从供应商读取实时余额"));}if token_result.is_none(){token_result=Some(body);}},
            Err(error)=>failures.push(error),
        }
    }
    for root in billing_roots(&p){
        let subscription=match get_json(agent,&format!("{root}/dashboard/billing/subscription"),key,false,""){Ok(v)=>v,Err(e)=>{failures.push(e);continue;}};
        if named_number(&subscription,&["hard_limit_usd","soft_limit_usd","system_hard_limit_usd"]).is_none(){continue;}
        let usage=match get_json(agent,&format!("{root}/dashboard/billing/usage"),key,false,""){Ok(v)=>v,Err(e)=>{failures.push(e);continue;}};
        if let Some(value)=billing_balance(&subscription,&usage){return Ok(api_live(&p,value,if value_has_unit(&subscription,&usage){"已从供应商读取实时余额"}else{"已读取站点账单额度；该站点未标明计量单位"}));}
    }
    // Cockpit Tools also auto-detects Sub2API's bearer-authenticated /usage
    // response after the New API billing protocol is unavailable.
    for root in billing_roots(&p){
        match get_json(agent,&format!("{root}/usage"),key,false,""){
            Ok(body)=>if let Some(value)=sub2api_balance(&body){return Ok(api_live(&p,value,"已从供应商读取实时余额"));},
            Err(error)=>failures.push(error),
        }
    }
    if let Some(body)=token_result{
        let data=body.get("data").unwrap_or(&body);
        let remaining=named_number(data,&["total_available","remaining_quota"]);
        let total=number(&data["total_granted"]);let used=number(&data["total_used"]);
        if let Some(value)=balance(remaining,total,used,"配额",data["unlimited_quota"]==true){return Ok(api_live(&p,value,"已读取令牌配额；供应商未提供货币余额换算"));}
    }
    if let Some(e)=failures.iter().find(|e|e.status==Some(401)||e.status==Some(403)){return Err(e.clone());}
    if let Some(e)=failures.iter().find(|e|e.status.is_none()){return Err(e.clone());}
    Err(ProbeError{status:Some(404),message:"该供应商未提供兼容的余额接口；API Key 不一定具有账单查询权限".into()})
}
fn value_has_unit(a:&Value,b:&Value)->bool{a["currency"].as_str().or_else(||b["currency"].as_str()).is_some_and(|s|!s.trim().is_empty())}

fn perform_refresh(m:&Value)->Result<Value,String>{
    // Read one coherent provider/key pair while credential import is excluded.
    let snapshot={let _guard=IO_LOCK.lock().unwrap_or_else(|e|e.into_inner());
        let current=meta(m["name"].as_str().unwrap_or(""))?;
        let auth=read_auth(&current);let base=auth.as_ref().ok().filter(|a|is_api(a)).map(|_|provider_base(&current)).transpose();
        (current,auth,base)
    };
    let (current,auth_result,base_result)=snapshot;
    let mode=auth_result.as_ref().map(|a|if is_api(a){"apikey"}else{"chatgpt"}).unwrap_or("unknown");
    let base=base_result.as_ref().ok().and_then(|v|v.as_deref()).unwrap_or("");
    let expected=auth_result.as_ref().ok().map(|a|identity(a,base)).unwrap_or_default();
    let result=match (&auth_result,&base_result){
        (Ok(auth),Ok(_))=>{let client=agent();if is_api(auth){fetch_api(auth,base,&client)}else{fetch_oauth(&current,auth,&client)}},
        (Err(e),_)|(_,Err(e))=>Err(ProbeError::local(e)),
    };
    let mut value=match result{
        Ok(v)=>v,
        Err(e)=>{
            let old=&current["chatgpt_quota"];
            let mut v=if old["identity"]==expected&&has_values(old){old.clone()}else{empty(mode,"")};
            v["state"]=json!(if has_values(&v){"cached"}else if e.status==Some(404){"unsupported"}else{"error"});
            v["message"]=json!(e.message);v["http_status"]=json!(e.status);v["attempted_at"]=json!(now());v
        }
    };
    let _guard=IO_LOCK.lock().unwrap_or_else(|e|e.into_inner());
    let mut latest=meta(current["name"].as_str().unwrap_or(""))?;
    if latest["chatgpt_home"]!=current["chatgpt_home"]{return Err("账号目录已变化，请重新读取额度".into());}
    if !expected.is_empty()&&current_identity(&latest).is_ok_and(|id|id!=expected){return Err("账号凭据已变化，请重新读取额度".into());}
    // Successful token rotation keeps the same account identity; derive again
    // after probing so the new snapshot belongs to the current auth generation.
    value["identity"]=json!(current_identity(&latest).unwrap_or(expected));
    latest["chatgpt_quota"]=value.clone();reg::save_instance_checked(APP,&latest)?;
    value.as_object_mut().map(|v|v.remove("identity"));
    Ok(json!({"quota":value,"msg":"额度读取已完成"}))
}

#[cfg(test)]mod tests{
    use super::*;
    #[test]fn official_windows_keep_real_zero_and_reset(){let body=json!({"account_id":"a","plan_type":"plus","rate_limit":{"primary_window":{"used_percent":100,"reset_after_seconds":120},"secondary_window":{"used_percent":0,"reset_at":1900000000}}});let v=parse_usage(&body,"a").unwrap();assert_eq!(v["windows"][0]["remaining_percent"],0.0);assert_eq!(v["windows"][1]["remaining_percent"],100.0);assert_eq!(v["windows"][1]["reset_at"],1900000000);assert!(parse_usage(&body,"b").is_err());}
    #[test]fn missing_windows_are_not_fabricated(){assert!(parse_usage(&json!({"rate_limit":{}}),"").is_err());assert!(parse_usage(&json!({"rate_limit":{"primary_window":{"used_percent":101}}}),"").is_err());}
    #[test]fn display_currency_does_not_use_raw_quota_multiplier(){let b=token_balance(&json!({"data":{"total_granted":900000000,"display":{"remaining":0,"total":180,"unit":"CNY"}}})).unwrap();assert_eq!(b["remaining"],0.0);assert_eq!(b["used"],180.0);assert_eq!(b["currency"],"CNY");}
    #[test]fn legacy_billing_divides_hundredths_and_does_not_guess_currency(){let b=billing_balance(&json!({"hard_limit_usd":90}),&json!({"total_usage":2000})).unwrap();assert_eq!(b["remaining"],70.0);assert_eq!(b["used"],20.0);assert_eq!(b["unit"],"");assert!(b["currency"].is_null());}
    #[test]fn provider_urls_reject_credentials_and_remote_plain_http(){for bad in ["https://user:secret@site.example/v1","https://site.example/v1?token=secret","http://site.example/v1","https://site.example/a/../v1"]{assert!(provider(bad).is_err());}let p=provider("https://api.aiwelink.cc/v1/").unwrap();assert_eq!(p.root,"https://api.aiwelink.cc");assert!(provider("http://127.0.0.1:8788/v1").is_ok());}
    #[test]fn bare_provider_origin_also_probes_v1_billing_without_changing_host(){let roots=billing_roots(&provider("https://api.aiwelink.cc").unwrap());assert_eq!(roots,vec!["https://api.aiwelink.cc","https://api.aiwelink.cc/v1"]);assert_eq!(billing_roots(&provider("https://api.aiwelink.cc/v1").unwrap()).len(),2);}
    #[test]fn sub2api_balance_preserves_explicit_currency_and_zero(){let b=sub2api_balance(&json!({"balance":0,"unit":"USD"})).unwrap();assert_eq!(b["remaining"],0.0);assert_eq!(b["currency"],"USD");let b=sub2api_balance(&json!({"quota":{"limit":20,"used":3,"unit":"CNY"}})).unwrap();assert_eq!(b["remaining"],17.0);}
    #[test]fn windows_proxy_formats_are_supported(){assert_eq!(proxy_address("https=127.0.0.1:7890;http=127.0.0.1:7890").as_deref(),Some("http://127.0.0.1:7890"));assert_eq!(proxy_address("127.0.0.1:7890").as_deref(),Some("http://127.0.0.1:7890"));}
    #[test]fn cache_identity_separates_keys_and_origins(){let a=json!({"OPENAI_API_KEY":"synthetic-one"});let b=json!({"OPENAI_API_KEY":"synthetic-two"});assert_ne!(identity(&a,"https://one.invalid"),identity(&b,"https://one.invalid"));assert_ne!(identity(&a,"https://one.invalid"),identity(&a,"https://two.invalid"));}
    #[test]#[ignore="opt-in read-only requests using registered accounts; never prints secrets"]
    fn live_registered_chatgpt_quotas(){
        if std::env::var("AGENTCOCKPIT_QUOTA_SMOKE").as_deref()!=Ok("1"){return;}
        let accounts=reg::load_instances(APP);assert!(!accounts.is_empty(),"register an account first");
        let mut failures=0;let mut checked=0;
        for (index,m) in accounts.iter().enumerate(){
            if std::env::var("AGENTCOCKPIT_QUOTA_API_ONLY").as_deref()==Ok("1")&&!read_auth(m).is_ok_and(|a|is_api(&a)){continue;}
            checked+=1;
            match refresh(m){Ok(result)=>{let q=&result["quota"];println!("account_index={} mode={} state={} windows={} balance={} status={}",index,q["mode"],q["state"],q["windows"],q["balance"],q["http_status"]);if q["state"]!="live"{failures+=1;eprintln!("quota_message={}",q["message"]);}},Err(_)=>{failures+=1;eprintln!("account_index={index} metadata unavailable");}}
        }
        assert!(checked>0,"no accounts match the requested quota check");assert_eq!(failures,0,"some registered accounts did not return live quota; see safe status output");
    }
}
