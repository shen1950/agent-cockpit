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

fn stable_provider_id(src: &str) -> String {
    format!("pv-{}", crate::wbproxy::hex(&crate::wbproxy::sha256(src.as_bytes())))
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

/// 用户手动停用的来源（Magpie 式开关）：停用后所有上游组与模型目录立即排除它。
/// 与成员在不在某个组里无关，这是"这个来源参与不参与网关"的总闸。
pub fn sources_off() -> Vec<String> {
    let proxy = proxy_cfg(&load_config());
    let mut off = string_list(&proxy["sources_off"]);
    // A removed provider remains blocked even if a legacy action rewrites sources_off.
    for src in string_list(&proxy["providers_removed"]) {
        if !off.contains(&src) { off.push(src); }
    }
    off
}

fn source_enabled(src: &str, off: &[String]) -> bool {
    !off.iter().any(|s| s == src)
}

/// 来源级开关：账号或台账 Key 一键停用/启用，写回 sources_off 列表
pub fn toggle_source(src: &str) -> (bool, String) {
    let src = src.trim();
    if src.is_empty() {
        return (false, "缺少来源标识".into());
    }
    if string_list(&proxy_cfg(&load_config())["providers_removed"]).iter().any(|s| s == src) {
        return (false, "这个供应商已删除，请先在供应商页重新添加".into());
    }
    let mut off = sources_off();
    let label = resolve_member(src, "", "")
        .map(|(label, _, _)| label)
        .unwrap_or_else(|| src.to_string());
    let now_off = if let Some(i) = off.iter().position(|s| s == src) {
        off.remove(i);
        false
    } else {
        off.push(src.to_string());
        true
    };
    save_proxy_field("sources_off", json!(off));
    (
        true,
        if now_off {
            format!("已停用「{label}」：网关不再使用这个来源，模型目录与所有上游组同步生效")
        } else {
            format!("已启用「{label}」：这个来源重新参与网关分流")
        },
    )
}

/// Magpie 式供应商视图：一个账号、一条台账 Key 或一个本地桥就是一行供应商。
/// 每行绑定一个隐式上游组，密钥授权仍然沿用原来的组授权模型，不迁移旧数据。
pub fn provider_options() -> Value {
    provider_rows(&member_options(), &proxy_cfg(&load_config()))
}

fn string_list(value: &Value) -> Vec<String> {
    value.as_array().into_iter().flatten().filter_map(|v| v.as_str().map(str::to_owned)).collect()
}

fn provider_rows(members: &Value, proxy: &Value) -> Value {
    // 显式标注：rows 只在闭包里被 push，闭包里的 `r["src"]` 会推不出类型（E0282）
    let mut rows: Vec<Value> = Vec::new();
    let removed = string_list(&proxy["providers_removed"]);
    let off = string_list(&proxy["sources_off"]);
    let pools = proxy["pools"].as_array().into_iter().flatten().collect::<Vec<_>>();
    let assigned: std::collections::HashSet<&str> = pools.iter()
        .flat_map(|p| p["members"].as_array().into_iter().flatten()).filter_map(|m| m["src"].as_str())
        .chain(proxy["dist"].as_array().into_iter().flatten()
            .flat_map(|d| d["pools"].as_array().into_iter().flatten()).filter_map(Value::as_str)).collect();
    let mut rows_from = |values: &[Value]| {
        for m in values {
            let src = m["src"].as_str().unwrap_or_default().to_string();
            if src.is_empty() || removed.contains(&src) || rows.iter().any(|r| r["src"] == src) {
                continue;
            }
            if !assigned.contains(src.as_str()) && proxy["provider_names"][&src].is_null()
                && !off.contains(&src) && !(pools.is_empty() && m["legacy"] == true) {
                continue;
            }
            let app = m["app_name"].as_str().unwrap_or("供应商").to_string();
            let instance = m["instance"].as_str().unwrap_or_default().to_string();
            let site = m["site_name"].as_str().unwrap_or_default().to_string();
            let label = m["label"].as_str().unwrap_or_default().to_string();
            let default_name = if !site.is_empty() {
                if label.is_empty() { site.clone() } else { format!("{site}/{label}") }
            } else if instance.is_empty() {
                app.clone()
            } else {
                format!("{app}/{instance}")
            };
            rows.push(json!({
                "id": stable_provider_id(&src),
                "src": src,
                "name": proxy["provider_names"][&src].as_str().filter(|name| !name.trim().is_empty()).unwrap_or(&default_name),
                "kind": if site.is_empty() { "instance" } else { "key" },
                "app": m["app"].as_str().unwrap_or_default(),
                "app_name": app,
                "site_name": site,
                "label": label,
                "masked": m["masked"].as_str().unwrap_or_default(),
                "has_token": m["has_token"].as_bool().unwrap_or(true),
                "off": off.contains(&src),
                "supported": m["supported"].as_bool().unwrap_or(false),
                "availability_reason": m["availability_reason"].as_str().unwrap_or_default(),
                "bridge": m["bridge"].as_bool().unwrap_or(false),
            }));
        }
    };
    if let Some(items) = members["instances"].as_array() {
        rows_from(items);
    }
    if let Some(items) = members["keys"].as_array() {
        rows_from(items);
    }
    json!(rows)
}

fn available_member<'a>(members: &'a Value, src: &str) -> Option<&'a Value> {
    ["instances", "keys"].into_iter().flat_map(|group| members[group].as_array().into_iter().flatten())
        .find(|member| member["src"].as_str() == Some(src))
}

