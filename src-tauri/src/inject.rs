// 把 API 台账里的一条 key 写进某个 Agent 的自定义模型配置，让那个 Agent 直接能用。
//
// 三条硬规矩（都是踩过坑定下来的，别绕）：
// 1. 目标程序在跑就不动它——Qoder / WorkBuddy 退出时会把手里的配置整份写回，
//    我们塞进去的那条会被覆盖掉，用户看到的就是"加了又没了"。
// 2. 写之前把原文件整份备份进数据根，且只改自己要改的那一段（读-改-写，绝不整份重建）。
// 3. 只认自己写进去的条目：写回的 id 记在台账 key 的 `injected` 字段里，
//    撤的时候按 id 精确删，不去猜"哪条看起来像驾驶舱加的"。
//
// 各家落点（2026-10-10 磁盘实测）：
//   Qoder 系  ~/.qoder-cn/settings.json / ~/.qoder/settings.json 的 providers 映射，
//             条目 id 形如 qoder-custom-<uuid>，字段与手工添加的完全一致
//   WorkBuddy ~/.workbuddy/models.json 的明文数组，一个模型一条
//             （WorkBuddy 与 WorkBuddy 国际版共用这一份，写一次两边都有）
//   TraeWork  本机没有明文 provider 落点（模型清单是 state.vscdb 里服务端下发的缓存），
//             不做直写
use crate::registry as reg;
use crate::secrets;
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};

/// 支持直写的产品。shared=true 表示它与别的档案目录共用同一份配置。
struct Target {
    app: &'static str,
    label: &'static str,
    kind: Kind,
    note: &'static str,
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    QoderProviders,
    WorkbuddyModels,
}

const TARGETS: &[Target] = &[
    Target {
        app: "qoder-cn",
        label: "Qoder CN",
        kind: Kind::QoderProviders,
        note: "写进 ~/.qoder-cn/settings.json 的 providers，与手工添加的自定义模型同一种条目",
    },
    Target {
        app: "qoder",
        label: "Qoder",
        kind: Kind::QoderProviders,
        note: "写进 ~/.qoder/settings.json 的 providers；国际版没实测过这个字段，若不认就撤掉改走反代",
    },
    Target {
        app: "workbuddy",
        label: "WorkBuddy",
        kind: Kind::WorkbuddyModels,
        note: "写进 ~/.workbuddy/models.json，一个模型一条；WorkBuddy 国际版共用这一份，写一次两边都有",
    },
];

fn targets() -> Vec<&'static Target> {
    TARGETS.iter().collect()
}

fn target_of(app: &str) -> Option<&'static Target> {
    TARGETS.iter().find(|t| t.app == app)
}

/// 配置文件位置。故意不拿 registry 的 profile——那是登录态/junction 目标，
/// 换号时会被搬走，配置落点必须钉在原生位置。
fn config_file(app: &str) -> Option<PathBuf> {
    let home = dirs_home()?;
    match app {
        "qoder-cn" => Some(home.join(".qoder-cn").join("settings.json")),
        "qoder" => Some(home.join(".qoder").join("settings.json")),
        "workbuddy" => Some(home.join(".workbuddy").join("models.json")),
        _ => None,
    }
}

fn dirs_home() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("USERPROFILE") {
        if !p.trim().is_empty() {
            return Some(PathBuf::from(p));
        }
    }
    std::env::home_dir()
}

/// 要注入哪些模型：站点的清单为主，key 自己那份是补充（都空时兜 key["model"]）
fn models_of(key: &Value, site: &Value) -> Vec<String> {
    let mut out = crate::keys::models_for(site, key);
    if out.is_empty() {
        let d = key["model"].as_str().unwrap_or("").trim();
        if !d.is_empty() {
            out.push(d.to_string());
        }
    }
    out
}

fn new_uuid() -> String {
    // 不引 uuid crate：Qoder 只要求这个 id 在 providers 里唯一，形状对上就行
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(1);
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let mix = t ^ (n.wrapping_mul(0x9E37_79B9_7F4A_7C15));
    let mut b = [0u8; 16];
    b[..8].copy_from_slice(&t.to_le_bytes());
    b[8..].copy_from_slice(&mix.to_le_bytes());
    b[6] = (b[6] & 0x0f) | 0x40; // version 4
    b[8] = (b[8] & 0x3f) | 0x80; // variant
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7], b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15]
    )
}

