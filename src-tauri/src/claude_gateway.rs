//! Claude Desktop's third-party deployment reads a profile library and only accepts
//! Anthropic-style model IDs. Its private URL names one upstream model explicitly.
use crate::{
    client_connect::{backup, backup_path, parse_json, read, restored, write, Backup},
    registry as reg,
};
use serde_json::{json, Value};
use std::{
    fs,
    path::{Path, PathBuf},
};

type Result<T> = std::result::Result<T, String>;
const ID: &str = "75d013cb-07cd-4de2-b522-8ae963adbe71";
const PREFIX: &str = "/clients/claude-desktop/";
pub const ALIAS: &str = "claude-cockpit";

fn model_url(url: &str, model: &str) -> String {
    let id = crate::secrets::b64_encode(model.as_bytes())
        .trim_end_matches('=')
        .replace('+', "-")
        .replace('/', "_");
    format!("{url}{PREFIX}{id}")
}
pub fn route(url: &str) -> Result<Option<(String, String)>> {
    let Some(rest) = url.strip_prefix(PREFIX) else {
        return Ok(None);
    };
    let (id, path) = rest.split_once('/').ok_or("缺少模型路径")?;
    if id.is_empty()
        || id.len() > 2048
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return Err("模型路径无效".into());
    }
    let mut b64 = id.replace('-', "+").replace('_', "/");
    while b64.len() % 4 != 0 {
        b64.push('=')
    }
    let model = String::from_utf8(crate::secrets::b64_decode(&b64)?).map_err(|_| "模型名称无效")?;
    let path = path.split('?').next().unwrap_or("");
    if !matches!(
        path,
        "v1/models" | "v1/messages" | "v1/messages/count_tokens"
    ) && path != format!("v1/models/{ALIAS}")
    {
        return Err("此地址只供 Claude 桌面版使用".into());
    }
    Ok(Some((model, format!("/{path}"))))
}

pub fn model_info(model: &str) -> Value {
    json!({"id":ALIAS,"type":"model","display_name":model,"description":model,"created_at":"2026-01-01T00:00:00Z"})
}
pub fn model_details(model: &Value) -> Value {
    let mut info = model_info(model["id"].as_str().unwrap_or_default());
    info["capabilities"] = json!({"image_input":{"supported":model["vision"] == true}});
    if let Some(n) = crate::model_caps::positive(model, "context_window") { info["max_input_tokens"] = json!(n); }
    if let Some(n) = crate::model_caps::positive(model, "max_output_tokens") { info["max_tokens"] = json!(n); }
    info
}
fn targets(local: &Path) -> Result<Vec<PathBuf>> {
    let mut roots = vec![local.to_owned()];
    let packages = local.join("Packages");
    if packages.is_dir() {
        for p in fs::read_dir(packages).map_err(|e| e.to_string())? {
            let p = p.map_err(|e| e.to_string())?;
            let name = p.file_name().to_string_lossy().to_lowercase();
            if name.starts_with("claude_") || name.starts_with("anthropicpbc.claude_") {
                roots.push(p.path().join("LocalCache/Local"));
            }
        }
    }
    let mut paths = Vec::new();
    for root in roots {
        let third = root.join("Claude-3p");
        let lib = third.join("configLibrary");
        paths.extend([
            lib.join(format!("{ID}.json")),
            lib.join("_meta.json"),
            third.join("claude_desktop_config.json"),
            root.join("Claude/claude_desktop_config.json"),
        ]);
    }
    Ok(paths)
}

fn content(path: &Path, before: Option<&str>, url: &str, key: &str, model: &str) -> Result<String> {
    let mut doc = before.map(parse_json).transpose()?.unwrap_or(json!({}));
    if !doc.is_object() {
        return Err("Claude 桌面配置需要 JSON 对象".into());
    }
    if path.ends_with("_meta.json") {
        let entries = doc
            .as_object_mut()
            .unwrap()
            .entry("entries")
            .or_insert_with(|| json!([]))
            .as_array_mut()
            .ok_or("Claude 档案列表格式不符")?;
        if entries.iter().any(|v| v["id"] == ID) {
            return Err("Claude 已使用同名的网关档案".into());
        }
        entries.push(json!({"id":ID,"name":"AgentCockpit"}));
        doc["appliedId"] = json!(ID);
    } else if path.ends_with("claude_desktop_config.json") {
        doc["deploymentMode"] = json!("3p");
    } else {
        if before.is_some() {
            return Err("网关档案路径已被使用，请先核对原文件".into());
        }
        // Carry over policies from the currently applied profile when there is one.
        let lib = path.parent().ok_or("缺少配置目录")?;
        if let Some(meta) = read(&lib.join("_meta.json"))? {
            if let Some(active) = parse_json(&meta)?["appliedId"]
                .as_str()
                .filter(|v| !v.is_empty())
            {
                if !active
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
                {
                    return Err("当前 Claude 档案标识无效".into());
                }
                if let Some(old) = read(&lib.join(format!("{active}.json")))? {
                    let previous = parse_json(&old)?;
                    for (k, v) in previous.as_object().ok_or("原 Claude 档案格式不符")? {
                        if !k.starts_with("inference") {
                            doc[k] = v.clone()
                        }
                    }
                }
            }
        }
        doc["inferenceProvider"] = json!("gateway");
        doc["inferenceGatewayBaseUrl"] = json!(model_url(url, model));
        doc["inferenceGatewayApiKey"] = json!(key);
        doc["inferenceGatewayAuthScheme"] = json!("bearer");
    }
    serde_json::to_string_pretty(&doc).map_err(|e| e.to_string())
}