fn require_provider_ready(member: &Value) -> Result<(), String> {
    if member["supported"] != true || member["has_token"] != true {
        return Err(member["availability_reason"].as_str().filter(|s| !s.is_empty())
            .unwrap_or("这个来源尚未具备可用的反代适配或登录态").into());
    }
    Ok(())
}

fn set_source_flag(proxy: &mut Value, field: &str, src: &str, present: bool) {
    let mut entries = string_list(&proxy[field]);
    entries.retain(|s| s != src);
    if present { entries.push(src.to_string()); }
    proxy[field] = json!(entries);
}

fn put_provider_pool(proxy: &mut Value, src: &str, name: &str) {
    let id = stable_provider_id(src);
    let mut list = proxy["pools"].as_array().cloned().unwrap_or_default();
    if let Some(found) = list.iter_mut().find(|p| p["id"] == id) {
        let mut member = found["members"].as_array().into_iter().flatten()
            .find(|member| member["src"].as_str() == Some(src)).cloned()
            .unwrap_or_else(|| json!({"src":src,"base_url":""}));
        member["enabled"] = json!(true);
        found["name"] = json!(format!("供应商 · {name}"));
        found["enabled"] = json!(true);
        found["members"] = json!([member]);
    } else {
        list.push(json!({"id":id,"name":format!("供应商 · {name}"),"enabled":true,
            "members":[{"src":src,"base_url":"","enabled":true}]}));
    }
    proxy["pools"] = json!(list);
}

/// Creating the first explicit pool switches off the legacy fallback. Preserve the old
/// routes and their effective endpoints; only accounts/keys actually in that fallback qualify.
fn preserve_legacy_pools(proxy: &mut Value, members: &Value) {
    if proxy["pools"].as_array().is_some_and(|list| !list.is_empty()) { return; }
    let rows = provider_rows(members, proxy);
    for row in rows.as_array().into_iter().flatten() {
        let src = row["src"].as_str().unwrap_or_default();
        let Some(member) = available_member(members, src) else { continue };
        if member["legacy"] != true { continue; }
        put_provider_pool(proxy, src, row["name"].as_str().unwrap_or_default());
        // Legacy API keys used the global upstream, including for sites whose own URL differs.
        if src.starts_with("key:") {
            let upstream = proxy["upstream"].as_str().unwrap_or("https://api.openai.com").to_owned();
            if let Some(pool) = proxy["pools"].as_array_mut().and_then(|list| list.last_mut()) {
                pool["members"][0]["base_url"] = json!(upstream);
            }
        }
    }
}

/// Pure config edits make removal/restoration testable without touching local credentials.
fn edit_provider_upsert(proxy: &mut Value, members: &Value, item: &Value) -> Result<String, String> {
    let src = item["src"].as_str().unwrap_or_default().trim();
    let name = item["name"].as_str().unwrap_or_default().trim();
    if name.is_empty() { return Err("供应商名称不能为空".into()); }
    if name.chars().count() > 120 { return Err("供应商名称不能超过 120 个字".into()); }
    let member = available_member(members, src).ok_or("没找到这个来源，请刷新后重新选择")?;
    require_provider_ready(member)?;
    preserve_legacy_pools(proxy, members);
    set_source_flag(proxy, "providers_removed", src, false);
    set_source_flag(proxy, "sources_off", src, false);
    if !proxy["provider_names"].is_object() { proxy["provider_names"] = json!({}); }
    proxy["provider_names"][src] = json!(name);
    put_provider_pool(proxy, src, name);
    Ok(format!("已保存供应商「{name}」"))
}

fn edit_provider_delete(proxy: &mut Value, members: &Value, id: &str) -> Result<String, String> {
    let src = ["instances", "keys"].into_iter()
        .flat_map(|group| members[group].as_array().into_iter().flatten())
        .filter_map(|member| member["src"].as_str())
        .find(|src| stable_provider_id(src) == id).map(str::to_owned)
        .or_else(|| string_list(&proxy["providers_removed"]).into_iter().find(|src| stable_provider_id(src) == id))
        .ok_or("没找到这个供应商")?;
    set_source_flag(proxy, "providers_removed", &src, true);
    set_source_flag(proxy, "sources_off", &src, true);
    // Preserve every dist scope. Removing its last pool ID would mean "all pools".
    // Old shared pools and original account/API key records also remain intact.
    if let Some(list) = proxy["pools"].as_array_mut() {
        for pool in list.iter_mut().filter(|pool| pool["id"].as_str() == Some(id)) {
            pool["enabled"] = json!(false);
        }
    }
    Ok("已删除供应商并停用来源；原账号、API Key 和授权范围已保留".into())
}

fn edit_provider_enabled(proxy: &mut Value, members: &Value, id: &str, enabled: bool) -> Result<String, String> {
    let row = provider_rows(members, proxy).as_array().into_iter().flatten()
        .find(|row| row["id"].as_str() == Some(id)).cloned().ok_or("没找到这个供应商，请刷新列表")?;
    let src = row["src"].as_str().ok_or("供应商缺少来源")?;
    if enabled {
        require_provider_ready(available_member(members, src).ok_or("没找到这个来源")?)?;
        preserve_legacy_pools(proxy, members);
        put_provider_pool(proxy, src, row["name"].as_str().unwrap_or("供应商"));
    }
    set_source_flag(proxy, "sources_off", src, !enabled);
    Ok(if enabled { "已启用供应商" } else { "已停用供应商" }.into())
}

