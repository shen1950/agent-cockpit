// 本地反代的「上游组」与「下发 Key」：
//   来源（某个 Agent 的某个账号 / API 台账里的某个 Key）→ 归到某个上游组 → 用一个下发 Key 对外开放
// 配置全部存在 config.json 的 proxy 段，纯本机。
use crate::registry::{all_apps, load_config, load_instances, save_config};
use serde_json::{json, Value};

/// 成员来源：instance:<app_id>/<实例名> 或 key:<站点id>/<keyid>
pub fn member_src_instance(app_id: &str, name: &str) -> String {
    format!("instance:{}/{}", app_id, name)
}

pub fn member_src_key(site_id: &str, key_id: &str) -> String {
    format!("key:{}/{}", site_id, key_id)
}

pub fn new_id(prefix: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    format!("{}{:x}{:04x}", prefix, crate::registry::now_secs(), n)
}

/// 生成一个可对外分发的 key：ac- + 32 位十六进制
pub fn gen_dist_key() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let seed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    // 用两次时间戳 + 指针地址做种，够本地分发用；不追求密码学强度
    let mut x = seed as u64 ^ 0x9E37_79B9_7F4A_7C15;
    let mut out = String::from("ac-");
    for _ in 0..32 {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        out.push(char::from_digit((x % 16) as u32, 16).unwrap());
    }
    out
}

fn proxy_cfg(cfg: &Value) -> Value {
    cfg.get("proxy").cloned().unwrap_or_else(|| json!({}))
}

/// 把早期内置的“免费模型 / 接入密钥”名字迁到书面语。
/// 只认精确的旧名字，用户自己起的名字（WB-qoder、沉浸式翻译）一律不动。
/// 迁移后再做同名片去重：历史 bug（Trae 按旧名找池，每轮一键都新建一个）
/// 会堆出一排同名上游组/密钥；成员完全相同就并成一个，引用被并组的密钥重映射。
pub fn migrate_legacy_names() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let mut cfg = load_config();
        let mut changed = false;
        if let Some(proxy) = cfg.get_mut("proxy").and_then(|v| v.as_object_mut()) {
            if let Some(arr) = proxy.get_mut("pools").and_then(|v| v.as_array_mut()) {
                for item in arr.iter_mut() {
                    if let Some(new) = rename_pool(item["name"].as_str().unwrap_or("")) {
                        item["name"] = json!(new);
                        changed = true;
                    }
                }
            }
            if let Some(arr) = proxy.get_mut("dist").and_then(|v| v.as_array_mut()) {
                for item in arr.iter_mut() {
                    if let Some(new) = rename_dist(item["name"].as_str().unwrap_or("")) {
                        item["name"] = json!(new);
                        changed = true;
                    }
                }
            }
            let remap = match proxy.get_mut("pools").and_then(|v| v.as_array_mut()) {
                Some(arr) => dedupe_pools(arr),
                None => Vec::new(),
            };
            if !remap.is_empty() {
                changed = true;
                if let Some(arr) = proxy.get_mut("dist").and_then(|v| v.as_array_mut()) {
                    remap_dist_pools(arr, &remap);
                }
            }
            if let Some(arr) = proxy.get_mut("dist").and_then(|v| v.as_array_mut()) {
                let before = arr.len();
                dedupe_dists(arr);
                if arr.len() != before {
                    changed = true;
                }
            }
        }
        if changed {
            save_config(&cfg);
        }
    });
}

