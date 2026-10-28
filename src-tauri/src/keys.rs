// 官网 / 中转站 API 聚合台账：两级结构 —— 站点(providers) → 该站下的多个 key
// 设计要点：
//   - 一个站点（如 LongCat）可以持有多个 key，分别用于不同地方（Pot / WorkBuddy / QoderCN）
//   - 每个 key 可各自带 base_url（同一站点常有 OpenAI 格式与 Anthropic 格式两种接入点）
//     与可用模型清单（OpenAI 风格 /v1/models 或 Anthropic 风格 /v1/models 都能探）
//   - key 明文只在本机保存，列表一律打码，复制时按需取原文
use crate::quota::days_left;
use crate::registry::base_dir;
use crate::secrets;
use serde_json::{json, Value};
use std::fs;
use std::path::PathBuf;

/// 保管箱里这条 key 的编号。按 key id 存，站点合并/改名都不会牵动凭据。
pub fn secret_id(key_id: &str) -> String {
    format!("apikey:{}", key_id)
}

/// 测试用的存储路径覆盖开关。全应用共用同一个 static，
/// 否则 store_file() 与测试代码各持一份，测试会写进用户真实文件。
#[cfg(test)]
fn store_override() -> &'static std::sync::Mutex<Option<PathBuf>> {
    use std::sync::OnceLock;
    static OVERRIDE: OnceLock<std::sync::Mutex<Option<PathBuf>>> = OnceLock::new();
    OVERRIDE.get_or_init(|| std::sync::Mutex::new(None))
}

pub fn store_file() -> PathBuf {
    #[cfg(test)]
    {
        if let Some(p) = store_override().lock().unwrap().clone() {
            return p;
        }
    }
    base_dir().join("apikeys.json")
}

/// id 生成：毫秒时间戳 + 进程内自增序号。
/// 只用毫秒时间戳会撞车——同一毫秒内连续新建两个 key 会拿到相同 id，
/// 后写的直接覆盖先写的，表现为「加了 key 但数量没变」。
pub fn new_id(prefix: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    format!("{}-{:x}{:03x}", prefix, ts, seq & 0xfff)
}

/// 归一化站点名：大小写/空白/常见分隔符差异都算同一个站
pub fn norm_site(name: &str) -> String {
    name.chars()
        .filter(|c| !c.is_whitespace() && !matches!(c, '-' | '_' | '.'))
        .flat_map(|c| c.to_lowercase())
        .collect()
}

/// 归一化 base_url：同一 host 的不同接入点（/openai/v1、/anthropic、/v1/chat/completions…）
/// 都要归一到「host|去掉接口段后的路径」，这样才能把同一站认出是一条。
pub fn norm_base(url: &str) -> String {
    let u = url.trim().trim_end_matches('/').to_lowercase();
    let host = u
        .split("://")
        .nth(1)
        .map(|h| h.split('/').next().unwrap_or(""))
        .unwrap_or("");
    // 取 host 之后的路径段，逐段剥掉尾部常见的接口/格式段
    let path_segs: Vec<&str> = u
        .split("://")
        .nth(1)
        .map(|rest| rest.split('/').skip(1).filter(|s| !s.is_empty()).collect())
        .unwrap_or_default();
    let mut path: Vec<&str> = path_segs;
    // 反复剥掉尾部常见的接口/格式段，直到不再匹配
    // （单趟不够：如 /openai/v1/chat/completions 需先剥 completions 再剥 chat/v1/openai）
    loop {
        let before = path.len();
        for suffix in ["chat", "completions", "messages", "v1", "anthropic", "openai", "api"] {
            if path.last().map(|s| *s == suffix).unwrap_or(false) {
                path.pop();
            }
        }
        if path.len() == before {
            break;
        }
    }
    if host.is_empty() {
        return String::new();
    }
    format!("{}|{}", host, path.join("/"))
}

pub fn sites() -> Vec<Value> {
    fill_keys(read_sites())
}

/// 磁盘上的文档里不再有明文 key：读出来时按保管箱补齐 `key` 字段，
/// 抓额度、反代 token 池、粘贴解析这些调用方一律照旧用同一个字段，不必各自解密。
/// 保管箱锁着时补出来是空的——那时就是"用不了 Key"，界面会显示已锁定。
fn fill_keys(mut items: Vec<Value>) -> Vec<Value> {
    for p in items.iter_mut() {
        let ks = match p["keys"].as_array().cloned() {
            Some(k) => k,
            None => continue,
        };
        let mut out = Vec::with_capacity(ks.len());
        for mut k in ks {
            if k["key"].as_str().unwrap_or("").trim().is_empty() {
                let kid = k["id"].as_str().unwrap_or("").to_string();
                if let Some(v) = secrets::get(&secret_id(&kid)) {
                    k["key"] = json!(v);
                }
            }
            out.push(k);
        }
        p["keys"] = json!(out);
    }
    items
}

fn read_sites() -> Vec<Value> {
    match fs::read_to_string(store_file()) {
        Ok(s) => {
            let v: Value = serde_json::from_str(&s).unwrap_or(Value::Null);
            match v {
                // 新结构
                Value::Object(o) if o.get("providers").is_some() => o
                    .get("providers")
                    .and_then(|p| p.as_array())
                    .cloned()
                    .unwrap_or_default(),
                // 旧扁平结构（单层 key 数组）：原地升级成「每条一个站点」，一个 key 也不丢
                Value::Array(ref a) => {
                    let upgraded: Vec<Value> = a
                        .iter()
                        .map(|k| {
                            json!({
                                "id": k["id"].clone(),
                                "name": k["name"].clone(),
                                "kind": k["kind"].clone(),
                                "homepage": "",
                                "keys": [k.clone()],
                            })
                        })
                        .collect();
                    let mut cfg = json!({ "providers": upgraded });
                    // 保留反代等旧顶层设置
                    if let Value::Object(ref o) = v {
                        for k in o.keys() {
                            if k != "providers" {
                                cfg[k] = v[k].clone();
                            }
                        }
                    }
                    save_sites(&upgraded, &cfg);
                    upgraded
                }
                _ => Vec::new(),
            }
        }
        Err(_) => Vec::new(),
    }
}