/// Qoder 的一条 provider。字段与用户手工添加的那 8 条对齐；
/// 能力位一律给保守默认（不声明 vision、不开思考），Qoder 里可以再手改。
pub fn qoder_entry(site_name: &str, base: &str, key: &str, models: &[String]) -> (String, Value) {
    let id = format!("qoder-custom-{}", new_uuid());
    let ms: Vec<Value> = models
        .iter()
        .map(|m| {
            json!({
                "model": m,
                "displayName": m,
                "contextWindow": 1_000_000,
                "maxOutputTokens": 8192,
                "capabilities": {
                    "vision": false,
                    "thinking": { "modes": [], "supportsEffort": false, "supportedEffortLevels": [] }
                }
            })
        })
        .collect();
    let first = models.first().cloned().unwrap_or_default();
    (
        id.clone(),
        json!({
            "apiKey": key,
            "authType": "bearer",
            "baseUrl": base,
            "displayName": format!("驾驶舱 · {}", site_name),
            "model": first,
            "models": ms,
            "protocol": "openai",
            "type": "openai-compatible"
        }),
    )
}

/// WorkBuddy 的一条模型（一个模型一条记录）
pub fn workbuddy_entries(site_name: &str, base: &str, key: &str, models: &[String]) -> Vec<Value> {
    models
        .iter()
        .map(|m| {
            json!({
                "id": m,
                "name": format!("{} · {}", m, site_name),
                "vendor": "Custom",
                "url": base,
                "apiKey": key,
                "supportsToolCall": true,
                "supportsImages": false,
                "supportsReasoning": false,
                "useCustomProtocol": false
            })
        })
        .collect()
}

/// 把 provider 插进已读出的 settings.json（只动 providers，别的键原样保留）
pub fn insert_qoder(doc: &mut Value, id: &str, entry: &Value) {
    if !doc["providers"].is_object() {
        doc["providers"] = json!({});
    }
    doc["providers"][id] = entry.clone();
}

pub fn remove_qoder(doc: &mut Value, ids: &[String]) -> usize {
    let mut n = 0;
    if let Some(map) = doc["providers"].as_object_mut() {
        for id in ids {
            if map.remove(id).is_some() {
                n += 1;
            }
        }
    }
    n
}

/// WorkBuddy 的 models.json 是裸数组，按 id 增删
pub fn insert_wb(arr: &mut Vec<Value>, entries: &[Value]) -> usize {
    let mut added = 0;
    for e in entries {
        let id = e["id"].as_str().unwrap_or("").to_string();
        match arr.iter().position(|x| x["id"].as_str() == Some(&id)) {
            Some(i) => arr[i] = e.clone(),
            None => {
                arr.push(e.clone());
                added += 1;
            }
        }
    }
    added
}

pub fn remove_wb(arr: &mut Vec<Value>, ids: &[String]) -> usize {
    let before = arr.len();
    arr.retain(|x| !ids.iter().any(|id| x["id"].as_str() == Some(id.as_str())));
    before - arr.len()
}

fn backup_file(p: &Path) -> PathBuf {
    let dir = reg::base_dir().join("backups").join("inject");
    let _ = fs::create_dir_all(&dir);
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
    let name = p
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "config".into());
    let dst = dir.join(format!("{}-{}", stamp, name));
    // 同一秒内注入多个产品会撞名，撞了就加序号——备份丢了就没退路，不能省
    let mut dst = dst;
    let mut i = 1;
    while dst.exists() {
        dst = dir.join(format!("{}-{}.{}", stamp, name, i));
        i += 1;
    }
    let _ = fs::copy(p, &dst);
    dst
}

/// 原子写：先落临时文件再改名覆盖，避免程序正好在读时拿到半份 JSON
fn write_atomic(p: &Path, text: &str) -> Result<(), String> {
    let tmp = p.with_extension("cockpit-tmp");
    fs::write(&tmp, text).map_err(|e| format!("写临时文件失败：{}", e))?;
    fs::rename(&tmp, p).map_err(|e| format!("替换配置失败：{}", e))?;
    Ok(())
}