/// 同名且成员 src 集合相同的上游组只留第一条，返回 (被并掉的组id, 保留组id) 映射。
/// 成员不同的同名组保留——可能是用户有意拆的，不去猜。
fn dedupe_pools(arr: &mut Vec<Value>) -> Vec<(String, String)> {
    fn srcs_of(p: &Value) -> Vec<String> {
        p["members"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .iter()
            .filter_map(|m| m["src"].as_str().map(|s| s.to_string()))
            .collect()
    }
    let mut kept: Vec<Value> = Vec::new();
    let mut remap: Vec<(String, String)> = Vec::new();
    for item in arr.iter() {
        let id = item["id"].as_str().unwrap_or("").to_string();
        let name = item["name"].as_str().unwrap_or("").to_string();
        let srcs = srcs_of(item);
        match kept
            .iter()
            .position(|k| k["name"].as_str().unwrap_or("") == name && srcs_of(k) == srcs)
        {
            Some(i) => {
                let kept_id = kept[i]["id"].as_str().unwrap_or("").to_string();
                if !id.is_empty() && !kept_id.is_empty() && id != kept_id {
                    remap.push((id, kept_id));
                }
            }
            None => kept.push(item.clone()),
        }
    }
    let dropped = remap.len();
    *arr = kept;
    let _ = dropped;
    remap
}

/// 把接入密钥授权里的组 id 按映射改写（被并掉的组 → 保留的组）
fn remap_dist_pools(arr: &mut Vec<Value>, remap: &[(String, String)]) {
    for d in arr.iter_mut() {
        if let Some(ps) = d["pools"].as_array_mut() {
            for v in ps.iter_mut() {
                let cur = v.as_str().unwrap_or("");
                if let Some((_, kept_id)) = remap.iter().find(|(dd, _)| dd == cur) {
                    *v = json!(kept_id);
                }
            }
        }
    }
}

/// 同名接入密钥只留调用次数最多的那把（被删过又被一键重建会堆出同名 key）
fn dedupe_dists(arr: &mut Vec<Value>) {
    let mut best: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for (i, d) in arr.iter().enumerate() {
        let name = d["name"].as_str().unwrap_or("").to_string();
        let calls = d["calls"].as_u64().unwrap_or(0);
        let e = best.entry(name).or_insert(i);
        if arr[*e]["calls"].as_u64().unwrap_or(0) < calls {
            *e = i;
        }
    }
    let mut out = Vec::new();
    for (i, d) in arr.iter().enumerate() {
        let name = d["name"].as_str().unwrap_or("").to_string();
        if best.get(&name) == Some(&i) {
            out.push(d.clone());
        }
    }
    *arr = out;
}

fn rename_pool(name: &str) -> Option<&'static str> {
    match name {
        "Qoder 免费模型" => Some("Qoder 上游组"),
        "WorkBuddy 免费模型" => Some("WorkBuddy 上游组"),
        "OpenCode 免费模型" => Some("OpenCode 上游组"),
        "Trae 模型" => Some("Trae 上游组"),
        _ => None,
    }
}

fn rename_dist(name: &str) -> Option<&'static str> {
    match name {
        "Qoder 免费（自用）" => Some("Qoder 接入"),
        "WorkBuddy 免费（自用）" => Some("WorkBuddy 接入"),
        "OpenCode 免费（自用）" => Some("OpenCode 接入"),
        "Trae 自用" => Some("Trae 接入"),
        _ => None,
    }
}

pub fn pools() -> Vec<Value> {
    proxy_cfg(&load_config())
        .get("pools")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default()
}

pub fn dist_keys() -> Vec<Value> {
    proxy_cfg(&load_config())
        .get("dist")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default()
}

/// 写回 proxy 段（保留 upstream / strategy 等既有字段）
pub fn save_proxy_field(field: &str, value: Value) {
    let mut cfg = load_config();
    let obj = match cfg.as_object_mut() {
        Some(o) => o,
        None => return,
    };
    let mut proxy = obj.get("proxy").cloned().unwrap_or_else(|| json!({}));
    if let Some(p) = proxy.as_object_mut() {
        p.insert(field.to_string(), value);
    } else {
        proxy = json!({ field: value });
    }
    obj.insert("proxy".into(), proxy);
    save_config(&cfg);
}

/// 一个可用的上游：label 用于日志，token 替换 Authorization，base_url 是转发目标
#[derive(Debug, Clone, PartialEq)]
pub struct Upstream {
    pub label: String,
    pub token: String,
    pub base_url: String,
    pub pool: String,
}

// ───────────────────────── 上游组增删改 ─────────────────────────

/// 把界面传来的成员数组规整成存储格式：去空、去重（同 src 只留一条）、补默认字段
fn norm_members(v: &Value) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    for m in v.as_array().cloned().unwrap_or_default() {
        let src = m["src"].as_str().unwrap_or("").trim().to_string();
        if src.is_empty() {
            continue;
        }
        if out.iter().any(|x| x["src"].as_str().unwrap_or("") == src) {
            continue;
        }
        out.push(json!({
            "src": src,
            "base_url": m["base_url"].as_str().unwrap_or("").trim(),
            "enabled": m["enabled"].as_bool().unwrap_or(true),
        }));
    }
    out
}

