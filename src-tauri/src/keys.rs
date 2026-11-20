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

/// 「请求」按钮真发对话时用的 User-Agent。
///
/// ureq 自己会报 `ureq/x.y`，有些中转站按 **UA 首 token** 做客户端白名单
/// （星驰 api.xingchiapi.com：只放行 WorkBuddy / Codex / Claude Code 这类编程客户端，
/// 裸脚本与 SDK 直连一律 403 `client_not_allowed`；2026-11-20 实测）。
/// 测试按钮要回答的是「这把 key 在**真正用它**的客户端下能不能通」，
/// 所以 UA 必须和那个客户端一致，否则测出来的 403 是假阴性。
const PROBE_UA: &str = "WorkBuddy/2.5.0";

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
    base_and_fmt(site, key).0
}

/// 同 `base_for`，但连这条接入点登记的兼容格式一起给出来。
///
/// 为什么必须成对返回：`base_for` 挑地址时有优先级（先 `openai`，再第一条），
/// 但「这条地址按哪种协议调用」是另一件事。原先 `chat_probe` 只拿到地址、
/// 一律按 chat/completions 打，站点登记成 `openai-resp` 也当没看见——
/// 星驰API 停用 chat 之后点「请求」必 400（2026-11-20 她的截图）。
/// 所以格式要和地址同源取，不能各挑各的，否则可能拿 A 口的地址配 B 口的协议。
///
/// 返回 (base_url, format)。format 是 `openai` / `openai-resp` / `anthropic` /
/// `gemini` / `other`，老数据没有 format 字段时给空串（调用方按老行为处理）。
pub fn base_and_fmt(site: &Value, key: &Value) -> (String, String) {
    let own = key["base_url"].as_str().unwrap_or("").trim();
    if !own.is_empty() {
        // key 上覆盖了地址，格式依次看：key 自己的 base_format → 地址后缀 → 站点第一条。
        // 地址后缀这一步很要紧：老数据没有 base_format 字段，但地址写成 /v1/responses
        // 本身就说明了要走 Responses（星驰那条就是这么登记的）。
        let kf = key["base_format"].as_str().unwrap_or("").trim();
        let f = if !kf.is_empty() {
            kf.to_string()
        } else if own.trim_end_matches('/').to_ascii_lowercase().ends_with("/responses") {
            "openai-resp".to_string()
        } else {
            site["base_urls"]
                .as_array()
                .and_then(|a| a.first())
                .map(|b| b["format"].as_str().unwrap_or("").trim().to_string())
                .unwrap_or_default()
        };
        return (own.to_string(), f);
    }
    if let Some(arr) = site["base_urls"].as_array() {
        // 优先 OpenAI 兼容口（/models 探活和注入都最认它）。
        // 分三档而不是一档里按数组顺序找：先 `openai`（保住老站点的既有行为，
        // 同时登记 chat 和 resp 的站不该因为 resp 排在前面就换协议），
        // 再 `openai-resp`（只登记 Responses 口的站，星驰就是），最后随便第一条。
        for want in ["openai", "openai-resp", ""] {
            for b in arr {
                let f = b["format"].as_str().unwrap_or("");
                let u = b["url"].as_str().unwrap_or("").trim();
                if !u.is_empty() && (want.is_empty() || f == want) {
                    return (u.to_string(), f.to_string());
                }
            }
        }
    }
    (String::new(), String::new())
}

/// 从地址反推它属于哪种兼容格式。密钥自带接入点时，要靠它判断这条地址
/// 对应矩阵里的哪一列——否则会拿 Responses 的地址去打 chat 的口。
pub fn fmt_of_url(url: &str) -> &'static str {
    let low = url.trim().trim_end_matches('/').to_ascii_lowercase();
    if low.ends_with("/responses") {
        "openai-resp"
    } else if low.ends_with("/messages") {
        "anthropic"
    } else if low.contains("generativelanguage") || low.contains(":generatecontent") {
        "gemini"
    } else {
        "openai"
    }
}

/// 矩阵的列：站点登记的接入点；一个都没登记时（老数据只把地址挂在 key 上），
/// 按密钥自带地址反推出的格式补出来，别让弹窗直接空着。
pub fn matrix_columns(site: &Value) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    if let Some(arr) = site["base_urls"].as_array() {
        for b in arr {
            let u = b["url"].as_str().unwrap_or("").trim();
            if u.is_empty() {
                continue;
            }
            let f = match b["format"].as_str().unwrap_or("").trim() {
                "" => fmt_of_url(u),
                other => other,
            };
            if !out.iter().any(|(x, _)| x == f) {
                out.push((f.to_string(), u.to_string()));
            }
        }
    }
    if out.is_empty() {
        if let Some(ks) = site["keys"].as_array() {
            for k in ks {
                let u = k["base_url"].as_str().unwrap_or("").trim();
                if u.is_empty() {
                    continue;
                }
                let f = fmt_of_url(u);
                if !out.iter().any(|(x, _)| x == f) {
                    out.push((f.to_string(), u.to_string()));
                }
            }
        }
    }
    out
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

