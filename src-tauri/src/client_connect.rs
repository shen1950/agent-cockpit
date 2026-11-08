//! Local gateway as a client provider. Each target owns its native configuration format.
use crate::{commands::AppState, gateway, inject, model_caps, procs, registry as reg};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    time::Duration,
};

type Result<T> = std::result::Result<T, String>;
const PROVIDER: &str = "cockpit_gateway";

/// Every client this module can configure, with its display name. The gateway
/// shutdown path walks this list to undo whatever is still attached.
pub(super) const CLIENTS: &[(&str, &str)] = &[
    ("codex", "Codex CLI"),
    ("codex-desktop", "ChatGPT / Codex 桌面版"),
    ("claude", "Claude Code CLI"),
    ("qoder", "Qoder"),
    ("qoder-cn", "Qoder CN"),
    ("workbuddy", "WorkBuddy"),
    ("workbuddy-intl", "WorkBuddy 国际版"),
    ("opencode", "OpenCode"),
    ("zcode", "ZCode"),
];

fn env_dir(name: &str, fallback: PathBuf) -> PathBuf {
    std::env::var_os(name)
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or(fallback)
}

fn target(app: &str, name: &str) -> Result<Option<PathBuf>> {
    let home = reg::home();
    let slot = if !name.is_empty() && matches!(app, "codex" | "claude" | "codex-desktop") {
        let m = reg::load_instances(app)
            .into_iter()
            .find(|m| m["name"] == name)
            .ok_or("找不到这个账号")?;
        let data = PathBuf::from(m["data_dir"].as_str().ok_or("账号没有资料目录")?);
        if app == "codex-desktop" && !reg::is_in_place(app, &data) {
            let path = m["chatgpt_home"]
                .as_str()
                .map(PathBuf::from)
                .unwrap_or_else(|| reg::base_dir().join("chatgpt-auth").join(name));
            crate::chatgpt::check_plain_path(&path)?;
            if !path.starts_with(reg::base_dir().join("chatgpt-auth")) {
                return Err("账号凭据目录超出受管范围".into());
            }
            Some(path)
        } else if app == "codex-desktop" {
            Some(home.join(".codex"))
        } else {
            Some(data)
        }
    } else {
        None
    };
    let path = match app {
        "codex" => slot
            .unwrap_or_else(|| env_dir("CODEX_HOME", home.join(".codex")))
            .join("config.toml"),
        "codex-desktop" => slot
            .unwrap_or_else(|| home.join(".codex"))
            .join("config.toml"),
        "claude" => slot
            .unwrap_or_else(|| env_dir("CLAUDE_CONFIG_DIR", home.join(".claude")))
            .join("settings.json"),
        "qoder" => env_dir("QODER_CONFIG_DIR", home.join(".qoder")).join("settings.json"),
        "qoder-cn" => home.join(".qoder-cn/settings.json"),
        "workbuddy" => env_dir("WORKBUDDY_CONFIG_DIR", home.join(".workbuddy")).join("models.json"),
        "workbuddy-intl" => home.join(".workbuddy-ai/models.json"),
        "opencode" => {
            let dir = env_dir(
                "OPENCODE_CONFIG_DIR",
                env_dir("XDG_CONFIG_HOME", home.join(".config")).join("opencode"),
            );
            let path = env_dir("OPENCODE_CONFIG", dir.join("opencode.json"));
            if path.extension().and_then(|v| v.to_str()) != Some("json")
                || dir.join("opencode.jsonc").exists()
            {
                return Err(
                    "检测到 OpenCode 的自定义或 JSONC 配置，请在该配置中手动添加提供商。".into(),
                );
            }
            path
        }
        "zcode" => {
            let native = home.join(".zcode/v2");
            let setting = read(&native.join("setting.json"))?;
            let location = match setting {
                Some(s) => parse_json(&s)?["dataBaseDir"].as_str().map(PathBuf::from),
                None => None,
            };
            let dir = location
                .filter(|p| p.is_absolute())
                .map(|p| p.join(".zcode/v2"))
                .unwrap_or(native);
            let rules = dir.join("provider_config.json");
            if rules.is_file() || !dir.join("config.json").is_file() {
                rules
            } else {
                dir.join("config.json")
            }
        }
        _ => return Ok(None),
    };
    if !path.is_absolute() {
        return Err("客户端配置目录无效".into());
    }
    Ok(Some(path))
}

