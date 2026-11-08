//! Provider admission, ordered failover and local settings. All attempts still pass gateway authorization.
use crate::{gateway::Upstream, gateway_cost::{Period, Rates}, ingress::Ingress};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{collections::{BTreeMap, HashMap, HashSet, VecDeque}, io::{self, BufRead, Read},
    sync::{Condvar, Mutex, OnceLock}, time::{Duration,Instant}};
use tiny_http::{Header, Request, Response, StatusCode};

#[derive(Clone, Default, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default,deny_unknown_fields)]
pub struct Rule {
    pub concurrency:u32, pub accounts:u32, pub rpm:u32, pub queue_limit:u32, pub queue_wait:u32,
    pub fallbacks:Vec<String>, pub order:Order,
    pub multiplier:Option<f64>, pub free:bool, pub periods:Vec<Period>,
}
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all="snake_case")]
pub enum Order { #[default] Listed, FreeFirst, Cheapest }
impl Rule {
    pub fn scheduled(&self)->bool {
        self.concurrency>0 || self.accounts>0 || self.rpm>0 || !self.fallbacks.is_empty()
    }
    fn validate(&self)->Result<(),String> {
        if self.concurrency>1000 || self.accounts>1000 || self.rpm>1_000_000 || self.queue_limit>10_000 || self.queue_wait>3600 {
            return Err("并发和账号上限最大 1000，RPM 最大 100 万，排队最大 1 万，等待最长 3600 秒".into());
        }
        if self.fallbacks.len()>24 || self.periods.len()>32 || self.multiplier.is_some_and(|n|!crate::gateway_cost::valid_factor(n)) || !self.periods.iter().all(Period::valid) {
            return Err("请检查价格倍率、时段和备用列表（最多 24 项，最多 32 个时段）".into());
        }
        Ok(())
    }
}
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings { pub providers:BTreeMap<String,Rule>, pub prices:BTreeMap<String,Rates> }
fn path()->std::path::PathBuf {crate::registry::base_dir().join("gateway-provider-settings.json")}
pub fn load()->Result<Settings,String>{
    crate::client_connect::read(&path())?.map(|s|serde_json::from_str(&s).map_err(|_|"供应商调度配置文件格式无效".into())).unwrap_or_else(||Ok(Settings::default()))
}
pub fn save(id:&str, row:&Value, scope:&str, input:Value, catalog:&[Value], groups:&[Value])->Result<(),String>{
    let mut settings=load()?;
    let owner=row["owned_by"].as_str().ok_or("模型缺少来源")?;
    match scope {
        "provider"=>{
            let rule:Rule=serde_json::from_value(input).map_err(|_|"供应商设置格式无效")?;
            rule.validate()?;
            let old=settings.providers.get(owner);
            for id in &rule.fallbacks {
                // Keep a previously saved route that's outside this key, but never use it with this key.
                if !old.is_some_and(|r|r.fallbacks.contains(id)) && !catalog.iter().any(|m|m["id"]==id.as_str())
                    && !groups.iter().any(|g|Some(g["id"].as_str().unwrap_or(""))==id.strip_prefix("pool:")) {
                    return Err(format!("这把密钥不能使用备用项 {id}"));
                }
            }
            if rule==Rule::default(){settings.providers.remove(owner);} else {settings.providers.insert(owner.to_owned(),rule);}
        }
        "model"=>{
            if input.is_null(){settings.prices.remove(id);} else {
                let rates:Rates=serde_json::from_value(input).map_err(|_|"需要填写每百万 token 的输入与输出单价")?;
                if !rates.valid(){return Err("单价必须为有效的非负数".into());}
                settings.prices.insert(id.to_owned(),rates);
            }
        }
        _=>return Err("设置范围无效".into()),
    }
    crate::client_connect::write(&path(),&serde_json::to_string_pretty(&settings).map_err(|e|e.to_string())?)
}

pub const MARK:&str="x-ac-scheduled-attempt";
fn secret()->Result<&'static str,String>{
    static SECRET:OnceLock<Result<String,String>>=OnceLock::new();
    SECRET.get_or_init(||crate::remote::random_hex(32)).as_deref().map_err(Clone::clone)
}
pub fn verified(mark:&str)->Option<&str>{
    let (proof,id)=mark.split_once('.')?;
    (proof==secret().ok()? && id.len()==64 && id.bytes().all(|b|b.is_ascii_hexdigit())).then_some(id)
}
pub fn lane_id(up:&Upstream)->String{
    crate::wbproxy::hex(&crate::wbproxy::sha256(format!("{}\0{}",up.base_url,up.label).as_bytes()))
}
#[derive(Default)]
struct Lane { running:usize, queue:VecDeque<u64>, starts:VecDeque<Instant>, rest:Option<Instant> }
#[derive(Default)]
struct Traffic {lanes:HashMap<String,Lane>,seq:u64, resting:HashMap<String,Instant>}
fn traffic()->&'static (Mutex<Traffic>,Condvar){
    static STATE:OnceLock<(Mutex<Traffic>,Condvar)>=OnceLock::new();
    STATE.get_or_init(Default::default)
}
struct Permit(String);
impl Drop for Permit {fn drop(&mut self){
    let (state,wake)=traffic(); let mut g=state.lock().unwrap_or_else(|e|e.into_inner());
    if let Some(l)=g.lanes.get_mut(&self.0){l.running=l.running.saturating_sub(1);}
    wake.notify_all();
}}
const MINUTE:Duration=Duration::from_secs(60);
fn prune(l:&mut Lane,now:Instant){while l.starts.front().is_some_and(|t|now.duration_since(*t)>=MINUTE){l.starts.pop_front();}}
fn room(l:&Lane, r:&Rule)->bool{(r.concurrency==0 || l.running<r.concurrency as usize) && (r.rpm==0 || l.starts.len()<r.rpm as usize)}
fn acquire(candidates:&[String],rule:&Rule)->Result<Permit,(u16,String)>{
    let (state,wake)=traffic();
    let mut g=state.lock().unwrap_or_else(|e|e.into_inner()); let start=Instant::now();
    let mut best=None;
    let max_accounts=if rule.accounts==0 {usize::MAX} else {rule.accounts as usize};
    let mut active=0;
    for id in candidates {
        let l=g.lanes.entry(id.clone()).or_default();
        prune(l,start);
        if l.rest.is_some_and(|t|t>start){continue;}
        if active>=max_accounts {break;} active+=1;
        let score=(!room(l,rule) || !l.queue.is_empty(),l.queue.len(),l.running);
        if best.as_ref().is_none_or(|(_,s)|score<*s){best=Some((id.clone(),score));}
    }
    let (id,_)=best.ok_or_else(||(503,"没有就绪的供应商账号".to_owned()))?;
    let ticket=g.seq; g.seq=g.seq.wrapping_add(1);
    let l=g.lanes.get_mut(&id).unwrap();
    if rule.queue_limit>0 && l.queue.len()>=rule.queue_limit as usize {return Err((429,"供应商账号队列已满".into()));}
    l.queue.push_back(ticket);
    loop {
        let now=Instant::now(); let l=g.lanes.get_mut(&id).unwrap(); prune(l,now);
        if l.rest.is_some_and(|t|t>now) {
            l.queue.retain(|t|*t!=ticket); wake.notify_all(); return Err((503,"供应商账号暂时不可用".into()));
        }
        if l.queue.front()==Some(&ticket) && room(l,rule) {
            l.queue.pop_front();l.running+=1;
            if rule.rpm>0 {l.starts.push_back(now);}
            wake.notify_all();return Ok(Permit(id));
        }
        let rpm_blocked=rule.rpm>0 && l.starts.len()>=rule.rpm as usize;
        let elapsed=now.duration_since(start);
        let mut wait=if rule.queue_wait>0 {Duration::from_secs(rule.queue_wait as u64).saturating_sub(elapsed)} else {Duration::MAX};
        if rpm_blocked {wait=wait.min(Duration::from_secs(120).saturating_sub(elapsed));}
        if wait.is_zero(){l.queue.retain(|t|*t!=ticket);wake.notify_all();return Err((429,"供应商排队等待超时".into()));}
        if rpm_blocked {wait=wait.min(MINUTE.saturating_sub(now.duration_since(*l.starts.front().unwrap())));}
        g=if wait==Duration::MAX {wake.wait(g).unwrap_or_else(|e|e.into_inner())}
            else {wake.wait_timeout(g,wait).unwrap_or_else(|e|e.into_inner()).0};
    }
}
fn rest_account(id:&str){let (s,w)=traffic();s.lock().unwrap_or_else(|e|e.into_inner()).lanes.entry(id.to_owned()).or_default().rest=Some(Instant::now()+MINUTE);w.notify_all();}
fn provider_rest(owner:&str,set:bool)->bool{
    let now=Instant::now();let mut s=traffic().0.lock().unwrap_or_else(|e|e.into_inner());
    if set{s.resting.insert(owner.to_owned(),now+MINUTE);}
    s.resting.get(owner).is_some_and(|t|*t>now)
}