/// 匹配档位：完全一致 / 打码一致 / 片段匹配
#[derive(PartialEq, Debug, Clone, Copy)]
pub enum KeyTier {
    Exact,
    Masked,
    Partial,
}

/// 纯函数：判断一把 key 命中哪一档。不碰全局状态，测试能独立跑。
///
/// `query` 是用户原样输入的串（用于全等比较，大小写敏感）；
/// `needle` 是归一后的片段串（小写、无空白，用于包含比较）。
///
/// 规则（从严到宽，先命中先算）：
///   ① 原文全等 —— 最确定，命中即「就是这个」；
///   ② 打码串全等 —— 用户只看到过 `sk-abc••••wxyz` 这种；
///   ③ 片段包含 —— 只记得头尾时的兜底候选。
///
/// 片段一律忽略大小写并去掉空白：key 常被人从 PDF、聊天记录里复制，
/// 中间会带换行和空格。全等比较不做任何归一，否则就不是「同一把」了。
pub fn tier_of(plain: &str, masked: &str, query: &str, needle: &str) -> Option<KeyTier> {
    let plain = plain.trim();
    let masked = masked.trim();
    let query = query.trim();
    if !plain.is_empty() && plain == query {
        return Some(KeyTier::Exact);
    }
    if !masked.is_empty() && masked == query {
        return Some(KeyTier::Masked);
    }
    if needle.is_empty() {
        return None;
    }
    if !plain.is_empty() {
        let norm: String = plain
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect::<String>()
            .to_lowercase();
        if norm.contains(needle) {
            return Some(KeyTier::Partial);
        }
    } else {
        // 锁箱：拿打码串的两头凑，够长（≥8 字符）才认为片段有代表性，
        // 两头都要对上——只靠一头命中太容易误报。
        let parts: Vec<&str> = masked.split("••••").collect();
        if needle.len() >= 8
            && parts.len() >= 2
            && needle.contains(&parts[0].to_lowercase())
            && needle.contains(&parts[1].to_lowercase())
        {
            return Some(KeyTier::Partial);
        }
    }
    None
}