fn save_provider_change(edit: impl FnOnce(&mut Value, &Value) -> Result<String, String>) -> (bool, String) {
    let members = member_options();
    let mut cfg = load_config();
    let mut proxy = proxy_cfg(&cfg);
    if !cfg.is_object() || !proxy.is_object() { return (false, "反代配置格式不正确，未保存".into()); }
    let message = match edit(&mut proxy, &members) {
        Ok(message) => message,
        Err(error) => return (false, error),
    };
    cfg["proxy"] = proxy;
    persist_proxy_config(&cfg, message)
}

fn persist_proxy_config(cfg: &Value, message: String) -> (bool, String) {
    save_config(cfg);
    // save_config predates Result-based writes; read the file, not its last-good cache.
    let saved = std::fs::read(crate::registry::config_file()).ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());
    if saved.as_ref().map(|value| &value["proxy"]) != Some(&cfg["proxy"]) {
        return (false, "反代配置未能写入磁盘，请检查数据目录权限后重试".into());
    }
    (true, message)
}

pub fn upsert_provider(item: &Value) -> (bool, String) {
    save_provider_change(|proxy, members| edit_provider_upsert(proxy, members, item))
}

pub fn remove_provider(id: &str) -> (bool, String) {
    save_provider_change(|proxy, members| edit_provider_delete(proxy, members, id))
}

pub fn set_provider_enabled(id: &str, enabled: bool) -> (bool, String) {
    save_provider_change(|proxy, members| edit_provider_enabled(proxy, members, id, enabled))
}

/// 用稳定的 id 查回供应商行；列表重建后来源不变，id 也不会变。
pub fn provider_by_id(id: &str) -> Option<Value> {
    provider_options().as_array()?.into_iter().find(|p| p["id"] == id).cloned()
}