pub fn authorized_groups(ups:&[Upstream])->Vec<Value>{
    crate::gateway::pools().into_iter().filter(|p|p["enabled"]!=false && ups.iter().any(|u|p["name"]==u.pool))
        .map(|p|json!({"id":p["id"],"name":p["name"]})).collect()
}
const REASONING_LEVELS:[&str;3]=["low","medium","high"];
/// 推理思考档位（模型/供应商设置）：low/medium/high 注入 `reasoning_effort`；
/// "off" 剥掉客户端自带的推理参数；没设置就不干预（自动）。
fn apply_reasoning(body:&mut Value,row:&Value){
    match row["reasoning_effort"].as_str() {
        Some("off")=>{if let Some(o)=body.as_object_mut(){for k in ["reasoning_effort","reasoning","enable_thinking","thinking"]{o.remove(k);}}}
        Some(level) if REASONING_LEVELS.contains(&level)=>{body["reasoning_effort"]=json!(level);}
        _=>{}
    }
}
fn retryable(status:u16)->bool{matches!(status,401|402|408|429|500|502|503|504)}
fn score(model:&Value,settings:&Settings,order:Order)->f64{
    let owner=model["owned_by"].as_str().unwrap_or("");
    if model["free"]==true || settings.providers.get(owner).is_some_and(|r|r.free){return 0.0;}
    if order==Order::FreeFirst{return 1.0;}
    crate::gateway_cost::effective(model["id"].as_str().unwrap_or(""),settings,chrono::Local::now().timestamp())
        .map_or(f64::INFINITY,|r|r.input+r.output)
}