/// 写盘的唯一出口：明文 key 先进保管箱，文档里只留打码值。
/// 保管箱锁着（用户设了主密码还没解锁）时**不能**删字段，否则那条 key 就真丢了——
/// 原样留在文档里并计数，界面提示"有 N 处明文待收"，解锁后 sweep 收走。
fn save_sites(items: &[Value], cfg: &Value) {
    let mut moved = 0usize;
    let mut left = 0usize;
    let mut scrubbed: Vec<Value> = Vec::with_capacity(items.len());
    for p in items {
        let mut q = p.clone();
        let ks = q["keys"].as_array().cloned().unwrap_or_default();
        let mut out = Vec::with_capacity(ks.len());
        for mut k in ks {
            let plain = k["key"].as_str().unwrap_or("").trim().to_string();
            if !plain.is_empty() {
                let kid = k["id"].as_str().unwrap_or("").to_string();
                k["masked"] = json!(mask(&plain));
                if secrets::available() && secrets::put(&secret_id(&kid), &plain) {
                    k.as_object_mut().map(|o| o.remove("key"));
                    moved += 1;
                } else {
                    left += 1;
                }
            }
            out.push(k);
        }
        q["keys"] = json!(out);
        scrubbed.push(q);
    }
    let _ = fs::create_dir_all(base_dir());
    let mut out = cfg.clone();
    if !out.is_object() {
        out = json!({});
    }
    out["providers"] = json!(scrubbed);
    let _ = fs::write(
        store_file(),
        serde_json::to_string_pretty(&out).unwrap_or_default(),
    );
    if moved > 0 {
        let _ = secrets::flush();
    }
    secrets::set_plaintext_left(left);
}

/// 解锁之后（以及启动时）把文档里残留的明文收进保管箱
pub fn sweep() -> usize {
    if !secrets::available() {
        return 0;
    }
    let items = read_sites();
    let n = items
        .iter()
        .filter_map(|p| p["keys"].as_array().cloned())
        .flatten()
        .filter(|k| !k["key"].as_str().unwrap_or("").trim().is_empty())
        .count();
    if n > 0 {
        let cfg = fs::read_to_string(store_file())
            .ok()
            .and_then(|s| serde_json::from_str::<Value>(&s).ok())
            .unwrap_or_else(|| json!({}));
        save_sites(&items, &cfg);
    }
    n
}

fn save(items: &[Value]) {
    let cfg = match fs::read_to_string(store_file()) {
        Ok(s) => serde_json::from_str::<Value>(&s).unwrap_or(Value::Null),
        Err(_) => Value::Null,
    };
    save_sites(items, &cfg);
}

/// 记下（或清掉）"这条 key 往哪个产品写过哪些条目"。注入器撤销时只认这份账，
/// 不去猜哪条像自己写的；账目必须跟台账一起落盘，所以走 save()（顺手把明文收回保管箱）。
pub fn set_injected(site_id: &str, key_id: &str, app: &str, ids: &[String]) {
    let mut items = sites();
    if let Some(p) = find_site_mut(&mut items, site_id) {
        if let Some(ks) = p["keys"].as_array_mut() {
            if let Some(k) = ks.iter_mut().find(|k| k["id"].as_str() == Some(key_id)) {
                if !k["injected"].is_object() {
                    k["injected"] = json!({});
                }
                if ids.is_empty() {
                    k["injected"].as_object_mut().map(|o| o.remove(app));
                } else {
                    k["injected"][app] = json!(ids);
                }
            }
        }
    }
    save(&items);
}

fn find_site_mut<'a>(items: &'a mut Vec<Value>, id: &str) -> Option<&'a mut Value> {
    items
        .iter_mut()
        .find(|p| p["id"].as_str().map(|s| s == id).unwrap_or(false))
}

/// 展示用：站点 + key 都补上打码、倒计时、key 数量
pub fn list_masked() -> Vec<Value> {
    let mut out = Vec::new();
    for mut p in sites() {
        let keys = p["keys"].as_array().cloned().unwrap_or_default();
        let mut decorated = Vec::new();
        for mut k in keys {
            let plain = k["key"].as_str().unwrap_or("").to_string();
            // 锁定时补不出明文，就用文档里存着的打码串——至少还认得出是哪条 key
            k["masked"] = if plain.is_empty() {
                json!(k["masked"].as_str().unwrap_or("••••"))
            } else {
                json!(mask(&plain))
            };
            k["secret_locked"] = json!(plain.is_empty());
            k["days_left"] = json!(days_left(k["expires_on"].as_str().unwrap_or("")));
            decorated.push(k);
        }
        let n = decorated.len();
        p["keys"] = json!(decorated);
        p["key_count"] = json!(n);
        // 站点级余额汇总（取任一 key 的额度，仅作展示线索）
        out.push(p);
    }
    out
}

pub fn mask(key: &str) -> String {
    let chars: Vec<char> = key.chars().collect();
    if chars.len() <= 10 {
        return "••••".to_string();
    }
    let head: String = chars[..6].iter().collect();
    let tail: String = chars[chars.len() - 4..].iter().collect();
    format!("{}••••{}", head, tail)
}

pub fn get_key_plain(site_id: &str, key_id: &str) -> String {
    sites()
        .iter()
        .find(|p| p["id"].as_str() == Some(site_id))
        .and_then(|p| p["keys"].as_array())
        .and_then(|ks| {
            ks.iter()
                .find(|k| k["id"].as_str() == Some(key_id))
                .and_then(|k| k["key"].as_str().map(String::from))
        })
        .unwrap_or_default()
}

fn clean_key(k: &Value) -> Value {
    let mut c = k.as_object().cloned().unwrap_or_default();
    // days_left / secret_locked 是每次展示现算的，不落盘；
    // masked 要留着——保管箱锁着的时候，界面只有它能让人认出是哪条 key
    c.remove("days_left");
    c.remove("secret_locked");
    Value::Object(c)
}