pub(super) fn parse_json(s: &str) -> Result<Value> {
    serde_json::from_str(s.trim_start_matches('\u{feff}'))
        .map_err(|e| format!("配置不是有效的 JSON（第 {} 行）", e.line()))
}

fn list<'a>(value: &'a mut Value, key: &str) -> Result<&'a mut Vec<Value>> {
    if !value.is_object() {
        return Err("客户端配置对象格式不符".into());
    }
    value
        .as_object_mut()
        .unwrap()
        .entry(key)
        .or_insert_with(|| json!([]))
        .as_array_mut()
        .ok_or_else(|| format!("配置字段 {key} 需要数组"))
}
fn object<'a>(value: &'a mut Value, key: &str) -> Result<&'a mut Value> {
    let child = value
        .as_object_mut()
        .ok_or("客户端配置需要对象")?
        .entry(key)
        .or_insert_with(|| json!({}));
    if !child.is_object() {
        return Err(format!("配置字段 {key} 需要对象"));
    }
    Ok(child)
}

fn copy_limit(dest: &mut Value, field: &str, row: &Value, source: &str) {
    if let Some(n) = model_caps::positive(row, source) {
        dest[field] = json!(n);
    } else if let Some(o) = dest.as_object_mut() {
        o.remove(field);
    }
}