/// 建/改上游组。members 里同一 src 去重；成员只保留存在性由 pool_upstreams 实时算。
pub fn upsert_pool(item: &Value) -> (bool, String) {
    let name = item["name"].as_str().unwrap_or("").trim().to_string();
    if name.is_empty() {
        return (false, "上游组名不能为空".into());
    }
    let members = norm_members(&item["members"]);
    let id = item["id"].as_str().unwrap_or("").trim().to_string();
    let is_new = id.is_empty();
    let pid = if is_new { new_id("gp") } else { id };
    let obj = json!({
        "id": pid,
        "name": name,
        "enabled": item["enabled"].as_bool().unwrap_or(true),
        "members": members,
    });
    let mut list = pools();
    let mut hit = false;
    for p in list.iter_mut() {
        if p["id"].as_str().unwrap_or("") == pid {
            *p = obj.clone();
            hit = true;
        }
    }
    if !hit {
        list.push(obj.clone());
    }
    save_proxy_field("pools", json!(list));
    let n = obj["members"].as_array().map(|a| a.len()).unwrap_or(0);
    if is_new {
        (true, format!("已建上游组「{}」，{} 个成员", name, n))
    } else {
        (true, format!("已保存上游组「{}」，{} 个成员", name, n))
    }
}

/// 删上游组，同时把引用它的分发 key 里的悬空 id 摘掉
pub fn remove_pool(id: &str) -> (bool, String) {
    let mut list = pools();
    let before = list.len();
    let name = list
        .iter()
        .find(|p| p["id"].as_str().unwrap_or("") == id)
        .and_then(|p| p["name"].as_str().map(|s| s.to_string()))
        .unwrap_or_default();
    list.retain(|p| p["id"].as_str().unwrap_or("") != id);
    if list.len() == before {
        return (false, "没找到这个上游组".into());
    }
    save_proxy_field("pools", json!(list));

    let mut dist = dist_keys();
    let mut touched = 0;
    for d in dist.iter_mut() {
        if let Some(arr) = d["pools"].as_array_mut() {
            let n0 = arr.len();
            arr.retain(|v| v.as_str().unwrap_or("") != id);
            if arr.len() != n0 {
                touched += 1;
            }
        }
    }
    if touched > 0 {
        save_proxy_field("dist", json!(dist));
    }
    (
        true,
        format!("已删除上游组「{}」，并同步更新了 {} 个接入密钥 的授权", name, touched),
    )
}

/// 上游组视图：带上「实际可用成员数」与成员标签，界面直接显示
pub fn pools_view(global_upstream: &str) -> Vec<Value> {
    pools()
        .into_iter()
        .map(|mut p| {
            let ups = pool_upstreams(&p, global_upstream);
            p["live"] = json!(ups.len());
            p["labels"] = json!(ups.iter().map(|u| u.label.clone()).collect::<Vec<_>>());
            p
        })
        .collect()
}

// ───────────────────────── 接入密钥 增删改 ─────────────────────────

/// 接入密钥 列表（打码）。完整 key 用 reveal_dist 单取。
pub fn list_dist_masked() -> Vec<Value> {
    dist_keys()
        .into_iter()
        .map(|mut d| {
            let k = d["key"].as_str().unwrap_or("").to_string();
            d["masked"] = json!(crate::keys::mask(&k));
            d["calls"] = json!(d["calls"].as_u64().unwrap_or(0));
            d["last_used"] = json!(d["last_used"].as_u64().unwrap_or(0));
            if let Some(o) = d.as_object_mut() {
                o.remove("key"); // 明文不在列表里出现，点「显示」再用 reveal_dist 单取
            }
            d
        })
        .collect()
}

/// 单个接入密钥 的明文（本地工具，点了才取）
pub fn reveal_dist(id: &str) -> Option<String> {
    dist_keys()
        .into_iter()
        .find(|d| d["id"].as_str().unwrap_or("") == id)
        .and_then(|d| d["key"].as_str().map(|s| s.to_string()))
}