/// 新增/更新一个站点；同归一化名已存在则更新（防止同站重复建条目）。
/// 第三个返回值是**最终落到的站点 id**——同名站会被并进来，前端拿它才能接着往这个站里加 key，
/// 别再去按名字回查（名字撞车或被归一化改写时会认错站）。
pub fn upsert_site(site: &Value) -> (bool, String, String) {
    let id = site["id"].as_str().unwrap_or("").to_string();
    let name = site["name"].as_str().unwrap_or("").trim().to_string();
    if name.is_empty() {
        return (false, "站点名不能为空".into(), String::new());
    }
    let mut items = sites();
    let id = if id.is_empty() {
        // 同名即同一个站；名字不同但 base_url 归一到同一站点族，也并进已有站点
        let first_base = site["keys"][0]["base_url"].as_str().unwrap_or("").to_string();
        let found = items
            .iter()
            .find(|p| norm_site(p["name"].as_str().unwrap_or("")) == norm_site(&name))
            .or_else(|| {
                if first_base.trim().is_empty() {
                    return None;
                }
                items.iter().find(|p| {
                    p["keys"].as_array().map(|ks| {
                        ks.iter().any(|k| {
                            let b = k["base_url"].as_str().unwrap_or("").trim();
                            !b.is_empty() && norm_base(b) == norm_base(&first_base)
                        })
                    })
                    .unwrap_or(false)
                })
            });
        match found {
            Some(p) => p["id"].as_str().unwrap_or("").to_string(),
            None => new_id("p"),
        }
    } else {
        id
    };
    let mut v = site.clone();
    v["id"] = json!(id.clone());
    let new_keys: Vec<Value> = v["keys"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(clean_key)
        .collect();
    // 同名/同站点族并进来时，保留已有 key，只补充新出现的
    let msg = match items.iter().position(|p| p["id"].as_str() == Some(&id)) {
        Some(i) => {
            let mut merged: Vec<Value> = items[i]["keys"]
                .as_array()
                .cloned()
                .unwrap_or_default()
                .iter()
                .map(clean_key)
                .collect();
            let before = merged.len();
            for nk in new_keys {
                let kid = nk["id"].as_str().unwrap_or("");
                if let Some(pos) = merged.iter().position(|k| k["id"].as_str() == Some(kid)) {
                    merged[pos] = nk;
                } else {
                    merged.push(nk);
                }
            }
            let added = merged.len() - before;
            // 站点元信息以新提交的为准，未提交的键保留原值
            for k in ["name", "kind", "homepage", "note", "base_urls", "models_list"] {
                if v.get(k).map(|x| !x.is_null()).unwrap_or(false) {
                    items[i][k] = v[k].clone();
                }
            }
            // 额度挂在站点上（不是每个 key 一份）：编辑弹窗每次都带这两个字段，
            // 所以按「有没有带」合并——带 null 就是用户清掉了额度
            for k in ["left", "total"] {
                if let Some(x) = v.get(k) {
                    items[i][k] = x.clone();
                }
            }
            items[i]["keys"] = json!(merged);
            items[i]["key_count"] = json!(merged.len());
            if added > 0 {
                format!("已保存站点，并合并 {} 个 key", added)
            } else {
                "已保存站点".to_string()
            }
        }
        None => {
            v["keys"] = json!(new_keys);
            v["key_count"] = json!(v["keys"].as_array().map(|a| a.len()).unwrap_or(0));
            items.push(v);
            "已新增站点".to_string()
        }
    };
    save(&items);
    (true, msg, id)
}

pub fn remove_site(id: &str) -> (bool, String) {
    let mut items = sites();
    let before = items.len();
    // 先记下这个站有哪些 key，好在删掉之后把保管箱里那几条也一起抹掉
    let doomed: Vec<String> = items
        .iter()
        .filter(|p| p["id"].as_str() == Some(id))
        .filter_map(|p| p["keys"].as_array().cloned())
        .flatten()
        .filter_map(|k| k["id"].as_str().map(secret_id))
        .collect();
    items.retain(|p| p["id"].as_str() != Some(id));
    if items.len() == before {
        return (false, "站点不存在".into());
    }
    save(&items);
    purge_secrets(&doomed);
    (true, "已删除站点".into())
}

/// 删掉 key 时把凭据也从保管箱抹掉，别留孤儿密文
fn purge_secrets(ids: &[String]) {
    if ids.is_empty() {
        return;
    }
    for id in ids {
        secrets::remove(id);
    }
    let _ = secrets::flush();
}

/// 一个站点的接入点是一组（阿里云百炼就是 OpenAI 兼容 / Anthropic 兼容 / DashScope 三种），
/// 挂在**站点**上；key 一般不单独指定，只有确实走别的入口时才在 key 上覆盖一个。
/// 测活、拉模型、注入都从这里取。
pub fn base_for(site: &Value, key: &Value) -> String {
    let own = key["base_url"].as_str().unwrap_or("").trim();
    if !own.is_empty() {
        return own.to_string();
    }
    if let Some(arr) = site["base_urls"].as_array() {
        // 优先 OpenAI 兼容口（/models 探活和注入都最认它）
        for want in ["openai", ""] {
            for b in arr {
                let f = b["format"].as_str().unwrap_or("");
                let u = b["url"].as_str().unwrap_or("").trim();
                if !u.is_empty() && (want.is_empty() || f == want) {
                    return u.to_string();
                }
            }
        }
    }
    String::new()
}

/// 站点的模型清单：站点上那份为主，key 自己那份是补充
pub fn models_for(site: &Value, key: &Value) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for src in [site["models_list"].as_str(), key["models_list"].as_str()] {
        for m in src.unwrap_or("").split([',', '，', '\n', ' ']) {
            let m = m.trim();
            if !m.is_empty() && !out.iter().any(|x| x == m) {
                out.push(m.to_string());
            }
        }
    }
    out
}