fn configured(
    app: &str,
    path: &Path,
    before: Option<&str>,
    url: &str,
    key: &str,
    rows: &[Value],
) -> Result<String> {
    let models: Vec<String> = rows.iter()
        .map(|r| r["id"].as_str().map(str::to_owned).ok_or("模型目录缺少 ID"))
        .collect::<std::result::Result<_, _>>()?;
    let model = models.first().ok_or("这把密钥还没有可用模型")?;
    let base = format!("{url}/v1");
    if matches!(app, "codex" | "codex-desktop") {
        use toml_edit::{value, Document, Item, Table};
        let mut doc: Document = before
            .unwrap_or("")
            .parse()
            .map_err(|_| "Codex 配置不是有效的 TOML")?;
        let providers = doc
            .as_table_mut()
            .entry("model_providers")
            .or_insert(Item::Table(Table::new()))
            .as_table_like_mut()
            .ok_or("model_providers 格式不符")?;
        if providers.contains_key(PROVIDER) {
            return Err("同名 Codex 提供商已存在，请先检查原配置".into());
        }
        let mut table = Table::new();
        table["name"] = value("AgentCockpit");
        table["base_url"] = value(base.clone());
        table["wire_api"] = value("responses");
        table["experimental_bearer_token"] = value(key);
        table["model_catalog_url"] = value(format!("{base}/model_catalog.json"));
        providers.insert(PROVIDER, Item::Table(table));
        doc["model_provider"] = value(PROVIDER);
        doc["model"] = value(model.as_str());
        return Ok(doc.to_string());
    }
    let is_buddy = matches!(app, "workbuddy" | "workbuddy-intl");
    let mut doc = match before {
        Some(s) => parse_json(s)?,
        None if is_buddy => json!([]),
        None => json!({}),
    };
    if is_buddy {
        if let Some(avail) = doc.get_mut("availableModels") {
            let ids = avail.as_array_mut().ok_or("availableModels 格式不符")?;
            if !ids.is_empty() {
                for id in &models {
                    if !ids.contains(&json!(id)) {
                        ids.push(json!(id))
                    }
                }
            }
        }
        let entries = if doc.is_array() {
            doc.as_array_mut().unwrap()
        } else {
            list(&mut doc, "models")?
        };
        for (mut entry, row) in inject::workbuddy_entries(
            "AgentCockpit",
            &format!("{base}/chat/completions"),
            key,
            &models,
        ).into_iter().zip(rows) {
            if entries.iter().any(|r| r["id"] == entry["id"]) {
                return Err(format!(
                    "WorkBuddy 已有模型 {}，请选其他模型或先处理重名条目",
                    entry["id"]
                ));
            }
            entry["supportsImages"] = json!(row["vision"] == true);
            copy_limit(&mut entry, "maxInputTokens", row, "context_window");
            copy_limit(&mut entry, "maxOutputTokens", row, "max_output_tokens");
            entries.push(entry);
        }
    } else if app == "claude" {
        let env = object(&mut doc, "env")?;
        for flag in [
            "CLAUDE_CODE_USE_BEDROCK",
            "CLAUDE_CODE_USE_VERTEX",
            "CLAUDE_CODE_USE_FOUNDRY",
        ] {
            if env[flag] == "1" || env[flag] == "true" {
                return Err(format!("当前档案启用了 {flag}，请先关闭该路由"));
            }
        }
        env["ANTHROPIC_BASE_URL"] = json!(url);
        env["ANTHROPIC_AUTH_TOKEN"] = json!(key);
        if env.get("ANTHROPIC_API_KEY").is_some() {
            env["ANTHROPIC_API_KEY"] = json!(key);
        }
        for field in [
            "ANTHROPIC_MODEL",
            "ANTHROPIC_DEFAULT_OPUS_MODEL",
            "ANTHROPIC_DEFAULT_SONNET_MODEL",
            "ANTHROPIC_DEFAULT_HAIKU_MODEL",
            "ANTHROPIC_DEFAULT_FABLE_MODEL",
        ] {
            env[field] = json!(model)
        }
        // All Claude tiers use the first selected model in this connection.
        for (key, field) in [
            ("CLAUDE_CODE_MAX_CONTEXT_TOKENS", "context_window"),
            ("CLAUDE_CODE_MAX_OUTPUT_TOKENS", "max_output_tokens"),
            ("CLAUDE_CODE_AUTO_COMPACT_WINDOW", "auto_compact_token_limit"),
        ] {
            if let Some(n) = model_caps::positive(&rows[0], field) {
                env[key] = json!(n.to_string());
            }
        }
        doc["model"] = json!(model);
    } else if matches!(app, "qoder" | "qoder-cn") {
        let (id, mut entry) = inject::qoder_entry("本地网关", &base, key, &models);
        for (item, row) in entry["models"].as_array_mut().unwrap().iter_mut().zip(rows) {
            item["capabilities"]["vision"] = json!(row["vision"] == true);
            copy_limit(item, "contextWindow", row, "context_window");
            copy_limit(item, "maxOutputTokens", row, "max_output_tokens");
        }
        object(&mut doc, "providers")?[&id] = entry;
        object(&mut doc, "model")?["name"] = json!(format!("{id}/{model}"));
    } else if app == "zcode" && path.ends_with("provider_config.json") {
        if !doc.is_object() || doc.get("schemaVersion").is_some_and(|v| v != 1) {
            return Err("ZCode 提供商规则版本不支持".into());
        }
        doc["schemaVersion"] = json!(1);
        let cfg = object(&mut doc, "config")?;
        let providers = list(object(cfg, "providerConfigRules")?, "providerRules")?;
        if providers.iter().any(|v| v["providerId"] == PROVIDER) {
            return Err("同名 ZCode 提供商已存在".into());
        }
        providers.push(json!({"providerId":PROVIDER,"providerName":"AgentCockpit","enabled":true,"config":{
            "group":"standard-personal","access":{"type":"api-key","apiKey":key},
            "api":{"type":"anthropic-messages","baseUrl":url},"personalModelIds":models,"modelOrder":models}}));
        let rules = object(cfg, "modelConfigRules")?;
        list(rules, "manualProviderModelRules")?;
        let rules = list(rules, "providerModelRules")?;
        for row in rows {
            let mut cfg = json!({"properties":{"inputFormat":{"supportsImage":row["vision"] == true}}});
            copy_limit(&mut cfg["properties"], "contextWindow", row, "context_window");
            if let Some(n) = model_caps::positive(row, "max_output_tokens") {
                cfg["optionSpecs"] = json!({"maxOutputTokens":{"max":n}});
            }
            rules.push(json!({"providerId":PROVIDER,"modelId":row["id"],"config":cfg}));
        }
    } else {
        let mut catalog = serde_json::Map::new();
        for (id, row) in models.iter().zip(rows) {
            let mut entry = json!({"name":id,"modalities":{"input":model_caps::input(row),"output":["text"]}});
            let mut limits = json!({});
            copy_limit(&mut limits, "context", row, "context_window");
            copy_limit(&mut limits, "output", row, "max_output_tokens");
            if !limits.as_object().unwrap().is_empty() { entry["limit"] = limits; }
            catalog.insert(id.clone(), entry);
        }
        let providers = object(&mut doc, "provider")?;
        if providers.get(PROVIDER).is_some() {
            return Err("同名提供商已存在，请先检查原配置".into());
        }
        if app == "opencode" {
            providers[PROVIDER] = json!({"npm":"@ai-sdk/openai-compatible","name":"AgentCockpit","options":{"baseURL":base,"apiKey":key},"models":catalog});
            doc["model"] = json!(format!("{PROVIDER}/{model}"));
        } else {
            providers[PROVIDER] = json!({"name":"AgentCockpit","kind":"anthropic","enabled":true,"source":"custom","options":{"baseURL":url,"apiKey":key},"models":catalog});
        }
    }
    serde_json::to_string_pretty(&doc).map_err(|e| e.to_string())
}