/// 建/改接入密钥。新建时返回明文 key 一并提示（只在这里出现一次也够用）。
pub fn upsert_dist(item: &Value) -> (bool, String) {
    let name = item["name"].as_str().unwrap_or("").trim().to_string();
    if name.is_empty() {
        return (false, "给它起个名字（谁在用），日后好停用".into());
    }
    let sel: Vec<String> = item["pools"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter_map(|v| v.as_str().map(|s| s.to_string()))
        .collect();
    let scope = if sel.is_empty() {
        "全部上游组".to_string()
    } else {
        format!("{} 个上游组", sel.len())
    };
    let id = item["id"].as_str().unwrap_or("").trim().to_string();
    let mut list = dist_keys();

    if id.is_empty() {
        let key = gen_dist_key();
        list.push(json!({
            "id": new_id("dk"),
            "key": key,
            "name": name,
            "pools": sel,
            "enabled": true,
            "calls": 0,
            "last_used": 0,
            "last_account": "",
            "created": crate::registry::now_secs(),
        }));
        save_proxy_field("dist", json!(list));
        return (
            true,
            format!("已生成接入密钥（{}）：{} —— 填到客户端当 API Key", scope, key),
        );
    }

    let mut hit = false;
    for d in list.iter_mut() {
        if d["id"].as_str().unwrap_or("") == id {
            d["name"] = json!(name);
            d["pools"] = json!(sel);
            if let Some(e) = item["enabled"].as_bool() {
                d["enabled"] = json!(e);
            }
            hit = true;
        }
    }
    if !hit {
        return (false, "没找到这条分发记录".into());
    }
    save_proxy_field("dist", json!(list));
    (true, format!("已保存接入密钥「{}」（授权 {}）", name, scope))
}

pub fn remove_dist(id: &str) -> (bool, String) {
    let mut list = dist_keys();
    let before = list.len();
    let name = list
        .iter()
        .find(|d| d["id"].as_str().unwrap_or("") == id)
        .and_then(|d| d["name"].as_str().map(|s| s.to_string()))
        .unwrap_or_default();
    list.retain(|d| d["id"].as_str().unwrap_or("") != id);
    if list.len() == before {
        return (false, "没找到这条分发记录".into());
    }
    save_proxy_field("dist", json!(list));
    (true, format!("已删除接入密钥「{}」，正在用它的人会立刻被拒", name))
}

/// 一键停用/启用（不改名字和授权范围）
pub fn toggle_dist(id: &str) -> (bool, String) {
    let mut list = dist_keys();
    let mut out: Option<(String, bool)> = None;
    for d in list.iter_mut() {
        if d["id"].as_str().unwrap_or("") == id {
            let now = !d["enabled"].as_bool().unwrap_or(true);
            d["enabled"] = json!(now);
            out = Some((d["name"].as_str().unwrap_or("").to_string(), now));
        }
    }
    match out {
        Some((name, on)) => {
            save_proxy_field("dist", json!(list));
            (true, format!("「{}」已{}", name, if on { "启用" } else { "停用" }))
        }
        None => (false, "没找到这条分发记录".into()),
    }
}

/// 单个成员解析成上游；来源失效（账号被删 / key 被删 / token 为空）返回 None
fn resolve_member(src: &str, member_base: &str, global_upstream: &str) -> Option<(String, String, String)> {
    if let Some(rest) = src.strip_prefix("instance:") {
        let (app_id, name) = rest.split_once('/')?;
        let app_name = all_apps()
            .into_iter()
            .find(|a| a.id == app_id)
            .map(|a| a.name)
            .unwrap_or_else(|| app_id.to_string());
        let meta = load_instances(app_id).into_iter().find(|m| {
            m["name"].as_str().unwrap_or("") == name
        })?;
        let mut tok = meta["token"].as_str().unwrap_or("").trim().to_string();
        // Qoder 系的令牌不在账号记录里，而在产品自己的 auth.v1.dat（见 qproxy）。
        // Trae 的在 storage.json（见 traeproxy）。
        // AStudio 的模型令牌在 astron-session.json 的 modelBearerToken（见 asproxy），
        // 账号记录里那个 token 是 SSO 会话令牌，不是模型 Bearer，不能顶替。
        // 这里读不到就说明这个号没登录 / 档案身份不符 —— 如实返回 None，不拿别人的号顶替。
        if crate::asproxy::APPS.contains(&app_id) {
            tok = crate::asproxy::token();
        } else if tok.is_empty() {
            tok = if crate::traeproxy::APPS.contains(&app_id) {
                crate::traeproxy::token_for(app_id, name).unwrap_or_default()
            } else if crate::wbproxy::APPS.contains(&app_id) {
                crate::wbproxy::token_for(app_id, name).unwrap_or_default()
            } else {
                crate::qproxy::token_for(app_id, name).unwrap_or_default()
            };
        }
        if tok.is_empty() {
            return None;
        }
        // 实例自带的上游优先，其次各家自己的网关，最后才是全局上游
        let own = meta["proxy_base_url"].as_str().unwrap_or("").trim().to_string();
        let tbase = if crate::traeproxy::APPS.contains(&app_id) {
            crate::traeproxy::BASE_MARK
        } else {
            ""
        };
        let wbase = if crate::wbproxy::APPS.contains(&app_id) {
            crate::wbproxy::base_mark(crate::wbproxy::region_of(app_id).unwrap_or("cn"))
        } else {
            String::new()
        };
        let qbase = crate::qproxy::region_of(app_id)
            .map(crate::qproxy::model_base)
            .unwrap_or("");
        // AStudio 的模型网关（不带 /v1，客户端发的 /v1/chat/completions 原样接上）
        let abase = if crate::asproxy::APPS.contains(&app_id) {
            crate::asproxy::GATEWAY_BASE.to_string()
        } else {
            String::new()
        };
        let base = if !member_base.trim().is_empty() {
            member_base.trim().to_string()
        } else if !own.is_empty() {
            own
        } else if !abase.is_empty() {
            abase
        } else if !tbase.is_empty() {
            tbase.to_string()
        } else if !wbase.is_empty() {
            wbase.to_string()
        } else if !qbase.is_empty() {
            qbase.to_string()
        } else {
            global_upstream.to_string()
        };
        Some((format!("{}/{}", app_name, name), tok, base))
    } else if let Some(rest) = src.strip_prefix("bridge:") {
        // bridge:qoder-cn/<账号> —— Qoder CN 的本地桥（官方运行时 + OpenAI 兼容外壳）。
        // 桥没在跑就没有上游，如实返回 None，别让上游组拿一个连不上的地址。
        let (app_id, name) = rest.split_once('/')?;
        if app_id != "qoder-cn" || !crate::qproxy::cn_bridge_running() {
            return None;
        }
        let app_name = all_apps()
            .into_iter()
            .find(|a| a.id == app_id)
            .map(|a| a.name)
            .unwrap_or_else(|| app_id.to_string());
        Some((
            format!("本地桥/{}/{}", app_name, name),
            crate::qproxy::CN_KEY.to_string(),
            format!("http://127.0.0.1:{}/v1", crate::qproxy::CN_PORT),
        ))
    } else if let Some(rest) = src.strip_prefix("key:") {
        let (site_id, key_id) = rest.split_once('/')?;
        for site in crate::keys::sites() {
            if site["id"].as_str().unwrap_or("") != site_id {
                continue;
            }
            let site_name = site["name"].as_str().unwrap_or("").to_string();
            let site_base = site["base_url"].as_str().unwrap_or("").to_string();
            if let Some(ks) = site["keys"].as_array() {
                for k in ks {
                    if k["id"].as_str().unwrap_or("") != key_id {
                        continue;
                    }
                    let tok = k["key"].as_str().unwrap_or("").trim().to_string();
                    if tok.is_empty() {
                        return None;
                    }
                    let label = k["label"].as_str().unwrap_or("").trim().to_string();
                    let name = if label.is_empty() {
                        site_name.clone()
                    } else {
                        format!("{}/{}", site_name, label)
                    };
                    let base = if !member_base.trim().is_empty() {
                        member_base.trim().to_string()
                    } else {
                        crate::keys::norm_base(&site_base)
                    };
                    return Some((format!("Key/{}", name), tok, base));
                }
            }
            return None;
        }
        None
    } else {
        None
    }
}

/// 把一个上游组展开成可用上游列表
pub fn pool_upstreams(pool: &Value, global_upstream: &str) -> Vec<Upstream> {
    let pool_name = pool["name"].as_str().unwrap_or("未命名池").to_string();
    let mut out = Vec::new();
    for m in pool["members"].as_array().cloned().unwrap_or_default() {
        if !m["enabled"].as_bool().unwrap_or(true) {
            continue;
        }
        let src = m["src"].as_str().unwrap_or("");
        let base = m["base_url"].as_str().unwrap_or("");
        if let Some((label, token, base_url)) = resolve_member(src, base, global_upstream) {
            out.push(Upstream {
                label,
                token,
                base_url,
                pool: pool_name.clone(),
            });
        }
    }
    out
}

/// 指定上游组（按 id）里的可用上游；ids 为空 = 全部启用的上游组
pub fn upstreams_for(ids: &[String], global_upstream: &str) -> Vec<Upstream> {
    let all = pools();
    let mut out = Vec::new();
    for p in &all {
        if !p["enabled"].as_bool().unwrap_or(true) {
            continue;
        }
        if !ids.is_empty() {
            let pid = p["id"].as_str().unwrap_or("");
            if !ids.iter().any(|x| x == pid) {
                continue;
            }
        }
        out.extend(pool_upstreams(p, global_upstream));
    }
    out
}

/// 按分发的 key 找条目（含停用的，便于告知"这个 key 被停用了"）
pub fn find_dist(token: &str) -> Option<Value> {
    let t = token.trim();
    if t.is_empty() {
        return None;
    }
    dist_keys()
        .into_iter()
        .find(|d| d["key"].as_str().unwrap_or("") == t)
}

/// 给日志/统计用：记一次调用
pub fn bump_dist(id: &str, account: &str) {
    let mut list = dist_keys();
    let mut hit = false;
    for d in list.iter_mut() {
        if d["id"].as_str().unwrap_or("") == id {
            let calls = d["calls"].as_u64().unwrap_or(0) + 1;
            d["calls"] = json!(calls);
            d["last_used"] = json!(crate::registry::now_secs());
            d["last_account"] = json!(account);
            hit = true;
        }
    }
    if hit {
        save_proxy_field("dist", json!(list));
    }
}

/// 可选的成员清单（给界面做勾选用）：按 Agent 分组列账号，再列 API 台账里的 key
pub fn member_options() -> Value {
    let mut instances = Vec::new();
    for app in all_apps() {
        for m in load_instances(&app.id) {
            let name = m["name"].as_str().unwrap_or("").to_string();
            let tok = m["token"].as_str().unwrap_or("").trim().to_string();
            // Qoder 系的令牌在产品自己的档案里（qproxy 读），这里按"反代真的能不能用"报
            let usable = !tok.is_empty()
                || (crate::qproxy::region_of(&app.id).is_some()
                    && crate::qproxy::token_for(&app.id, &name).is_some())
                || (crate::traeproxy::APPS.contains(&app.id.as_str())
                    && crate::traeproxy::token_for(&app.id, &name).is_some())
                || (crate::asproxy::APPS.contains(&app.id.as_str())
                    && !crate::asproxy::token().is_empty());
            instances.push(json!({
                "src": member_src_instance(&app.id, &name),
                "app": app.id,
                "app_name": app.name,
                "instance": name,
                "has_token": usable,
                "proxy_base_url": m["proxy_base_url"].as_str().unwrap_or(""),
                "qregion": crate::qproxy::region_of(&app.id).unwrap_or(""),
                "qstate": crate::qproxy::stored_state(&app.id, &name).unwrap_or_default(),
                "trae": crate::traeproxy::APPS.contains(&app.id.as_str()),
                "astudio": crate::asproxy::APPS.contains(&app.id.as_str()),
            }));
            // Qoder CN：CLI 登录过就能挂本地桥（国内版唯一通路，见 qproxy.rs）
            if app.id == "qoder-cn" && crate::qproxy::cn_logged_in(&name) {
                let up = crate::qproxy::cn_bridge_running();
                instances.push(json!({
                    "src": format!("bridge:{}/{}", app.id, name),
                    "app": app.id,
                    "app_name": app.name,
                    "instance": name,
                    "has_token": up,
                    "proxy_base_url": format!("http://127.0.0.1:{}/v1", crate::qproxy::CN_PORT),
                    "qregion": "cn",
                    "qstate": if up { "bridge" } else { "bridge_idle" },
                    "bridge": true,
                }));
            }
        }
    }
    let mut keys = Vec::new();
    for site in crate::keys::sites() {
        let sid = site["id"].as_str().unwrap_or("").to_string();
        let sname = site["name"].as_str().unwrap_or("").to_string();
        let sbase = site["base_url"].as_str().unwrap_or("").to_string();
        for k in site["keys"].as_array().cloned().unwrap_or_default() {
            let kid = k["id"].as_str().unwrap_or("").to_string();
            keys.push(json!({
                "src": member_src_key(&sid, &kid),
                "site_id": sid,
                "site_name": sname,
                "base_url": sbase,
                "label": k["label"].as_str().unwrap_or(""),
                "masked": crate::keys::mask(k["key"].as_str().unwrap_or("")),
            }));
        }
    }
    json!({ "instances": instances, "keys": keys })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dist_key_shape_is_stable() {
        let k = gen_dist_key();
        assert!(k.starts_with("ac-"));
        assert_eq!(k.len(), 3 + 32);
        assert!(k[3..].chars().all(|c| c.is_ascii_hexdigit()));
        // 两次生成不该一样
        let mut seen = std::collections::HashSet::new();
        for _ in 0..20 {
            assert!(seen.insert(gen_dist_key()), "生成的 key 重复了");
        }
    }

    #[test]
    fn member_src_roundtrip() {
        let s = member_src_instance("qoder-cn", "账号1");
        assert_eq!(s, "instance:qoder-cn/账号1");
        let k = member_src_key("p-1", "k-2");
        assert_eq!(k, "key:p-1/k-2");
    }

    /// 成员规整：去空、同 src 去重（保留先出现的那条）、首尾空白剥掉、缺字段补默认
    #[test]
    fn members_are_deduped_and_trimmed() {
        let v = json!([
            { "src": " instance:a/b ", "enabled": true },
            { "src": "instance:a/b", "enabled": false },   // 重复 → 丢
            { "src": "   ", "enabled": true },             // 空 → 丢
            { "src": "key:s/k", "base_url": " https://x/v1 ", "enabled": false },
            { "src": "key:s/k2" },                         // 缺 enabled → 默认 true
        ]);
        let m = norm_members(&v);
        assert_eq!(m.len(), 3);
        assert_eq!(m[0]["src"], "instance:a/b");
        assert_eq!(m[0]["enabled"], true, "重复时应保留先出现的那条");
        assert_eq!(m[1]["src"], "key:s/k");
        assert_eq!(m[1]["base_url"], "https://x/v1", "base_url 应剥掉首尾空白");
        assert_eq!(m[1]["enabled"], false);
        assert_eq!(m[2]["enabled"], true, "缺 enabled 时默认启用");

        assert!(norm_members(&json!(null)).is_empty());
    }

    /// 停用/失效的成员不进池；成员自带 base_url 优先于全局
    #[test]
    fn pool_upstreams_filters_and_prefers_member_base() {
        let pool = json!({
            "id": "gp-1", "name": "测试池", "enabled": true,
            "members": [
                { "src": "instance:no-such-app/x", "enabled": true },
                { "src": "instance:also-missing/y", "enabled": false },
                { "src": "bogus", "enabled": true },
            ]
        });
        // 三个成员都无法解析（前两个不存在、第三个格式不对）→ 空池
        assert!(pool_upstreams(&pool, "https://api.openai.com").is_empty());

        // 全是停用成员 → 同样不产出
        let all_off = json!({
            "id": "gp-2", "name": "停用池", "enabled": true,
            "members": [{ "src": "instance:a/b", "enabled": false }]
        });
        assert!(pool_upstreams(&all_off, "https://x").is_empty());
    }

    /// 真机往返：上游组 + 接入密钥 的增删改串到一起跑一遍，并验证它真的能改变反代的路由。
    /// 会临时改写 ~/.agent-cockpit/config.json，结束时（含 panic/断言失败）原样还原。
    /// 默认不跑：`cargo test -- --include-ignored`
    #[test]
    #[ignore]
    fn real_roundtrip_pools_and_dist_routing() {
        // 现场保管：无论断言怎么失败，Drop 时都会把 config.json 写回去
        struct CfgGuard(Option<String>, std::path::PathBuf);
        impl Drop for CfgGuard {
            fn drop(&mut self) {
                match &self.0 {
                    Some(s) => {
                        let _ = std::fs::write(&self.1, s);
                    }
                    None => {
                        let _ = std::fs::remove_file(&self.1);
                    }
                }
            }
        }
        let path = crate::registry::config_file();
        let _guard = CfgGuard(std::fs::read_to_string(&path).ok(), path);

        // 建上游组：成员指向不存在的账号，保证不会真把请求发出去
        let (ok, msg) = upsert_pool(&json!({
            "name": "__gw_test_pool",
            "members": [
                { "src": "instance:__t/__x" },
                { "src": "instance:__t/__x" },   // 重复，应被去掉
                { "src": "" },                   // 空，应被去掉
            ],
        }));
        assert!(ok, "{}", msg);
        let p = pools()
            .into_iter()
            .find(|p| p["name"] == "__gw_test_pool")
            .expect("上游组没落盘");
        let pid = p["id"].as_str().unwrap().to_string();
        assert_eq!(p["members"].as_array().unwrap().len(), 1, "成员应已去重去空");

        // 视图里 可用 0 / 勾了 1 —— 成员解析不出来要如实报 0，不能假装可用
        let view = pools_view("https://upstream.invalid");
        let pv = view.iter().find(|x| x["id"] == pid.as_str()).unwrap();
        assert_eq!(pv["live"], 0);
        assert_eq!(pv["members"].as_array().unwrap().len(), 1);

        // 生成接入密钥：明文只该在这次返回值里出现
        let (ok, msg) = upsert_dist(&json!({ "name": "__gw_test_dist", "pools": [pid] }));
        assert!(ok, "{}", msg);
        let key = msg
            .split('：')
            .nth(1)
            .and_then(|s| s.split_whitespace().next())
            .unwrap_or("")
            .to_string();
        assert!(key.starts_with("ac-") && key.len() == 35, "msg 里应带回明文 key：{}", msg);

        let d = find_dist(&key).expect("按 key 找不到分发记录");
        let did = d["id"].as_str().unwrap().to_string();
        assert_eq!(d["name"], "__gw_test_dist");
        assert_eq!(d["enabled"], true);

        // 列表里不该出现明文
        let masked = list_dist_masked();
        let md = masked.iter().find(|x| x["id"] == did.as_str()).unwrap();
        assert!(md.get("key").is_none(), "列表必须打码，不能带明文");
        assert_eq!(reveal_dist(&did).as_deref(), Some(key.as_str()));

        // 路由：带这个 key → 只走它授权的上游组（并把命中的 key 名带回日志）
        let r = crate::proxy::pick_upstreams(&key, "https://upstream.invalid");
        assert_eq!(r.dist_id, did);
        assert!(!r.disabled);
        assert!(r.ups.is_empty(), "池里只有一个解析不出来的成员，上游应为空");

        // 停用 → 立刻拒
        assert!(toggle_dist(&did).0);
        let r = crate::proxy::pick_upstreams(&key, "https://upstream.invalid");
        assert!(r.disabled, "停用后应被标记为拒绝");
        assert!(toggle_dist(&did).0, "再次切换应恢复启用");

        // 删上游组 → 授权里的悬空 id 要同步摘掉，且接入密钥 本身还在
        assert!(remove_pool(&pid).0);
        let d2 = find_dist(&key).expect("分发记录不该被一起删掉");
        assert!(d2["pools"].as_array().unwrap().is_empty(), "悬空池 id 应被摘掉");

        // 删接入密钥 → 这个 key 立刻不认识
        assert!(remove_dist(&did).0);
        assert!(find_dist(&key).is_none());

        // 收尾：上游组里不该还有测试残留
        assert!(!pools().iter().any(|x| x["name"] == "__gw_test_pool"));
        assert!(!dist_keys().iter().any(|x| x["name"] == "__gw_test_dist"));
    }
}