/// 新增/更新站点下的某个 key（可改 base_url / 模型 / 额度）。
/// 第三个返回值是这条 key 最终的 id——空 id 表示新建，这里就地补号，
/// 批量登记（一次建多条）不必在前端自己拼时间戳。
pub fn upsert_key(site_id: &str, key: &Value) -> (bool, String, String) {
    let mut kid = key["id"].as_str().unwrap_or("").to_string();
    let mut items = sites();
    let p = match find_site_mut(&mut items, site_id) {
        Some(p) => p,
        None => return (false, "站点不存在".into(), String::new()),
    };
    let mut ks = p["keys"].as_array().cloned().unwrap_or_default();
    let label = key["label"].as_str().unwrap_or("").trim().to_string();
    let mut v = clean_key(key);
    if kid.is_empty() {
        kid = new_id("k");
        v["id"] = json!(kid);
    }

    // 编辑时前端可能把 key 文本留空（列表里始终打码，用户没改就不该清空）
    if v["key"].as_str().unwrap_or("").trim().is_empty() {
        if let Some(old) = ks
            .iter()
            .find(|k| k["id"].as_str() == Some(&kid))
            .and_then(|k| k["key"].as_str().map(String::from))
        {
            v["key"] = json!(old);
        }
    }

    let msg = match ks.iter().position(|k| k["id"].as_str() == Some(&kid)) {
        Some(i) => {
            ks[i] = v;
            "已更新该 key".to_string()
        }
        None => {
            // 同站内同一条 key 原文只留一个条目：识别时连点/重复贴出来的重复，
            // 在落盘这一步就被并掉——盖信息、保留原 id（反代与接入记的账不断）。
            let kt = v["key"].as_str().unwrap_or("").trim().to_string();
            if !kt.is_empty() {
                if let Some(old) = ks
                    .iter()
                    .position(|k| k["key"].as_str().unwrap_or("").trim() == kt)
                {
                    let old_label = ks[old]["label"]
                        .as_str()
                        .unwrap_or("")
                        .trim()
                        .to_string();
                    let oid = ks[old]["id"].as_str().unwrap_or("").to_string();
                    let shown = if old_label.is_empty() { "未命名" } else { &old_label };
                    v["id"] = json!(oid);
                    ks[old] = v;
                    let n = ks.len();
                    p["keys"] = json!(ks);
                    p["key_count"] = json!(n);
                    save(&items);
                    return (
                        true,
                        format!("这条 key 站里已经有了（用途「{shown}」），没重复建，只更新了它的信息"),
                        oid,
                    );
                }
            }
            ks.push(v);
            "已新增该 key".to_string()
        }
    };
    let n = ks.len();
    p["keys"] = json!(ks);
    p["key_count"] = json!(n);
    save(&items);
    let _ = label;
    (true, msg, kid)
}

pub fn remove_key(site_id: &str, key_id: &str) -> (bool, String) {
    let mut items = sites();
    let p = match find_site_mut(&mut items, site_id) {
        Some(p) => p,
        None => return (false, "站点不存在".into()),
    };
    let mut ks = p["keys"].as_array().cloned().unwrap_or_default();
    let before = ks.len();
    ks.retain(|k| k["id"].as_str() != Some(key_id));
    if ks.len() == before {
        return (false, "key 不存在".into());
    }
    let n = ks.len();
    p["keys"] = json!(ks);
    p["key_count"] = json!(n);
    save(&items);
    purge_secrets(&[secret_id(key_id)]);
    (true, "已删除该 key".into())
}

/// 按 key 原文给全台账查重：同一个 key 出现 ≥2 条就算一组（跨站/同站都算）。
/// 保管箱锁着拿不到原文时退回打码串比对——头 6 尾 4 撞车的概率可以忽略，
/// 但界面要说明这时是按打码串比的。只回有重复的组，条数多的在前。
pub fn dedup_scan() -> (bool, Vec<Value>) {
    let locked = !secrets::available();
    let mut groups: Vec<(String, Vec<Value>)> = Vec::new();
    for p in sites() {
        let sid = p["id"].as_str().unwrap_or("").to_string();
        let sname = p["name"].as_str().unwrap_or("").to_string();
        for k in p["keys"].as_array().cloned().unwrap_or_default() {
            let masked = k["masked"].as_str().unwrap_or("").trim().to_string();
            if masked.is_empty() {
                continue; // 空条目连打码串都没有，谈不上重复
            }
            let plain = k["key"].as_str().unwrap_or("").trim().to_string();
            // 分组键：优先原文；锁箱时退打码串（p:/m: 前缀防两把 key 一明一打码时串组）
            let gk = if plain.is_empty() {
                format!("m:{masked}")
            } else {
                format!("p:{plain}")
            };
            let entry = json!({
                "site": sid,
                "site_name": sname,
                "id": k["id"],
                "label": k["label"],
                "masked": masked,
                "left": k["left"],
                "total": k["total"],
                "days_left": k["days_left"],
                "last_status": k["last_status"],
                "use_in_proxy": k["use_in_proxy"],
            });
            match groups.iter_mut().find(|(g, _)| *g == gk) {
                Some((_, es)) => es.push(entry),
                None => groups.push((gk, vec![entry])),
            }
        }
    }
    groups.retain(|(_, es)| es.len() >= 2);
    groups.sort_by(|a, b| b.1.len().cmp(&a.1.len()));
    let out = groups
        .into_iter()
        .map(|(_, es)| {
            let n = es.len();
            let first_masked = es[0]["masked"].as_str().unwrap_or("").to_string();
            json!({ "masked": first_masked, "count": n, "entries": es })
        })
        .collect();
    (locked, out)
}