pub(super) fn read(file: &Path) -> Result<Option<String>> {
    match fs::read_to_string(file) {
        Ok(text) => Ok(Some(text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.to_string()),
    }
}
pub(super) fn write(file: &Path, text: &str) -> Result<()> {
    let parent = file.parent().ok_or("配置路径无效")?;
    fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    let mut tmp = tempfile::NamedTempFile::new_in(parent).map_err(|e| e.to_string())?;
    tmp.write_all(text.as_bytes())
        .and_then(|_| tmp.as_file().sync_all())
        .map_err(|e| e.to_string())?;
    tmp.persist(file).map_err(|e| e.error.to_string())?;
    Ok(())
}

#[derive(Serialize, Deserialize)]
pub(super) struct Backup {
    pub(super) before: Option<String>,
    pub(super) applied: String,
    pub(super) model: String,
    // A refresh replaces two files. An interruption after the backup write
    // must still allow restoring the client that has its previous settings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) previous: Option<String>,
}
pub(super) fn backup_path(path: &Path) -> PathBuf {
    path.with_extension("cockpit-gateway-restore")
}
pub(super) fn backup(path: &Path) -> Result<Option<Backup>> {
    read(&backup_path(path))?
        .map(|s| {
            serde_json::from_str(&s)
                .map_err(|_| "恢复记录损坏，请检查配置旁的 cockpit-gateway-restore 文件".into())
        })
        .transpose()
}

// Three-way undo. Changes outside the fields we wrote belong to the client.
fn undo_json(
    before: Option<&Value>,
    after: Option<&Value>,
    current: Option<&Value>,
) -> Result<Option<Value>> {
    if before == after || before == current {
        return Ok(current.cloned());
    }
    if after == current {
        return Ok(before.cloned());
    }
    if let (Some(a), Some(c)) = (
        after.and_then(Value::as_object),
        current.and_then(Value::as_object),
    ) {
        let b = before.and_then(Value::as_object);
        let mut out = c.clone();
        for key in a
            .keys()
            .chain(b.into_iter().flat_map(|o| o.keys()))
            .collect::<std::collections::BTreeSet<_>>()
        {
            match undo_json(b.and_then(|o| o.get(key)), a.get(key), c.get(key))? {
                Some(v) => {
                    out.insert(key.clone(), v);
                }
                None => {
                    out.remove(key);
                }
            }
        }
        return Ok(if before.is_none() && out.is_empty() {
            None
        } else {
            Some(Value::Object(out))
        });
    }
    if let (Some(a), Some(c)) = (
        after.and_then(Value::as_array),
        current.and_then(Value::as_array),
    ) {
        let b = before
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        if a.starts_with(b) {
            let mut out = c.clone();
            for added in &a[b.len()..] {
                let idx = out
                    .iter()
                    .position(|v| v == added)
                    .ok_or("客户端修改了接入条目，请核对恢复记录")?;
                out.remove(idx);
            }
            return Ok(if before.is_none() && out.is_empty() {
                None
            } else {
                Some(json!(out))
            });
        }
    }
    Err(
        "客户端已修改接入字段。为保留修改，请核对配置旁的 cockpit-gateway-restore 恢复记录。"
            .into(),
    )
}

/// Undo one client connection. Returns true when a restore record existed.
/// Only the fields this app wrote are removed; later client edits stay.
pub(super) fn disconnect(path: &Path) -> Result<bool> {
    let Some(saved) = backup(path)? else {
        return Ok(false);
    };
    let current = read(path)?;
    let original = restored(path, &saved, current.as_deref())?;
    if current != read(path)? {
        return Err("配置刚被修改，请重试".into());
    }
    if original != current {
        match original {
            Some(text) => write(path, &text)?,
            None => fs::remove_file(path).map_err(|e| e.to_string())?,
        }
    }
    fs::remove_file(backup_path(path)).map_err(|e| e.to_string())?;
    Ok(true)
}

/// Every client that still has a restore record beside its configuration.
/// Used when the gateway stops, so nothing keeps pointing at a dead port.
pub(super) fn connected(apps: &[&str]) -> Vec<(String, PathBuf)> {
    let mut out = Vec::new();
    for app in apps {
        if let Ok(Some(path)) = target(app, "") {
            if backup_path(&path).is_file() {
                out.push((app.to_string(), path));
            }
        }
    }
    out
}

pub(super) fn restored(path: &Path, saved: &Backup, now: Option<&str>) -> Result<Option<String>> {
    if now == saved.before.as_deref() || now == Some(&saved.applied)
        || saved.previous.as_deref().is_some_and(|p| now == Some(p)) {
        return Ok(saved.before.clone());
    }
    if path.extension().and_then(|s| s.to_str()) == Some("toml") {
        use toml_edit::Document;
        let parse = |s: &str| s.parse::<Document>().map_err(|_| "Codex 配置格式已改变");
        let mut current = parse(now.ok_or("Codex 配置已移走")?)?;
        let before = parse(saved.before.as_deref().unwrap_or(""))?;
        let after = parse(&saved.applied)?;
        for field in ["model", "model_provider"] {
            if current.get(field).and_then(|v| v.as_str()) != after[field].as_str() {
                return Err("Codex 已改用其他默认模型，请先核对恢复记录".into());
            }
            if let Some(v) = before.get(field) {
                current[field] = v.clone()
            } else {
                current.remove(field);
            }
        }
        let tables = current
            .get_mut("model_providers")
            .and_then(|v| v.as_table_like_mut())
            .ok_or("Codex 提供商配置已移走")?;
        if tables.get(PROVIDER).map(ToString::to_string)
            != Some(after["model_providers"][PROVIDER].to_string())
        {
            return Err("Codex 提供商已改变，请核对恢复记录".into());
        }
        tables.remove(PROVIDER);
        if tables.is_empty() && before.get("model_providers").is_none() {
            current.remove("model_providers");
        }
        return Ok(Some(current.to_string()));
    }
    let before = saved.before.as_deref().map(parse_json).transpose()?;
    let current = now.map(parse_json).transpose()?;
    let undo = undo_json(
        before.as_ref(),
        Some(&parse_json(&saved.applied)?),
        current.as_ref(),
    )?;
    Ok(undo.map(|v| serde_json::to_string_pretty(&v).unwrap()))
}

fn catalog(port: u16, key: &str) -> Result<Vec<Value>> {
    let url = format!("http://127.0.0.1:{port}/v1/models");
    let data: Value = ureq::AgentBuilder::new()
        .redirects(0)
        .timeout(Duration::from_secs(25))
        .build()
        .get(&url)
        .set("Authorization", &format!("Bearer {key}"))
        .call()
        .map_err(|e| format!("读取网关模型失败：{e}"))?
        .into_json()
        .map_err(|_| "网关响应格式不符")?;
    let mut seen = std::collections::HashSet::new();
    Ok(data["data"].as_array().ok_or("缺少模型目录")?.iter()
        .filter(|m| m["id"].as_str().is_some_and(|id| !id.is_empty() && seen.insert(id)))
        .cloned().collect())
}

#[tauri::command]
pub async fn client_gateway(
    st: tauri::State<'_, AppState>,
    app: String,
    name: String,
    op: String,
    key_id: String,
    model: String,
    policy: Option<model_caps::Policy>,
    scope: Option<String>,
    settings: Option<Value>,
) -> Result<Value> {
    if op == "routes" {return Ok(crate::gateway_routes::snapshot());}
    let get_key = || {
        gateway::dist_keys()
            .into_iter()
            .find(|d| d["id"] == key_id && d["enabled"] != false)
            .and_then(|d| {
                d["key"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned)
            })
            .ok_or("请选择有效的接入密钥")
    };
    let port = st.proxy.port();
    let mut rows = Vec::new();
    let mut key = String::new();
    if matches!(op.as_str(), "models" | "connect" | "refresh" | "save_policy" | "provider_info" | "save_provider" | "refresh_prices") {
        if !st.proxy.is_running() {
            return Err("本地网关尚未启动".into());
        }
        key = get_key()?;
        if op == "refresh_prices" {
            let n=tauri::async_runtime::spawn_blocking(crate::gateway_cost::refresh_book).await.map_err(|e|e.to_string())??;
            return Ok(json!({"count":n}));
        }
        rows = catalog(port, &key)?;
        if op == "models" {
            let mut policies = model_caps::load()?;
            policies.retain(|scope, _| rows.iter().any(|m| m["id"] == scope.as_str() || m["owned_by"] == scope.as_str()));
            return Ok(json!({"models":rows.iter().map(|m| &m["id"]).collect::<Vec<_>>(),"details":rows,"policies":policies}));
        }
        if op == "save_policy" {
            let _lock = crate::commands::cmd_guard();
            let provider = match scope.as_deref() {
                Some("provider") => true, Some("model") => false, _ => return Err("请选择设置范围".into()),
            };
            model_caps::save(&model, provider, policy.ok_or("缺少模型设置")?, &rows)?;
            return Ok(json!({"ok":true}));
        }
        if matches!(op.as_str(),"provider_info" | "save_provider") {
            let row=rows.iter().find(|m|m["id"]==model).ok_or("这把密钥不提供所选模型")?;
            let source=row["owned_by"].as_str().ok_or("模型缺少来源")?;
            let (upstream,_)=st.proxy.config();
            let route=crate::proxy::pick_upstreams(&key,&upstream);
            let groups=crate::gateway_dispatch::authorized_groups(&route.ups);
            if op=="save_provider"{
                let _lock=crate::commands::cmd_guard();
                crate::gateway_dispatch::save(&model,row,scope.as_deref().unwrap_or(""),settings.unwrap_or(Value::Null),&rows,&groups)?;
                return Ok(json!({"ok":true}));
            }
            let settings=crate::gateway_dispatch::load()?;
            return Ok(json!({"rule":settings.providers.get(source).cloned().unwrap_or_default(),"price":settings.prices.get(&model),
                "official":crate::gateway_cost::facts(&model),"groups":groups,
                "effective":crate::gateway_cost::effective(&model,&settings,chrono::Local::now().timestamp()),
                "summary":crate::gateway_cost::summary(if scope.as_deref()==Some("model"){&model}else{source},&settings)?}));
        }
        if !model.is_empty() {
            rows.retain(|m| m["id"] == model);
            if rows.is_empty() {
                return Err("这把密钥当前不提供所选模型，请刷新目录".into());
            }
        }
    }
    let _lock = crate::commands::cmd_guard();
    if app == "claude-desktop" {
        if op != "status" && procs::agent_running(&app, &procs::snapshot_fresh()) {
            return Err("请先关闭 Claude 桌面版，再修改网关档案".into());
        }
        return crate::claude_gateway::action(
            &op,
            &format!("http://127.0.0.1:{port}"),
            &key,
            &model,
        );
    }
    let Some(path) = target(&app, &name)? else {
        return Ok(json!({"supported":false}));
    };
    let saved = backup(&path)?;
    let current = read(&path)?;
    let connected = saved.as_ref().is_some_and(|b| {
        current.as_ref() == Some(&b.applied)
            || (current != b.before && restored(&path, b, current.as_deref()).is_ok())
    });
    if op == "status" {
        return Ok(
            json!({"supported":true,"connected":connected,"managed":saved.is_some(),"path":path,"model":saved.as_ref().map(|b|&b.model)}),
        );
    }
    if !matches!(op.as_str(), "disconnect" | "connect" | "refresh") {
        return Err("未知接入动作".into());
    }
    if procs::agent_running(&app, &procs::snapshot_fresh()) {
        return Err("请先关闭目标客户端，再修改其模型配置。".into());
    }
    if op == "disconnect" {
        disconnect(&path)?;
        // 客户端自己的配置已还原，但它缓存里那批指向本机网关的模型还在。
        // 断开之后那些条目只会连不上，顺手清掉，不必等用户手动删。
        // 清理要起内置 Python 读 SQLite，放到后台，别让「断开」按钮干等。
        let target = app.clone();
        std::thread::spawn(move || {
            crate::gateway_cleanup::sweep(port, Some(&target));
        });
        return Ok(json!({"ok": true}));
    } else {
        if op == "connect" && saved.is_some() {
            return Err("已有恢复记录，请先断开接入".into());
        }
        let original = if op == "refresh" {
            let old = saved.as_ref().ok_or("此客户端尚未接入")?;
            restored(&path, old, current.as_deref())?
        } else { current.clone() };
        let applied = configured(
            &app,
            &path,
            original.as_deref(),
            &format!("http://127.0.0.1:{port}"),
            &key,
            &rows,
        )?;
        let mut plan = Backup {
            before: original,
            applied,
            model: if rows.len() == 1 {
                rows[0]["id"].as_str().unwrap_or_default().to_owned()
            } else {
                format!("{} 个模型", rows.len())
            },
            previous: saved.as_ref().and(current.clone()),
        };
        if read(&path)? != current {
            return Err("客户端刚更新了配置，请刷新后重试".into());
        }
        write(
            &backup_path(&path),
            &serde_json::to_string(&plan).map_err(|e| e.to_string())?,
        )?;
        if read(&path)? != current {
            return Err("客户端刚更新了配置，请核对恢复记录".into());
        }
        write(&path, &plan.applied)?;
        if plan.previous.take().is_some() {
            // The staged backup is already valid if this final cleanup fails.
            let _ = write(&backup_path(&path), &serde_json::to_string(&plan).unwrap());
        }
    }
    Ok(json!({"ok":true}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gateway_facts_reach_each_native_client_and_text_models_stay_text_only() {
        let rows = [json!({"id":"trae/eye","vision":true,"context_window":1_000_000,"max_output_tokens":64_000,
            "auto_compact_token_limit":500_000}), json!({"id":"trae/text","vision":false})];
        let make = |app, path| parse_json(&configured(app, Path::new(path), None, "http://localhost", "key", &rows).unwrap()).unwrap();
        let q = make("qoder-cn", "settings.json");
        let q = &q["providers"].as_object().unwrap().values().next().unwrap()["models"];
        assert_eq!(q[0]["capabilities"]["vision"], true);
        assert_eq!(q[0]["contextWindow"], 1_000_000);
        assert_eq!(q[1]["capabilities"]["vision"], false);
        assert!(q[1].get("contextWindow").is_none());
        let w = make("workbuddy", "models.json");
        assert_eq!((w[0]["supportsImages"].as_bool(),w[0]["maxInputTokens"].as_u64(),w[0]["maxOutputTokens"].as_u64()),(Some(true),Some(1_000_000),Some(64_000)));
        for (app,path) in [("zcode","config.json"),("opencode","opencode.json")] {
            let doc = make(app,path);
            let m = &doc["provider"][PROVIDER]["models"];
            assert_eq!(m["trae/eye"]["modalities"]["input"],json!(["text","image"]));
            assert_eq!(m["trae/text"]["modalities"]["input"],json!(["text"]));
            assert_eq!(m["trae/eye"]["limit"],json!({"context":1_000_000,"output":64_000}));
        }
        let z = make("zcode","provider_config.json");
        let cfg = &z["config"]["modelConfigRules"]["providerModelRules"][0]["config"];
        assert_eq!(cfg["properties"]["inputFormat"]["supportsImage"], true);
        assert_eq!(cfg["optionSpecs"]["maxOutputTokens"]["max"],64_000);
        let c = make("claude","settings.json");
        assert_eq!(c["env"]["CLAUDE_CODE_MAX_CONTEXT_TOKENS"],"1000000");
        assert_eq!(c["env"]["CLAUDE_CODE_AUTO_COMPACT_WINDOW"],"500000");
        assert_eq!(c["env"]["CLAUDE_CODE_MAX_OUTPUT_TOKENS"],"64000");
        let c = crate::codexcatalog::catalog(&rows);
        assert_eq!(c["models"][0]["auto_compact_token_limit"],500_000);
        assert_eq!(c["models"][0]["input_modalities"],json!(["text","image"]));
        let c = crate::claude_gateway::model_details(&rows[0]);
        assert_eq!(c["max_tokens"],64_000);
        assert_eq!(c["capabilities"]["image_input"]["supported"],true);
    }

    #[test]
    fn native_providers_roundtrip_and_preserve_unrelated_edits() {
        let targets = [
            (
                "codex",
                "config.toml",
                "model = 'before'\n# custom note\n[projects.example]\ntrust_level = 'trusted'\n",
            ),
            (
                "claude",
                "settings.json",
                r#"{"env":{"ANTHROPIC_AUTH_TOKEN":"original"},"model":"sonnet"}"#,
            ),
            (
                "qoder-cn",
                "settings.json",
                r#"{"providers":{"mine":{}},"model":{"name":"old"}}"#,
            ),
            ("opencode", "opencode.json", r#"{"provider":{"mine":{}}}"#),
            (
                "zcode",
                "provider_config.json",
                r#"{"schemaVersion":1,"config":{"providerConfigRules":{"providerRules":[{"providerId":"mine"}]}}}"#,
            ),
            ("zcode", "config.json", r#"{"provider":{"mine":{}}}"#),
            (
                "workbuddy-intl",
                "models.json",
                r#"{"models":[{"id":"mine"}],"availableModels":["mine"]}"#,
            ),
            ("workbuddy", "models.json", r#"[{"id":"mine"}]"#),
        ];
        for (app, file, input) in targets {
            let path = Path::new(file);
            let output = configured(
                app,
                path,
                Some(input),
                "http://127.0.0.1:1234",
                "fixture-key",
                &[json!({"id":"demo/test"})],
            )
            .unwrap();
            let undo = Backup {
                before: Some(input.into()),
                applied: output.clone(),
                model: "demo/test".into(),
                previous: None,
            };
            assert!(
                output.contains("demo/test") && output.contains("fixture-key"),
                "{app}"
            );
            assert_eq!(
                restored(path, &undo, Some(&output)).unwrap().as_deref(),
                Some(input)
            );
            if file.ends_with(".toml") {
                let edited = format!("# after connect\n{output}");
                assert!(restored(path, &undo, Some(&edited))
                    .unwrap()
                    .unwrap()
                    .contains("custom note"));
            } else {
                let mut doc = parse_json(&output).unwrap();
                let mut expected = parse_json(input).unwrap();
                if doc.is_array() {
                    doc.as_array_mut().unwrap().push(json!({"id":"added"}));
                    expected.as_array_mut().unwrap().push(json!({"id":"added"}));
                } else {
                    doc["new_setting"] = json!(42);
                    expected["new_setting"] = json!(42);
                }
                let text = restored(path, &undo, Some(&doc.to_string()))
                    .unwrap()
                    .unwrap();
                assert_eq!(parse_json(&text).unwrap(), expected, "{app}");
            }
        }
    }

    #[test]
    fn reject_foreign_models_and_malformed_configs() {
        let path = Path::new("models.json");
        let ids = vec![json!({"id":"mine"})];
        for src in [r#"[{"id":"mine"}]"#, r#"{"models":42}"#, "invalid"] {
            assert!(configured(
                "workbuddy",
                path,
                Some(src),
                "http://localhost",
                "unused",
                &ids
            )
            .is_err());
        }
        let cfg = r#"{"env":{"ABC":"old"}}"#;
        let applied = configured(
            "claude",
            path,
            Some(cfg),
            "http://localhost",
            "fixture",
            &ids,
        )
        .unwrap();
        let mut edited = parse_json(&applied).unwrap();
        edited["env"]["ANTHROPIC_BASE_URL"] = json!("elsewhere");
        let undo = Backup {
            before: Some(cfg.into()),
            applied,
            model: "mine".into(),
            previous: None,
        };
        assert!(restored(path, &undo, Some(&edited.to_string())).is_err());
    }
}