/// 密钥搜索：拿一段 key（完整原文，或只记得头尾/中间片段）去全台账里定位。
///
/// 用户常见场景是「手上有一把 key，不知道它属于哪家、也没记下来保存过没有」。
/// 分三档匹配，从严到宽，详见 `tier_of`。
///
/// 保管箱锁着时拿不到原文（`key` 为空），此时只能退到 ②③ 两档，
/// 界面上要如实说「锁箱，只能按打码串与片段比对」。
pub fn search_keys(query: &str) -> Value {
    let q = query.trim();
    if q.is_empty() {
        return json!({
            "ok": true,
            "locked": !secrets::available(),
            "query": q,
            "exact": [],
            "masked": [],
            "partial": [],
            "total": 0,
        });
    }
    // 片段匹配用的归一串：去掉空白并转小写。
    // 注意「完全一致」不能用它——key 里有大写字母（sk-…ZZ），
    // 拿小写串去比原文永远比不上，会把确定命中降级成片段候选。
    let needle: String = q
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect::<String>()
        .to_lowercase();

    let locked = !secrets::available();
    let mut exact: Vec<Value> = Vec::new();
    let mut by_masked: Vec<Value> = Vec::new();
    let mut partial: Vec<Value> = Vec::new();

    for p in sites() {
        let sid = p["id"].as_str().unwrap_or("").to_string();
        let sname = p["name"].as_str().unwrap_or("").to_string();
        let shome = p["homepage"].as_str().unwrap_or("").to_string();
        let skind = p["kind"].as_str().unwrap_or("").to_string();
        let base = base_for(&p, &Value::Null);
        for k in p["keys"].as_array().cloned().unwrap_or_default() {
            let plain = k["key"].as_str().unwrap_or("").trim().to_string();
            let masked = k["masked"].as_str().unwrap_or("").trim().to_string();
            if plain.is_empty() && masked.is_empty() {
                continue;
            }
            let entry = json!({
                "site": sid,
                "site_name": sname,
                "site_kind": skind,
                "homepage": shome,
                "base_url": base,
                "id": k["id"],
                "label": k["label"],
                "masked": masked,
                "source": k["source"],
                "note": k["note"],
                "expires_on": k["expires_on"],
                "use_in_proxy": k["use_in_proxy"],
                "last_status": k["last_status"],
                "last_check": k["last_check"],
                "models": k["models"],
                "has_plain": !plain.is_empty(),
            });

            // 三档匹配：先严后宽，命中即止（一把 key 只归一档）
            match tier_of(&plain, &masked, q, &needle) {
                Some(KeyTier::Exact) => exact.push(entry),
                Some(KeyTier::Masked) => by_masked.push(entry),
                Some(KeyTier::Partial) => partial.push(entry),
                None => {}
            }
        }
    }

    let total = exact.len() + by_masked.len() + partial.len();
    json!({
        "ok": true,
        "locked": locked,
        "query": q,
        "exact": exact,
        "masked": by_masked,
        "partial": partial,
        "total": total,
    })
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

/// 从上游的错误体里抠一句人话出来。各家写法不一：OpenAI 是 error.message，
/// 网关是 message / detail / title，抠不到就原样截一段，总比只报 HTTP 403 强。
fn upstream_hint(body: &str) -> String {
    let v: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    let pick = |p: &[&str]| -> String {
        let mut cur = &v;
        for k in p {
            cur = &cur[*k];
        }
        cur.as_str().unwrap_or("").trim().to_string()
    };
    let mut said = String::new();
    for path in [
        &["error", "message"][..],
        &["error", "detail"][..],
        &["error", "type"][..],
        &["detail"][..],
        &["message"][..],
        &["error_type"][..],
        &["title"][..],
    ] {
        let s = pick(path);
        if s.is_empty() || said.contains(&s) {
            continue;
        }
        if !said.is_empty() {
            said.push_str(" / ");
        }
        said.push_str(&s);
        if said.chars().count() > 160 {
            break;
        }
    }
    if said.is_empty() {
        said = body.trim().chars().take(120).collect();
    }
    let said: String = said.chars().take(160).collect();
    if said.is_empty() { String::new() } else { format!("；上游说：{said}") }
}

/// 真发一条最小对话，验这把 key 到底能不能用。
///
/// 为什么需要它：`probe()` 打的是 `.../models`，有的站点这个接口**不校验 key**
/// （谁问都给模型清单），于是一把根本用不了的 key 也会显示「可用」——小红书 Dots
/// 就是这么骗过测试的（2026-11-05 实测：假 key 与不带 key 都返回 200，真发对话才 403）。
/// 只有真发一次对话才知道上游认不认这把 key。
///
/// 返回 (可用?, 说明)。说明里带耗时和上游原话，失败时能一眼看出是 401 不认 key、
/// 403 不让调，还是地址压根不是 OpenAI 兼容口。
///
/// `fmt` 是站点上这条接入点登记的兼容格式（UI 的 `sbase-fmt`，见 ui/app.js 的 BASE_FMT）。
/// 2026-11-20 加：**`openai-resp` 必须走 Responses 协议**。星驰API（api.xingchiapi.com）
/// 已经把 `/v1/chat/completions` 整条停用（回 `chat_completions_disabled`，让客户端改用
/// `/v1/responses`），而原先这里一律拼 chat/completions，于是站点明明登记成
/// 「OpenAI Responses」、地址也填的 `/v1/responses/`，点「请求」照样被 400 顶回来
/// （她的截图就是这么来的）。格式登记了却不用，等于没登记。
///
/// 认不出 fmt 时保持老行为（chat/completions），别把没登记格式的站点搞坏。
/// 一次（密钥 × 协议）探测的结构化结果。
///
/// `state` 直接给界面用，取值固定：
///   `ok` 通 / `site_disabled` 站点停用该协议 / `key_denied` 密钥无权 /
///   `model_mismatch` 模型名不被接受 / `error` 其他错误。
///
/// 为什么要单列 `site_disabled`：星驰这类站把 `/v1/chat/completions` 整条停了，
/// 那一列里每把密钥都会失败。若一律标成「密钥无权」，三行全红会被读成
/// 「三把 key 都坏了」，实际是站点没开这个协议——结论完全不同。
pub struct ProbeOutcome {
    pub state: &'static str,
    pub code: u16,
    pub ms: u128,
    pub said: String,
    pub hint: String,
    pub extra: &'static str,
    /// 传输层失败（连不上/超时）：此时 code 为 0，`hint` 是翻译过的人话。
    pub net_err: bool,
}

impl ProbeOutcome {
    fn fail(state: &'static str, code: u16, ms: u128, hint: impl Into<String>) -> Self {
        ProbeOutcome {
            state,
            code,
            ms,
            said: String::new(),
            hint: hint.into(),
            extra: "",
            net_err: false,
        }
    }
}

/// 按 HTTP 状态码与上游原话判失败性质。
///
/// 400 的三种写法要分开：「停用接口」「模型不认」「其他」。前两种的处置完全不同
/// （换协议 / 先取模型清单），糊成一句「请求被拒」等于没说。
fn classify_failure(code: u16, body: &str) -> &'static str {
    let low = body.to_ascii_lowercase();
    let disabled = low.contains("chat_completions_disabled")
        || body.contains("已停用")
        || low.contains("dispatch")
        || low.contains("does not allow")
        || low.contains("not allowed for this");
    let model_bad = low.contains("model")
        && (low.contains("not found")
            || low.contains("does not exist")
            || low.contains("unknown")
            || low.contains("invalid"));
    match code {
        400 => {
            if disabled {
                "site_disabled"
            } else if model_bad {
                "model_mismatch"
            } else {
                "error"
            }
        }
        // 401 鉴权失败、403 不允许调、429 额度用尽：都归到这把 key 身上
        401 | 403 | 429 => "key_denied",
        // 404 多半是这个地址根本没有该协议的接口
        404 => "site_disabled",
        _ => "error",
    }
}