/// 保证每个供应商都有一个一对一的隐式上游组。旧组不受影响。
pub fn ensure_provider_pool(id: &str) -> Result<Value, String> {
    let row = provider_by_id(id).ok_or("没找到这个供应商")?;
    let src = row["src"].as_str().ok_or("供应商缺少来源")?;
    let pool_id = stable_provider_id(src);
    let mut list = pools();
    if let Some(found) = list.iter_mut().find(|p| p["id"] == pool_id) {
        let changed = found["members"].as_array().map(|m| m.len()) != Some(1)
            || found["members"].as_array().and_then(|m| m.first()).and_then(|m| m["src"].as_str()) != Some(src)
            || found["enabled"] == false;
        if !changed {
            return Ok(found.clone());
        }
        found["name"] = json!(format!("供应商 · {}", row["name"].as_str().unwrap_or_default()));
        found["enabled"] = json!(true);
        found["members"] = json!([{"src":src,"base_url":"","enabled":true}]);
    } else {
        list.push(json!({
            "id": pool_id,
            "name": format!("供应商 · {}", row["name"].as_str().unwrap_or_default()),
            "enabled": true,
            "members": [{"src":src,"base_url":"","enabled":true}],
        }));
    }
    save_proxy_field("pools", json!(list));
    Ok(pools().into_iter().find(|p| p["id"] == pool_id).ok_or("隐式上游组保存失败")?)
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

/// Accept stable provider IDs and the source selectors saved by the earlier UI.
/// Unknown/legacy IDs stay in the scope: dropping the last one would grant all pools.
fn normalize_provider_scope(proxy: &mut Value, members: &Value, ids: &[String], activate: bool) -> Vec<String> {
    let rows = provider_rows(members, proxy);
    let mut normalized = Vec::new();
    for selected in ids {
        let row = rows.as_array().into_iter().flatten()
            .find(|row| row["id"].as_str() == Some(selected) || row["src"].as_str() == Some(selected));
        let id = if let Some(row) = row {
            let id = row["id"].as_str().unwrap_or_default();
            let exists = proxy["pools"].as_array().into_iter().flatten().any(|pool| pool["id"] == id);
            if (!exists || activate) && row["off"] == false && row["supported"] == true && row["has_token"] == true {
                preserve_legacy_pools(proxy, members);
                put_provider_pool(proxy, row["src"].as_str().unwrap_or_default(), row["name"].as_str().unwrap_or("供应商"));
            }
            id.to_string()
        } else { selected.clone() };
        if !normalized.contains(&id) { normalized.push(id); }
    }
    normalized
}

/// 建/改接入密钥。新建时返回明文 key 一并提示（只在这里出现一次也够用）。
pub fn upsert_dist(item: &Value) -> (bool, String) {
    let name = item["name"].as_str().unwrap_or("").trim().to_string();
    if name.is_empty() {
        return (false, "给它起个名字（谁在用），日后好停用".into());
    }
    let mut cfg = load_config();
    let mut proxy = proxy_cfg(&cfg);
    if !cfg.is_object() || !proxy.is_object() { return (false, "反代配置格式不正确，未保存".into()); }
    let sel = normalize_provider_scope(&mut proxy, &member_options(), &string_list(&item["pools"]), true);
    let scope = if sel.is_empty() {
        "全部上游组".to_string()
    } else {
        format!("{} 个上游组", sel.len())
    };
    let id = item["id"].as_str().unwrap_or("").trim().to_string();
    let mut list = proxy["dist"].as_array().cloned().unwrap_or_default();

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
        proxy["dist"] = json!(list);
        cfg["proxy"] = proxy;
        return persist_proxy_config(&cfg, format!("已生成接入密钥（{}）：{} —— 填到客户端当 API Key", scope, key));
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
    proxy["dist"] = json!(list);
    cfg["proxy"] = proxy;
    persist_proxy_config(&cfg, format!("已保存接入密钥「{}」（授权 {}）", name, scope))
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

/// 一键反代的接线保障：确保「成员 ∈ 某个启用的上游组」且「该组 ⊆ 某把启用的接入密钥」。
///
/// 匹配顺序是刻意安排的：
///   1. 先找**已经含这个成员**的启用上游组——用户手工编过的编制原样复用，绝不再建一个重复的；
///   2. 找不到才按 `pool_id` 找（老编制按固定 id 建过）、把成员补进去；
///   3. 还没有才新建 `{pool_id}`（固定 id，天然幂等——这就是老一键「按名匹配、每轮新建」
///      那个教训的解法：名字会撞，id 不会）。
///   接入密钥同理：先找覆盖该组的启用 key，找不到才按 `dist_id` 补授权/新建。
///
/// 返回 (pool_id, key, steps)：key 为空 = 组有了但没 key（罕见：组被建出来时就是没人认领）。
pub fn ensure_wiring(
    pool_id: &str,
    pool_name: &str,
    member_src: &str,
    dist_id: &str,
    dist_name: &str,
) -> (String, String, Vec<String>) {
    let mut steps: Vec<String> = Vec::new();

    // ── 上游组 ──
    let mut pid = String::new();
    let mut pools = pools();
    for p in pools.iter_mut() {
        if p["enabled"].as_bool().unwrap_or(true)
            && p["members"]
                .as_array()
                .map(|ms| ms.iter().any(|m| m["src"].as_str().unwrap_or("") == member_src))
                .unwrap_or(false)
        {
            pid = p["id"].as_str().unwrap_or("").to_string();
            break;
        }
    }
    if pid.is_empty() {
        // 没有现成的组：按固定 id 找，找到就补成员；没有就整条新建
        let mut hit = false;
        for p in pools.iter_mut() {
            if p["id"].as_str().unwrap_or("") == pool_id {
                let ms = p["members"].as_array().cloned().unwrap_or_default();
                let mut members = norm_members(&Value::Array(ms));
                members.push(json!({ "src": member_src, "enabled": true }));
                p["members"] = json!(members);
                p["enabled"] = json!(true);
                hit = true;
                break;
            }
        }
        if !hit {
            pools.push(json!({
                "id": pool_id,
                "name": pool_name,
                "enabled": true,
                "members": [{ "src": member_src, "enabled": true }],
            }));
            steps.push(format!("已建上游组「{pool_name}」"));
        } else {
            steps.push(format!("已把成员补进「{pool_name}」"));
        }
        save_proxy_field("pools", json!(pools));
        pid = pool_id.to_string();
    }

    // ── 接入密钥 ──
    let mut key = String::new();
    let mut dist = dist_keys();
    for d in dist.iter() {
        if d["enabled"].as_bool().unwrap_or(true)
            && d["pools"]
                .as_array()
                .map(|ps| ps.iter().any(|x| x.as_str() == Some(pid.as_str())))
                .unwrap_or(false)
        {
            key = d["key"].as_str().unwrap_or("").to_string();
            break;
        }
    }
    if key.is_empty() {
        let mut hit = false;
        for d in dist.iter_mut() {
            if d["id"].as_str().unwrap_or("") == dist_id {
                let mut ps: Vec<Value> = d["pools"].as_array().cloned().unwrap_or_default();
                if !ps.iter().any(|x| x.as_str() == Some(pid.as_str())) {
                    ps.push(json!(pid));
                }
                d["pools"] = json!(ps);
                d["enabled"] = json!(true);
                key = d["key"].as_str().unwrap_or("").to_string();
                hit = true;
                break;
            }
        }
        if !hit {
            let obj = json!({
                "id": dist_id,
                "key": gen_dist_key(),
                "name": dist_name,
                "pools": [pid],
                "enabled": true,
                "calls": 0,
                "last_used": 0,
                "last_account": "",
                "created": crate::registry::now_secs(),
            });
            key = obj["key"].as_str().unwrap_or("").to_string();
            dist.push(obj);
            steps.push(format!("已生成接入密钥「{dist_name}」"));
        } else {
            steps.push(format!("已把「{pool_name}」授权给「{dist_name}」"));
        }
        save_proxy_field("dist", json!(dist));
    }
    (pid, key, steps)
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
        if app_id == "zcode" {
            // Account names are handles, never bearer tokens. Ignore arbitrary
            // base overrides so saved ZCode credentials cannot leave this adapter.
            return crate::zcode::proxy_available(name).then(|| (
                format!("{app_name}/{name}"), name.to_string(), crate::zcproxy::BASE_MARK.to_string()
            ));
        }
        let mut tok = meta["token"].as_str().unwrap_or("").trim().to_string();
        // Qoder 系的令牌不在账号记录里，而在产品自己的 auth.v1.dat（见 qproxy）。
        // Trae 的在 storage.json（见 traeproxy）。
        // AStudio 的模型令牌在 astron-session.json 的 modelBearerToken（见 asproxy），
        // 账号记录里那个 token 是 SSO 会话令牌，不是模型 Bearer，不能顶替。
        // 这里读不到就说明这个号没登录 / 档案身份不符 —— 如实返回 None，不拿别人的号顶替。
        if crate::asproxy::APPS.contains(&app_id) {
            tok = crate::asproxy::token();
        } else if crate::mmproxy::APPS.contains(&app_id) {
            // MiniMax Code 的 OAuth 令牌在 ~/.minimax/auth/<env>/<region>/mcode-public/auth.json
            // （mmproxy 读 + 必要时刷新）；账号记录里的 token 字段一直是空的。
            tok = crate::mmproxy::token();
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
        // MiniMax Code：上游是 Anthropic 协议，proxy 见到这个标记就走 mmproxy 的翻译层
        let mbase = if crate::mmproxy::APPS.contains(&app_id) {
            crate::mmproxy::BASE_MARK.to_string()
        } else {
            String::new()
        };
        let base = if !member_base.trim().is_empty() {
            member_base.trim().to_string()
        } else if !own.is_empty() {
            own
        } else if !abase.is_empty() {
            abase
        } else if !mbase.is_empty() {
            mbase
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
        let app_name = all_apps()
            .into_iter()
            .find(|a| a.id == app_id)
            .map(|a| a.name)
            .unwrap_or_else(|| app_id.to_string());
        // bridge:qwenwork/<账号> —— 千问办公的本地桥：走网页版 chat-ws，桥自己不校验令牌。
        // **不再要求桥在跑**：桥挂了就让成员照常进池子，serve() 那头连不上时会自己把桥拉起来
        // （自愈）。反过来"桥没跑→成员消失"会让整条池子静默蒸发，用户只看到一句没头没尾的 404。
        if crate::qwbridge::APPS.contains(&app_id) {
            // 基址按 config 里记的实际端口来（首选口被占时起桥会换备用口并写回）
            return Some((
                format!("本地桥/{}", app_name),
                "local".to_string(),
                crate::qwbridge::base(),
            ));
        }
        // bridge:catpaw/<账号> —— 妙手改成了**进程内云端直连**（协议底账见 pawbridge.rs）：
        // 凭据就是 ~/.meituan-catpaw/auth.json 的 accessToken，反代自己带着它打妙手云端，
        // 不再有桥进程、不再有命名管道、妙手桌面端也不用开。auth.json 在就给上游，
        // 令牌过不过期由 serve_paw 现场验、把话说清楚。
        if crate::pawbridge::APPS.contains(&app_id) {
            if !crate::pawbridge::auth_file_exists() {
                return None;
            }
            return Some((
                format!("直连/{}", app_name),
                "local".to_string(),
                crate::pawbridge::BASE_MARK.to_string(),
            ));
        }
        if app_id != "qoder-cn" || !crate::qproxy::cn_bridge_running() {
            return None;
        }
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
                        key_proxy_base(&site, k)
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
    let off = sources_off();
    for m in pool["members"].as_array().cloned().unwrap_or_default() {
        if !m["enabled"].as_bool().unwrap_or(true) {
            continue;
        }
        let src = m["src"].as_str().unwrap_or("");
        if !source_enabled(src, &off) {
            continue;
        }
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
    let mut proxy = proxy_cfg(&load_config());
    // Resolve old source scopes without requiring users to edit each existing key.
    // This is an in-memory view; request handling never writes provider config.
    let needs_compat = ids.iter().any(|id| id.starts_with("key:") || id.starts_with("instance:") || id.starts_with("bridge:"));
    let normalized = if needs_compat { normalize_provider_scope(&mut proxy, &member_options(), ids, false) }
        else { ids.to_vec() };
    let all = proxy["pools"].as_array().cloned().unwrap_or_default();
    let mut out = Vec::new();
    for p in &all {
        if !p["enabled"].as_bool().unwrap_or(true) {
            continue;
        }
        if !normalized.is_empty() {
            let pid = p["id"].as_str().unwrap_or("");
            if !normalized.iter().any(|x| x == pid) {
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

fn http_base(value: &str) -> bool {
    tauri::Url::parse(value.trim()).is_ok_and(|url| matches!(url.scheme(), "http" | "https") && url.host_str().is_some())
}

fn key_proxy_base(site: &Value, key: &Value) -> String {
    let base = crate::keys::base_for(site, key);
    let base = if base.is_empty() { site["base_url"].as_str().unwrap_or("") } else { &base };
    let trimmed = base.trim_end_matches('/');
    for suffix in ["/chat/completions", "/responses", "/messages", "/models"] {
        if let Some(base) = trimmed.strip_suffix(suffix) { return base.to_owned(); }
    }
    trimmed.to_owned()
}

fn instance_supported(app: &str, own_base: &str) -> bool {
    crate::qproxy::region_of(app).is_some()
        || crate::traeproxy::APPS.contains(&app)
        || crate::wbproxy::APPS.contains(&app)
        || crate::asproxy::APPS.contains(&app)
        || crate::mmproxy::APPS.contains(&app)
        || app == "zcode"
        || http_base(own_base)
}

/// 可选的成员清单（给界面做勾选用）：按 Agent 分组列账号，再列 API 台账里的 key
pub fn member_options() -> Value {
    let off = sources_off();
    let mut instances = Vec::new();
    for app in all_apps() {
        for m in load_instances(&app.id) {
            let name = m["name"].as_str().unwrap_or("").to_string();
            let tok = m["token"].as_str().unwrap_or("").trim().to_string();
            let own_base = m["proxy_base_url"].as_str().unwrap_or("");
            let supported = instance_supported(&app.id, own_base);
            // Read adapter credentials, never confuse a product's SSO token with a model key.
            // MiniMax reads must stay quiet: listing providers must not refresh OAuth tokens.
            let usable = if crate::asproxy::APPS.contains(&app.id.as_str()) {
                !crate::asproxy::token().is_empty()
            } else if crate::mmproxy::APPS.contains(&app.id.as_str()) {
                !crate::mmproxy::token_quiet().is_empty()
            } else if app.id == "zcode" {
                crate::zcode::proxy_available(&name)
            } else if app.id == "qoder-cn" {
                crate::qproxy::token_for(&app.id, &name).is_some()
            } else {
                !tok.is_empty()
                    || (crate::qproxy::region_of(&app.id).is_some() && crate::qproxy::token_for(&app.id, &name).is_some())
                    || (crate::traeproxy::APPS.contains(&app.id.as_str()) && crate::traeproxy::token_for(&app.id, &name).is_some())
                    || (crate::wbproxy::APPS.contains(&app.id.as_str()) && crate::wbproxy::token_for(&app.id, &name).is_some())
            };
            let reason = if !supported {
                "尚无账号反代适配；可在密钥页登记兼容服务的 API Key"
            } else if !usable && app.id == "qoder-cn" {
                "国内版直连需要官方签名；请启动已登录账号的本地桥"
            } else if !usable {
                "未读取到可用登录态或模型令牌，请先登录对应账号"
            } else { "" };
            instances.push(json!({
                "src": member_src_instance(&app.id, &name),
                "app": app.id,
                "app_name": app.name,
                "instance": name,
                "has_token": usable,
                "legacy": if app.id == "zcode" { usable } else { !tok.is_empty() },
                "supported": supported,
                "availability_reason": reason,
                "proxy_base_url": own_base,
                "qregion": crate::qproxy::region_of(&app.id).unwrap_or(""),
                "qstate": crate::qproxy::stored_state(&app.id, &name).unwrap_or_default(),
                "trae": crate::traeproxy::APPS.contains(&app.id.as_str()),
                "astudio": crate::asproxy::APPS.contains(&app.id.as_str()),
                "minimax": crate::mmproxy::APPS.contains(&app.id.as_str()),
                "off": off.contains(&member_src_instance(&app.id, &name)),
            }));
            // 千问办公：桌面端登录态在就能挂「本地桥」（桥没跑 serve() 会自愈拉起）
            if crate::qwbridge::APPS.contains(&app.id.as_str()) {
                let up = crate::qwbridge::cred_exists();
                instances.push(json!({
                    "src": format!("bridge:{}/{}", app.id, name),
                    "app": app.id,
                    "app_name": app.name,
                    "instance": name,
                    "has_token": up,
                    "supported": true,
                    "availability_reason": if up { "" } else { "请先登录千问办公客户端" },
                    "proxy_base_url": crate::qwbridge::base(),
                    "qregion": "",
                    "qstate": if up { "bridge" } else { "bridge_idle" },
                    "bridge": true,
                    "off": off.contains(&format!("bridge:{}/{}", app.id, name)),
                }));
            }
            // 妙手：改成进程内云端直连了——auth.json 在就能挂，桌面端跑不跑无关
            if crate::pawbridge::APPS.contains(&app.id.as_str()) {
                let up = crate::pawbridge::auth_file_exists();
                instances.push(json!({
                    "src": format!("bridge:{}/{}", app.id, name),
                    "app": app.id,
                    "app_name": app.name,
                    "instance": name,
                    "has_token": up,
                    "supported": true,
                    "availability_reason": if up { "" } else { "请先登录妙手客户端" },
                    "proxy_base_url": crate::pawbridge::BASE_MARK,
                    "qregion": "",
                    "qstate": if up { "bridge" } else { "bridge_idle" },
                    "bridge": true,
                    "off": off.contains(&format!("bridge:{}/{}", app.id, name)),
                }));
            }
            // Qoder CN：CLI 登录过就能挂本地桥（国内版唯一通路，见 qproxy.rs）
            if app.id == "qoder-cn" && crate::qproxy::cn_logged_in(&name) {
                let up = crate::qproxy::cn_bridge_running();
                instances.push(json!({
                    "src": format!("bridge:{}/{}", app.id, name),
                    "app": app.id,
                    "app_name": app.name,
                    "instance": name,
                    "has_token": up,
                    "supported": true,
                    "availability_reason": if up { "" } else { "请先启动 Qoder CN 本地桥" },
                    "proxy_base_url": format!("http://127.0.0.1:{}/v1", crate::qproxy::CN_PORT),
                    "qregion": "cn",
                    "qstate": if up { "bridge" } else { "bridge_idle" },
                    "bridge": true,
                    "off": off.contains(&format!("bridge:{}/{}", app.id, name)),
                }));
            }
        }
    }
    let mut keys = Vec::new();
    for site in crate::keys::sites() {
        let sid = site["id"].as_str().unwrap_or("").to_string();
        let sname = site["name"].as_str().unwrap_or("").to_string();
        for k in site["keys"].as_array().cloned().unwrap_or_default() {
            let kid = k["id"].as_str().unwrap_or("").to_string();
            let src = member_src_key(&sid, &kid);
            let has_token = !k["key"].as_str().unwrap_or("").trim().is_empty();
            let sbase = key_proxy_base(&site, &k);
            let supported = http_base(&sbase);
            keys.push(json!({
                "src": src,
                "site_id": sid,
                "site_name": sname,
                "base_url": sbase,
                "label": k["label"].as_str().unwrap_or(""),
                "masked": crate::keys::mask(k["key"].as_str().unwrap_or("")),
                "has_token": has_token,
                "legacy": has_token && k["use_in_proxy"].as_bool().unwrap_or(false),
                "supported": supported,
                "availability_reason": if !supported { "请先在密钥页填写有效的 HTTP(S) 服务地址" }
                    else if !has_token { "请先在密钥页填写 API Key" } else { "" },
                "off": off.contains(&src),
            }));
        }
    }
    json!({ "instances": instances, "keys": keys })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider_fixture() -> (Value, Value, String) {
        let src = "instance:qoder/account-a";
        let id = stable_provider_id(src);
        let members = json!({"instances":[{"src":src,"app":"qoder","app_name":"Qoder",
            "instance":"account-a","supported":true,"has_token":true}],"keys":[]});
        let proxy = json!({"upstream":"https://example.test","strategy":"round-robin",
            "pools":[
                {"id":id,"name":"供应商 · Qoder/account-a","enabled":true,
                    "members":[{"src":src,"base_url":"https://saved-endpoint.test"}]},
                {"id":"legacy","name":"旧共享组","enabled":true,"members":[{"src":src},{"src":"key:other/key"}]}],
            "dist":[{"id":"restricted","pools":[id],"enabled":true,"key":"fixture-only"},
                {"id":"shared","pools":["legacy"],"enabled":true}],
            "sources_off":["key:already/off"],"unrelated":{"preserve":true}});
        (members, proxy, id)
    }

    #[test]
    fn provider_delete_is_idempotent_and_never_broadens_key_scope() {
        let (members, mut proxy, id) = provider_fixture();
        let before = proxy.clone();
        edit_provider_delete(&mut proxy, &members, &id).unwrap();
        assert!(provider_rows(&members, &proxy).as_array().unwrap().is_empty());
        assert_eq!(proxy["dist"], before["dist"], "last authorized pool must never become all-pools scope");
        assert_eq!(proxy["pools"][1], before["pools"][1], "shared pools must remain intact");
        assert_eq!(proxy["pools"][0]["enabled"], false);
        assert_eq!(proxy["unrelated"], before["unrelated"]);
        assert_eq!(proxy["upstream"], before["upstream"]);
        assert!(string_list(&proxy["sources_off"]).contains(&"instance:qoder/account-a".into()));
        let removed = proxy.clone();
        edit_provider_delete(&mut proxy, &members, &id).unwrap();
        assert_eq!(proxy, removed, "deleting an already disabled/removed supplier must not re-enable it");
        assert!(edit_provider_enabled(&mut proxy, &members, &id, true).is_err());
        assert_eq!(proxy, removed, "removed suppliers require explicit restoration");
    }

    #[test]
    fn provider_restore_reuses_pool_and_persists_name_and_admission() {
        let (members, mut proxy, id) = provider_fixture();
        let old_dist = proxy["dist"].clone();
        edit_provider_enabled(&mut proxy, &members, &id, false).unwrap();
        edit_provider_delete(&mut proxy, &members, &id).unwrap();
        let item = json!({"src":"instance:qoder/account-a","name":"  工作账号  "});
        edit_provider_upsert(&mut proxy, &members, &item).unwrap();
        let first = proxy.clone();
        edit_provider_upsert(&mut proxy, &members, &item).unwrap();
        assert_eq!(proxy, first, "retrying a successful add must not duplicate pools");
        assert_eq!(proxy["dist"], old_dist);
        assert_eq!(proxy["pools"].as_array().unwrap().len(), 2);
        assert_eq!(proxy["pools"][0]["id"], id);
        assert_eq!(proxy["pools"][0]["name"], "供应商 · 工作账号");
        assert_eq!(proxy["pools"][0]["members"][0]["src"], item["src"]);
        assert_eq!(proxy["pools"][0]["members"][0]["base_url"], "https://saved-endpoint.test");
        assert_eq!(proxy["pools"][0]["enabled"], true);
        let rows = provider_rows(&members, &proxy);
        assert_eq!(rows[0]["name"], "工作账号");
        assert_eq!(rows[0]["off"], false);
        assert!(!string_list(&proxy["sources_off"]).contains(&"instance:qoder/account-a".into()));
        assert!(string_list(&proxy["sources_off"]).contains(&"key:already/off".into()));
        edit_provider_enabled(&mut proxy, &members, &id, false).unwrap();
        let disabled = proxy.clone();
        edit_provider_enabled(&mut proxy, &members, &id, false).unwrap();
        assert_eq!(proxy, disabled, "set-enabled is idempotent, unlike toggle");
        assert_eq!(provider_rows(&members, &proxy)[0]["off"], true);
    }

    #[test]
    fn provider_upsert_rejects_unavailable_or_unknown_sources_without_changes() {
        let (mut members, mut proxy, id) = provider_fixture();
        let before = proxy.clone();
        for item in [json!({"src":"instance:missing/account","name":"Unknown"}),
            json!({"src":"instance:qoder/account-a","name":"  "})] {
            assert!(edit_provider_upsert(&mut proxy, &members, &item).is_err());
            assert_eq!(proxy, before);
        }
        let item = json!({"src":"instance:qoder/account-a","name":"Valid"});
        for missing in ["has_token", "supported"] {
            members["instances"][0][missing] = json!(false);
            assert!(edit_provider_upsert(&mut proxy, &members, &item).is_err());
            assert!(edit_provider_enabled(&mut proxy, &members, &id, true).is_err());
            assert_eq!(proxy, before);
            members["instances"][0][missing] = json!(true);
        }
        assert!(edit_provider_delete(&mut proxy, &members, "gp-not-a-provider").is_err());
        assert_eq!(proxy, before);
    }

    #[test]
    fn provider_admission_requires_adapter_or_explicit_generic_endpoint() {
        assert!(instance_supported("qoder", ""));
        assert!(instance_supported("zcode", ""));
        assert!(!instance_supported("codex", ""), "Codex OAuth is not an OpenAI API key");
        assert!(!instance_supported("custom", "file:///secret"));
        assert!(!instance_supported("custom", "not a URL"));
        assert!(instance_supported("custom", "https://example.test/v1"));
        assert!(http_base("http://127.0.0.1:8211/v1"));
    }

    #[test]
    fn provider_isolated_disk_roundtrip() {
        // Running in a subprocess keeps both the data-root environment and static caches
        // isolated from the rest of the parallel Rust suite and the user's running cockpit.
        const CHILD: &str = "AGENTCOCKPIT_SUPPLIER_DISK_FIXTURE";
        if std::env::var_os(CHILD).is_none() {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(dir.path().join("supplier-fixture"), b"test-only").unwrap();
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "gateway::tests::provider_isolated_disk_roundtrip", "--nocapture"])
                .env("AGENT_COCKPIT_HOME", dir.path()).env(CHILD, dir.path()).status().unwrap();
            assert!(status.success());
            assert!(dir.path().join("supplier-test-completed").is_file());
            return;
        }
        let root = crate::registry::base_dir();
        assert_eq!(Some(root.as_os_str().to_owned()), std::env::var_os(CHILD));
        assert_eq!(std::fs::read(root.join("supplier-fixture")).unwrap(), b"test-only");
        let cfg = json!({"proxy":{"upstream":"https://legacy.example.test","pools":[],"dist":[]}});
        save_config(&cfg);
        let src = "key:site/candidate";
        let id = stable_provider_id(src);
        let keys = json!({"providers":[{"id":"site","name":"Fixture","base_urls":[
            {"format":"openai","url":"https://example.test/compatible/v1"}],"keys":[
            {"id":"candidate","key":"fixture-candidate","label":"new"},
            {"id":"already","key":"fixture-legacy","label":"old","use_in_proxy":true}]}]});
        std::fs::write(crate::keys::store_file(), keys.to_string()).unwrap();
        assert!(available_member(&member_options(), src).is_some());
        let rows = provider_options();
        assert!(!rows.as_array().unwrap().iter().any(|row| row["src"] == src));
        let legacy_id = stable_provider_id("key:site/already");
        assert!(provider_by_id(&legacy_id).is_some());
        assert!(remove_provider(&legacy_id).0);
        assert!(crate::proxy::legacy_pool("https://legacy.example.test").is_empty());
        assert!(upsert_provider(&json!({"src":"key:site/already","name":"restored"})).0);
        // Re-enter the old, empty-pools configuration to exercise its first-pool migration.
        save_config(&cfg);
        assert!(upsert_provider(&json!({"src":src,"name":"My supplier"})).0);
        assert_eq!(provider_by_id(&id).unwrap()["name"], "My supplier");
        assert!(provider_by_id(&legacy_id).is_some());
        let old_route = upstreams_for(&[legacy_id], "https://legacy.example.test");
        assert_eq!(old_route[0].base_url, "https://legacy.example.test");
        assert!(upsert_dist(&json!({"name":"scoped","pools":[src]})).0);
        let d = dist_keys().into_iter().find(|d| d["name"] == "scoped").unwrap();
        assert_eq!(d["pools"], json!([id]));
        let route = || crate::proxy::pick_upstreams(d["key"].as_str().unwrap(), "https://legacy.example.test").ups;
        assert_eq!(route().len(), 1);
        assert_eq!(route()[0].base_url, "https://example.test/compatible/v1");
        let mut old = load_config();
        old["proxy"]["dist"][0]["pools"] = json!([src]);
        old["proxy"]["pools"].as_array_mut().unwrap().retain(|p| p["id"] != id);
        save_config(&old);
        assert_eq!(route().len(), 1, "keys with old source scopes work before re-saving");
        assert!(set_provider_enabled(&id, false).0);
        assert!(route().is_empty());
        assert!(remove_provider(&id).0);
        assert!(provider_by_id(&id).is_none());
        assert!(route().is_empty());
        let stored: Value = serde_json::from_slice(&std::fs::read(crate::registry::config_file()).unwrap()).unwrap();
        assert_eq!(stored["proxy"]["dist"][0]["pools"], json!([src]));
        assert_eq!(std::fs::read_to_string(crate::keys::store_file()).unwrap(), keys.to_string());
        assert!(upsert_provider(&json!({"src":src,"name":"Restored"})).0);
        assert_eq!(provider_by_id(&id).unwrap()["name"], "Restored");
        assert_eq!(route().len(), 1);
        std::fs::write(root.join("supplier-test-completed"), b"ok").unwrap();
    }

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