/// 批量删除：一次读盘写盘，逐条对账。返回 (删掉条数, 没找到的 id)。
pub fn remove_keys_batch(pairs: &[(String, String)]) -> (usize, Vec<String>) {
    let mut items = sites();
    let mut dead: Vec<String> = Vec::new();
    let mut misses: Vec<String> = Vec::new();
    for (sid, kid) in pairs {
        let mut hit = false;
        if let Some(p) = find_site_mut(&mut items, sid) {
            let mut ks = p["keys"].as_array().cloned().unwrap_or_default();
            let before = ks.len();
            ks.retain(|k| k["id"].as_str() != Some(kid.as_str()));
            if ks.len() < before {
                hit = true;
                let n = ks.len();
                p["keys"] = json!(ks);
                p["key_count"] = json!(n);
            }
        }
        if hit {
            dead.push(kid.clone());
        } else {
            misses.push(kid.clone());
        }
    }
    if !dead.is_empty() {
        save(&items);
        purge_secrets(&dead);
    }
    (dead.len(), misses)
}

/// 批量改同几个字段：`patch` 里出现哪些键就改哪些。
///
/// 只认"可以批量改"的字段——**id / key 原文 / masked 一律不许批量动**：
/// 把一批 key 原文改成同一个不是功能，是把一堆号改成同一个的事故。
/// 她 2026-10-17 要的是"像管理购物车那样，批量勾选之后能做单个 key 能做的一切"，
/// 所以这里把单个 key 表单里那几个可改字段全放开了：名称、来源、过期日、备注、模型清单、
/// 是否进反代、接入点覆盖。
pub fn update_keys_batch(pairs: &[(String, String)], patch: &Value) -> (usize, Vec<String>) {
    const ALLOW: &[&str] = &[
        "label", "source", "expires_on", "note", "models_list", "use_in_proxy", "base_url",
    ];
    let mut items = sites();
    let mut n = 0usize;
    let mut misses: Vec<String> = Vec::new();
    for (sid, kid) in pairs {
        let mut hit = false;
        if let Some(p) = find_site_mut(&mut items, sid) {
            let mut ks = p["keys"].as_array().cloned().unwrap_or_default();
            if let Some(k) = ks.iter_mut().find(|k| k["id"].as_str() == Some(kid.as_str())) {
                if let Some(o) = k.as_object_mut() {
                    for f in ALLOW {
                        if let Some(v) = patch.get(*f) {
                            o.insert((*f).to_string(), v.clone());
                        }
                    }
                    hit = true;
                    n += 1;
                }
            }
            let len = ks.len();
            p["keys"] = json!(ks);
            p["key_count"] = json!(len);
        }
        if !hit {
            misses.push(kid.clone());
        }
    }
    if n > 0 {
        save(&items);
    }
    (n, misses)
}

/// 按给定 id 顺序重排一个「带 id 的对象数组」。order 里没提到的条目按原相对顺序
/// 排在后面——前端传漏了、或拖拽那一刻列表刚好变了，最多是顺序不理想，
/// **一条都不会丢**：这里只挪位置，不碰任何字段。
fn apply_order(list: Vec<Value>, order: &[String]) -> Vec<Value> {
    let mut rest = list;
    let mut out = Vec::with_capacity(rest.len());
    for want in order {
        if let Some(i) = rest.iter().position(|x| x["id"].as_str() == Some(want.as_str())) {
            out.push(rest.remove(i));
        }
    }
    out.extend(rest);
    out
}

/// 站点排序：order = 全部站点 id 的新顺序。拖完一次读盘一次写盘。
/// 反代、接入、测活记的账都按站点 id 找人，顺序变了不影响。
pub fn reorder_sites(order: &[String]) -> (bool, String) {
    let mut items = sites();
    if items.is_empty() {
        return (true, "还没有站点，不用排".into());
    }
    let n = items.len();
    items = apply_order(items, order);
    save(&items);
    (true, format!("已按新顺序保存 {} 个站点", n))
}

/// 站点内密钥排序：order = 该站点全部 key id 的新顺序。
pub fn reorder_keys(site_id: &str, order: &[String]) -> (bool, String) {
    let mut items = sites();
    let Some(p) = find_site_mut(&mut items, site_id) else {
        return (false, "没找到这个站点".into());
    };
    let name = p["name"].as_str().unwrap_or("").to_string();
    let mut ks = p["keys"].as_array().cloned().unwrap_or_default();
    if ks.is_empty() {
        return (true, format!("「{name}」里还没有密钥，不用排"));
    }
    let n = ks.len();
    ks = apply_order(ks, order);
    p["keys"] = json!(ks);
    p["key_count"] = json!(n);
    save(&items);
    (true, format!("「{name}」里 {} 条密钥已按新顺序保存", n))
}

/// 把一批 key 挪到另一个站点：原站点删掉、目标站点按**原 id** 追加。
/// 原 id 不变，反代与"接入到程序"那边记的账就不会断。
/// 目标站里已经有同一条 key 原文的，跳过并记进 `dupes`——否则台账里又长一条重复。
pub fn move_keys_batch(
    pairs: &[(String, String)],
    target: &str,
) -> (usize, usize, Vec<String>, Vec<String>) {
    let mut items = sites();
    if !items.iter().any(|p| p["id"].as_str() == Some(target)) {
        return (0, 0, Vec::new(), vec!["目标站点不存在".into()]);
    }
    let mut picked: Vec<Value> = Vec::new();
    let mut misses: Vec<String> = Vec::new();
    for (sid, kid) in pairs {
        let mut taken: Option<Value> = None;
        if let Some(p) = find_site_mut(&mut items, sid) {
            let mut ks = p["keys"].as_array().cloned().unwrap_or_default();
            if let Some(pos) = ks.iter().position(|k| k["id"].as_str() == Some(kid.as_str())) {
                taken = Some(ks.remove(pos));
            }
            let len = ks.len();
            p["keys"] = json!(ks);
            p["key_count"] = json!(len);
        }
        match taken {
            Some(k) => picked.push(k),
            None => misses.push(kid.clone()),
        }
    }
    let mut moved = 0usize;
    let mut dupes: Vec<String> = Vec::new();
    if let Some(t) = find_site_mut(&mut items, target) {
        let mut ks = t["keys"].as_array().cloned().unwrap_or_default();
        for k in picked {
            let kt = k["key"].as_str().unwrap_or("").trim().to_string();
            let dup = !kt.is_empty()
                && ks
                    .iter()
                    .any(|x| x["key"].as_str().unwrap_or("").trim() == kt);
            if dup {
                dupes.push(k["label"].as_str().unwrap_or("未命名").to_string());
                continue;
            }
            ks.push(k);
            moved += 1;
        }
        let len = ks.len();
        t["keys"] = json!(ks);
        t["key_count"] = json!(len);
    }
    if moved > 0 || !misses.is_empty() {
        save(&items);
    }
    (moved, dupes.len(), dupes, misses)
}