/// 真发一次最小对话，返回结构化结果（矩阵与单点检测共用这一份实现）。
pub fn probe_combo(
    base_url: &str,
    key: &str,
    model: &str,
    fmt: &str,
    timeout_secs: u64,
) -> ProbeOutcome {
    // 地址本身带 /responses 的，就算 fmt 没登记也按 Responses 打——
    // 用户已经把「我要走这个协议」写在地址里了，不该再被改成 chat。
    let wants_responses = fmt.eq_ignore_ascii_case("openai-resp")
        || base_url
            .trim()
            .trim_end_matches('/')
            .to_ascii_lowercase()
            .ends_with("/responses");
    let base = strip_action_suffix(base_url.trim()).trim_end_matches('/');
    let (url, body) = if wants_responses {
        match responses_url(base) {
            Some(u) => (
                u,
                json!({
                    "model": model,
                    "input": "ping",
                    "max_output_tokens": 16,
                    "stream": false,
                }),
            ),
            None => return ProbeOutcome::fail("error", 0, 0, "未填 Base URL"),
        }
    } else {
        match chat_url(base) {
            Some(u) => (
                u,
                json!({
                    "model": model,
                    "messages": [{ "role": "user", "content": "ping" }],
                    "max_tokens": 1,
                    "stream": false,
                }),
            ),
            None => return ProbeOutcome::fail("error", 0, 0, "未填 Base URL"),
        }
    };
    let t0 = std::time::Instant::now();
    let mut req = ureq::post(&url)
        .timeout(std::time::Duration::from_secs(timeout_secs))
        .set("Content-Type", "application/json");
    // ureq 默认 UA 是 `ureq/x.y`，有些中转站按 UA 首 token 放行（星驰只认
    // WorkBuddy / Codex / Claude Code 这类编程客户端，裸脚本一律 403 client_not_allowed）。
    // 检测的意义是「验证这把 key 在这个站能不能用」，不是「验证 ureq 能不能用」，
    // 所以这里声明成 WorkBuddy——和真正调用它的客户端一致，测出来的结论才作数。
    req = req.set("User-Agent", PROBE_UA);
    if !key.is_empty() {
        req = req.set("Authorization", &format!("Bearer {}", key));
        // Anthropic 系的口认 x-api-key 不认 Bearer；多带一个头对 OpenAI 系没影响
        req = req.set("x-api-key", key);
    }
    match req.send_json(body) {
        Ok(resp) => {
            let ms = t0.elapsed().as_millis();
            let txt = resp.into_string().unwrap_or_default();
            let v: Value = serde_json::from_str(&txt).unwrap_or(Value::Null);
            // 两种协议的返回体长得不一样：
            //   chat      -> choices[0].message.content
            //   responses -> output[].content[].text（output_text 类型）
            let said = if wants_responses {
                v["output"]
                    .as_array()
                    .map(|items| {
                        items
                            .iter()
                            .filter_map(|it| it["content"].as_array())
                            .flatten()
                            .filter(|c| c["type"].as_str() == Some("output_text"))
                            .filter_map(|c| c["text"].as_str())
                            .collect::<Vec<_>>()
                            .join(" ")
                    })
                    .unwrap_or_default()
            } else {
                v["choices"][0]["message"]["content"]
                    .as_str()
                    .unwrap_or("")
                    .to_string()
            }
            .replace(['\n', '\r'], " ");
            let said: String = said.trim().chars().take(40).collect();
            ProbeOutcome {
                state: "ok",
                code: 200,
                ms,
                said,
                hint: String::new(),
                extra: "",
                net_err: false,
            }
        }
        Err(ureq::Error::Status(code, resp)) => {
            let txt = resp.into_string().unwrap_or_default();
            let hint = upstream_hint(&txt);
            let extra = match code {
                400 => "：请求被拒，通常是模型名这个接入点不认",
                401 => "：这把 key 上游不认（鉴权失败）",
                403 => "：上游认得出 key，但不允许它调这个模型",
                404 if wants_responses => "：这个地址没有 /responses，确认兼容格式选对了没",
                404 => "：这个地址没有 /chat/completions，可能不是 OpenAI 兼容口",
                429 => "：限流，或这把 key 的额度用完了",
                _ => "",
            };
            ProbeOutcome {
                state: classify_failure(code, &txt),
                code,
                ms: t0.elapsed().as_millis(),
                said: String::new(),
                hint,
                extra,
                net_err: false,
            }
        }
        Err(e) => ProbeOutcome {
            state: "error",
            code: 0,
            ms: t0.elapsed().as_millis(),
            said: String::new(),
            hint: friendly_net_err(&e, timeout_secs),
            extra: "",
            net_err: true,
        },
    }
}

