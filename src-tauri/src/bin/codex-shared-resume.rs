//! Stdio adapter: history is shared, inference credentials belong to this window.
use serde_json::Value;
use std::{io::{self, BufRead, Write}, path::Path, process::{Command, Stdio}};

fn route(line: &str, config: &str) -> Result<String, String> {
    let mut request: Value = match serde_json::from_str(line) { Ok(v) => v, Err(_) => return Ok(line.into()) };
    if !matches!(request["method"].as_str(), Some("thread/resume" | "thread/fork")) || request.get("id").is_none() {
        return Ok(line.into());
    }
    let config: toml::Value = config.parse().map_err(|e| format!("account config: {e}"))?;
    let provider = config.get("model_provider").and_then(toml::Value::as_str).unwrap_or("openai");
    let model = config.get("model").and_then(toml::Value::as_str).filter(|s| !s.is_empty())
        .ok_or("Shared resume requires a model selected in this account's config.toml")?;
    let params = request.get_mut("params").and_then(Value::as_object_mut).ok_or("Missing resume parameters")?;
    // Null means the desktop is restoring saved settings, including a foreign
    // provider. Never rewrite the shared rollout or SQLite provider metadata.
    let local = params.get("modelProvider").and_then(Value::as_str) == Some(provider);
    if !local || params.get("model").and_then(Value::as_str).is_none() {
        params.insert("model".into(), model.into());
    }
    params.insert("modelProvider".into(), provider.into());
    // A caller's config override must not send this account to the old endpoint.
    if let Some(overrides) = params.get_mut("config").and_then(Value::as_object_mut) {
        overrides.remove("model_provider");
        overrides.remove("model");
    }
    Ok(request.to_string())
}

fn main() {
    if let Err(e) = run() { eprintln!("Shared conversation adapter: {e}"); std::process::exit(1); }
}
fn run() -> Result<(), Box<dyn std::error::Error>> {
    let real = std::env::var_os("AGENTCOCKPIT_CODEX_REAL").ok_or("Missing original Codex executable")?;
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    let mut cmd = Command::new(real);
    cmd.args(&args).env_remove("CODEX_CLI_PATH").env_remove("AGENTCOCKPIT_CODEX_REAL");
    #[cfg(windows)] { use std::os::windows::process::CommandExt; cmd.creation_flags(0x08000000); }
    if !args.iter().any(|a| a == "app-server") {
        std::process::exit(cmd.status()?.code().unwrap_or(1));
    }
    let home = std::env::var_os("CODEX_HOME").ok_or("Missing account CODEX_HOME")?;
    let file = Path::new(&home).join("config.toml");
    // Fail before spawning if account routing cannot be read.
    std::fs::read_to_string(&file)?;
    let mut child = cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::inherit()).spawn()?;
    let mut input = child.stdin.take().unwrap();
    let output = child.stdout.take().unwrap();
    let writer = std::sync::Arc::new(std::sync::Mutex::new(io::stdout()));
    let forwarded = writer.clone();
    let reader = std::thread::spawn(move || {
        for line in io::BufReader::new(output).split(b'\n') {
            let mut buf = match line { Ok(v) => v, Err(_) => break };
            buf.push(b'\n');
            let mut out = forwarded.lock().unwrap();
            if out.write_all(&buf).and_then(|_| out.flush()).is_err() { break; }
        }
    });
    for line in io::stdin().lock().lines() {
        let line = line?;
        let routed = std::fs::read_to_string(&file).map_err(|e| e.to_string()).and_then(|c| route(&line, &c));
        match routed {
            Ok(line) => { writeln!(input, "{line}")?; input.flush()?; }
            Err(error) => {
                let req: Value = serde_json::from_str(&line)?;
                let reply = serde_json::json!({"id":req["id"],"error":{"code":-32603,"message":error}});
                let mut out = writer.lock().unwrap(); writeln!(out,"{reply}")?; out.flush()?;
            }
        }
    }
    drop(input);
    let status = child.wait()?;
    let _ = reader.join();
    std::process::exit(status.code().unwrap_or(1));
}

#[cfg(test)] mod tests {
    use super::*;
    #[test] fn foreign_and_null_provider_use_window_account() {
        for old in [Value::Null, Value::String("agentcockpit".into())] {
            let req=serde_json::json!({"id":1,"method":"thread/resume","params":{"threadId":"same-id","modelProvider":old,"model":"minimax/old"}});
            let out:Value=serde_json::from_str(&route(&req.to_string(),"model_provider='openai'\nmodel='official-model'").unwrap()).unwrap();
            assert_eq!(out["params"]["modelProvider"],"openai"); assert_eq!(out["params"]["model"],"official-model"); assert_eq!(out["params"]["threadId"],"same-id");
        }
    }
    #[test] fn api_window_and_explicit_local_choice() {
        let req=r#"{"id":2,"method":"thread/resume","params":{"modelProvider":"openai","model":"official"}}"#;
        let out:Value=serde_json::from_str(&route(req,"model_provider='agentcockpit'\nmodel='api-model'").unwrap()).unwrap();
        assert_eq!(out["params"]["model"],"api-model"); assert_eq!(out["params"]["modelProvider"],"agentcockpit");
        let local=out.to_string().replace("api-model","chosen");
        assert!(route(&local,"model_provider='agentcockpit'\nmodel='default'").unwrap().contains("chosen"));
    }
    #[test] fn ordinary_messages_are_byte_preserved() {
        let line=r#"{"id":3, "method":"turn/start","params":{"input":[]}}"#;
        assert_eq!(route(line,"bad config").unwrap(),line);
    }
}