fn disconnect(paths: &[PathBuf]) -> Result<()> {
    let mut plans = Vec::new();
    for p in paths.iter().rev() {
        if let Some(b) = backup(p)? {
            let now = read(p)?;
            let original = restored(p, &b, now.as_deref())?;
            plans.push((p, now, original));
        }
    }
    for (p, now, original) in plans {
        if now != read(p)? {
            return Err("Claude 配置刚发生变化，请重试".into());
        }
        if now != original {
            if let Some(text) = original {
                write(p, &text)?
            } else {
                fs::remove_file(p).map_err(|e| e.to_string())?
            }
        }
        fs::remove_file(backup_path(p)).map_err(|e| e.to_string())?;
    }
    Ok(())
}
fn handle(paths: &[PathBuf], op: &str, url: &str, key: &str, model: &str) -> Result<Value> {
    let mut managed = 0;
    let mut connected = 0;
    let mut active = String::new();
    for p in paths {
        if let Some(b) = backup(p)? {
            managed += 1;
            active = b.model.clone();
            let now = read(p)?;
            if now.as_deref() == Some(&b.applied)
                || (now != b.before && restored(p, &b, now.as_deref()).is_ok())
            {
                connected += 1
            }
        }
    }
    if op == "status" {
        return Ok(
            json!({"supported":true,"single":true,"path":paths[0],"managed":managed>0,"connected":connected==paths.len(),"model":active}),
        );
    }
    if op == "disconnect" {
        disconnect(paths)?;
        return Ok(json!({"ok":true}));
    }
    if op != "connect" || model.is_empty() {
        return Err("请选择 Claude 桌面版要使用的一个网关模型".into());
    }
    if managed != 0 {
        return Err("已有 Claude 网关恢复记录，请先断开并恢复".into());
    }
    let mut plans = Vec::new();
    for p in paths {
        let before = read(p)?;
        let applied = content(p, before.as_deref(), url, key, model)?;
        plans.push((
            p,
            Backup {
                before,
                applied,
                model: model.to_owned(),
                previous: None,
            },
        ));
    }
    let result = (|| -> Result<()> {
        for (p, b) in &plans {
            if read(p)? != b.before {
                return Err("Claude 配置刚发生变化，请重试".into());
            }
            write(
                &backup_path(p),
                &serde_json::to_string(b).map_err(|e| e.to_string())?,
            )?;
            write(p, &b.applied)?;
        }
        Ok(())
    })();
    if let Err(e) = result {
        disconnect(paths).map_err(|why| format!("{e}，部分档案待恢复：{why}"))?;
        return Err(e);
    }
    Ok(json!({"ok":true}))
}
/// Whether any Claude desktop profile still carries a restore record.
/// The gateway shutdown path asks this before touching those libraries.
pub fn managed() -> bool {
    let Some(dir) = reg::native_data_dir("claude-desktop") else {
        return false;
    };
    let Some(parent) = dir.parent() else {
        return false;
    };
    targets(parent)
        .map(|paths| paths.iter().any(|p| backup_path(p).is_file()))
        .unwrap_or(false)
}

pub fn action(op: &str, url: &str, key: &str, model: &str) -> Result<Value> {
    let dir = reg::native_data_dir("claude-desktop").ok_or("找不到 Claude 桌面版数据目录")?;
    let paths = targets(dir.parent().ok_or("找不到应用目录")?)?;
    handle(&paths, op, url, key, model)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn packaged_and_native_profiles_round_trip() {
        let d = tempfile::tempdir().unwrap();
        fs::create_dir_all(d.path().join("Packages/Claude_example")).unwrap();
        let paths = targets(d.path()).unwrap();
        assert_eq!(paths.len(), 8);
        let source = r#"{"deploymentMode":"1p","mcpServers":{"mine":{}}}"#;
        write(&paths[3], source).unwrap();
        handle(
            &paths,
            "connect",
            "http://127.0.0.1:11234",
            "test",
            "demo/测试",
        )
        .unwrap();
        assert_eq!(
            handle(&paths, "status", "", "", "").unwrap()["connected"],
            true
        );
        let profile = parse_json(&read(&paths[0]).unwrap().unwrap()).unwrap();
        let url = profile["inferenceGatewayBaseUrl"]
            .as_str()
            .unwrap()
            .strip_prefix("http://127.0.0.1:11234")
            .unwrap();
        assert_eq!(
            route(&format!("{url}/v1/messages?beta=1"))
                .unwrap()
                .unwrap(),
            ("demo/测试".into(), "/v1/messages".into())
        );
        handle(&paths, "disconnect", "", "", "").unwrap();
        assert_eq!(read(&paths[3]).unwrap().as_deref(), Some(source));
        assert!(!paths[0].exists() && !paths[7].exists());
    }
}