/// 用 GET {base}/v1/models 测活；返回 (可用?, 说明, 模型数)
pub fn probe(base_url: &str, key: &str) -> (bool, String, usize) {
    let base = base_url.trim().trim_end_matches('/');
    if base.is_empty() {
        return (false, "未填 Base URL".into(), 0);
    }
    // base 可能已经是 /v1 或更深（如 /openai/v1）、也可能直接给到 /chat/completions；
    // 统一走 models_url 归一（剥动作后缀 + 拼 /models），别在这里自己再拼一套
    let url = match models_url(base) {
        Some(u) => u,
        None => return (false, "未填 Base URL".into(), 0),
    };
    // 测活是随手点的动作，超时压到 10 秒：站点不通时干等 20 秒太磨人
    // （B.ai 连不上那次，光一次测试就把整个后端锁了 20 秒）
    let mut req = ureq::get(&url).timeout(std::time::Duration::from_secs(10));
    if !key.is_empty() {
        req = req.set("Authorization", &format!("Bearer {}", key));
        // Anthropic 系的口认 x-api-key 不认 Bearer；多带一个头对 OpenAI 系没影响
        req = req.set("x-api-key", key);
    }
    match req.call() {
        Ok(resp) => {
            let n = resp
                .into_json::<Value>()
                .ok()
                .and_then(|v| {
                    v["data"].as_array()
                        .or_else(|| v["models"].as_array())   // Gemini 的清单在 models 字段
                        .map(|a| a.len())
                })
                .unwrap_or(0);
            (true, format!("可用（列出 {} 个模型）", n), n)
        }
        Err(ureq::Error::Status(code, _)) => (false, format!("HTTP {}", code), 0),
        Err(e) => (false, friendly_net_err(&e, 10), 0),
    }
}

/// 网络错误翻译成人话：超时（Windows 上表现为 os error 10060）要说清
/// "等久了没连上"，别把系统错误号原样怼到脸上
pub fn friendly_net_err(e: &ureq::Error, secs: u64) -> String {
    let s = e.to_string();
    if s.contains("timed out") || s.contains("timeout") || s.contains("10060") || s.contains("10061") {
        format!("连接超时（{} 秒没连上）——站点可能暂时不可达，稍后再试", secs)
    } else if s.contains("Connection refused") || s.contains("10061") {
        format!("连接被拒——站点没在监听这个端口：{}", s)
    } else {
        s
    }
}

/// 拉上游的模型 ID 清单（「添加账号」的 API 方式里点「从上游获取」用）。
/// 和 probe 走同一条 .../models，只是把 ID 带回来而不只是数个数。
pub fn probe_models(base_url: &str, key: &str) -> (bool, String, Vec<String>) {
    let url = match models_url(base_url) {
        Some(u) => u,
        None => return (false, "未填 Base URL".into(), Vec::new()),
    };
    let mut req = ureq::get(&url).timeout(std::time::Duration::from_secs(20));
    if !key.is_empty() {
        req = req.set("Authorization", &format!("Bearer {}", key));
        // Anthropic 系的口认 x-api-key 不认 Bearer；多带一个头对 OpenAI 系没影响
        req = req.set("x-api-key", key);
    }
    match req.call() {
        Ok(resp) => {
            let ids: Vec<String> = resp
                .into_json::<Value>()
                .ok()
                .and_then(|v| {
                    // OpenAI 系清单在 data，Gemini 在 models；Gemini 的条目名在 "name"
                    let a = v["data"].as_array().or_else(|| v["models"].as_array());
                    a.map(|a| {
                        a.iter()
                            .filter_map(|m| {
                                m["id"].as_str()
                                    .or_else(|| m["name"].as_str())
                                    .map(|s| s.to_string())
                            })
                            .collect::<Vec<_>>()
                    })
                })
                .unwrap_or_default()
                .into_iter()
                // Gemini 的 name 是 "models/gemini-..."，去掉前缀好认
                .map(|s| s.strip_prefix("models/").map(|x| x.to_string()).unwrap_or(s))
                .collect();
            let n = ids.len();
            (
                true,
                if n > 0 {
                    format!("拉到 {n} 个模型")
                } else {
                    "连上了，但没解析出模型清单".into()
                },
                ids,
            )
        }
        Err(ureq::Error::Status(code, _)) => (false, format!("HTTP {code}"), Vec::new()),
        Err(e) => (false, friendly_net_err(&e, 20), Vec::new()),
    }
}

/// 从文档复制来的地址常常已经带上了动作后缀
/// （…/v1/chat/completions、…/responses、…/v1/messages 之类）。
/// 测活和拉模型要的是 API 根：先把这些后缀剥掉再拼 /models，
/// 否则拼出来是 .../chat/completions/v1/models，必然 HTTP 404
/// （她 2026-10-28：带 chat/completions 或 responses 的永远测不通，就这个原因）。
fn strip_action_suffix(mut base: &str) -> &str {
    const ACTIONS: &[&str] = &[
        "/chat/completions", "/responses", "/completions", "/messages", "/embeddings",
    ];
    loop {
        let lower = base.to_ascii_lowercase();
        match ACTIONS.iter().find(|s| lower.ends_with(*s)) {
            Some(s) => base = &base[..base.len() - s.len()],
            None => return base,
        }
    }
}