type Reader=Box<dyn Read+Send+Sync>;
struct Attempt {status:u16,ctype:String,reader:Reader}
fn forward(port:u16,path:&str,key:&str,headers:&[(String,String)],lane:&str,payload:&Value,describing:bool)->Result<Attempt,(u16,String)>{
    let agent=ureq::AgentBuilder::new().redirects(0).timeout(Duration::from_secs(600)).build();
    let mut req=agent.post(&format!("http://127.0.0.1:{port}{path}"));
    for (k,v) in headers {req=req.set(k,v);}
    if describing {req=req.set(crate::gateway_vision::DESCRIBER_HEADER,"1");}
    if let Some(mark)=crate::gateway_routes::marker(){req=req.set(crate::gateway_routes::HEADER,&mark);}
    let mark=format!("{}.{lane}",secret().map_err(|e|(500,e))?);
    let res=match req.set("Authorization",&format!("Bearer {key}")).set(MARK,&mark).send_json(payload) {
        Ok(r)|Err(ureq::Error::Status(_,r))=>r,Err(_)=>return Err((502,"供应商请求连接失败".into()))
    };
    let status=res.status(); let ctype=res.header("content-type").unwrap_or("application/json").to_string();
    let mut reader=res.into_reader();
    if status>=400 {let mut note=String::new();let _=reader.take(32768).read_to_string(&mut note);return Err((status,note));}
    if ctype.contains("text/event-stream") {reader=peek_stream(reader)?;}
    Ok(Attempt {status,ctype,reader})
}
/// Inspect complete SSE events before committing. Once content/tool output starts, the stream is never replayed.
fn peek_stream(reader:Reader)->Result<Reader,(u16,String)>{
    let mut input=io::BufReader::new(reader);let mut prefix=Vec::new(); let mut start=0;
    while prefix.len()<65536 {
        let n=(&mut input).take((65536-prefix.len()) as u64).read_until(b'\n',&mut prefix).map_err(|_|(502,"上游流在回复开始前断开".to_owned()))?;
        if n==0 {return Err((502,"上游流在回复开始前结束".into()));}
        let line=String::from_utf8_lossy(&prefix[start..]).trim().to_owned(); start=prefix.len();
        if line=="event: error" || line=="event:error" {continue;}
        let Some(data)=line.strip_prefix("data:").map(str::trim) else {continue};
        if data=="[DONE]"{break;}
        if let Ok(v)=serde_json::from_str::<Value>(data){
            if !v["error"].is_null() || v["type"]=="error" || v["type"]=="response.failed" {
                let msg=v.get("error").filter(|v|!v.is_null()).unwrap_or(&v);
                let kind=msg["type"].as_str().or_else(||msg["code"].as_str()).unwrap_or("");
                let status=match kind {"invalid_request_error"|"context_length_exceeded"=>400,"rate_limit_error"=>429,_=>502};
                return Err((status,msg.to_string()));
            }
            if v["type"]=="message_start" || v["type"]=="response.created" || v["type"]=="response.in_progress" {continue;}
            if let Some(delta)=v.pointer("/choices/0/delta").and_then(Value::as_object) {
                if delta.iter().all(|(k,v)|k=="role" || v.is_null() || v==""){continue;}
            }
            break;
        }
    }
    Ok(Box::new(io::Cursor::new(prefix).chain(input)))
}

