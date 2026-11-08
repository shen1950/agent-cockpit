//! Bounded, prompt-free observations of real gateway requests. Internal hops
//! join a trace only with a process-local random proof, never with a client ID.
use crate::gateway::Upstream;
use serde::Serialize;
use serde_json::{json,Value};
use std::{cell::Cell,collections::VecDeque,sync::{Mutex,OnceLock}};

pub const HEADER:&str="x-ac-route-trace";
thread_local! {static CURRENT:Cell<Option<u64>>=const {Cell::new(None)};}
#[derive(Clone,Serialize)]
struct Attempt {model:String,provider:String,account:String,state:String,reason:String,status:Option<u16>,started_at:i64}
#[derive(Clone,Serialize)]
struct Trace {id:u64,started_at:i64,agent:String,model:String,status:String,attempts:Vec<Attempt>,finished_at:Option<i64>,error:Option<String>,#[serde(skip)]http:Option<u16>}
#[derive(Default)]
struct State {seq:u64,total:u64,rerouted:u64,errors:u64,requests:VecDeque<Trace>}
fn state()->&'static Mutex<State>{static S:OnceLock<Mutex<State>>=OnceLock::new();S.get_or_init(Default::default)}
fn proof()->Option<&'static str>{static P:OnceLock<Option<String>>=OnceLock::new();P.get_or_init(||crate::remote::random_hex(32).ok()).as_deref()}
fn now()->i64{chrono::Local::now().timestamp_millis()}
fn modify(f:impl FnOnce(&mut Trace)) {CURRENT.with(|c|if let Some(id)=c.get(){let mut s=state().lock().unwrap_or_else(|e|e.into_inner());if let Some(t)=s.requests.iter_mut().find(|t|t.id==id && t.finished_at.is_none()){f(t);}});}
fn agent(ua:&str)->String{
    let s=ua.to_ascii_lowercase();
    for (needle,name) in [("workbuddy","WorkBuddy"),("qoder","Qoder"),("codex","Codex"),("claude-code","Claude Code"),("claude-cli","Claude Code"),("claude","Claude"),("opencode","OpenCode"),("trae","Trae"),("zcode","ZCode"),("dsh","DSH")] {
        if s.contains(needle){return name.into();}
    }
    "未识别 Agent".into()
}
pub struct Guard {id:u64,previous:Option<u64>,owner:bool}
pub fn begin(model:&str,ua:&str,mark:&str)->Guard{
    let mut s=state().lock().unwrap_or_else(|e|e.into_inner());
    let joined=mark.split_once('.').and_then(|(p,n)|(Some(p)==proof()).then(||n.parse::<u64>().ok()).flatten())
        .filter(|id|s.requests.iter().any(|t|t.id==*id && t.finished_at.is_none()));
    let id=joined.unwrap_or_else(||{
        s.seq=s.seq.wrapping_add(1); let id=s.seq;s.total+=1;
        s.requests.push_back(Trace{id,started_at:now(),agent:agent(ua),model:model.into(),status:"attempting".into(),attempts:vec![],finished_at:None,error:None,http:None});
        if s.requests.len()>64 {s.requests.pop_front();} id
    });
    Guard{id,previous:CURRENT.with(|c|c.replace(Some(id))),owner:joined.is_none()}
}
impl Drop for Guard {fn drop(&mut self){
    if self.owner {
        let mut s=state().lock().unwrap_or_else(|e|e.into_inner());
        let counts=if let Some(t)=s.requests.iter_mut().find(|t|t.id==self.id) {
            let ok=t.http.is_some_and(|n|(200..300).contains(&n));
            t.status=if ok{"completed"}else{"failed"}.into();t.finished_at=Some(now());
            if !ok {t.error=Some(t.http.map_or("请求中断".into(),|n|format!("HTTP {n}")));}
            if let Some(a)=t.attempts.last_mut(){if a.state!="failed"{a.state=t.status.clone();}}
            (t.attempts.len()>1,!ok)
        } else {(false,false)};
        s.rerouted+=u64::from(counts.0);s.errors+=u64::from(counts.1);
    }
    CURRENT.with(|c|c.set(self.previous));
}}
pub fn marker()->Option<String>{CURRENT.with(|c|Some(format!("{}.{}",proof()?,c.get()?)))}
pub fn snapshot()->Value{let s=state().lock().unwrap_or_else(|e|e.into_inner());json!({"requests":s.requests.iter().rev().collect::<Vec<_>>(),"total":s.total,"rerouted":s.rerouted,"errors":s.errors})}
pub fn waiting(_model:&str){modify(|t|{t.status="queued".into();});}
pub fn attempt(model:&str,up:&Upstream,reason:&str){modify(|t|{
    t.status="attempting".into();
    let account=format!("账号 {}",&crate::gateway_dispatch::lane_id(up)[..6]);
    if t.attempts.last().is_some_and(|a|a.model==model && a.account==account && a.status.is_none()){return;}
    if t.attempts.len()>=64{return;}
    t.http=None;
    t.attempts.push(Attempt{model:model.into(),provider:crate::proxy::owner_of_base(&up.base_url).unwrap_or("自定义上游").into(),account,state:"attempting".into(),reason:reason.into(),status:None,started_at:now()});
});}
pub fn response(status:u16){modify(|t|{
    // HTTP 200 can precede an SSE error. A later completion log must not erase
    // that error. Starting a distinct attempt above resets the status.
    if status<400 && t.http.is_some_and(|n|n>=400){return;}
    t.http=Some(status);t.status=if (200..300).contains(&status){"streaming"}else{"attempting"}.into();
    if let Some(a)=t.attempts.last_mut(){a.status=Some(status);a.state=if status<300{"streaming"}else{"failed"}.into();}
});}

#[cfg(test)]mod tests {
    use super::*;
    #[test] fn trace_joins_only_signed_hops_and_masks_accounts(){
        let outer=begin("trae/test","codex-cli","forged.1");
        let token=marker().unwrap();
        let nested=begin("trae/test","agent-cockpit",&token);
        assert_eq!(outer.id,nested.id);
        attempt("trae/test",&Upstream {label:"private@example.test".into(),token:"private-key".into(),base_url:"https://unit.test".into(),pool:"private-pool".into()},"可用账号");
        response(200);drop(nested);drop(outer);
        let data=snapshot();let t=data["requests"].as_array().unwrap().iter().find(|t|t["id"]==outer_id(&token)).unwrap();
        assert_eq!(t["agent"],"Codex");assert_eq!(t["status"],"completed");assert!(!t.to_string().contains("private"));
    }
    fn outer_id(token:&str)->u64{token.split_once('.').unwrap().1.parse().unwrap()}
    #[test]fn stream_failure_survives_http_completion_and_fallback_can_recover(){
        let guard=begin("trae/test","test","");let id=guard.id;
        let up=Upstream{label:"unit".into(),token:String::new(),base_url:"https://unit.test".into(),pool:String::new()};
        attempt("trae/test",&up,"first");response(200);response(502);response(200);
        assert_eq!(snapshot()["requests"].as_array().unwrap().iter().find(|t|t["id"]==id).unwrap()["attempts"][0]["status"],502);
        waiting("trae/backup");attempt("trae/backup",&up,"fallback");response(200);drop(guard);
        let data=snapshot();let t=data["requests"].as_array().unwrap().iter().find(|t|t["id"]==id).unwrap();
        assert_eq!(t["status"],"completed");assert_eq!(t["model"],"trae/test");assert_eq!(t["attempts"].as_array().unwrap().len(),2);
    }
}