/// base 可能已经是 /v1 或更深（如 /openai/v1）；统一拼到 .../models
fn models_url(base_url: &str) -> Option<String> {
    let base = strip_action_suffix(base_url.trim()).trim_end_matches('/');
    if base.is_empty() {
        return None;
    }
    let has_version_tail = ["/v1", "/v1beta", "/v1beta1", "/v4"]
        .iter()
        .any(|s| base.ends_with(s));
    Some(if base.ends_with("/models") {
        base.to_string()
    } else if has_version_tail {
        format!("{base}/models")
    } else {
        format!("{base}/v1/models")
    })
}

/// 用该 key 配置的余额来源抓剩余/总额
pub fn fetch_key_quota(item: &Value) -> (Option<f64>, Option<f64>, Option<String>) {
    let meta = json!({
        "token": item["key"].as_str().unwrap_or(""),
        "base_url": item["base_url"].as_str().unwrap_or(""),
        "quota_adapter": item["quota_adapter"].as_str().unwrap_or(""),
        "quota_url": item["quota_url"].as_str().unwrap_or(""),
        "quota_path": item["quota_path"].as_str().unwrap_or(""),
        "quota_headers": item["quota_headers"].as_str().unwrap_or("{}"),
    });
    crate::quota::fetch_quota(&meta)
}

/// 反代 token 池：勾选了「进反代」的 key（label 为空时回退站点名）
pub fn proxy_tokens() -> Vec<(String, String)> {
    let mut out = Vec::new();
    for p in sites() {
        let site = p["name"].as_str().unwrap_or("").to_string();
        if let Some(ks) = p["keys"].as_array() {
            for k in ks {
                if !k["use_in_proxy"].as_bool().unwrap_or(false) {
                    continue;
                }
                let key = k["key"].as_str().unwrap_or("").trim().to_string();
                if key.is_empty() {
                    continue;
                }
                let label = k["label"].as_str().unwrap_or("").trim().to_string();
                let name = if label.is_empty() { site.clone() } else { format!("{}/{}", site, label) };
                out.push((format!("Key/{}", name), key));
            }
        }
    }
    out
}

