// 「这台机器上某个产品现在登的是谁」——产品级的一份真值。
//
// 以前只有账号卡片上那个 quota_user_id，于是出现这样的错：人在客户端里换成了
// 另一个号，卡片还挂着旧号的余额，刷新半天数值不动（读数被 userId 守卫挡回去），
// 而界面上没有任何地方说明"现在登的是谁"。
// 判据得换一层：本机当前登录是一等事实，卡片上的数属于谁是另一回事，两者要分开显示。
//
// 读一次要走 agenthub 子进程（约 1 秒，还要打服务端），所以不能放在每次界面刷新里，
// 改成后台按 TTL 刷、界面只拿缓存。
use crate::local_quota;
use crate::registry;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// 缓存多久算旧：45 秒——比界面 4 秒一轮慢得多，又能在"刚换完号"时自己追上来
pub const TTL: u64 = 45;

static CACHE: std::sync::OnceLock<Mutex<HashMap<String, (Instant, Value)>>> =
    std::sync::OnceLock::new();
static BUSY: std::sync::OnceLock<Mutex<HashSet<String>>> = std::sync::OnceLock::new();

fn busy() -> &'static Mutex<HashSet<String>> {
    BUSY.get_or_init(|| Mutex::new(HashSet::new()))
}

fn cache() -> &'static Mutex<HashMap<String, (Instant, Value)>> {
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn put(app_id: &str, v: Value) {
    if let Ok(mut c) = cache().lock() {
        c.insert(app_id.to_string(), (Instant::now(), v));
    }
}

/// 真读一次：走该产品当前的默认档案目录（换号后它就是现在挂着的那个账号）。
pub fn read(app_id: &str) -> Value {
    let store = match local_quota::store_for(app_id) {
        Some(s) => s,
        None => {
            // 把原因说明白：不是"没登录成功"，是这个产品根本没登记直读通道——
            // 早先只回 unsupported 没有话术，向导里落成一句没头没尾的"读不到登录状态"
            // OpenCode 例外（2026-10-18）：不是没登记通道，是这台机器上压根没有账号——
            // ~/.local/share/opencode/opencode.db 的 account 三表实测 0 行，厂商也没有
            // 余额接口。generic 那句"没登记通道"会暗示"以后会支持"，那是空头支票，照实说。
            if app_id == "opencode" {
                return json!({
                    "ok": false, "unsupported": true, "app": app_id,
                    "msg": "OpenCode 在这台机器上没有账号可读：它的账号表实测 0 行（没登录过），\
                           厂商也没有余额接口。登录态读不到不是你操作错了——先用它在官网/客户端\
                           登录一次，再回来读。",
                });
            }
            return json!({
                "ok": false, "unsupported": true, "app": app_id,
                "msg": "这个程序没有登记本机直读通道，登录态读不了（不是你没登录成功）",
            });
        }
    };
    let dir = match registry::profile_dir(app_id) {
        Some(d) => d,
        None => {
            return json!({
                "ok": false, "app": app_id,
                "msg": "这个程序没登记档案目录，读不到它现在登的是谁"
            });
        }
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    match local_quota::fetch(store, Some(dir.to_string_lossy().as_ref())) {
        Ok(q) => {
            // 认出现在登的是谁，就顺手把这份登录态收进保管箱：她手动登录那一次是最后一次，
            // 以后切回来是一键（见 authpack.rs）
            crate::authpack::capture_current(app_id, &q.account, &q.user_id);
            json!({
                "ok": true, "app": app_id, "at": now,
                "account": q.account, "user_id": q.user_id, "user_type": q.user_type,
                "left": q.left, "total": q.total, "expires_on": q.expires_on,
                "source": q.source, "stale": q.is_stale(), "live_error": q.live_error,
                "auth_from": q.auth_from,
                // 只有登录身份、没有余额接口的店（MiMo/Antigravity/Coze）：
                // 向导据此说"只认到了登录身份"，而不是"剩 0 / 共 0"
                "account_only": q.is_account_only(),
                // 逐笔积分也带上：向导在建卡之前就要把额度摊给人看，那时还没有卡片可写
                "portions": q.portions,
                "profile_dir": dir.to_string_lossy(),
                "usage_only": q.is_usage_only(),
            })
        }
        Err(e) => json!({
            "ok": false, "app": app_id, "at": now, "err": e,
            "profile_dir": dir.to_string_lossy(),
        }),
    }
}

/// 后台刷一次，成功失败都写进缓存；同一产品同时只允许一个在跑。
fn spawn_refresh(app_id: &str) {
    let key = app_id.to_string();
    {
        let Ok(mut b) = busy().lock() else { return };
        if !b.insert(key.clone()) {
            return;
        }
    }
    let id = key.clone();
    std::thread::spawn(move || {
        let v = read(&id);
        put(&id, v);
        if let Ok(mut b) = busy().lock() {
            b.remove(&id);
        }
    });
}

/// 界面每轮都调这个：只拿缓存，过期就顺手叫后台去刷，绝不在这儿等网络。
pub fn current(app_id: &str) -> Value {
    let fresh = cache()
        .lock()
        .ok()
        .and_then(|c| {
            c.get(app_id).map(|(t, v)| {
                if t.elapsed() < Duration::from_secs(TTL) {
                    (v.clone(), true)
                } else {
                    (v.clone(), false)
                }
            })
        })
        .unwrap_or_else(|| (Value::Null, false));
    if fresh.1 {
        return json!({ "known": true, "refreshing": false, "value": fresh.0 });
    }
    spawn_refresh(app_id);
    json!({
        "known": !fresh.0.is_null(),
        "refreshing": true,
        "value": if fresh.0.is_null() { json!({}) } else { fresh.0 },
    })
}

/// 手动读数（点「读余额」、向导里等登录完成时）：等结果出来再返回，别拿旧缓存糊。
pub fn refresh_now(app_id: &str) -> Value {
    let v = read(app_id);
    put(app_id, v.clone());
    v
}

/// 换号写完登录态后把缓存清掉：不清的话界面最长 45 秒还停在上一个号，
/// 看起来就像"切了没生效"。
pub fn invalidate(app_id: &str) {
    if let Ok(mut c) = cache().lock() {
        c.remove(app_id);
    }
}

/// 这张卡的数到底是不是本机当前登录那个号的。
/// 没读到登录态（unknown）时不能判"不一致"，否则没装 agenthub 的机器上每张卡都报错。
pub fn mismatch(card_user_id: &str, machine: &Value) -> bool {
    if card_user_id.is_empty() || !machine["ok"].as_bool().unwrap_or(false) {
        return false;
    }
    let uid = machine["user_id"].as_str().unwrap_or("");
    !uid.is_empty() && uid != card_user_id
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_machine_login_never_counts_as_mismatch() {
        assert!(!mismatch("u-1", &json!({})));
        assert!(!mismatch("u-1", &json!({"ok": false, "err": "没装 agenthub"})));
        assert!(!mismatch("", &json!({"ok": true, "user_id": "u-2"})));
    }

    #[test]
    fn a_different_logged_in_account_is_a_mismatch() {
        assert!(mismatch("u-1", &json!({"ok": true, "user_id": "u-2"})));
        assert!(!mismatch("u-1", &json!({"ok": true, "user_id": "u-1"})));
    }
}
