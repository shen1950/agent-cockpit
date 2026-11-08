//! ZCode Start Plan adapter. A pinned, separately licensed zcode-api component
//! handles upstream protocol details. Cockpit owns account selection and keys.
//! JWTs only travel in memory, over an authenticated private loopback connection.
use serde_json::{json, Value};
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

pub const BASE_MARK: &str = "zcode://start-plan";
pub const REVISION: &str = "37155a5f75f3f2d7bde1b83054c8d6eb2e23a935";
static COMPONENT: Mutex<Option<Component>> = Mutex::new(None);
static INSTALL: Mutex<()> = Mutex::new(());
struct Component { child: Child, port: u16, key: String }
fn root() -> PathBuf { crate::registry::base_dir().join("tools/zcode-start-plan") }
fn source() -> PathBuf { root().join(format!("zcode-api-{REVISION}")) }
pub fn is_base(base: &str) -> bool { base.trim().trim_end_matches('/') == BASE_MARK }
pub fn installed() -> bool {
    fs::read_to_string(source().join(".agentcockpit-installed")).is_ok_and(|s| s.trim() == REVISION)
        && source().join("node_modules/happy-dom/package.json").is_file()
}
fn hidden(command: &mut Command) -> &mut Command {
    #[cfg(windows)] { use std::os::windows::process::CommandExt; command.creation_flags(0x0800_0000); }
    command
}
pub fn setup() -> Result<Value, String> {
    let _guard = INSTALL.lock().map_err(|_| "ZCode 安装锁不可用")?;
    if installed() { return Ok(json!({"msg":"ZCode 组件已安装","revision":REVISION})); }
    fs::create_dir_all(root()).map_err(|_| "无法创建 ZCode 组件目录")?;
    let script = root().join("setup.ps1");
    fs::write(&script, include_str!("../../tools/setup-zcode-runtime.ps1")).map_err(|_| "无法准备 ZCode 安装脚本")?;
    let mut cmd = Command::new("powershell.exe");
    cmd.args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"]).arg(script).arg("-RuntimeRoot").arg(root());
    let output = hidden(&mut cmd).output().map_err(|_| "无法启动 ZCode 安装；需要 Node.js 24+ 和 npm")?;
    if !output.status.success() || !installed() {
        return Err(format!("ZCode 组件安装失败，请检查 Node.js 24+、网络和 {}", root().join("install.log").display()));
    }
    Ok(json!({"msg":"ZCode 组件已安装","revision":REVISION}))
}
fn ensure_component() -> Result<(u16, String), String> {
    let mut guard = COMPONENT.lock().map_err(|_| "ZCode 组件锁不可用")?;
    if let Some(c) = guard.as_mut() {
        if c.child.try_wait().ok() == Some(None) { return Ok((c.port, c.key.clone())); }
    }
    *guard = None;
    if !installed() { return Err("请先在 ZCode 反代面板安装本地组件".into()); }
    fs::create_dir_all(root()).map_err(|_| "无法准备 ZCode 组件")?;
    for (name, text) in [("zcode-runtime.mjs", include_str!("../scripts/zcode-runtime.mjs")),
                         ("zcode-loader.mjs", include_str!("../scripts/zcode-loader.mjs"))] {
        fs::write(root().join(name), text).map_err(|_| "无法写入 ZCode 适配器")?;
    }
    // The worker shares the same TypeScript import hook and inherits Node's flag.
    let worker = root().join("zcode-worker.mjs");
    let worker_script = format!("import './zcode-loader.mjs';\nawait import({});\n",
        json!(format!("./zcode-api-{REVISION}/src/proxy/captcha-worker-entry.ts")));
    fs::write(&worker, worker_script).map_err(|_| "无法准备 ZCode worker")?;
    let device = fs::read(crate::zcode::native_root().join("v2/telemetry-state.json")).ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok()).unwrap_or(json!({}));
    let config_path = root().join("config.json");
    let config = json!({"server":{"host":"127.0.0.1","port":8080},"plan":"start-plan",
        "claim":{"enabled":false,"auto":false},"async":{"enabled":false},"highspeed":{"enabled":false},
        "clientSigning":{"enabled":false},"mcp":{"enabled":false,"gateway":{"enabled":false}},
        "identity":{"deviceMid":device["deviceMid"].as_str().unwrap_or("")}});
    fs::write(&config_path, config.to_string()).map_err(|_| "无法写入 ZCode 组件配置")?;
    let mut cmd = Command::new("node");
    cmd.args(["--disable-warning=ExperimentalWarning", "--experimental-transform-types"]).arg(root().join("zcode-runtime.mjs"))
        .current_dir(root()).env("ZCODE_CAPTCHA_WORKER", worker).env("ZCODE_CAPTCHA_REQUIRE_WORKER", "1")
        .env_remove("ZCODE_DUMP_UPSTREAM").env("ZCODE_CLAIM_ENABLED", "false").env("ZCODE_HIGHSPEED_ENABLED", "false")
        .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
    let mut child = hidden(&mut cmd).spawn().map_err(|_| "无法启动 ZCode 组件，需要 Node.js 24+")?;
    let init = json!({"source":source(),"config":config_path});
    if child.stdin.as_mut().ok_or("ZCode 组件管道不可用")?.write_all(format!("{init}\n").as_bytes()).is_err() {
        let _ = child.kill(); let _ = child.wait(); return Err("ZCode 组件初始化失败".into());
    }
    let stdout = child.stdout.take().ok_or("ZCode 组件启动管道不可用")?;
    let (tx, rx) = mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        let _ = reader.by_ref().take(8192).read_line(&mut line);
        let _ = tx.send(line);
        // Workers can emit diagnostics after startup. Drain without logging so
        // closing the startup reader cannot crash them with a broken stdout.
        let _ = std::io::copy(&mut reader, &mut std::io::sink());
    });
    let ready = rx.recv_timeout(Duration::from_secs(20)).ok().and_then(|line| serde_json::from_str::<Value>(&line).ok());
    if let Some(v) = ready.filter(|v| v["revision"] == REVISION) {
        if let (Some(port), Some(key)) = (v["port"].as_u64().filter(|p| *p > 0 && *p <= 65535), v["key"].as_str().filter(|s| s.len() == 64)) {
            let out = (port as u16, key.to_string());
            *guard = Some(Component { child, port: out.0, key: out.1.clone() }); return Ok(out);
        }
    }
    let _ = child.kill(); let _ = child.wait();
    Err("ZCode 组件启动失败，请检查 Node.js 24+ 或重新安装组件".into())
}
pub fn stop() {
    if let Ok(mut held) = COMPONENT.lock() { if let Some(mut c) = held.take() { let _ = c.child.kill(); let _ = c.child.wait(); } }
}
pub fn request(name: &str, body: &Value, anthropic: bool) -> Result<ureq::Response, (u16, String)> {
    let credential = crate::zcode::proxy_credential(name).map_err(|e| (401, e))?;
    let (port, key) = ensure_component().map_err(|e| (503, e))?;
    // This credential is never the caller's distribution key or another card's JWT.
    let envelope = json!({"credential":credential,"body":body,"format":if anthropic {"anthropic"}else{"openai"}});
    let agent = ureq::AgentBuilder::new().redirects(0).build();
    match agent.post(&format!("http://127.0.0.1:{port}/relay")).set("Authorization", &format!("Bearer {key}"))
        .set("Content-Type", "application/json").timeout(Duration::from_secs(600)).send_string(&envelope.to_string()) {
        Ok(r) => Ok(r),
        Err(ureq::Error::Status(code, r)) => {
            let text = r.into_string().unwrap_or_default();
            let raw = serde_json::from_str::<Value>(&text).ok();
            let message = raw.as_ref().and_then(|v| v["error"]["message"].as_str()).unwrap_or("ZCode 上游请求被拒绝");
            // Upstream error text can echo request material; redact before returning.
            Err(upstream_failure(code, &redact(message, credential["jwt"].as_str().unwrap_or(""))))
        },
        Err(_) => Err((502, "ZCode 本地组件连接失败或超时".into())),
    }
}
fn redact(message: &str, jwt: &str) -> String {
    let message = if jwt.is_empty() { message.to_string() } else { message.replace(jwt, "[redacted]") };
    message.chars().take(500).collect()
}
fn upstream_failure(status: u16, message: &str) -> (u16, String) {
    // Some gateways return a billing JSON error with HTTP 200. zcode-api
    // rejects that as an invalid Anthropic reply, preserving the original JSON.
    let embedded = message.find('{').and_then(|i| serde_json::from_str::<Value>(&message[i..]).ok());
    if embedded.as_ref().is_some_and(|v| v["code"] == 1005 && v["msg"].as_str().is_some_and(|s| s.contains("quota"))) {
        return (402, "该账号的 ZCode Start Plan 额度不足或已到期，请在客户端检查套餐".into());
    }
    (status, message.to_string())
}
pub fn models(name: &str) -> Vec<Value> {
    let meta = crate::registry::find_instance("zcode", name).unwrap_or(json!({}));
    let provider = meta["zcode_provider"].as_str().unwrap_or("");
    let cfg = fs::read(crate::zcode::native_root().join("v2/config.json")).ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok()).unwrap_or(json!({}));
    let mut catalog = model_catalog(&cfg, provider);
    // A non-active provider's builtin entry can be empty while a saved login
    // still works. The selected card's own billing records supply candidates;
    // expiry and actual availability are always confirmed by an inference test.
    for m in billing_models(&meta) {
        if !catalog.iter().any(|x| x["id"] == m["id"]) { catalog.push(m); }
    }
    catalog
}
fn billing_models(meta: &Value) -> Vec<Value> {
    let mut out = Vec::new();
    for group in meta["quota_groups"].as_array().into_iter().flatten() {
        for item in group["items"].as_array().into_iter().flatten() {
            let label = item["label"].as_str().unwrap_or("");
            for word in label.split_whitespace() {
                if word.to_ascii_lowercase().starts_with("glm-") && word.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
                    && !out.iter().any(|m: &Value| m["id"] == word) {
                    out.push(json!({"id":word,"name":format!("{word}（额度记录，待实测）"),"source":"该账号的额度记录","tools":true,"reasoning":true}));
                }
            }
        }
    }
    out
}
fn model_catalog(cfg: &Value, provider: &str) -> Vec<Value> {
    if !["zai", "bigmodel"].contains(&provider) { return Vec::new(); }
    let key = format!("builtin:{provider}-start-plan");
    let Some(models) = cfg["provider"][&key]["models"].as_object() else { return Vec::new(); };
    models.iter().filter(|(_, m)| m["disabled"] != true).map(|(id, m)| json!({
        "id":id,"name":m["name"].as_str().unwrap_or(id),"source":"ZCode 本机套餐目录",
        "context_window":m["limit"]["context"],"max_output_tokens":m["limit"]["output"],
        "vision":m["modalities"]["input"].as_array().map(|a| a.iter().any(|v| v == "image")),
        "tools":true,"reasoning":true
    })).collect()
}
pub fn view() -> Value {
    let accounts: Vec<Value> = crate::registry::load_instances("zcode").iter().map(|m| {
        let name = m["name"].as_str().unwrap_or("");
        let usable = crate::zcode::proxy_available(name);
        json!({"name":name,"app":"zcode","provider":m["zcode_provider"],"usable":usable,
            "models":models(name),"text":if usable {"登录已保存，待实测"}else{"请在 ZCode 登录后重新保存"}})
    }).collect();
    json!({"accounts":accounts,"installed":installed(),"revision":REVISION,"plan":"start-plan"})
}
pub fn one_click(proxy: &Arc<crate::proxy::Proxy>, name: &str) -> Result<Value, String> {
    crate::zcode::proxy_credential(name)?;
    ensure_component()?;
    let src = crate::gateway::member_src_instance("zcode", name);
    let (pool, key, steps) = crate::gateway::ensure_wiring("gp_zcode_start", "ZCode Start Plan 上游组", &src, "dk_zcode_start", "ZCode Start Plan 接入");
    if key.is_empty() { return Err("ZCode 接入密钥未能生成".into()); }
    let port = if proxy.is_running() { proxy.port() } else {
        let port = crate::registry::load_config()["proxy"]["port"].as_u64().filter(|p| *p > 0 && *p <= 65535).unwrap_or(crate::registry::PROXY_PORT as u64) as u16;
        let (ok, msg) = proxy.start(port); if !ok { return Err(msg); } port
    };
    Ok(json!({"msg":"ZCode Start Plan 已配置，请测试所选模型","key":key,"pool":pool,"steps":steps,
        "base_url":format!("http://127.0.0.1:{port}/v1")}))
}
pub fn test(name: &str, model: &str) -> Value {
    let started = std::time::Instant::now();
    let body = json!({"model":model.trim_start_matches("zcode/"),"messages":[{"role":"user","content":"Reply with OK."}],"max_tokens":32,"stream":false});
    match request(name, &body, false) {
        Ok(r) => { let v = r.into_json::<Value>().unwrap_or(json!({})); let good = v["choices"][0]["message"].is_object();
            json!({"ok":good,"model_used":model,"ms":started.elapsed().as_millis(),"usage":v["usage"],
                "content":v["choices"][0]["message"]["content"],"error":if good {""}else{"上游未返回有效模型回复"}}) },
        Err((status, message)) => json!({"ok":false,"status":status,"error":message,"ms":started.elapsed().as_millis()}),
    }
}