/// 后台自动抓余额用的 key 清单（站点 id + key 对象）
pub fn all_keys() -> Vec<(String, Value)> {
    let mut out = Vec::new();
    for p in sites() {
        let sid = p["id"].as_str().unwrap_or("").to_string();
        if let Some(ks) = p["keys"].as_array() {
            for k in ks {
                out.push((sid.clone(), k.clone()));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 上游 base 的形态五花八门（有的带 /v1、有的到 /v4、有的直接给了 /models），
    /// 「从上游获取」要能把这些都拼对，否则用户点了只会得到一个 404
    #[test]
    fn models_url_handles_common_base_shapes() {
        assert_eq!(models_url("").as_deref(), None);
        assert_eq!(models_url("   ").as_deref(), None);
        assert_eq!(
            models_url("https://api.openai.com/v1").as_deref(),
            Some("https://api.openai.com/v1/models")
        );
        assert_eq!(
            models_url("https://api.openai.com").as_deref(),
            Some("https://api.openai.com/v1/models")
        );
        assert_eq!(
            models_url("https://api.openai.com/v1/").as_deref(),
            Some("https://api.openai.com/v1/models")
        );
        // 智谱那类 /v4
        assert_eq!(
            models_url("https://open.bigmodel.cn/api/paas/v4").as_deref(),
            Some("https://open.bigmodel.cn/api/paas/v4/models")
        );
        // 已经写到 /models 就别再叠一层
        assert_eq!(
            models_url("https://x.example.com/v1/models").as_deref(),
            Some("https://x.example.com/v1/models")
        );
    }

    /// 从文档复制来的地址带动作后缀时（…/chat/completions、…/responses），
    /// 直接拼 /models 必 404——先剥后缀再拼（2026-10-28 她报的 404 就是这个）
    #[test]
    fn models_url_strips_action_suffixes() {
        assert_eq!(
            models_url("https://maas-api.unisound.com/v1/chat/completions").as_deref(),
            Some("https://maas-api.unisound.com/v1/models")
        );
        assert_eq!(
            models_url("https://api.openai.com/v1/responses").as_deref(),
            Some("https://api.openai.com/v1/models")
        );
        assert_eq!(
            models_url("https://api.anthropic.com/v1/messages").as_deref(),
            Some("https://api.anthropic.com/v1/models")
        );
        // 裸域 + 动作后缀：剥完没有版本尾巴，按缺 /v1 补
        assert_eq!(
            models_url("https://x.example.com/embeddings").as_deref(),
            Some("https://x.example.com/v1/models")
        );
        // Gemini 的版本尾巴
        assert_eq!(
            models_url("https://generativelanguage.googleapis.com/v1beta").as_deref(),
            Some("https://generativelanguage.googleapis.com/v1beta/models")
        );
    }

    /// 把测试数据重定向到临时目录，绝不碰用户真实的 ~/.agent-cockpit/apikeys.json
    fn use_tmp_store(tag: &str) {
        let dir = std::env::temp_dir().join(format!("cockpit-keys-test-{}", tag));
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::create_dir_all(&dir);
        *store_override().lock().unwrap() = Some(dir.join("apikeys.json"));
    }

    /// 两级存储回路：建站 → 加多个 key → 打码展示 → 取原文 → 改 key → 删 key → 删站
    #[test]
    fn nested_store_roundtrip() {
        use_tmp_store("roundtrip");
        let site_id = new_id("p");
        let site = json!({
            "id": site_id, "name": "测试站", "kind": "relay", "keys": []
        });
        assert!(upsert_site(&site).0);

        let k1 = json!({
            "id": new_id("k"), "label": "Pot", "key": "sk-aaaaaaaaaaaaaaaa1111",
            "base_url": "https://api.test.example/openai/v1", "models_list": "LongCat-2.0",
        });
        let k2 = json!({
            "id": new_id("k"), "label": "WorkBuddy", "key": "sk-bbbbbbbbbbbbbbbb2222",
            "base_url": "https://api.test.example/anthropic", "use_in_proxy": true,
        });
        assert!(upsert_key(&site_id, &k1).0);
        assert!(upsert_key(&site_id, &k2).0);

        let sites = list_masked();
        let p = sites.iter().find(|p| p["id"] == json!(site_id)).expect("站点应存在");
        assert_eq!(p["key_count"].as_i64(), Some(2), "同一站点应持有两个 key");

        let ks = p["keys"].as_array().cloned().unwrap_or_default();
        let pot = ks.iter().find(|k| k["label"] == json!("Pot")).unwrap();
        assert_eq!(pot["masked"], json!("sk-aaa••••1111"));
        assert_eq!(pot["models_list"], json!("LongCat-2.0"));

        // 取原文
        let kid = k1["id"].as_str().unwrap();
        assert_eq!(get_key_plain(&site_id, kid), "sk-aaaaaaaaaaaaaaaa1111");

        // 反代池只收勾选的
        assert!(proxy_tokens().iter().any(|(l, t)| l.contains("WorkBuddy") && t == "sk-bbbbbbbbbbbbbbbb2222"));
        assert!(!proxy_tokens().iter().any(|(l, _)| l.contains("Pot")));

        // 修改 key：换 label 与 base_url（同一个 key id，不新增记录）
        let mut upd = k1.clone();
        upd["label"] = json!("QoderCN");
        upd["base_url"] = json!("https://api.test.example/anthropic");
        assert!(upsert_key(&site_id, &upd).0);
        let sites = list_masked();
        let p = sites.iter().find(|p| p["id"] == json!(site_id)).unwrap();
        let ks = p["keys"].as_array().cloned().unwrap_or_default();
        assert_eq!(ks.len(), 2, "更新不应新增记录");
        assert!(ks.iter().any(|k| k["label"] == json!("QoderCN")));

        // 同名站点再提交（无 id）应并进已有站点，且不丢旧 key
        let site_again = json!({
            "id": "", "name": "测试站", "kind": "relay",
            "keys": [json!({ "id": new_id("k"), "label": "TieZ", "key": "sk-cccccccccccccccc3333" })],
        });
        assert!(upsert_site(&site_again).0);
        let all = list_masked();
        let p = all
            .iter()
            .find(|p| p["name"] == json!("测试站"))
            .expect("应仍是一条站点");
        assert_eq!(
            p["id"], json!(site_id),
            "同名提交应并进原站点而不是新建一条；现有站点: {:?}",
            all.iter()
                .map(|x| (x["name"].clone(), x["key_count"].clone()))
                .collect::<Vec<_>>()
        );
        assert_eq!(p["key_count"].as_i64(), Some(3), "并站应合并而非新建站点");

        // 批量登记走的正是"空 id 新建"：后端必须自己补号，不能一句「缺少 key id」把整批挡掉
        let (o_mint, _, minted) = upsert_key(
            &site_id,
            &json!({ "id": "", "label": "批量", "key": "sk-mintaaaaaaaaaa1111" }),
        );
        assert!(o_mint, "空 id 的新 key 应由后端补号建成");
        assert!(!minted.is_empty(), "补出来的 id 要回给调用方，否则没法接着测活");
        assert_eq!(get_key_plain(&site_id, &minted), "sk-mintaaaaaaaaaa1111");
        assert!(remove_key(&site_id, &minted).0);

        // 删除单个 key，站点保留
        assert!(remove_key(&site_id, k2["id"].as_str().unwrap()).0);
        assert_eq!(
            list_masked().iter().find(|p| p["id"] == json!(site_id)).unwrap()["key_count"].as_i64(),
            Some(2)
        );

        assert!(remove_site(&site_id).0);
        assert!(list_masked().iter().all(|p| p["id"] != json!(site_id)));
    }

    /// 接入点挂在站点上：一个站常有 OpenAI / Anthropic / DashScope 三种口，
    /// 默认取 OpenAI 兼容那个，key 自己填了才覆盖
    #[test]
    fn base_url_prefers_the_key_override_then_the_site_openai_entry() {
        let site = json!({
            "models_list": "a, b",
            "base_urls": [
                { "format": "anthropic", "url": "https://x/apps/anthropic" },
                { "format": "openai", "url": "https://x/compatible-mode/v1" },
                { "format": "dashscope", "url": "https://x/api/v1" }
            ]
        });
        assert_eq!(base_for(&site, &json!({})), "https://x/compatible-mode/v1");
        assert_eq!(
            base_for(&site, &json!({ "base_url": "https://mine/v1" })),
            "https://mine/v1",
            "key 上有覆盖就用它"
        );
        // 只有非 OpenAI 口时退回列表里第一条，别返回空
        let only_anth = json!({ "base_urls": [{ "format": "anthropic", "url": "https://y/a" }] });
        assert_eq!(base_for(&only_anth, &json!({})), "https://y/a");
        assert_eq!(base_for(&json!({}), &json!({})), "", "什么都没有就该是空，让调用方去提示");
        // 老数据（只有 key 上的 base_url，站点没记）照旧能用
        assert_eq!(
            base_for(&json!({}), &json!({ "base_url": "https://old/v1" })),
            "https://old/v1"
        );
        assert_eq!(models_for(&site, &json!({})), vec!["a", "b"]);
        assert_eq!(models_for(&site, &json!({ "models_list": "b, c" })), vec!["a", "b", "c"]);
    }

    #[test]
    fn norm_site_and_base() {
        assert_eq!(norm_site("Long Cat"), norm_site("longcat"));
        assert_eq!(norm_site("Aliwelink-API"), norm_site("aliwelink api"));
        // 不同接入点归一到同一站点族
        let a = norm_base("https://api.longcat.chat/openai/v1/chat/completions");
        let b = norm_base("https://api.longcat.chat/anthropic");
        assert_eq!(a, b, "同一 host 的不同接入点应归一: {} vs {}", a, b);
        assert_ne!(a, norm_base("https://api.other.example/v1"));
        assert_ne!(a, norm_base("https://api.longcat.chat:8443/openai/v1"));
    }

    #[test]
    fn mask_short_and_long() {
        assert_eq!(mask("short"), "••••");
        assert_eq!(mask("sk-proj-0000000000000000abcd"), "sk-pro••••abcd");
    }
}