/// 单点检测（「请求」按钮）：沿用原来的两元组与文案，内部走 `probe_combo`。
pub fn chat_probe(base_url: &str, key: &str, model: &str, fmt: &str) -> (bool, String) {
    let o = probe_combo(base_url, key, model, fmt, 20);
    if o.net_err {
        return (false, o.hint);
    }
    if o.state == "ok" {
        let msg = if o.said.is_empty() {
            format!("真请求通过（{}ms）：上游收下了这把 key（模型 {model}）", o.ms)
        } else {
            format!("真请求通过（{}ms）：模型 {model} 回了「{}」", o.ms, o.said)
        };
        (true, msg)
    } else {
        (
            false,
            format!("真请求失败（HTTP {}{}）{}", o.code, o.extra, o.hint),
        )
    }
}

/// 矩阵里的一格：谁 × 哪种协议 × 结果。
pub struct MatrixCell {
    pub key_id: String,
    pub fmt: String,
    pub state: &'static str,
    pub code: u16,
    pub ms: u128,
    pub msg: String,
}

/// 批量探测：把任务摊到固定数量的工作线程上。
///
/// 为什么必须并发：一个 10 密钥 × 3 协议的站是 30 次请求，串行按 20 秒超时算
/// 最坏要等十分钟，界面上就是「点了没反应」。并发上限压在 6，既快又不至于
/// 把中转站的限流打爆。
pub fn probe_matrix(
    jobs: Vec<(String, String, String, String, String)>,
    timeout_secs: u64,
    workers: usize,
) -> Vec<MatrixCell> {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    let total = jobs.len();
    if total == 0 {
        return Vec::new();
    }
    let jobs = std::sync::Arc::new(jobs);
    let cursor = AtomicUsize::new(0);
    let out: Mutex<Vec<Option<MatrixCell>>> = Mutex::new((0..total).map(|_| None).collect());
    let n = workers.clamp(1, total).min(6);

    std::thread::scope(|s| {
        for _ in 0..n {
            s.spawn(|| loop {
                let i = cursor.fetch_add(1, Ordering::SeqCst);
                if i >= total {
                    break;
                }
                let (key_id, fmt, url, key, model) = &jobs[i];
                let o = probe_combo(url, key, model, fmt, timeout_secs);
                let msg = if o.net_err {
                    o.hint.clone()
                } else if o.state == "ok" {
                    if o.said.is_empty() {
                        "连通".to_string()
                    } else {
                        format!("返回「{}」", o.said)
                    }
                } else {
                    let h = o.hint.trim_start_matches("；上游说：");
                    if h.is_empty() {
                        format!("HTTP {}", o.code)
                    } else {
                        format!("HTTP {}：{}", o.code, h)
                    }
                };
                out.lock().unwrap()[i] = Some(MatrixCell {
                    key_id: key_id.clone(),
                    fmt: fmt.clone(),
                    state: o.state,
                    code: o.code,
                    ms: o.ms,
                    msg,
                });
            });
        }
    });

    out.into_inner()
        .unwrap()
        .into_iter()
        .flatten()
        .collect()
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

/// 一个站点常有多个接入点（OpenAI 兼容 / Anthropic 兼容 / DashScope…）。
/// 界面上「获取模型」全站只留一个入口，那它就得把这些口都问一遍——
/// 只问第一口，登记顺序把 Responses 排在前面的站会显示「没拉到模型」，
/// 而其实是那个口拿不到清单、另一个口本来就正常。
///
/// 每个口按密钥顺序试到一把能用的为止：有的站某把密钥只对某个口开放，
/// 从头到尾只用第一把，那一整口都会判死。
/// 返回（去重合并后的模型 id，逐口说明）。
pub fn probe_models_all(site: &Value) -> (Vec<String>, Vec<String>) {
    let mut ids: Vec<String> = Vec::new();
    let mut notes: Vec<String> = Vec::new();

    let keys: Vec<String> = site["keys"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|k| k["key"].as_str())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default();

    // 同一个地址登记两遍（不同格式标签）不该发两次请求
    let mut bases: Vec<(String, String)> = Vec::new();
    for b in site["base_urls"].as_array().into_iter().flatten() {
        let url = b["url"].as_str().unwrap_or("").trim().to_string();
        if url.is_empty() || bases.iter().any(|(u, _)| *u == url) {
            continue;
        }
        bases.push((url, b["format"].as_str().unwrap_or("openai").to_string()));
    }
    if bases.is_empty() {
        return (ids, vec!["这个站点还没登记接入点".into()]);
    }

    for (url, fmt) in &bases {
        let label = match fmt.as_str() {
            "openai" => "OpenAI 兼容".to_string(),
            "anthropic" => "Anthropic 兼容".to_string(),
            "dashscope" => "DashScope".to_string(),
            "openai-resp" => "OpenAI Responses".to_string(),
            other => other.to_string(),
        };
        // 一把密钥都没有时用空 key 试一次：有的站 /models 不校验
        let attempts: Vec<String> = if keys.is_empty() {
            vec![String::new()]
        } else {
            keys.clone()
        };
        let mut got: Option<Vec<String>> = None;
        let mut last = String::new();
        let mut which = 0usize;
        for (i, k) in attempts.iter().enumerate() {
            let (good, msg, list) = probe_models(url, k);
            if good {
                got = Some(list);
                which = i + 1;
                break;
            }
            last = msg;
        }
        match got {
            Some(list) => {
                let n = list.len();
                let mut added = 0;
                for m in list {
                    if !ids.iter().any(|x| *x == m) {
                        ids.push(m);
                        added += 1;
                    }
                }
                let who = if attempts.len() > 1 {
                    format!("（第 {which} 把密钥）")
                } else {
                    String::new()
                };
                notes.push(format!("{label}：{n} 个，新增 {added}{who}"));
            }
            None => notes.push(format!("{label}：{last}")),
        }
    }
    (ids, notes)
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

/// 真请求打的地址：和 models_url 一套规矩（先剥动作后缀，再按有没有版本尾巴补）
fn chat_url(base_url: &str) -> Option<String> {
    let base = strip_action_suffix(base_url.trim()).trim_end_matches('/');
    if base.is_empty() {
        return None;
    }
    let has_version_tail = ["/v1", "/v1beta", "/v1beta1", "/v4"]
        .iter()
        .any(|s| base.ends_with(s));
    Some(if base.ends_with("/chat/completions") {
        base.to_string()
    } else if has_version_tail {
        format!("{base}/chat/completions")
    } else {
        format!("{base}/v1/chat/completions")
    })
}

/// Responses 协议真请求的地址（站点兼容格式登记成 `openai-resp` 时走这里）。
/// 规矩和 chat_url 一样：先剥动作后缀，再看有没有版本尾巴。
/// 2026-11-20 加，起因是星驰API 停用了 /v1/chat/completions。
fn responses_url(base_url: &str) -> Option<String> {
    let base = strip_action_suffix(base_url.trim()).trim_end_matches('/');
    if base.is_empty() {
        return None;
    }
    let has_version_tail = ["/v1", "/v1beta", "/v1beta1", "/v4"]
        .iter()
        .any(|s| base.ends_with(s));
    Some(if base.ends_with("/responses") {
        base.to_string()
    } else if has_version_tail {
        format!("{base}/responses")
    } else {
        format!("{base}/v1/responses")
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
    /// 真请求的地址：跟 models_url 同一套归一规则，别再叠一层动作后缀
    #[test]
    fn chat_url_normalizes_the_same_way() {
        assert_eq!(
            chat_url("https://api.senseaudio.cn/v1").as_deref(),
            Some("https://api.senseaudio.cn/v1/chat/completions")
        );
        assert_eq!(
            chat_url("https://note3-prev-api.askdiandian.com").as_deref(),
            Some("https://note3-prev-api.askdiandian.com/v1/chat/completions")
        );
        assert_eq!(
            chat_url("https://x.example.com/v1/chat/completions").as_deref(),
            Some("https://x.example.com/v1/chat/completions")
        );
        assert_eq!(chat_url("   ").as_deref(), None);
    }

    /// Responses 协议的地址归一：登记成 openai-resp 的站要打 /responses，
    /// 不能被打成 /chat/completions（星驰API 已停用 chat，2026-11-20）。
    #[test]
    fn responses_url_normalizes_like_chat() {
        // 星驰截图里的地址：带尾斜杠 + 已经带 /responses
        assert_eq!(
            responses_url("https://api.xingchiapi.com/v1/responses/").as_deref(),
            Some("https://api.xingchiapi.com/v1/responses"),
            "已经带 /responses 的不许再叠一层"
        );
        assert_eq!(
            responses_url("https://api.xingchiapi.com/v1").as_deref(),
            Some("https://api.xingchiapi.com/v1/responses")
        );
        assert_eq!(
            responses_url("https://api.example.com").as_deref(),
            Some("https://api.example.com/v1/responses")
        );
        // 关键：chat 的地址交给 responses_url，必须换成 /responses 而不是原样返回
        assert_eq!(
            responses_url("https://api.example.com/v1/chat/completions").as_deref(),
            Some("https://api.example.com/v1/responses")
        );
        assert_eq!(responses_url("   ").as_deref(), None);
    }

    /// 地址和格式必须同源：只登记了 Responses 口的站，base_for 不能掉到
    /// 「随便第一条」去，否则拿 A 口的地址配 B 口的协议。
    #[test]
    fn base_and_fmt_pairs_url_with_its_format() {
        // 星驰的实际情况：只有一条 openai-resp
        let only_resp = json!({
            "base_urls": [{ "format": "openai-resp", "url": "https://api.xingchiapi.com/v1/responses" }]
        });
        assert_eq!(
            base_and_fmt(&only_resp, &json!({})),
            (
                "https://api.xingchiapi.com/v1/responses".to_string(),
                "openai-resp".to_string()
            ),
            "只登记 Responses 口时也要选中它，并带回格式"
        );

        // 同时有 chat 和 resp：优先 chat（老行为不变），格式跟着地址走
        let both = json!({
            "base_urls": [
                { "format": "openai-resp", "url": "https://x/v1/responses" },
                { "format": "openai", "url": "https://x/v1" },
            ]
        });
        assert_eq!(
            base_and_fmt(&both, &json!({})),
            ("https://x/v1".to_string(), "openai".to_string())
        );

        // key 上覆盖地址时，格式取 key 自己的
        assert_eq!(
            base_and_fmt(
                &only_resp,
                &json!({ "base_url": "https://mine/v1", "base_format": "openai-resp" })
            ),
            ("https://mine/v1".to_string(), "openai-resp".to_string())
        );
        // key 覆盖地址但没写格式：退回站点第一条格式
        assert_eq!(
            base_and_fmt(&only_resp, &json!({ "base_url": "https://mine/v1" })),
            ("https://mine/v1".to_string(), "openai-resp".to_string())
        );

        // 空站点
        assert_eq!(
            base_and_fmt(&json!({}), &json!({})),
            (String::new(), String::new())
        );
        // base_for 与 base_and_fmt 的地址必须一致（前者是后者的薄包装）
        assert_eq!(base_for(&only_resp, &json!({})), "https://api.xingchiapi.com/v1/responses");
    }

    /// 协议判定的核心：fmt=openai-resp 或地址自带 /responses 都要走 Responses。
    /// 这条规则写在 chat_probe 里，抽出来单测，避免真发网络请求。
    #[test]
    fn wants_responses_covers_format_and_url() {
        let wants = |fmt: &str, url: &str| {
            fmt.eq_ignore_ascii_case("openai-resp")
                || url.trim().trim_end_matches('/').to_ascii_lowercase().ends_with("/responses")
        };
        assert!(wants("openai-resp", "https://api.xingchiapi.com/v1"));
        assert!(wants("OpenAI-Resp", "https://x/v1"), "格式名大小写不该影响判断");
        assert!(wants("", "https://api.xingchiapi.com/v1/responses/"), "地址写明了就照办");
        assert!(!wants("openai", "https://x/v1"), "普通 chat 站保持老行为");
        assert!(!wants("", "https://x/v1"));
    }

    /// 失败归因必须把「站点停用这个协议」和「这把密钥不行」分开。
    /// 混成一句「请求失败」，三行全红的矩阵会被读成「三把 key 都坏了」。
    #[test]
    fn classify_failure_separates_protocol_from_key() {
        assert_eq!(
            classify_failure(
                400,
                r#"{"error":{"code":"chat_completions_disabled","message":"本站已停用 /v1/chat/completions"}}"#
            ),
            "site_disabled"
        );
        assert_eq!(
            classify_failure(400, r#"{"error":{"message":"This group does not allow /v1/messages dispatch"}}"#),
            "site_disabled"
        );
        assert_eq!(
            classify_failure(400, r#"{"error":{"message":"model gpt-x not found"}}"#),
            "model_mismatch"
        );
        assert_eq!(classify_failure(400, r#"{"error":{"message":"bad request"}}"#), "error");
        assert_eq!(classify_failure(401, "unauthorized"), "key_denied");
        assert_eq!(classify_failure(403, "forbidden"), "key_denied");
        assert_eq!(classify_failure(429, "slow down"), "key_denied");
        assert_eq!(classify_failure(404, ""), "site_disabled");
        assert_eq!(classify_failure(500, ""), "error");
    }

    /// 密钥自带接入点时，要靠地址反推它属于矩阵的哪一列；
    /// 推错就会拿 Responses 的地址去打 chat 的口。
    #[test]
    fn fmt_of_url_reads_the_protocol_from_the_path() {
        assert_eq!(fmt_of_url("https://api.xingchiapi.com/v1/responses"), "openai-resp");
        assert_eq!(fmt_of_url("https://api.xingchiapi.com/v1/responses/"), "openai-resp");
        assert_eq!(fmt_of_url("https://x/v1/messages"), "anthropic");
        assert_eq!(fmt_of_url("https://x/v1/chat/completions"), "openai");
        assert_eq!(fmt_of_url("https://x/openai/v1"), "openai");
        assert_eq!(fmt_of_url("https://generativelanguage.googleapis.com/v1beta"), "gemini");
    }

    /// 列来自站点登记的接入点；站点没登记时退回密钥自带地址，别让弹窗空着。
    #[test]
    fn matrix_columns_prefers_site_then_falls_back_to_keys() {
        let site = json!({"base_urls": [
            {"format": "openai-resp", "url": "https://a/v1/responses"},
            {"format": "openai",      "url": "https://a/v1"},
            {"format": "openai",      "url": "https://a/v1-duplicate"},
            {"format": "anthropic",   "url": "  "}
        ]});
        let cols = matrix_columns(&site);
        assert_eq!(cols.len(), 2, "同一格式只占一列，空地址要跳过");
        assert_eq!(cols[0], ("openai-resp".to_string(), "https://a/v1/responses".to_string()));
        assert_eq!(cols[1], ("openai".to_string(), "https://a/v1".to_string()));

        let only_keys = json!({"keys": [
            {"base_url": "https://b/v1/responses"},
            {"base_url": ""},
            {"base_url": "https://b/v1/responses"}
        ]});
        let c2 = matrix_columns(&only_keys);
        assert_eq!(c2.len(), 1);
        assert_eq!(c2[0].0, "openai-resp");

        assert!(matrix_columns(&json!({})).is_empty());
    }

    /// 上游报错的写法五花八门：网关一套、OpenAI 一套，抠不出 JSON 时也得截一段给看。
    /// 只有「HTTP 403」三个字等于没说——她要的是「到底谁不放行」。
    #[test]
    fn upstream_hint_reads_both_error_shapes() {
        let gateway = r#"{"title":"Request processing error","status":403,"detail":"The request could not be completed.","error_type":"governance.dots_platform_key_not_allowed"}"#;
        let h = upstream_hint(gateway);
        assert!(h.contains("could not be completed"), "网关 detail 要认: {h}");
        assert!(h.contains("dots_platform_key_not_allowed"), "错误类型要认: {h}");

        let openai = r#"{"error":{"message":"Incorrect API key provided","type":"invalid_request_error"}}"#;
        let h = upstream_hint(openai);
        assert!(h.contains("Incorrect API key provided"), "OpenAI 形状要认: {h}");

        assert!(upstream_hint("plain text boom").contains("plain text boom"));
        assert_eq!(upstream_hint(""), "");
    }

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

    /// 找密钥：三档匹配（原文全等 / 打码全等 / 片段包含）各归各的档，
    /// 不能把「片段命中」混进「完全一致」——那会让用户拿候选当定论去用。
    /// 这里只测纯匹配规则，不碰全局存储：并行跑测试时 store_override 是共享的，
    /// 一旦两个用例各写各的临时目录就会互相覆盖。
    #[test]
    fn tier_of_matches_in_three_tiers() {
        let full = "sk-abcdef0123456789ZZ";
        let masked = "sk-abc••••78ZZ";
        // 片段串是归一过的（小写无空白）；query 是用户原样输入
        let q = full.to_string();
        let needle = "abcdef0123".to_string();

        // ① 完整原文 → 完全一致
        assert_eq!(
            tier_of(full, masked, &q, &needle),
            Some(KeyTier::Exact)
        );
        // ② 打码形式 → 打码一致（不是完全一致）
        assert_eq!(
            tier_of(full, masked, masked, &needle),
            Some(KeyTier::Masked)
        );
        // ③ 片段 → 片段匹配，且不能同时算作完全一致
        assert_eq!(
            tier_of(full, masked, "abcdef0123", &needle),
            Some(KeyTier::Partial)
        );
        // 查无此 key → 不命中
        assert_eq!(
            tier_of(full, masked, "sk-does-not-exist", "sk-does-not-exist"),
            None
        );
        // 空输入 → 不命中
        assert_eq!(tier_of(full, masked, "", ""), None);
    }

    /// 大写字母的 key 必须还能被「完全一致」认出来：
    /// 全等比较若拿小写归一串去比原文，永远比不上，会把确定命中降级成片段候选。
    #[test]
    fn tier_of_exact_is_case_sensitive_on_the_original() {
        let full = "sk-abcdef0123456789ZZ";
        let masked = "sk-abc••••78ZZ";
        let needle = "sk-abcdef0123456789zz".to_string(); // 归一后的小写
        assert_eq!(
            tier_of(full, masked, full, &needle),
            Some(KeyTier::Exact),
            "原文全等应按用户原样输入比对，不能用小写归一串"
        );
    }

    /// 锁箱时只有打码串：片段要两头都对上才算命中，
    /// 只靠一头（4 个字符满地都是）会误报一片。
    #[test]
    fn tier_of_locked_needs_both_ends() {
        let masked = "sk-abc••••78ZZ";
        // 两头都有 → 命中
        assert_eq!(
            tier_of("", masked, "sk-abc78zz", "sk-abc78zz"),
            Some(KeyTier::Partial)
        );
        // 只有一头 → 不命中
        assert_eq!(tier_of("", masked, "sk-abc", "sk-abc"), None);
        assert_eq!(tier_of("", masked, "78zz", "78zz"), None);
        // 片段太短 → 不命中
        assert_eq!(tier_of("", masked, "skabc78zz", "skabc78zz"), None);
    }

    /// 片段比对忽略大小写与空白：key 常从 PDF、聊天记录里复制，中间带换行空格。
    #[test]
    fn tier_of_ignores_case_and_whitespace() {
        let full = "sk-abcdef0123456789ZZ";
        let masked = "sk-abc••••78ZZ";
        assert_eq!(
            tier_of(full, masked, "ABCDEF0123", "abcdef0123"),
            Some(KeyTier::Partial)
        );
        assert_eq!(
            tier_of(full, masked, "abcdef 0123", "abcdef0123"),
            Some(KeyTier::Partial)
        );
    }
}