/// 某个产品现在能不能动它的配置
fn guard_running(app: &str) -> Option<String> {
    let snap = crate::procs::snapshot();
    if crate::procs::agent_running(app, &snap) {
        return Some(format!(
            "{} 正在运行：它会把自己内存里的配置在退出时整份写回，现在塞进去的会被覆盖。先关掉这个程序再注入。",
            reg::app_def(app).map(|a| a.name).unwrap_or_else(|| app.to_string())
        ));
    }
    None
}

/// 台账里这条 key 已经往哪些产品写过什么
fn injected_of(key: &Value, app: &str) -> Vec<String> {
    key["injected"][app]
        .as_array()
        .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
        .unwrap_or_default()
}

fn set_injected(site: &str, key_id: &str, app: &str, ids: &[String]) {
    crate::keys::set_injected(site, key_id, app, ids);
}

/// 目标清单：给界面摆"能接到哪、现在什么状态"
pub fn list_for(key: &Value) -> Vec<Value> {
    targets()
        .into_iter()
        .map(|t| {
            let file = config_file(t.app);
            let exists = file.as_ref().map(|p| p.is_file()).unwrap_or(false);
            let ids = injected_of(key, t.app);
            json!({
                "app": t.app,
                "label": t.label,
                "note": t.note,
                "file": file.map(|p| p.to_string_lossy().to_string()).unwrap_or_default(),
                "config_exists": exists,
                "running": guard_running(t.app).is_some(),
                "injected": ids,
            })
        })
        .collect()
}

/// 把这条 key 注入勾选的产品。逐个来，一个失败不影响别个。
pub fn apply(site: &str, key_id: &str, apps: &[String]) -> Vec<Value> {
    let sobj = match crate::keys::sites()
        .into_iter()
        .find(|p| p["id"].as_str() == Some(site))
    {
        Some(p) => p,
        None => return vec![json!({ "ok": false, "msg": "台账里没找到这个站点" })],
    };
    let full = match sobj["keys"].as_array().and_then(|ks| {
        ks.iter().find(|k| k["id"].as_str() == Some(key_id)).cloned()
    }) {
        Some(k) => k,
        None => return vec![json!({ "ok": false, "msg": "台账里没找到这条 key" })],
    };
    // 接入点挂在站点上（一个站常有 OpenAI / Anthropic / DashScope 三种口），
    // key 自己那份只是覆盖
    let base = crate::keys::base_for(&sobj, &full).trim().to_string();
    let plain = full["key"].as_str().unwrap_or("").trim().to_string();
    if base.is_empty() || plain.is_empty() {
        return vec![json!({ "ok": false, "msg": "这个站点还没填接入点，或这条 key 的原文是空的——先补全再注入" })];
    }
    let models = models_of(&full, &sobj);
    if models.is_empty() {
        return vec![json!({ "ok": false, "msg": "还没有可用模型清单——先在站点上点「拉模型」勾几个" })];
    }
    let site_name = sobj["name"].as_str().unwrap_or("API").to_string();

    apps.iter()
        .filter_map(|app| inject_one(app, &site_name, &base, &plain, &models, site, key_id, &full))
        .collect()
}

