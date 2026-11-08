//! Local price book and a usage-only ledger. Historical costs are calculated at read time.
use chrono::{Datelike, Local, TimeZone, Timelike};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{collections::BTreeMap, fs, io::{self, BufRead, Read, Write}, sync::{Mutex, OnceLock, RwLock}};
use tiny_http::{Request, Response};

#[derive(Default, Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Rates { pub input: f64, pub output: f64, pub cache_read: Option<f64>, pub cache_write: Option<f64> }
impl Rates {
    pub fn valid(&self) -> bool {
        [Some(self.input), Some(self.output), self.cache_read, self.cache_write].into_iter().flatten()
            .all(|n| n.is_finite() && (0.0..=1_000_000.0).contains(&n))
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Period { pub start: String, pub end: String, #[serde(default)] pub days: Vec<u8>, pub factor: f64 }
fn minute(s: &str) -> Option<u32> {
    let t = chrono::NaiveTime::parse_from_str(s,"%H:%M").ok()?;
    Some(t.hour()*60+t.minute())
}
impl Period {
    pub fn valid(&self) -> bool {
        minute(&self.start).is_some() && minute(&self.end).is_some() && self.start != self.end
            && self.days.iter().all(|n| (1..=7).contains(n)) && valid_factor(self.factor)
    }
    fn matches(&self, now: u32, day: u8) -> bool {
        let (Some(start),Some(end)) = (minute(&self.start),minute(&self.end)) else { return false; };
        let (hit, weekday) = if start < end { (now>=start && now<end, day) }
            else if now>=start { (true,day) } else { (now<end, if day==1 {7} else {day-1}) };
        hit && (self.days.is_empty() || self.days.contains(&weekday))
    }
}
pub fn valid_factor(n:f64) -> bool { n.is_finite() && (0.0..=10_000.0).contains(&n) }

type Facts = BTreeMap<String, Value>;
fn book() -> &'static RwLock<Facts> {
    static BOOK: OnceLock<RwLock<Facts>> = OnceLock::new();
    BOOK.get_or_init(|| RwLock::new(fs::read(crate::registry::base_dir().join("gateway-public-models.json"))
        .ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()))
}
pub fn facts(id: &str) -> Option<Value> {
    let bare = id.split_once('/')?.1.to_lowercase();
    book().read().ok()?.get(&bare).cloned()
}
pub fn refresh_book() -> Result<usize,String> {
    let response = ureq::get("https://models.dev/api.json").timeout(std::time::Duration::from_secs(30))
        .call().map_err(|e|format!("无法读取 models.dev：{e}"))?;
    let data: Value = serde_json::from_reader(response.into_reader().take(32*1024*1024))
        .map_err(|_|"models.dev 目录格式无效")?;
    // Reseller prices for the same name differ. Use the publishers' own entries only.
    let mut entries=Facts::new();
    for vendor in ["openai","anthropic","google","deepseek","zai","moonshotai","minimax","alibaba","xai","stepfun","mistral","xiaomi"] {
        for (id,model) in data[vendor]["models"].as_object().into_iter().flatten() {
            let rate = serde_json::from_value::<Rates>(model["cost"].clone()).ok().filter(Rates::valid);
            entries.insert(id.to_lowercase(),json!({"vendor":vendor,"rates":rate,
                "context_window":model["limit"]["context"],"max_output_tokens":model["limit"]["output"],
                "vision":model["modalities"]["input"].as_array().map(|a|a.iter().any(|v|v=="image"))}));
        }
    }
    if entries.is_empty() { return Err("未获取到厂商模型，保留上次目录".into()); }
    crate::client_connect::write(&crate::registry::base_dir().join("gateway-public-models.json"),
        &serde_json::to_string(&entries).map_err(|e|e.to_string())?)?;
    let n=entries.len();
    *book().write().map_err(|_|"价格目录无法更新")?=entries;
    Ok(n)
}

fn factor(rule: &crate::gateway_dispatch::Rule, custom: bool, ts: i64) -> f64 {
    let time_factor=Local.timestamp_opt(ts,0).single().and_then(|dt|rule.periods.iter()
        .find(|p|p.matches(dt.hour()*60+dt.minute(),dt.weekday().number_from_monday() as u8))).map_or(1.0,|p|p.factor);
    time_factor * if custom {1.0} else {rule.multiplier.unwrap_or(1.0)}
}
pub fn effective(model:&str, settings:&crate::gateway_dispatch::Settings, ts:i64) -> Option<Rates> {
    let custom=settings.prices.get(model);
    let mut rate=custom.cloned().or_else(||serde_json::from_value(facts(model)?["rates"].clone()).ok())?;
    let rule=settings.providers.get(model.split_once('/')?.0);
    let f=rule.map_or(1.0,|r|factor(r,custom.is_some(),ts));
    rate.input*=f; rate.output*=f; rate.cache_read=rate.cache_read.map(|n|n*f); rate.cache_write=rate.cache_write.map(|n|n*f);
    Some(rate)
}

#[derive(Default, Clone, Debug, Serialize, Deserialize)]
struct Usage { input:u64, output:u64, cache_read:u64, cache_write:u64 }
impl Usage {
    fn cost(&self,r:&Rates) -> f64 {
        (self.input as f64*r.input + self.output as f64*r.output
            + self.cache_read as f64*r.cache_read.unwrap_or(r.input) + self.cache_write as f64*r.cache_write.unwrap_or(r.input))/1_000_000.0
    }
    fn merge(&mut self,v:&Value) -> bool {
        let u = v.get("usage").or_else(||v.pointer("/message/usage")).or_else(||v.pointer("/response/usage"))
            .or_else(||v.get("usageMetadata"));
        let Some(u)=u.filter(|u|u.is_object()) else {return false};
        let n=|k:&str| u[k].as_u64().unwrap_or(0);
        let cached=u.pointer("/prompt_tokens_details/cached_tokens").or_else(||u.pointer("/input_tokens_details/cached_tokens"))
            .and_then(Value::as_u64).unwrap_or(n("cachedContentTokenCount"));
        // Anthropic excludes cache reads/writes from input_tokens. OpenAI/Gemini include the cache.
        self.input=self.input.max(n("prompt_tokens").max(n("input_tokens")).max(n("promptTokenCount")).saturating_sub(cached));
        self.output=self.output.max(n("completion_tokens").max(n("output_tokens")).max(n("candidatesTokenCount").saturating_add(n("thoughtsTokenCount"))));
        self.cache_read=self.cache_read.max(cached.max(n("cache_read_input_tokens")));
        self.cache_write=self.cache_write.max(n("cache_creation_input_tokens"));
        ["prompt_tokens","input_tokens","output_tokens","completion_tokens","promptTokenCount","candidatesTokenCount","cache_read_input_tokens"]
            .iter().any(|k|u[*k].is_number())
    }
}
#[derive(Serialize,Deserialize)]
struct Call { model:String, ts:i64, usage:Option<Usage> }
static LEDGER:Mutex<()>=Mutex::new(());
fn ledger()->std::path::PathBuf { crate::registry::base_dir().join("gateway-usage.jsonl") }
fn record(call:Call) {
    if let Ok(_lock)=LEDGER.lock() {
        if let Ok(mut f)=fs::OpenOptions::new().create(true).append(true).open(ledger()) {
            let _=writeln!(f,"{}",json!(call));
        }
    }
}
pub fn summary(scope:&str, settings:&crate::gateway_dispatch::Settings)->Result<Value,String> {
    let mut usd=0.0; let (mut total,mut missing_usage,mut missing_price)=(0u64,0u64,0u64);
    let f=match fs::File::open(ledger()) { Ok(f)=>Some(f), Err(e) if e.kind()==io::ErrorKind::NotFound=>None, Err(e)=>return Err(e.to_string()) };
    if let Some(f)=f {
        for line in io::BufReader::new(f).lines() {
            let line=line.map_err(|e|e.to_string())?;
            let Ok(c)=serde_json::from_str::<Call>(&line) else {continue};
            if c.model!=scope && c.model.split_once('/').map(|(s,_)|s)!=Some(scope) {continue;}
            total+=1;
            match (c.usage,effective(&c.model,settings,c.ts)) {
                (None,_)=>missing_usage+=1,(Some(_),None)=>missing_price+=1,
                (Some(u),Some(r))=>usd+=u.cost(&r),
            }
        }
    }
    Ok(json!({"requests":total,"priced":total-missing_usage-missing_price,"usd":usd,"missing_usage":missing_usage,"missing_price":missing_price}))
}

struct UsageTap<R> { inner:R, sse:bool, buf:Vec<u8>, overflow:bool, usage:Usage, known:bool }
impl<R> UsageTap<R> {
    fn parse(&mut self) {
        if !self.overflow {
            let data=if self.sse {self.buf.strip_prefix(b"data:").unwrap_or(&[])} else {&self.buf};
            if let Ok(v)=serde_json::from_slice::<Value>(data) {
                self.known |= self.usage.merge(&v);
                if !v["error"].is_null() || v["type"]=="error" || v["type"]=="response.failed" {crate::gateway_routes::response(502);}
            }
        }
        self.buf.clear(); self.overflow=false;
    }
}
impl<R:Read> Read for UsageTap<R> {
    fn read(&mut self,buf:&mut [u8])->io::Result<usize> {
        let n=self.inner.read(buf)?;
        let mut rest=&buf[..n];
        while !rest.is_empty() {
            let cut=if self.sse {rest.iter().position(|b|*b==b'\n')} else {None};
            let end=cut.unwrap_or(rest.len());
            if self.buf.len()+end>8*1024*1024 {self.overflow=true;}
            if !self.overflow {self.buf.extend_from_slice(&rest[..end]);}
            if cut.is_none() {break;}
            self.parse(); rest=&rest[end+1..];
        }
        Ok(n)
    }
}
/// Only the final upstream hop is metered. Observe bytes as they go to the client.
pub struct Metered { request:Request, model:Option<String>, ts:i64, scheduled:bool }
impl Metered {
    pub fn new(request:Request,model:Option<String>,scheduled:bool)->Self {Self {request,model,ts:Local::now().timestamp(),scheduled}}
    pub fn scheduled(&self)->bool{self.scheduled}
    pub fn respond<R:Read>(self,r:Response<R>)->io::Result<()> {
        crate::gateway_routes::response(r.status_code().0);
        let Some(model)=self.model.filter(|_|(200..300).contains(&r.status_code().0)) else {return self.request.respond(r);};
        let status=r.status_code(); let headers=r.headers().to_vec(); let len=r.data_length();
        let sse=headers.iter().any(|h|h.field.equiv("Content-Type") && h.value.as_str().contains("text/event-stream"));
        let mut tap=UsageTap {inner:r.into_reader(),sse,buf:vec![],overflow:false,usage:Usage::default(),known:false};
        let result=self.request.respond(Response::new(status,headers,&mut tap,len,None));
        if result.is_err(){crate::gateway_routes::response(499);}
        tap.parse();
        record(Call {model,ts:self.ts,usage:tap.known.then_some(tap.usage)});
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test] fn midnight_uses_start_day_and_usage_is_not_double_charged() {
        let p=Period {start:"22:00".into(),end:"06:00".into(),days:vec![7],factor:0.5};
        assert!(p.matches(90,1)); assert!(!p.matches(360,1)); assert!(!p.matches(60,7));
        let mut u=Usage::default();
        u.merge(&json!({"usage":{"prompt_tokens":100,"prompt_tokens_details":{"cached_tokens":30},"completion_tokens":4}}));
        assert_eq!((u.input,u.cache_read,u.output),(70,30,4));
        let rates=Rates {input:2.0,output:4.0,cache_read:Some(0.5),cache_write:None};
        assert_eq!(u.cost(&rates),171.0/1e6);
    }
    #[test]fn streaming_usage_merges_split_anthropic_events_without_counting_twice(){
        let input="data: {\"message\":{\"usage\":{\"input_tokens\":10,\"cache_read_input_tokens\":20}}}\r\n\r\ndata: {\"usage\":{\"output_tokens\":8}}\n\ndata: {\"usage\":{\"output_tokens\":8}}\n\ndata: [DONE]\n\n";
        let mut tap=UsageTap{inner:io::Cursor::new(input),sse:true,buf:vec![],overflow:false,usage:Usage::default(),known:false};
        let mut b=[0;7];while tap.read(&mut b).unwrap()>0{}tap.parse();
        assert!(tap.known);assert_eq!((tap.usage.input,tap.usage.cache_read,tap.usage.output),(10,20,8));
        let rule=crate::gateway_dispatch::Rule{multiplier:Some(2.0),..Default::default()};
        assert_eq!(factor(&rule,true,0),1.0);assert_eq!(factor(&rule,false,0),2.0);
    }
}