#[cfg(test)] mod tests {
    use super::*;
    #[test] fn marker_cannot_be_a_remote_url_or_unknown_plan() {
        assert!(is_base("zcode://start-plan/"));
        for s in ["https://zcode.z.ai", "zcode://coding-plan", "zcode://start-plan.evil", "zcode://start-plan/other"] { assert!(!is_base(s)); }
    }
    #[test] fn models_are_provider_scoped_and_empty_entitlements_stay_empty() {
        let cfg = json!({"provider":{"builtin:zai-start-plan":{"models":{}},"builtin:bigmodel-start-plan":{"models":{"GLM-5.3-Flash":{"name":"Flash"},"blocked":{"disabled":true}}}}});
        assert!(model_catalog(&cfg,"zai").is_empty());
        assert!(model_catalog(&cfg,"unknown").is_empty());
        let m=model_catalog(&cfg,"bigmodel"); assert_eq!(m.len(),1); assert_eq!(m[0]["id"],"GLM-5.3-Flash");
    }
    #[test] fn upstream_errors_do_not_echo_selected_account_jwt() { assert_eq!(redact("bad secret-jwt value", "secret-jwt"), "bad [redacted] value"); }
    #[test] fn billing_candidates_only_come_from_the_selected_cards_model_labels() {
        let meta = json!({"quota_groups":[{"items":[{"label":"资源包 GLM-5.3-Flash","expired":true},{"label":"资源包 GLM-5.3-Flash"},{"label":"unrelated quota"}]}]});
        let models = billing_models(&meta); assert_eq!(models.len(), 1); assert_eq!(models[0]["id"], "GLM-5.3-Flash");
        assert!(billing_models(&json!({})).is_empty());
    }
    #[test] fn billing_failures_keep_their_meaning_after_protocol_validation() {
        assert_eq!(upstream_failure(502, "upstream returned invalid Anthropic message: {\"code\":1005,\"msg\":\"exceed quota limit\"}").0,402);
        assert_eq!(upstream_failure(403,"captcha verify failed").0,403);
    }
    #[test] #[ignore = "explicit native ZCode component and small inference smoke"]
    fn native_smoke() {
        if std::env::var("AGENTCOCKPIT_ZCODE_PROXY_SMOKE").as_deref() != Ok("1") { return; }
        crate::secrets::load();
        let path = crate::zcode::native_root().join("v2/credentials.json");
        let before = fs::read(&path).unwrap();
        let (port, key) = ensure_component().expect("installed Node component must start");
        let health = ureq::get(&format!("http://127.0.0.1:{port}/health")).set("Authorization", &format!("Bearer {key}"))
            .call().unwrap().into_json::<Value>().unwrap();
        assert_eq!(health["revision"], REVISION);
        assert_eq!(ureq::get(&format!("http://127.0.0.1:{port}/health")).call().unwrap_err().into_response().unwrap().status(),401);
        let mut successful = None;
        for m in crate::registry::load_instances("zcode") {
            let name = m["name"].as_str().unwrap();
            let result = test(name, "GLM-5.3-Flash");
            println!("ZCode native provider={} ok={} status={} ms={} error={}",m["zcode_provider"],result["ok"],result["status"],result["ms"],result["error"]);
            if result["ok"] == true { successful = Some(name.to_string()); }
        }
        if std::env::var("AGENTCOCKPIT_ZCODE_PROXY_WIRE").as_deref() == Ok("1") {
            let name = successful.expect("one real account must answer before wiring smoke");
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port(); drop(listener);
            let proxy = crate::proxy::Proxy::shared(); assert!(proxy.start(port).0);
            let wired = one_click(&proxy, &name).unwrap();
            let key = wired["key"].as_str().unwrap();
            let chat = json!({"model":"zcode/GLM-5.3-Flash","messages":[{"role":"user","content":"Say OK."}],"max_tokens":128,"stream":true});
            let r = ureq::post(&format!("http://127.0.0.1:{port}/v1/chat/completions"))
                .set("Authorization", &format!("Bearer {key}")).timeout(Duration::from_secs(180)).send_json(chat).unwrap();
            let sse = r.into_string().unwrap(); assert!(sse.contains("chat.completion.chunk") && sse.contains("[DONE]"));
            let responses = json!({"model":"zcode/GLM-5.3-Flash","input":"Say OK.","max_output_tokens":128,"stream":false});
            let r = ureq::post(&format!("http://127.0.0.1:{port}/v1/responses"))
                .set("Authorization", &format!("Bearer {key}")).timeout(Duration::from_secs(180)).send_json(responses).unwrap().into_json::<Value>().unwrap();
            assert_eq!(r["object"],"response");
            let anthropic = json!({"model":"zcode/GLM-5.3-Flash","messages":[{"role":"user","content":"Say OK."}],"max_tokens":128,"stream":false});
            let r = ureq::post(&format!("http://127.0.0.1:{port}/v1/messages"))
                .set("x-api-key", key).timeout(Duration::from_secs(180)).send_json(anthropic).unwrap().into_json::<Value>().unwrap();
            assert_eq!(r["type"],"message");
            assert!(models(&name).iter().any(|m| m["id"] == "GLM-5.3-Flash"));
            proxy.stop();
            println!("ZCode gateway chat_stream=true responses=true native_anthropic=true selected_account_wired=true");
        }
        assert_eq!(fs::read(path).unwrap(), before, "inference must never rewrite the native login");
        stop();
        println!("ZCode native credentials_unchanged=true component_stopped=true");
    }
}