fn inject_one(
    app: &str,
    site_name: &str,
    base: &str,
    plain: &str,
    models: &[String],
    site: &str,
    key_id: &str,
    full: &Value,
) -> Option<Value> {
    let t = match target_of(app) {
        Some(t) => t,
        None => return Some(json!({ "ok": false, "app": app, "msg": "这个产品还没接直写通道" })),
    };
    if let Some(msg) = guard_running(app) {
        return Some(json!({ "ok": false, "app": app, "msg": msg }));
    }
    let path = match config_file(app) {
        Some(p) => p,
        None => return Some(json!({ "ok": false, "app": app, "msg": "找不到配置文件路径" })),
    };
    if !path.is_file() {
        return Some(json!({
            "ok": false, "app": app,
            "msg": format!("{} 还不存在。先在这个程序里手工加一次任意自定义模型，让它把配置文件建出来，再回来注入。", path.display())
        }));
    }
    let raw = match fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) => return Some(json!({ "ok": false, "app": app, "msg": format!("读不了配置：{}", e) })),
    };

    let r = match t.kind {
        Kind::QoderProviders => {
            let mut doc: Value = match serde_json::from_str(&raw) {
                Ok(v) => v,
                Err(e) => {
                    return Some(json!({ "ok": false, "app": app,
                        "msg": format!("配置不是合法 JSON，怕改坏，没动它：{}", e) }))
                }
            };
            let (id, entry) = qoder_entry(site_name, base, plain, models);
            insert_qoder(&mut doc, &id, &entry);
            let mut ids = injected_of(full, app);
            ids.push(id.clone());
            (json!({ "added": [id], "models": models.len() }), ids, doc)
        }
        Kind::WorkbuddyModels => {
            let mut arr: Vec<Value> = match serde_json::from_str(&raw) {
                Ok(v) => v,
                Err(e) => {
                    return Some(json!({ "ok": false, "app": app,
                        "msg": format!("配置不是合法 JSON 数组，怕改坏，没动它：{}", e) }))
                }
            };
            let entries = workbuddy_entries(site_name, base, plain, models);
            let ids: Vec<String> = entries.iter().filter_map(|e| e["id"].as_str().map(String::from)).collect();
            insert_wb(&mut arr, &entries);
            let mut mine = injected_of(full, app);
            for id in &ids {
                if !mine.iter().any(|x| x == id) {
                    mine.push(id.clone());
                }
            }
            let doc = Value::Array(arr);
            (json!({ "added": ids, "models": models.len() }), mine, doc)
        }
    };
    let (detail, ids, doc) = r;
    let text = serde_json::to_string_pretty(&doc).unwrap_or_default();
    let bak = backup_file(&path);
    if let Err(e) = write_atomic(&path, &text) {
        return Some(json!({ "ok": false, "app": app, "msg": e }));
    }
    set_injected(site, key_id, app, &ids);
    Some(json!({
        "ok": true, "app": app, "label": t.label,
        "msg": format!("已写入 {} 个模型", detail["models"].as_u64().unwrap_or(0)),
        "file": path.to_string_lossy(), "backup": bak.to_string_lossy(), "added": detail["added"],
    }))
}

/// 撤掉这条 key 往某个产品写过的条目
pub fn revert(site: &str, key_id: &str, app: &str) -> Value {
    let t = match target_of(app) {
        Some(t) => t,
        None => return json!({ "ok": false, "msg": "这个产品还没接直写通道" }),
    };
    if let Some(msg) = guard_running(app) {
        return json!({ "ok": false, "msg": msg });
    }
    let full = match crate::keys::sites()
        .iter()
        .find(|p| p["id"].as_str() == Some(site))
        .and_then(|p| p["keys"].as_array())
        .and_then(|ks| ks.iter().find(|k| k["id"].as_str() == Some(key_id)).cloned())
    {
        Some(k) => k,
        None => return json!({ "ok": false, "msg": "台账里没找到这条 key" }),
    };
    let ids = injected_of(&full, app);
    if ids.is_empty() {
        return json!({ "ok": false, "msg": "这条 key 没往它写过，没什么可撤" });
    }
    let path = match config_file(app) {
        Some(p) => p,
        None => return json!({ "ok": false, "msg": "找不到配置文件路径" }),
    };
    let raw = match fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) => return json!({ "ok": false, "msg": format!("读不了配置：{}", e) }),
    };
    let mut doc: Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(e) => return json!({ "ok": false, "msg": format!("配置不是合法 JSON，不敢动：{}", e) }),
    };
    let removed = match t.kind {
        Kind::QoderProviders => remove_qoder(&mut doc, &ids),
        Kind::WorkbuddyModels => {
            let mut arr = doc.as_array().cloned().unwrap_or_default();
            let n = remove_wb(&mut arr, &ids);
            doc = Value::Array(arr);
            n
        }
    };
    let text = serde_json::to_string_pretty(&doc).unwrap_or_default();
    let bak = backup_file(&path);
    if let Err(e) = write_atomic(&path, &text) {
        return json!({ "ok": false, "msg": e });
    }
    // 只删我们自己记的那几条里剩下的部分
    let left: Vec<String> = injected_of(&full, app)
        .into_iter()
        .filter(|x| !ids.contains(x))
        .collect();
    set_injected(site, key_id, app, &left);
    json!({
        "ok": true, "app": app,
        "msg": format!("已撤掉 {} 条（配置里剩下的没动）", removed),
        "backup": bak.to_string_lossy(),
    })
}

