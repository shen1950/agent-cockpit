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
        // mgmt_h / mgmt_ratio：左下角「管理」那一截的历史定高键，**已废弃**（2026-10-18）。
        // 那一截现在是固定高度（正好装下 首页/更多/设置 三行，不随窗口变化、也不能拖），没有任何代码读它们。
        // 旧键 nav_h（列表区定高）同理，早废弃了。
        // 侧栏底部与弹层（她 10-25 定的默认与顺序，外观页可自定义）：
        // side_visible = 直接出现在侧栏上的管理项（插在「首页」和「更多」之间，按序）；
        // side_menu = 「··· 更多」弹层里的项（末尾固定接 分界线+「编辑侧导菜单」）；
        // gear_menu = 「设置」弹层里跳的设置页分组。缺键 = 走默认划分，空数组 = 一项都不放。
        "appearance": { "theme": "system", "density": "comfortable", "font_scale": 100, "nav_w": 248, "nav_collapsed": false,
                        "side_visible": [],
                        "side_menu": ["collab", "keys", "proxy", "checkin"],
                        "gear_menu": ["settings", "appearance", "logs", "help"] },
        "quota": { "warn_days": 30, "danger_days": 7, "auto_fetch_min": 30 },
        "proxy": { "port": 8788, "upstream": "https://api.openai.com", "log_keep": 100 },
        // 每日签到：领取是写操作，总开关默认关，开了还要逐家勾（auto 里没写到的就是关）。
        // 提醒是只读的，默认开着——到点把"今天还有几家没领"报一次，一天只报一次，不催第二遍。
        "checkin": {
            "enabled": false,
            "window_start": 6,
            "window_end": 12,
            "auto": {},
            "remind": true,
            "remind_at": "10:05",
            "remind_mute": {}
        },
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

/// 这两张表的键是产品名，值只有「开」这一种写法。深合并只覆盖补丁里出现的键、
/// 从不删键（那是给 general / appearance 这种分区字段留的），所以界面想关掉某一家
/// 只能提交显式 false，落盘前在这里统一抹掉——否则「auto 里没写到的就是关」这条
/// 约定根本做不到，取消勾选的下一家重启就又被磁盘上那条 true 复活（Trae CN，2026-10-21）。
const FLAG_MAPS: &[&[&str]] = &[&["checkin", "auto"], &["checkin", "remind_mute"]];

fn prune_flags(s: &mut Value) {
    for path in FLAG_MAPS {
        if let Some(obj) = flag_map(s, path).and_then(|n| n.as_object_mut()) {
            obj.retain(|_, v| v.as_bool().unwrap_or(false));
        }
    }
}

fn flag_map<'a>(root: &'a mut Value, path: &[&str]) -> Option<&'a mut Value> {
    let mut node = root;
    for key in path {
        node = node.get_mut(*key)?;
    }
    Some(node)
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
    prune_flags(&mut s);
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

    /// 取消勾选一家：界面发的是显式 false，落盘后这家必须真的没了，
    /// 同时不能牵连别家（重启后 auto 表里剩下的就是还开着的）
    #[test]
    fn unchecking_one_product_really_clears_it() {
        let mut s = json!({ "checkin": { "enabled": true, "auto": { "trae-cn": true, "workbuddy": true } } });
        merge(
            &mut s,
            &json!({ "checkin": { "auto": { "trae-cn": false, "workbuddy": true } } }),
        );
        prune_flags(&mut s);
        assert_eq!(s["checkin"]["auto"], json!({ "workbuddy": true }));
    }

    /// 只改总开关时补丁里根本没有 auto：那一张表要原样留着，不能被这次保存清空
    #[test]
    fn a_patch_without_the_map_leaves_it_untouched() {
        let mut s = json!({ "checkin": { "enabled": true, "auto": { "minimax": true, "trae-cn": false } } });
        merge(&mut s, &json!({ "checkin": { "enabled": false } }));
        prune_flags(&mut s);
        assert_eq!(s["checkin"]["enabled"], json!(false));
        assert_eq!(s["checkin"]["auto"], json!({ "minimax": true }));
    }

    #[test]
    fn remind_mute_clears_the_same_way() {
        let mut s = json!({ "checkin": { "remind_mute": { "trae-cn": true } } });
        merge(&mut s, &json!({ "checkin": { "remind_mute": { "trae-cn": false } } }));
        prune_flags(&mut s);
        assert_eq!(s["checkin"]["remind_mute"], json!({}));
    }
}
