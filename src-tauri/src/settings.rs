// 应用设置：默认值 + 与 config.json 里 settings 的深合并
use crate::registry::{load_config, save_config};
use serde_json::{json, Value};

pub fn defaults() -> Value {
    json!({
        "profile": { "nickname": "", "note": "" },
        "general": {
            "autostart": false,
            "close_action": "tray",      // tray | exit
            "notify_alerts": true,
            "refresh_sec": 4,
            "auto_scan": true
        },
        "appearance": { "theme": "system", "density": "comfortable", "font_scale": 100, "nav_w": 248, "nav_collapsed": false },
        "quota": { "warn_days": 30, "danger_days": 7, "auto_fetch_min": 30 },
        "proxy": { "port": 8788, "upstream": "https://api.openai.com", "log_keep": 100 },
        "app_order": [],
        "ai": {
            "base_url": "https://dashscope.aliyuncs.com/compatible-mode/v1",
            // 文本识别（key 清单转 JSON）用小快模型就够了；截图另有 model_vision 一档
            "model": "qwen-flash",
            "model_vision": "qwen-vl-flash",
            "key": "",
            // 磁盘上不存原文，只在保管箱里；界面用它表示"已经配过 Key"
            "key_held": false
        }
    })
}

fn merge(base: &mut Value, patch: &Value) {
    match (base.as_object_mut(), patch.as_object()) {
        (Some(b), Some(p)) => {
            for (k, v) in p {
                match b.get_mut(k) {
                    Some(slot) if slot.is_object() && v.is_object() => merge(slot, v),
                    _ => {
                        b.insert(k.clone(), v.clone());
                    }
                }
            }
        }
        _ => *base = patch.clone(),
    }
}

pub fn get() -> Value {
    let mut s = defaults();
    let cfg = load_config();
    if let Some(saved) = cfg.get("settings") {
        merge(&mut s, saved);
    }
    refill_ai_key(&mut s);
    s
}

/// AI Key 只存在保管箱里，config.json 里那个字段是空的；读出来时补回内存
pub const AI_KEY_ID: &str = "ai:key";

fn refill_ai_key(s: &mut Value) {
    let empty = s["ai"]["key"].as_str().unwrap_or("").is_empty();
    if empty {
        if let Some(k) = crate::secrets::get(AI_KEY_ID) {
            s["ai"]["key"] = json!(k);
        }
    }
}

/// 老 config.json 里可能还躺着明文 AI Key，收进保管箱并把那个字段抹空。
/// 走 update() 会被当成"用户清空了 Key"，所以这里单独读写那一个字段。
pub fn sweep() -> usize {
    let mut cfg = load_config();
    let key = cfg["settings"]["ai"]["key"]
        .as_str()
        .unwrap_or("")
        .trim()
        .to_string();
    if key.is_empty() || !crate::secrets::available() || !crate::secrets::put(AI_KEY_ID, &key) {
        return 0;
    }
    let _ = crate::secrets::flush();
    if let Some(ai) = cfg
        .get_mut("settings")
        .and_then(|s| s.get_mut("ai"))
        .and_then(|a| a.as_object_mut())
    {
        ai.insert("key".into(), json!(""));
        ai.insert("key_held".into(), json!(true));
    }
    save_config(&cfg);
    1
}

/// 界面只提交改动的那一个字段（setSetting 发的是 `{ai:{model:…}}`），
/// 所以"补丁里没有 ai.key"必须理解成**没动 Key**，不能拿 config.json 里那个空串
/// 当成"用户要清空"——否则改一次模型名就把保管箱里的 Key 删了（真发生过）。
fn key_submitted(patch: &Value) -> bool {
    patch.get("ai").and_then(|a| a.get("key")).is_some()
}

pub fn update(patch: &Value) -> Value {
    let mut cfg = load_config();
    let mut s = cfg.get("settings").cloned().unwrap_or_else(|| json!({}));
    let clearing = key_submitted(patch);
    merge(&mut s, patch);
    // 内存里那份带原文，回给界面继续显示；写进 config.json 的那份收进保管箱
    let key = s["ai"]["key"].as_str().unwrap_or("").trim().to_string();
    let mut disk = s.clone();
    if !key.is_empty() {
        if crate::secrets::available() && crate::secrets::put(AI_KEY_ID, &key) {
            disk["ai"]["key"] = json!("");
            disk["ai"]["key_held"] = json!(true);
            let _ = crate::secrets::flush();
        }
    } else if clearing && crate::secrets::available() {
        crate::secrets::remove(AI_KEY_ID);
        disk["ai"]["key_held"] = json!(false);
        let _ = crate::secrets::flush();
    }
    cfg["settings"] = disk;
    save_config(&cfg);
    let mut full = defaults();
    merge(&mut full, &s);
    full
}

fn num(v: &Value, key: &str, section: &str, min: i64, max: i64, fallback: i64) -> i64 {
    v.get(section)
        .and_then(|s| s.get(key))
        .and_then(|x| x.as_i64())
        .map(|x| x.clamp(min, max))
        .unwrap_or(fallback)
}

pub fn warn_days(s: &Value) -> i64 {
    num(s, "warn_days", "quota", 1, 365, 30)
}
pub fn danger_days(s: &Value) -> i64 {
    num(s, "danger_days", "quota", 1, 365, 7)
}
pub fn auto_fetch_min(s: &Value) -> u64 {
    num(s, "auto_fetch_min", "quota", 5, 1440, 30) as u64
}
pub fn notify_alerts(s: &Value) -> bool {
    s.get("general")
        .and_then(|g| g.get("notify_alerts"))
        .and_then(|v| v.as_bool())
        .unwrap_or(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 改别的字段不算"清空 Key"；只有补丁里真带了 ai.key 才算
    #[test]
    fn only_an_explicit_ai_key_field_counts_as_a_clear() {
        assert!(
            key_submitted(&json!({ "ai": { "key": "" } })),
            "用户把 Key 框删空后保存，要能真的清掉"
        );
        assert!(key_submitted(&json!({ "ai": { "key": "sk-x" } })));
        assert!(
            !key_submitted(&json!({ "ai": { "model": "glm-5.3-flash" } })),
            "只改模型名时补丁里没有 key 字段，不能把保管箱那条删掉"
        );
        assert!(!key_submitted(&json!({ "general": { "autostart": true } })));
        assert!(!key_submitted(&json!({})));
    }
}