/// 保管箱锁着的时候不能写（拿不到 Key 原文），界面据此置灰
pub fn vault_ready() -> bool {
    !secrets::locked()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entry_shapes_match_what_the_apps_already_read() {
        // 字段名一个都不能错：Qoder 认 type/protocol/baseUrl/model/models[]，
        // WorkBuddy 认 vendor/url/apiKey/supports*
        let (id, e) = qoder_entry("LongCat", "https://api.longcat.chat/openai/v1", "sk-x", &["LongCat-2.0".into()]);
        assert!(id.starts_with("qoder-custom-"), "id 要跟着 Qoder 自己的写法");
        assert_eq!(e["type"], "openai-compatible");
        assert_eq!(e["protocol"], "openai");
        assert_eq!(e["authType"], "bearer");
        assert_eq!(e["baseUrl"], "https://api.longcat.chat/openai/v1");
        assert_eq!(e["model"], "LongCat-2.0", "默认模型取清单里第一个");
        assert_eq!(e["models"][0]["model"], "LongCat-2.0");
        assert_eq!(e["models"][0]["maxOutputTokens"], 8192);

        let w = workbuddy_entries("LongCat", "https://x/v1", "sk-x", &["a".into(), "b".into()]);
        assert_eq!(w.len(), 2, "WorkBuddy 是一个模型一条");
        assert_eq!(w[0]["id"], "a");
        assert_eq!(w[0]["vendor"], "Custom");
        assert_eq!(w[0]["url"], "https://x/v1");
    }

    #[test]
    fn writing_preserves_everything_else_in_the_config() {
        // 用户 settings.json 里还有 enabledPlugins / mcpServers，注入不能把它们抹掉
        let mut doc = json!({
            "enabledPlugins": { "a@b": true },
            "mcpServers": { "playwright": {} },
            "providers": { "qoder-custom-existing": { "model": "keep-me" } }
        });
        insert_qoder(&mut doc, "qoder-custom-new", &json!({ "model": "glm" }));
        assert_eq!(doc["enabledPlugins"]["a@b"], json!(true));
        assert!(doc["mcpServers"]["playwright"].is_object());
        assert_eq!(doc["providers"]["qoder-custom-existing"]["model"], "keep-me");
        assert_eq!(doc["providers"]["qoder-custom-new"]["model"], "glm");
        assert_eq!(remove_qoder(&mut doc, &["qoder-custom-new".into(), "没这个".into()]), 1);
        assert!(doc["providers"]["qoder-custom-existing"].is_object(), "只删自己写的那条");
        assert_eq!(doc["enabledPlugins"]["a@b"], json!(true));
    }

    #[test]
    fn workbuddy_list_is_updated_in_place_not_rebuilt() {
        let mut arr = vec![json!({ "id": "老模型", "name": "老" })];
        let added = insert_wb(&mut arr, &[json!({ "id": "新模型", "name": "新" }), json!({ "id": "老模型", "name": "改了" })]);
        assert_eq!(added, 1, "同 id 要就地更新而不是再塞一条");
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["name"], "改了");
        assert_eq!(remove_wb(&mut arr, &["新模型".into()]), 1);
        assert_eq!(arr.len(), 1);
    }

    #[test]
    fn model_list_comes_from_the_site_with_the_key_as_extra() {
        // 站点那份在前，key 自己补的在后，逗号/中文逗号/换行/空格都算分隔，重复的并掉
        let site = json!({ "models_list": "glm-5.3-flash, qwen3.8-flash，u2-flash\nhy3  hy3" });
        let key = json!({ "models_list": "LongCat-2.0 glm-5.3-flash" });
        assert_eq!(
            models_of(&key, &site),
            vec!["glm-5.3-flash", "qwen3.8-flash", "u2-flash", "hy3", "LongCat-2.0"]
        );
        assert_eq!(
            models_of(&json!({ "model": "only-one" }), &json!({})),
            vec!["only-one"],
            "站点和 key 都没清单时用默认模型兜一条"
        );
        assert!(models_of(&json!({}), &json!({})).is_empty());
    }
}