pub struct Dispatch<'a>{pub port:u16,pub key:&'a str,pub path:&'a str,pub model:&'a str,pub body:&'a [u8],
    pub headers:&'a [(String,String)],pub ups:&'a [Upstream],pub settings:&'a Settings,pub models:&'a [Value],pub describing:bool}
pub fn run(client:Request, d:Dispatch<'_>){
    let owner=d.model.split_once('/').map(|s|s.0).unwrap_or("");
    let default=Rule::default();let first=d.settings.providers.get(owner).unwrap_or(&default);
    let mut picks=vec![(d.model.to_owned(),None::<String>)];let groups=authorized_groups(d.ups);
    for pick in &first.fallbacks {
        if let Some(id)=pick.strip_prefix("pool:"){
            if let Some(name)=groups.iter().find(|g|g["id"]==id).and_then(|g|g["name"].as_str()){
                for m in d.models.iter().filter(|m|d.ups.iter().any(|u|u.pool==name && crate::proxy::owner_of_base(&u.base_url)==m["owned_by"].as_str())){
                    if let Some(id)=m["id"].as_str(){picks.push((id.into(),Some(name.into())));}
                }
            }
        }else if d.models.iter().any(|m|m["id"]==pick.as_str()){picks.push((pick.clone(),None));}
    }
    if first.order!=Order::Listed {
        let cost=|id:&str|d.models.iter().find(|m|m["id"]==id).map_or(f64::INFINITY,|m|score(m,d.settings,first.order));
        picks[1..].sort_by(|a,b|cost(&a.0).total_cmp(&cost(&b.0)));
    }
    let wire=Ingress::detect("POST",d.path);
    let error=|status,msg:&str|wire.map_or_else(||json!({"error":{"message":msg}}),|i|crate::ingress::error_json(i,status,msg));
    let Ok(original)=serde_json::from_slice::<Value>(d.body) else {crate::gateway_routes::response(400);let _=client.respond(crate::proxy::text_response(400,&error(400,"请求体不是 JSON").to_string()));return;};
    let mut last=(503,"没有获得授权且可用的备用模型或账号".to_owned());let mut tried=HashSet::new();
    'models: for (model,pool) in picks {
        let Some((vendor,_))=model.split_once('/') else {continue};
        if provider_rest(vendor,false){continue;}
        let rule=d.settings.providers.get(vendor).unwrap_or(&default);
        let mut ids:Vec<_>=d.ups.iter().filter(|u|crate::proxy::owner_of_base(&u.base_url)==Some(vendor) && pool.as_ref().is_none_or(|p|p==&u.pool)).map(lane_id).collect();
        let mut seen=HashSet::new(); ids.retain(|l|seen.insert(l.clone()) && !tried.contains(&(model.clone(),l.clone())));
        if ids.is_empty(){continue;}
        let mut body=original.clone();
        let path=if wire==Some(Ingress::Gemini){d.path.replacen(&format!("/models/{}:",d.model),&format!("/models/{model}:"),1)}else{body["model"]=json!(model);d.path.to_owned()};
        // Vision may use the same provider. Complete it before taking a slot so
        // concurrency=1 cannot deadlock while waiting for its own describer.
        let mut prepared=serde_json::to_vec(&body).expect("JSON body");
        if let Err(e)=crate::gateway_vision::prepare(&path,&model,&mut prepared,d.key,d.port,d.describing,||d.models.to_vec()) {
            last=e;continue;
        }
        body=serde_json::from_slice(&prepared).expect("prepared JSON");
        if let Some(row)=d.models.iter().find(|m|m["id"]==model){apply_reasoning(&mut body,row);}
        while !ids.is_empty(){
            crate::gateway_routes::waiting(&model);
            let permit=match acquire(&ids,rule){Ok(v)=>v,Err(e)=>{last=e;continue 'models;}};
            let lane=&permit.0; ids.retain(|l|l!=lane);tried.insert((model.clone(),lane.clone()));
            if let Some(up)=d.ups.iter().find(|u|lane_id(u)==*lane){crate::gateway_routes::attempt(&model,up,if model==d.model {"按供应商设置选择可用账号"}else{"前一来源不可用，尝试备用路由"});}
            match forward(d.port,&path,d.key,d.headers,lane,&body,d.describing){
                Ok(a)=>{
                    crate::gateway_routes::response(a.status);
                    let headers=vec![Header::from_bytes("Content-Type",a.ctype).unwrap(),Header::from_bytes("Access-Control-Allow-Origin","*").unwrap()];
                    if client.respond(Response::new(StatusCode(a.status),headers,a.reader,None,None)).is_err(){crate::gateway_routes::response(499);} return;
                }
                Err(e) if retryable(e.0)=>{crate::gateway_routes::response(e.0);rest_account(lane);last=e;}
                Err(e)=>{crate::gateway_routes::response(e.0);last=e;break 'models;}
            }
        }
        if !first.fallbacks.is_empty(){provider_rest(vendor,true);}
    }
    let (status,msg)=last;
    crate::gateway_routes::response(status);
    let message=serde_json::from_str::<Value>(&msg).ok().and_then(|v|v.pointer("/error/message").or_else(||v.get("message")).and_then(Value::as_str).map(str::to_owned)).unwrap_or(msg);
    let _=client.respond(crate::proxy::text_response(status,&error(status,&message).to_string()));
}

#[cfg(test)]mod tests {
    use super::*;
    #[test]fn reasoning_override_injects_and_off_strips_client_fields(){
        let mut b=json!({"model":"x","reasoning_effort":"medium","enable_thinking":true});
        apply_reasoning(&mut b,&json!({"reasoning_effort":"off"}));
        assert!(b.get("reasoning_effort").is_none());
        assert!(b.get("enable_thinking").is_none());
        apply_reasoning(&mut b,&json!({"reasoning_effort":"low"}));
        assert_eq!(b["reasoning_effort"],"low");
        // 没设置档位（自动）时不干预客户端自己的选择
        apply_reasoning(&mut b,&json!({}));
        assert_eq!(b["reasoning_effort"],"low");
        // 供应商范围来的未知值不注入
        let mut c=json!({"model":"x"});
        apply_reasoning(&mut c,&json!({"reasoning_effort":"turbo"}));
        assert!(c.get("reasoning_effort").is_none());
    }
    #[test]fn limits_queue_fifo_release_and_rpm_window(){
        let lane=format!("unit-{}",crate::remote::random_hex(8).unwrap());
        let rule=Rule{concurrency:1,queue_limit:1,queue_wait:2,..Default::default()};
        let first=acquire(&[lane.clone()],&rule).unwrap();
        let (tx,rx)=std::sync::mpsc::channel();let second_lane=lane.clone();let second_rule=rule.clone();
        let thread=std::thread::spawn(move||{let p=acquire(&[second_lane],&second_rule).unwrap();tx.send(()).unwrap();drop(p);});
        // Wait until the second ticket is in the queue, with a bounded deadline.
        let end=Instant::now()+Duration::from_secs(1);
        loop {if traffic().0.lock().unwrap().lanes[&lane].queue.len()==1{break;}
            assert!(Instant::now()<end);std::thread::yield_now();}
        assert!(matches!(acquire(&[lane.clone()],&rule),Err((429,_))));
        assert!(rx.try_recv().is_err());drop(first);
        rx.recv_timeout(Duration::from_secs(2)).unwrap();thread.join().unwrap();
        assert_eq!(traffic().0.lock().unwrap().lanes[&lane].running,0);
        let mut l=Lane::default();let now=Instant::now();l.starts.push_back(now-MINUTE);
        let rpm=Rule{rpm:1,..Default::default()};assert!(!room(&l,&rpm));prune(&mut l,now);assert!(room(&l,&rpm));
    }
    #[test]fn stream_retry_stops_at_first_content(){
        let probe=|s:&str|peek_stream(Box::new(io::Cursor::new(s.as_bytes().to_vec())));
        assert!(matches!(probe("data: {\"choices\":[{\"delta\":{\"role\":\"assistant\"}}]}\n\ndata: {\"error\":{\"type\":\"rate_limit_error\"}}\n\n"),Err((429,_))));
        assert!(matches!(probe("data: {\"type\":\"response.created\"}\n\n"),Err((502,_))));
        let s="data: {\"choices\":[{\"delta\":{\"content\":\"hello\"}}]}\n\ndata: {\"error\":{\"message\":\"late\"}}\n\n";
        let mut reader=probe(s).unwrap();let mut out=String::new();reader.read_to_string(&mut out).unwrap();assert_eq!(out,s);
    }
    #[test]fn forged_admission_marker_is_rejected(){
        assert!(verified(&format!("fake.{}","a".repeat(64))).is_none());
        assert!(verified(&format!("{}.{}",secret().unwrap(),"a".repeat(64))).is_some());
    }
    #[test]fn real_loopback_fallback_obeys_authorized_catalog(){
        use tiny_http::Server;
        let inner=Server::http("127.0.0.1:0").unwrap();let port=inner.server_addr().to_ip().unwrap().port();
        let mock=std::thread::spawn(move||{
            let mut asked=vec![];
            for i in 0..2 {
                let mut r=inner.recv_timeout(Duration::from_secs(5)).unwrap().unwrap();
                let mut text=String::new();r.as_reader().read_to_string(&mut text).unwrap();
                let body:Value=serde_json::from_str(&text).unwrap();asked.push(body["model"].clone());
                assert!(r.headers().iter().any(|h|h.field.equiv(MARK) && verified(h.value.as_str()).is_some()));
                let content=if i==0 {"data: {\"error\":{\"type\":\"rate_limit_error\"}}\n\n"}
                    else {"data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\ndata: [DONE]\n\n"};
                r.respond(Response::from_string(content).with_header(Header::from_bytes("Content-Type","text/event-stream").unwrap())).unwrap();
            }
            asked
        });
        let front=Server::http("127.0.0.1:0").unwrap();let addr=front.server_addr().to_ip().unwrap();
        let dispatch=std::thread::spawn(move||{
            let req=front.recv_timeout(Duration::from_secs(5)).unwrap().unwrap();
            let ups=vec![Upstream{label:crate::remote::random_hex(8).unwrap(),base_url:crate::traeproxy::BASE_MARK.into(),token:String::new(),pool:"unit-only".into()},
                Upstream{label:crate::remote::random_hex(8).unwrap(),base_url:crate::mmproxy::BASE_MARK.into(),token:String::new(),pool:"unit-only".into()}];
            let mut settings=Settings::default();settings.providers.insert("trae".into(),Rule{fallbacks:vec!["qoder/forbidden".into(),"minimax/backup".into()],..Default::default()});
            let models=vec![json!({"id":"trae/primary","owned_by":"trae"}),json!({"id":"minimax/backup","owned_by":"minimax"})];
            let body=br#"{"model":"trae/primary","messages":[{"role":"user","content":"test"}],"stream":true}"#;
            run(req,Dispatch{port,key:"unit-key",path:"/v1/chat/completions",model:"trae/primary",body,headers:&[],ups:&ups,settings:&settings,models:&models,describing:false});
        });
        let result=ureq::post(&format!("http://{addr}/v1/chat/completions")).timeout(Duration::from_secs(10)).send_json(json!({})).unwrap().into_string().unwrap();
        assert!(result.contains("ok"));assert!(!result.contains("rate_limit_error"));
        dispatch.join().unwrap();assert_eq!(mock.join().unwrap(),vec![json!("trae/primary"),json!("minimax/backup")]);
        // This test owns the only provider cooldown it created.
        traffic().0.lock().unwrap().resting.remove("trae");
    }
    #[test]fn vision_preparation_finishes_before_its_provider_slot_is_taken(){
        use tiny_http::Server;
        let inner=Server::http("127.0.0.1:0").unwrap();let port=inner.server_addr().to_ip().unwrap().port();
        let up=Upstream{label:crate::remote::random_hex(8).unwrap(),base_url:crate::wbproxy::base_mark("cn"),token:String::new(),pool:"unit-only".into()};
        let vendor=crate::proxy::owner_of_base(&up.base_url).unwrap();let model=format!("{vendor}/text");let eye=format!("{vendor}/eye-{}",up.label);let lane=lane_id(&up);let check=lane.clone();
        let mock=std::thread::spawn(move||{
            for i in 0..2 {
                let mut r=inner.recv_timeout(Duration::from_secs(5)).unwrap().unwrap();let mut input=String::new();r.as_reader().read_to_string(&mut input).unwrap();
                let running=traffic().0.lock().unwrap().lanes.get(&check).map_or(0,|l|l.running);
                if i==0 {assert_eq!(running,0);assert!(input.contains("image_url"));}
                else {assert_eq!(running,1);assert!(input.contains("图片描述"));assert!(!input.contains("image_url"));}
                r.respond(Response::from_string(json!({"choices":[{"message":{"content":"blue square"}}]}).to_string())).unwrap();
            }
        });
        let front=Server::http("127.0.0.1:0").unwrap();let addr=front.server_addr().to_ip().unwrap();
        let dispatch=std::thread::spawn(move||{
            let req=front.recv_timeout(Duration::from_secs(5)).unwrap().unwrap();let mut settings=Settings::default();settings.providers.insert(vendor.into(),Rule{concurrency:1,..Default::default()});
            let models=vec![json!({"id":model,"native_vision":false,"vision_model":eye}),json!({"id":eye,"native_vision":true})];
            let body=serde_json::to_vec(&json!({"model":model,"messages":[{"role":"user","content":[{"type":"image_url","image_url":{"url":"data:image/png;base64,YQ=="}}]}]})).unwrap();
            run(req,Dispatch{port,key:"unit-key",path:"/v1/chat/completions",model:&model,body:&body,headers:&[],ups:&[up],settings:&settings,models:&models,describing:false});
        });
        let reply=ureq::post(&format!("http://{addr}")).timeout(Duration::from_secs(10)).send_json(json!({})).unwrap().into_string().unwrap();
        assert!(reply.contains("blue square"));dispatch.join().unwrap();mock.join().unwrap();assert_eq!(traffic().0.lock().unwrap().lanes[&lane].running,0);
    }
}
