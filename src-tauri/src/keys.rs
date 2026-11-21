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
            if let Some(fields) = k.as_object_mut() { fields.remove("key"); }
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
                    merged[pos] = merge_key_edit(Some(&merged[pos]), &nk);
                } else {
                    merged.push(merge_key_edit(None, &nk));
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
    } else if low.ends_with("/messages") || low.split('/').any(|s| s == "anthropic") {
        "anthropic"
    } else if low.contains("generativelanguage") || low.contains(":generatecontent")
        || low.ends_with("/v1beta") || low.ends_with("/v1beta/models")
    {
        "gemini"
    } else {
        "openai"
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProbeEndpoint {
    pub id: String,
    pub fmt: String,
    pub url: String,
}

pub fn endpoint_id(fmt: &str, url: &str) -> String {
    let fmt = if fmt.trim().is_empty() { fmt_of_url(url) } else { fmt.trim() };
    format!("{}|{}", fmt, url.trim().trim_end_matches('/'))
}

/// 身份包含协议和完整接入点，不能把同协议的不同根地址折叠。
pub fn matrix_endpoints(site: &Value) -> Vec<ProbeEndpoint> {
    let mut out: Vec<ProbeEndpoint> = Vec::new();
    let mut add = |fmt: &str, url: &str| {
        let url = url.trim().trim_end_matches('/');
        if url.is_empty() {
            return;
        }
        let fmt = if fmt.trim().is_empty() { fmt_of_url(url) } else { fmt.trim() };
        let id = endpoint_id(fmt, url);
        if !out.iter().any(|e| e.id == id) {
            out.push(ProbeEndpoint { id, fmt: fmt.into(), url: url.into() });
        }
    };
    for b in site["base_urls"].as_array().into_iter().flatten() {
        let fmt = b["base_format"].as_str().filter(|s| !s.trim().is_empty())
            .or_else(|| b["format"].as_str()).unwrap_or("");
        add(fmt, b["url"].as_str().unwrap_or(""));
    }
    for k in site["keys"].as_array().into_iter().flatten() {
        add(k["base_format"].as_str().unwrap_or(""), k["base_url"].as_str().unwrap_or(""));
    }
    out
}

/// 站点明确登记的地址是共享端点；只登记在某把密钥上的地址不向其他密钥发送请求。
pub fn endpoint_applies(site: &Value, key: &Value, endpoint: &ProbeEndpoint) -> bool {
    let shared = site["base_urls"].as_array().into_iter().flatten().any(|base| {
        let url = base["url"].as_str().unwrap_or("");
        let fmt = base["base_format"].as_str().filter(|s| !s.trim().is_empty())
            .or_else(|| base["format"].as_str()).unwrap_or("");
        !url.trim().is_empty() && endpoint_id(fmt, url) == endpoint.id
    });
    let own = key["base_url"].as_str().unwrap_or("");
    shared || (!own.trim().is_empty() && endpoint_id(key["base_format"].as_str().unwrap_or(""), own) == endpoint.id)
}

/// 兼容旧调用方，但不再按格式去重。
pub fn matrix_columns(site: &Value) -> Vec<(String, String)> {
    matrix_endpoints(site).into_iter().map(|e| (e.fmt, e.url)).collect()
}

fn append_models(out: &mut Vec<String>, source: &Value) {
    let models: Vec<&str> = if let Some(s) = source.as_str() {
        s.split(|c: char| c == ',' || c == '，' || c.is_whitespace()).collect()
    } else {
        source.as_array().into_iter().flatten().filter_map(Value::as_str).collect()
    };
    for model in models {
        let model = model.trim();
        if !model.is_empty() && !out.iter().any(|m| m == model) {
            out.push(model.to_string());
        }
    }
}

/// 精确目录一旦存在，就以该次结果为准；空、错误、过期目录不能借用其他权限。
pub fn models_for_endpoint(_site: &Value, key: &Value, endpoint: &str) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(catalog) = key["model_catalogs"].as_array()
        .and_then(|a| a.iter().rev().find(|c| c["endpoint_id"].as_str() == Some(endpoint)))
    {
        if catalog["state"].as_str() == Some("ok") {
            append_models(&mut out, &catalog["models"]);
        }
        return out;
    }
    append_models(&mut out, &key["models_list"]);
    out
}

/// 注入优先使用本 Key 的目录与手工清单，只有完全没有 Key 记录才兼容站点旧数据。
pub fn models_for(site: &Value, key: &Value) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(catalogs) = key["model_catalogs"].as_array() {
        for (i, catalog) in catalogs.iter().enumerate() {
            let Some(id) = catalog["endpoint_id"].as_str().filter(|s| !s.is_empty()) else { continue; };
            if catalogs[i + 1..].iter().any(|c| c["endpoint_id"].as_str() == Some(id)) {
                continue;
            }
            if catalog["state"].as_str() == Some("ok") {
                append_models(&mut out, &catalog["models"]);
            }
        }
    }
    append_models(&mut out, &key["models_list"]);
    if key.get("model_catalogs").is_none() && key.get("models_list").is_none() {
        append_models(&mut out, &site["models_list"]);
    }
    out
}

/// 编辑表单可省略目录和凭据，但凭据、接入点或手工模型变更会使旧发现失效。
fn merge_key_edit(old: Option<&Value>, incoming: &Value) -> Value {
    let mut merged = old.cloned().unwrap_or_else(|| json!({}));
    for (field, value) in clean_key(incoming).as_object().into_iter().flatten() {
        if field == "key" && value.as_str().unwrap_or("").trim().is_empty() && old.is_some() {
            continue;
        }
        if field == "model_catalogs" && old.is_some()
            && (value.is_null() || value.as_array().map(|a| a.is_empty()).unwrap_or(false))
        {
            continue;
        }
        merged[field] = value.clone();
    }
    let changed = old.map(|old| ["key", "base_url", "base_format", "models_list"]
        .iter().any(|field| old[*field] != merged[*field])).unwrap_or(true);
    if changed {
        merged["probe_revision"] = json!(new_id("probe"));
        if let Some(catalogs) = merged["model_catalogs"].as_array_mut() {
            for catalog in catalogs.iter_mut().filter(|c| c.is_object()) {
                catalog["state"] = json!("stale");
                catalog["msg"] = json!("密钥或探测设置已变更，请重新获取模型");
            }
        }
    } else if let Some(revision) = old.and_then(|o| o.get("probe_revision")) {
        merged["probe_revision"] = revision.clone();
    }
    merged
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
            ks[i] = merge_key_edit(Some(&ks[i]), &v);
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
                    ks[old] = merge_key_edit(Some(&ks[old]), &v);
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
            ks.push(merge_key_edit(None, &v));
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
        "label", "source", "expires_on", "note", "models_list", "use_in_proxy", "base_url", "base_format",
    ];
    let mut items = sites();
    let mut n = 0usize;
    let mut misses: Vec<String> = Vec::new();
    for (sid, kid) in pairs {
        let mut hit = false;
        if let Some(p) = find_site_mut(&mut items, sid) {
            let mut ks = p["keys"].as_array().cloned().unwrap_or_default();
            if let Some(k) = ks.iter_mut().find(|k| k["id"].as_str() == Some(kid.as_str())) {
                let mut edit = json!({});
                for f in ALLOW {
                    if let Some(v) = patch.get(*f) {
                        edit[*f] = v.clone();
                    }
                }
                *k = merge_key_edit(Some(k), &edit);
                hit = true;
                n += 1;
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

/// 模型清单测活也必须验证协议结构，不能把 HTML 或错误对象算作可用。
pub fn probe(base_url: &str, key: &str) -> (bool, String, usize) {
    let cancel = std::sync::atomic::AtomicBool::new(false);
    let result = discover_endpoint_models(base_url, key, fmt_of_url(base_url), &cancel, 10);
    (matches!(result.state, "ok" | "empty"), result.msg, result.models.len())
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
/// 未填 fmt 时按地址推断；明确登记但不支持的格式不冒充其他协议。
/// 一次（密钥 × 协议）探测的结构化结果。
///
/// `state` 直接给界面用，取值固定：
///   `ok` 通 / `site_disabled` 站点停用该协议 / `key_denied` 密钥无权 /
///   `model_mismatch` 模型名不被接受 / `rate_limited` 限流 /
///   `incomplete` Responses 未完成 / `error` 其他错误。
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
    if code == 429 || low.contains("rate_limit_exceeded") || low.contains("rate_limit_error") {
        return "rate_limited";
    }
    let scoped = ["this group", "your group", "group does", "group is", "group_id", "group_disabled",
        "group_not_allowed", "for group", "\"group\"", "this key", "your key", "api key", "api_key",
        "key_id", "key_disabled", "key has", "key is", "\"key\"", "this token", "your token", "token_",
        "\"token\"", "account", "分组", "密钥", "令牌", "账户", "账号"]
        .iter().any(|s| low.contains(s));
    let disabled = ["chat_completions_disabled", "responses_disabled", "protocol_disabled",
        "endpoint_disabled", "disabled for all", "本站已停用", "全站停用", "站点已停用"]
        .iter().any(|s| low.contains(s))
        || (low.contains("disabled") && ["site-wide", "sitewide", "globally", "entire site"]
            .iter().any(|s| low.contains(s)));
    if disabled && !scoped && !["group", "for this", "for your", "user", "用户"]
        .iter().any(|s| low.contains(s))
    {
        return "site_disabled";
    }
    let model_bad = low.contains("model") && ["not found", "does not exist", "unknown",
        "invalid", "model_not_found", "unsupported model", "not supported"]
        .iter().any(|s| low.contains(s));
    if model_bad && !scoped {
        return "model_mismatch";
    }
    if matches!(code, 401 | 403) || scoped || low.contains("permission_denied")
        || low.contains("not allowed for this") || low.contains("does not allow")
    {
        return "key_denied";
    }
    "error"
}

fn protocol_format(fmt: &str, url: &str) -> String {
    if fmt.trim().is_empty() { fmt_of_url(url).into() } else { fmt.trim().to_ascii_lowercase() }
}

fn protocol_auth(req: ureq::Request, key: &str, fmt: &str) -> ureq::Request {
    let req = req.set("User-Agent", PROBE_UA).set("Accept", "application/json");
    match fmt {
        "anthropic" => req.set("x-api-key", key).set("anthropic-version", "2023-06-01"),
        "gemini" => req.set("x-goog-api-key", key),
        _ => req.set("Authorization", &format!("Bearer {}", key)),
    }
}

fn url_component(value: &str) -> String {
    let mut out = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

fn redact_probe_text(text: &str, key: &str) -> String {
    if key.is_empty() {
        return text.to_string();
    }
    let mut text = text.replace(key, "[已隐藏凭据]");
    if let Ok(escaped) = serde_json::to_string(key) {
        let escaped = escaped.trim_matches('"');
        if !escaped.is_empty() {
            text = text.replace(escaped, "[已隐藏凭据]");
        }
    }
    text.replace(&url_component(key), "[已隐藏凭据]")
}

fn safe_upstream_hint(body: &str, key: &str) -> String {
    // 先解码再序列化，防止上游用 Unicode 转义回显凭据后绕过脱敏与截断。
    let decoded = serde_json::from_str::<Value>(body).map(|v| v.to_string())
        .unwrap_or_else(|_| body.to_string());
    upstream_hint(&redact_probe_text(&decoded, key))
}

fn read_probe_body(resp: ureq::Response) -> Result<String, String> {
    use std::io::Read;
    if resp.header("Content-Type").unwrap_or("").to_ascii_lowercase().contains("text/html") {
        return Err("上游返回 HTML 页面，不是 API JSON 响应".into());
    }
    const LIMIT: u64 = 2 * 1024 * 1024;
    let mut text = String::new();
    resp.into_reader().take(LIMIT + 1).read_to_string(&mut text)
        .map_err(|_| "读取上游响应失败".to_string())?;
    if text.len() as u64 > LIMIT {
        return Err("上游响应超过安全大小限制".into());
    }
    Ok(text)
}

fn valid_probe_base(base: &str) -> bool {
    let base = base.trim();
    let rest = base.strip_prefix("http://").or_else(|| base.strip_prefix("https://"));
    rest.map(|rest| {
        let host = rest.split('/').next().unwrap_or("");
        !host.is_empty() && !host.contains('@') && !base.contains(['?', '#'])
            && !base.chars().any(char::is_whitespace)
    }).unwrap_or(false)
}

fn protocol_error(v: &Value) -> bool {
    v.get("error").map(|e| !e.is_null()).unwrap_or(false)
        || v["type"].as_str() == Some("error") || v["object"].as_str() == Some("error")
        || v["success"].as_bool() == Some(false)
        || matches!(v["status"].as_str(), Some("error" | "failed"))
        || ["status", "code"].iter().any(|field| v[*field].as_u64().map(|n| n >= 400).unwrap_or(false))
        || v["error_code"].as_i64().map(|n| n != 0).unwrap_or(false)
}

/// 返回的成功必须由协议对象和终止状态共同证明，而不取决于是否有文字。
fn parse_probe_success(fmt: &str, v: &Value) -> Result<String, &'static str> {
    if !v.is_object() || protocol_error(v) {
        return Err("上游返回错误或非协议对象");
    }
    let mut text = Vec::new();
    match fmt {
        "openai" => {
            if v["object"].as_str() != Some("chat.completion") {
                return Err("响应不是 Chat Completions 对象");
            }
            let choices = v["choices"].as_array().filter(|a| !a.is_empty())
                .ok_or("Chat 响应缺少 choices")?;
            for choice in choices {
                let message = &choice["message"];
                let finish = choice["finish_reason"].as_str().unwrap_or("");
                if message["role"].as_str() != Some("assistant")
                    || !matches!(finish, "stop" | "length" | "tool_calls" | "function_call")
                {
                    return Err("Chat 响应结构或结束状态不合法");
                }
                let tools = message["tool_calls"].as_array().map(|calls| !calls.is_empty()
                    && calls.iter().all(|call| call["type"] == "function"
                        && call["function"]["name"].is_string() && call["function"]["arguments"].is_string()))
                    .unwrap_or(false);
                let function = message["function_call"]["name"].is_string()
                    && message["function_call"]["arguments"].is_string();
                if (finish == "tool_calls" && !tools) || (finish == "function_call" && !function) {
                    return Err("Chat 工具调用结构错误");
                }
                match message.get("content") {
                    Some(Value::String(s)) => text.push(s.as_str()),
                    Some(Value::Null) | None if finish == "length" || tools || function => {},
                    _ => return Err("Chat 响应缺少合法消息内容"),
                }
            }
        }
        "openai-resp" => {
            if v["object"].as_str() != Some("response") || v["id"].as_str().unwrap_or("").is_empty()
                || v["status"].as_str() != Some("completed")
            {
                return Err("Responses 未完成或不是合法 response 对象");
            }
            let output = v["output"].as_array().ok_or("Responses 缺少 output 数组")?;
            for item in output {
                match item["type"].as_str().unwrap_or("") {
                    "message" => {
                        if item["role"].as_str() != Some("assistant")
                            || item["status"].as_str().map(|s| s != "completed").unwrap_or(false)
                        {
                            return Err("Responses 消息未完成或角色不合法");
                        }
                        for part in item["content"].as_array().ok_or("Responses 消息缺少 content")? {
                            match part["type"].as_str().unwrap_or("") {
                                "output_text" => text.push(part["text"].as_str().ok_or("Responses 文本结构错误")?),
                                "refusal" if part["refusal"].is_string() => {},
                                _ => return Err("Responses 内容结构错误"),
                            }
                        }
                    }
                    "reasoning" if item["summary"].is_array() => {},
                    "function_call" if item["name"].is_string() && item["arguments"].is_string() => {},
                    _ => return Err("Responses output 不是合法协议条目"),
                }
            }
        }
        "anthropic" => {
            if v["type"].as_str() != Some("message") || v["role"].as_str() != Some("assistant")
                || v["id"].as_str().unwrap_or("").is_empty()
                || !matches!(v["stop_reason"].as_str().unwrap_or(""),
                    "end_turn" | "max_tokens" | "stop_sequence" | "tool_use" | "pause_turn")
            {
                return Err("响应不是已结束的 Anthropic message 对象");
            }
            for part in v["content"].as_array().ok_or("Anthropic 响应缺少 content")? {
                match part["type"].as_str().unwrap_or("") {
                    "text" => text.push(part["text"].as_str().ok_or("Anthropic 文本结构错误")?),
                    "thinking" if part["thinking"].is_string() => {},
                    "redacted_thinking" if part["data"].is_string() => {},
                    "tool_use" if part["name"].is_string() && part["input"].is_object() => {},
                    _ => return Err("Anthropic 内容结构错误"),
                }
            }
        }
        "gemini" => {
            let candidates = v["candidates"].as_array().filter(|a| !a.is_empty())
                .ok_or("Gemini 没有返回可用候选内容")?;
            for candidate in candidates {
                let finish = candidate["finishReason"].as_str().unwrap_or("");
                if !matches!(finish, "STOP" | "MAX_TOKENS") {
                    return Err("Gemini 候选被拦截或尚未正常结束");
                }
                if finish == "MAX_TOKENS" && candidate.get("content").is_none() {
                    continue;
                }
                let content = &candidate["content"];
                if content["role"].as_str().map(|r| r != "model").unwrap_or(false) {
                    return Err("Gemini 候选角色不合法");
                }
                for part in content["parts"].as_array().ok_or("Gemini 候选缺少 parts")? {
                    if let Some(s) = part["text"].as_str() {
                        text.push(s);
                    } else {
                        let function = part["functionCall"]["name"].is_string()
                            && part["functionCall"]["args"].is_object();
                        let data = part["inlineData"]["mimeType"].is_string() && part["inlineData"]["data"].is_string();
                        if !function && !data {
                            return Err("Gemini part 结构错误");
                        }
                    }
                }
            }
        }
        _ => return Err("不支持该协议的探测"),
    }
    Ok(text.join(" ").replace(['\n', '\r'], " "))
}

/// 真发一次最小对话，返回结构化结果（矩阵与单点检测共用这一份实现）。
pub fn probe_combo(
    base_url: &str,
    key: &str,
    model: &str,
    fmt: &str,
    timeout_secs: u64,
) -> ProbeOutcome {
    let fmt = protocol_format(fmt, base_url);
    if !valid_probe_base(base_url) {
        return ProbeOutcome::fail("error", 0, 0, "Base URL 为空或不合法（只接受无凭据、无查询参数的 HTTP(S) 接入点）");
    }
    if key.trim().is_empty() {
        return ProbeOutcome::fail("key_denied", 0, 0, "密钥为空或已锁定，未发送匿名请求");
    }
    if model.trim().is_empty() {
        return ProbeOutcome::fail("model_mismatch", 0, 0, "未提供该密钥在此接入点的模型");
    }
    let (url, body) = match fmt.as_str() {
        "openai" => (chat_url(base_url), json!({
            "model": model, "messages": [{"role": "user", "content": "ping"}],
            "max_tokens": 1, "stream": false,
        })),
        "openai-resp" => (responses_url(base_url), json!({
            "model": model, "input": "ping", "max_output_tokens": 16, "stream": false,
        })),
        "anthropic" => (action_url(base_url, "/messages", "/v1"), json!({
            "model": model, "messages": [{"role": "user", "content": "ping"}],
            "max_tokens": 16, "stream": false,
        })),
        "gemini" => (gemini_generate_url(base_url, model), json!({
            "contents": [{"role": "user", "parts": [{"text": "ping"}]}],
            "generationConfig": {"maxOutputTokens": 16},
        })),
        _ => return ProbeOutcome::fail("error", 0, 0, "不支持该协议的探测"),
    };
    let Some(url) = url else {
        return ProbeOutcome::fail("error", 0, 0, "未填 Base URL");
    };
    let t0 = std::time::Instant::now();
    // 禁止重定向，避免把协议鉴权头转发给另一个站点。
    let agent = ureq::AgentBuilder::new().redirects(0).build();
    let req = protocol_auth(agent.post(&url), key, &fmt)
        .timeout(std::time::Duration::from_secs(timeout_secs.max(1)))
        .set("Content-Type", "application/json");
    match req.send_json(body) {
        Ok(resp) => {
            let code = resp.status();
            let txt = match read_probe_body(resp) {
                Ok(txt) => txt,
                Err(msg) => return ProbeOutcome::fail("error", code, t0.elapsed().as_millis(), msg),
            };
            let v: Value = match serde_json::from_str(&txt) {
                Ok(v) => v,
                Err(_) => return ProbeOutcome::fail("error", code, t0.elapsed().as_millis(), "上游不是合法 JSON 响应"),
            };
            if !(200..300).contains(&code) {
                return ProbeOutcome::fail("error", code, t0.elapsed().as_millis(), "上游重定向或非成功状态");
            }
            if protocol_error(&v) {
                return ProbeOutcome::fail(classify_failure(code, &txt), code,
                    t0.elapsed().as_millis(), safe_upstream_hint(&txt, key));
            }
            if fmt == "openai-resp" && v["status"] == "incomplete" {
                return ProbeOutcome::fail("incomplete", code, t0.elapsed().as_millis(), "Responses 返回 incomplete，尚未成功完成");
            }
            match parse_probe_success(&fmt, &v) {
                Ok(said) => ProbeOutcome {
                    state: "ok", code, ms: t0.elapsed().as_millis(),
                    said: redact_probe_text(&said, key).trim().chars().take(40).collect(),
                    hint: String::new(), extra: "", net_err: false,
                },
                Err(hint) => ProbeOutcome::fail("error", code, t0.elapsed().as_millis(), hint),
            }
        }
        Err(ureq::Error::Status(code, resp)) => {
            let txt = read_probe_body(resp).unwrap_or_default();
            let mut outcome = ProbeOutcome::fail(classify_failure(code, &txt), code,
                t0.elapsed().as_millis(), safe_upstream_hint(&txt, key));
            outcome.extra = match code {
                401 => "：鉴权失败",
                403 => "：密钥或分组权限被拒绝",
                404 => "：模型或接入路径不存在（不能据此断定全站停用）",
                429 => "：上游限流或额度限制",
                _ => "",
            };
            outcome
        }
        Err(e) => ProbeOutcome {
            state: "error", code: 0, ms: t0.elapsed().as_millis(), said: String::new(),
            hint: friendly_net_err(&e, timeout_secs), extra: "", net_err: true,
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

pub struct ProbeJob {
    pub key_id: String,
    pub endpoint_id: String,
    pub fmt: String,
    pub url: String,
    pub key: String,
    pub model: String,
}

pub struct MatrixCell {
    pub key_id: String,
    pub endpoint_id: String,
    pub fmt: String,
    pub url: String,
    pub model: String,
    pub state: &'static str,
    pub code: u16,
    pub ms: u128,
    pub msg: String,
}

pub fn probe_matrix(jobs: Vec<ProbeJob>, timeout_secs: u64, workers: usize) -> Vec<MatrixCell> {
    probe_matrix_cancellable(jobs, timeout_secs, workers,
        std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)))
}

/// 最多六个在途请求；取消后尚未发送的任务保留身份并返回 cancelled。
pub fn probe_matrix_cancellable(
    jobs: Vec<ProbeJob>,
    timeout_secs: u64,
    workers: usize,
    cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> Vec<MatrixCell> {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;
    let total = jobs.len();
    if total == 0 {
        return Vec::new();
    }
    let cursor = AtomicUsize::new(0);
    let out: Mutex<Vec<Option<MatrixCell>>> = Mutex::new((0..total).map(|_| None).collect());
    std::thread::scope(|scope| {
        for _ in 0..workers.clamp(1, total).min(6) {
            scope.spawn(|| loop {
                if cancel.load(Ordering::Acquire) {
                    break;
                }
                let i = cursor.fetch_add(1, Ordering::SeqCst);
                if i >= total {
                    break;
                }
                let job = &jobs[i];
                if cancel.load(Ordering::Acquire) {
                    break;
                }
                let outcome = probe_combo(&job.url, &job.key, &job.model, &job.fmt, timeout_secs);
                let msg = if outcome.state == "ok" {
                    if outcome.said.is_empty() { "连通".into() }
                    else { format!("返回「{}」", outcome.said) }
                } else {
                    let hint = outcome.hint.trim_start_matches("；上游说：");
                    format!("HTTP {}：{}", outcome.code, hint)
                };
                out.lock().unwrap()[i] = Some(matrix_cell(job, outcome.state,
                    outcome.code, outcome.ms, redact_probe_text(&msg, &job.key)));
            });
        }
    });
    out.into_inner().unwrap().into_iter().enumerate().map(|(i, cell)| {
        cell.unwrap_or_else(|| matrix_cell(&jobs[i], "cancelled", 0, 0, "已取消，未发送请求".into()))
    }).collect()
}

fn matrix_cell(job: &ProbeJob, state: &'static str, code: u16, ms: u128, msg: String) -> MatrixCell {
    let id = endpoint_id(&job.fmt, &job.url);
    MatrixCell {
        key_id: job.key_id.clone(),
        endpoint_id: if job.endpoint_id == id { job.endpoint_id.clone() } else { id },
        fmt: protocol_format(&job.fmt, &job.url), url: job.url.clone(), model: job.model.clone(),
        state, code, ms, msg,
    }
}

/// 网络错误翻译成人话：超时（Windows 上表现为 os error 10060）要说清
/// "等久了没连上"，别把系统错误号原样怼到脸上
pub fn friendly_net_err(e: &ureq::Error, secs: u64) -> String {
    let s = e.to_string();
    if s.contains("timed out") || s.contains("timeout") || s.contains("10060") {
        format!("连接超时（{} 秒没连上）——站点可能暂时不可达，稍后再试", secs)
    } else if s.contains("Connection refused") || s.contains("10061") {
        "连接被拒——站点没有监听该端口".into()
    } else {
        // 传输错误可能含请求 URL，不回显以免带出查询凭据或代理认证信息。
        "网络请求失败，请检查地址、代理、证书或网络连接".into()
    }
}

pub fn probe_models(base_url: &str, key: &str) -> (bool, String, Vec<String>) {
    probe_models_for_endpoint(base_url, key, fmt_of_url(base_url))
}

pub fn probe_models_for_endpoint(base_url: &str, key: &str, fmt: &str) -> (bool, String, Vec<String>) {
    let cancel = std::sync::atomic::AtomicBool::new(false);
    let result = discover_endpoint_models(base_url, key, fmt, &cancel, 20);
    (matches!(result.state, "ok" | "empty"), result.msg, result.models)
}

struct ModelDiscovery {
    models: Vec<String>,
    state: &'static str,
    msg: String,
}

fn model_discovery_result(models: Vec<String>, state: &'static str, msg: impl Into<String>) -> ModelDiscovery {
    ModelDiscovery { models, state, msg: msg.into() }
}

/// 只接受协议数组，合法空清单与解析失败必须区分。
fn parse_model_page(v: &Value, fmt: &str) -> Result<Vec<String>, &'static str> {
    if !v.is_object() || protocol_error(v) {
        return Err("上游返回错误对象或非模型清单");
    }
    let field = if fmt == "gemini" { "models" } else { "data" };
    let items = v[field].as_array().ok_or("响应缺少该协议的模型数组")?;
    let mut out = Vec::new();
    for item in items {
        let id = item[if fmt == "gemini" { "name" } else { "id" }]
            .as_str().filter(|s| !s.trim().is_empty()).ok_or("模型条目缺少合法 ID")?;
        let id = if fmt == "gemini" { id.strip_prefix("models/").unwrap_or(id) } else { id };
        if id.is_empty() {
            return Err("模型 ID 为空");
        }
        if !out.iter().any(|m| m == id) {
            out.push(id.to_string());
        }
    }
    Ok(out)
}

fn nonempty_page_token(v: &Value) -> Result<Option<String>, &'static str> {
    match v {
        Value::Null => Ok(None),
        Value::String(s) if s.is_empty() => Ok(None),
        Value::String(s) => Ok(Some(s.clone())),
        _ => Err("分页游标格式不合法"),
    }
}

/// 不跟随上游提供的任意 URL，只在原模型接口上附加协议规定的游标。
fn next_model_page(v: &Value, fmt: &str) -> Result<Option<(String, String)>, &'static str> {
    if fmt == "gemini" {
        return Ok(nonempty_page_token(&v["nextPageToken"])?
            .map(|token| ("pageToken".into(), token)));
    }
    for field in ["next_page_token", "nextPageToken", "next_cursor"] {
        if let Some(token) = nonempty_page_token(&v[field])? {
            let parameter = if field == "next_cursor" { "cursor" } else { "page_token" };
            return Ok(Some((parameter.into(), token)));
        }
    }
    if let Some(page) = v.get("next_page").filter(|p| !p.is_null()) {
        let page = match page {
            Value::String(s) if s.is_empty() => None,
            Value::String(s) => Some(s.clone()),
            Value::Number(n) if n.as_u64().unwrap_or(0) > 0 => Some(n.to_string()),
            Value::Number(n) if n.as_u64() == Some(0) => None,
            _ => return Err("下一页页码格式不合法"),
        };
        if let Some(page) = page {
            return Ok(Some(("page".into(), page)));
        }
    }
    if v.get("has_more").map(|x| !x.is_boolean()).unwrap_or(false) {
        return Err("has_more 不是布尔值");
    }
    if v["has_more"].as_bool() == Some(true) {
        let last = v["last_id"].as_str().filter(|s| !s.is_empty())
            .or_else(|| v["data"].as_array()?.last()?["id"].as_str())
            .filter(|s| !s.is_empty()).ok_or("上游仍有下一页但未返回游标")?;
        return Ok(Some(((if fmt == "anthropic" { "after_id" } else { "after" }).into(), last.into())));
    }
    for field in ["next", "next_url", "next_page_url"] {
        if v.get(field).map(|x| !x.is_null() && x.as_str() != Some("") && x != &Value::Bool(false)).unwrap_or(false) {
            return Err("上游仍有下一页但分页形式不受支持，结果不完整");
        }
    }
    Ok(None)
}

fn discover_endpoint_models(
    base_url: &str,
    key: &str,
    fmt: &str,
    cancel: &std::sync::atomic::AtomicBool,
    timeout_secs: u64,
) -> ModelDiscovery {
    use std::sync::atomic::Ordering;
    let mut models = Vec::new();
    if cancel.load(Ordering::Acquire) {
        return model_discovery_result(models, "cancelled", "已取消，未发送请求");
    }
    if key.trim().is_empty() {
        return model_discovery_result(models, "error", "密钥为空或已锁定，未发送匿名请求");
    }
    if !valid_probe_base(base_url) {
        return model_discovery_result(models, "error", "Base URL 为空或不合法");
    }
    let fmt = protocol_format(fmt, base_url);
    if !matches!(fmt.as_str(), "openai" | "openai-resp" | "anthropic" | "gemini") {
        return model_discovery_result(models, "error", "不支持该协议的模型发现");
    }
    let url = if fmt == "gemini" { action_url(base_url, "/models", "/v1beta") } else { models_url(base_url) };
    let Some(url) = url else {
        return model_discovery_result(models, "error", "未填 Base URL");
    };
    let agent = ureq::AgentBuilder::new().redirects(0).build();
    let mut query: Option<(String, String)> = None;
    let mut seen = Vec::new();
    const MAX_PAGES: usize = 64;
    for page in 0..MAX_PAGES {
        if cancel.load(Ordering::Acquire) {
            return model_discovery_result(models, "cancelled", "已取消，模型清单可能不完整");
        }
        let mut req = protocol_auth(agent.get(&url), key, &fmt)
            .timeout(std::time::Duration::from_secs(timeout_secs.max(1)));
        if let Some((name, value)) = &query {
            req = req.query(name, value);
        }
        let resp = match req.call() {
            Ok(resp) => resp,
            Err(ureq::Error::Status(code, resp)) => {
                let detail = read_probe_body(resp).map(|s| safe_upstream_hint(&s, key))
                    .unwrap_or_else(|s| format!("；{s}"));
                return model_discovery_result(models, "error", format!("HTTP {code}{detail}；模型发现未完成"));
            }
            Err(e) => return model_discovery_result(models, "error", friendly_net_err(&e, timeout_secs)),
        };
        if !(200..300).contains(&resp.status()) {
            return model_discovery_result(models, "error", "上游重定向或非成功状态，未转发凭据");
        }
        let header_next = resp.header("Link").map(|s| s.contains("rel=\"next\"") || s.contains("rel=next")).unwrap_or(false)
            || resp.header("X-Next-Page").map(|s| !s.is_empty() && s != "0").unwrap_or(false);
        let txt = match read_probe_body(resp) {
            Ok(txt) => txt,
            Err(msg) => return model_discovery_result(models, "error", msg),
        };
        let v: Value = match serde_json::from_str(&txt) {
            Ok(v) => v,
            Err(_) => return model_discovery_result(models, "error", "上游不是合法模型 JSON"),
        };
        let ids = match parse_model_page(&v, &fmt) {
            Ok(ids) => ids,
            Err(msg) => {
                let detail = safe_upstream_hint(&txt, key);
                return model_discovery_result(models, "error", format!("{msg}{detail}"));
            }
        };
        for id in ids {
            if !models.contains(&id) {
                models.push(id);
            }
        }
        let next = match next_model_page(&v, &fmt) {
            Ok(next) => next,
            Err(msg) => return model_discovery_result(models, "error", msg),
        };
        let Some(next) = next else {
            if header_next {
                return model_discovery_result(models, "error", "分页响应头仍指向下一页，结果不完整");
            }
            let state = if models.is_empty() { "empty" } else { "ok" };
            let msg = if models.is_empty() { "上游返回合法空模型清单".to_string() }
                else { format!("已完整获取 {} 个模型", models.len()) };
            return model_discovery_result(models, state, msg);
        };
        if page + 1 == MAX_PAGES || seen.contains(&next) {
            return model_discovery_result(models, "error", "分页达到安全上限或游标重复，结果不完整");
        }
        seen.push(next.clone());
        query = Some(next);
    }
    model_discovery_result(models, "error", "模型分页未完成")
}

/// 返回每一把 Key × 每一个端点的独立目录；本函数不读写存储或保管箱。
pub fn discover_site_models(site: &Value, cancel: &std::sync::atomic::AtomicBool) -> Vec<Value> {
    let endpoints = matrix_endpoints(site);
    let mut out = Vec::new();
    for key in site["keys"].as_array().into_iter().flatten() {
        for endpoint in &endpoints {
            if !endpoint_applies(site, key, endpoint) { continue; }
            let plain = key["key"].as_str().unwrap_or("");
            let result = if cancel.load(std::sync::atomic::Ordering::Acquire) {
                model_discovery_result(Vec::new(), "cancelled", "已取消，未发送请求")
            } else if key["secret_locked"].as_bool() == Some(true) {
                model_discovery_result(Vec::new(), "error", "密钥已锁定，未发送匿名请求")
            } else {
                discover_endpoint_models(&endpoint.url, plain, &endpoint.fmt, cancel, 20)
            };
            out.push(json!({
                "key_id": key["id"].as_str().unwrap_or(""), "endpoint_id": endpoint.id,
                "fmt": endpoint.fmt, "url": endpoint.url, "models": result.models,
                "state": result.state, "msg": redact_probe_text(&result.msg, plain),
                "checked_at": chrono::Local::now().to_rfc3339(),
            }));
        }
    }
    out
}

/// 兼容旧调用：只汇总完整成功的逐 Key 目录，绝不首成功即停止。
pub fn probe_models_all(site: &Value) -> (Vec<String>, Vec<String>) {
    let cancel = std::sync::atomic::AtomicBool::new(false);
    let results = discover_site_models(site, &cancel);
    let mut ids = Vec::new();
    let mut notes = Vec::new();
    for result in results {
        if result["state"] == "ok" {
            append_models(&mut ids, &result["models"]);
        }
        notes.push(format!("{} × {}：{}", result["key_id"].as_str().unwrap_or(""),
            result["endpoint_id"].as_str().unwrap_or(""), result["msg"].as_str().unwrap_or("")));
    }
    if notes.is_empty() {
        notes.push("没有可发现的密钥或接入点，未发送匿名请求".into());
    }
    (ids, notes)
}

/// 从文档复制来的地址常常已经带上了动作后缀
/// （…/v1/chat/completions、…/responses、…/v1/messages 之类）。
/// 测活和拉模型要的是 API 根：先把这些后缀剥掉再拼 /models，
/// 否则拼出来是 .../chat/completions/v1/models，必然 HTTP 404
/// （她 2026-10-28：带 chat/completions 或 responses 的永远测不通，就这个原因）。
fn strip_action_suffix(mut base: &str) -> &str {
    base = base.trim_end_matches('/');
    let lower = base.to_ascii_lowercase();
    if lower.contains(":generatecontent") {
        if let Some(i) = lower.rfind("/models/") {
            base = &base[..i];
        }
    }
    const ACTIONS: &[&str] = &[
        "/chat/completions", "/responses", "/completions", "/messages", "/embeddings", "/models",
    ];
    loop {
        let lower = base.to_ascii_lowercase();
        match ACTIONS.iter().find(|s| lower.ends_with(*s)) {
            Some(s) => base = base[..base.len() - s.len()].trim_end_matches('/'),
            None => return base,
        }
    }
}

fn action_url(base_url: &str, action: &str, default_version: &str) -> Option<String> {
    let explicit = base_url.trim().trim_end_matches('/');
    if action == "/models" && explicit.to_ascii_lowercase().ends_with("/models") {
        return Some(explicit.into());
    }
    let base = strip_action_suffix(base_url.trim());
    if base.is_empty() {
        return None;
    }
    let has_version_tail = ["/v1", "/v1beta", "/v1beta1", "/v4"]
        .iter().any(|s| base.ends_with(s));
    Some(if has_version_tail { format!("{base}{action}") }
        else { format!("{base}{default_version}{action}") })
}

fn gemini_generate_url(base_url: &str, model: &str) -> Option<String> {
    let models = action_url(base_url, "/models", "/v1beta")?;
    Some(format!("{}/{}:generateContent", models, url_component(model.strip_prefix("models/").unwrap_or(model))))
}

/// base 可能已经是 /v1 或更深（如 /openai/v1）；统一拼到 .../models
fn models_url(base_url: &str) -> Option<String> {
    action_url(base_url, "/models", "/v1")
}

/// 真请求打的地址：和 models_url 一套规矩（先剥动作后缀，再按有没有版本尾巴补）
fn chat_url(base_url: &str) -> Option<String> {
    action_url(base_url, "/chat/completions", "/v1")
}

/// Responses 协议真请求的地址（站点兼容格式登记成 `openai-resp` 时走这里）。
/// 规矩和 chat_url 一样：先剥动作后缀，再看有没有版本尾巴。
/// 2026-11-20 加，起因是星驰API 停用了 /v1/chat/completions。
fn responses_url(base_url: &str) -> Option<String> {
    action_url(base_url, "/responses", "/v1")
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
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    fn local_server<F>(requests: usize, mut handler: F) -> (String, std::thread::JoinHandle<()>)
    where
        F: FnMut(usize, &mut tiny_http::Request) -> (u16, &'static str, String) + Send + 'static,
    {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let base = format!("http://{}", server.server_addr().to_ip().unwrap());
        let thread = std::thread::spawn(move || {
            for i in 0..requests {
                let mut request = server.recv_timeout(Duration::from_secs(5)).unwrap()
                    .expect("本地探测请求应按预期到达");
                let (status, content_type, body) = handler(i, &mut request);
                let response = tiny_http::Response::from_string(body).with_status_code(status)
                    .with_header(tiny_http::Header::from_bytes("Content-Type", content_type).unwrap());
                request.respond(response).unwrap();
            }
        });
        (base, thread)
    }

    fn request_header(request: &tiny_http::Request, name: &str) -> String {
        request.headers().iter().find(|h| h.field.to_string().eq_ignore_ascii_case(name))
            .map(|h| h.value.as_str().to_string()).unwrap_or_default()
    }

    fn request_json(request: &mut tiny_http::Request) -> Value {
        let mut text = String::new();
        request.as_reader().read_to_string(&mut text).unwrap();
        serde_json::from_str(&text).unwrap()
    }

    fn chat_response(text: &str) -> Value {
        json!({"id":"chat-fake", "object":"chat.completion", "choices":[{
            "index":0, "message":{"role":"assistant", "content":text}, "finish_reason":"stop"
        }]})
    }

    fn test_job(base: &str, n: usize) -> ProbeJob {
        ProbeJob {
            key_id: format!("key-{n}"), endpoint_id: endpoint_id("openai", base),
            fmt: "openai".into(), url: base.into(), key: format!("fake-key-{n}"), model: format!("fake-model-{n}"),
        }
    }

    #[test]
    fn local_http_probe_uses_four_real_protocols() {
        for fmt in ["openai", "openai-resp", "anthropic", "gemini"] {
            let (base, server) = local_server(1, move |_, request| {
                assert_eq!(request.method().as_str(), "POST");
                assert_eq!(request_header(request, "User-Agent"), PROBE_UA);
                assert_eq!(request_header(request, "Content-Type"), "application/json");
                let expected_path = match fmt {
                    "openai" => "/gateway/v1/chat/completions",
                    "openai-resp" => "/gateway/v1/responses",
                    "anthropic" => "/gateway/v1/messages",
                    _ => "/gateway/v1beta/models/fake-model:generateContent",
                };
                assert_eq!(request.url(), expected_path);
                match fmt {
                    "anthropic" => {
                        assert_eq!(request_header(request, "x-api-key"), "fake-secret");
                        assert_eq!(request_header(request, "anthropic-version"), "2023-06-01");
                        assert_eq!(request_header(request, "Authorization"), "");
                    }
                    "gemini" => {
                        assert_eq!(request_header(request, "x-goog-api-key"), "fake-secret");
                        assert_eq!(request_header(request, "Authorization"), "");
                        assert_eq!(request_header(request, "x-api-key"), "");
                    }
                    _ => {
                        assert_eq!(request_header(request, "Authorization"), "Bearer fake-secret");
                        assert_eq!(request_header(request, "x-api-key"), "");
                    }
                }
                let body = request_json(request);
                let response = match fmt {
                    "openai" => {
                        assert_eq!(body["messages"][0], json!({"role":"user","content":"ping"}));
                        assert_eq!(body["max_tokens"], 1);
                        assert!(body.get("input").is_none());
                        chat_response("pong")
                    }
                    "openai-resp" => {
                        assert_eq!(body["input"], "ping");
                        assert_eq!(body["max_output_tokens"], 16);
                        assert!(body.get("messages").is_none());
                        json!({"id":"resp-fake","object":"response","status":"completed","output":[{
                            "type":"message","role":"assistant","status":"completed",
                            "content":[{"type":"output_text","text":"pong"}]
                        }]})
                    }
                    "anthropic" => {
                        assert_eq!(body["messages"][0]["content"], "ping");
                        assert_eq!(body["max_tokens"], 16);
                        json!({"id":"msg-fake","type":"message","role":"assistant",
                            "stop_reason":"end_turn","content":[{"type":"text","text":"pong"}]})
                    }
                    _ => {
                        assert_eq!(body["contents"], json!([{"role":"user","parts":[{"text":"ping"}]}]));
                        assert_eq!(body["generationConfig"]["maxOutputTokens"], 16);
                        assert!(body.get("messages").is_none());
                        assert!(body.get("model").is_none());
                        json!({"candidates":[{"finishReason":"STOP","content":{"role":"model","parts":[{"text":"pong"}]}}]})
                    }
                };
                if fmt != "gemini" {
                    assert_eq!(body["model"], "fake-model");
                    assert_eq!(body["stream"], false);
                }
                (200, "application/json", response.to_string())
            });
            let suffix = match fmt {
                "openai" => "/gateway/v1/chat/completions/",
                "openai-resp" => "/gateway/v1/responses/",
                "anthropic" => "/gateway/v1/messages/",
                _ => "/gateway",
            };
            let outcome = probe_combo(&format!("{base}{suffix}"), "fake-secret", "fake-model", fmt, 3);
            server.join().unwrap();
            assert_eq!(outcome.state, "ok", "{fmt}：{}", outcome.hint);
            assert_eq!(outcome.said, "pong");
        }
    }

    #[test]
    fn local_http_rejects_html_errors_malformed_and_unfinished_responses() {
        let cases = vec![
            ("openai", "text/html", "<html>ok</html>".to_string()),
            ("openai", "application/json", "not JSON".into()),
            ("openai", "application/json", json!({"text":"pong"}).to_string()),
            ("openai", "application/json", json!({"error":{"message":"fake upstream error"}}).to_string()),
            ("openai", "application/json", json!({"choices":[{"message":{"content":"pong"}}]}).to_string()),
            ("openai-resp", "application/json", json!({"id":"r","object":"response","status":"failed","output":[]}).to_string()),
            ("openai-resp", "application/json", json!({"id":"r","object":"response","status":"incomplete",
                "output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"pong"}]}]}).to_string()),
            ("anthropic", "application/json", json!({"content":[{"text":"pong"}]}).to_string()),
            ("gemini", "application/json", json!({"candidates":[{"finishReason":"SAFETY","content":{"parts":[{"text":"pong"}]}}]}).to_string()),
        ];
        for (fmt, content_type, body) in cases {
            let (base, server) = local_server(1, move |_, _| (200, content_type, body.clone()));
            let result = probe_combo(&base, "fake-secret", "fake-model", fmt, 3);
            server.join().unwrap();
            assert_ne!(result.state, "ok", "{fmt} 不得假成功");
        }
        for fmt in ["openai-resp", "anthropic", "gemini"] {
            let (base, server) = local_server(1, |_, _| (200, "text/html", "<html>登录</html>".into()));
            assert_ne!(probe_combo(&base, "fake-secret", "fake-model", fmt, 3).state, "ok");
            server.join().unwrap();
        }
    }

    #[test]
    fn local_http_legal_empty_content_can_succeed_without_text() {
        let cases = vec![
            ("openai", json!({"object":"chat.completion","choices":[{
                "message":{"role":"assistant","content":null},"finish_reason":"length"}]})),
            ("openai-resp", json!({"id":"r","object":"response","status":"completed","output":[]})),
            ("anthropic", json!({"id":"m","type":"message","role":"assistant","content":[],"stop_reason":"max_tokens"})),
            ("gemini", json!({"candidates":[{"finishReason":"MAX_TOKENS"}]})),
        ];
        for (fmt, response) in cases {
            let (base, server) = local_server(1, move |_, _| (200, "application/json", response.to_string()));
            let result = probe_combo(&base, "fake-secret", "fake-model", fmt, 3);
            server.join().unwrap();
            assert_eq!(result.state, "ok", "{fmt}：{}", result.hint);
            assert!(result.said.is_empty());
        }
    }

    #[test]
    fn local_http_failure_attribution_and_credential_redaction() {
        for (code, body, state) in [
            (404, json!({"error":{"message":"model fake-model not found"}}), "model_mismatch"),
            (404, json!({"message":"unknown route"}), "error"),
            (400, json!({"message":"This group does not allow messages dispatch"}), "key_denied"),
            (403, json!({"message":"This API key is disabled: fake-secret"}), "key_denied"),
            (429, json!({"message":"slow down"}), "rate_limited"),
            (400, json!({"error":{"code":"chat_completions_disabled","message":"本站已停用 /v1/chat/completions"}}), "site_disabled"),
        ] {
            let (base, server) = local_server(1, move |_, _| (code, "application/json", body.to_string()));
            let result = probe_combo(&base, "fake-secret", "fake-model", "openai", 3);
            server.join().unwrap();
            assert_eq!(result.state, state);
            assert!(!result.hint.contains("fake-secret"));
        }
    }

    #[test]
    fn upstream_credential_redaction_handles_unicode_before_truncation() {
        let hint = safe_upstream_hint(r#"{"error":{"message":"rejected \u0066ake-secret"}}"#, "fake-secret");
        assert!(!hint.contains("fake-secret"));
        assert!(hint.contains("已隐藏凭据"));
        let long_key = "fake-secret-".repeat(30);
        let body = json!({"error":{"message":format!("rejected {long_key}")}}).to_string();
        let hint = safe_upstream_hint(&body, &long_key);
        assert!(!hint.contains("fake-secret"));
        assert!(hint.contains("已隐藏凭据"));
    }

    #[test]
    fn local_http_discovers_every_key_even_after_success_or_empty() {
        for empty_first in [false, true] {
            let (base, server) = local_server(4, move |i, request| {
                assert_eq!(request.method().as_str(), "GET");
                assert_eq!(request.url(), "/v1/models");
                let first = i % 2 == 0;
                assert_eq!(request_header(request, "Authorization"),
                    if first { "Bearer fake-A" } else { "Bearer fake-B" });
                let data = if first && empty_first { json!([]) }
                    else if first { json!([{"id":"model-A"}]) } else { json!([{"id":"model-B"}]) };
                (200, "application/json", json!({"object":"list","data":data}).to_string())
            });
            let site = json!({"base_urls":[{"format":"openai","url":base}],
                "keys":[{"id":"A","key":"fake-A"},{"id":"B","key":"fake-B"}]});
            let results = discover_site_models(&site, &AtomicBool::new(false));
            assert_eq!(results.len(), 2);
            assert_eq!(results[0]["state"], if empty_first { "empty" } else { "ok" });
            assert_eq!(results[1]["models"], json!(["model-B"]));
            for result in &results {
                assert!(chrono::DateTime::parse_from_rfc3339(result["checked_at"].as_str().unwrap()).is_ok());
                assert_eq!(result["endpoint_id"], endpoint_id("openai", &base));
                assert!(!result.to_string().contains("fake-A"));
                assert!(!result.to_string().contains("fake-B"));
            }
            let (models, notes) = probe_models_all(&site);
            server.join().unwrap();
            assert_eq!(notes.len(), 2);
            assert_eq!(models, if empty_first { vec!["model-B"] } else { vec!["model-A", "model-B"] });
        }
    }

    #[test]
    fn local_http_model_lists_use_protocol_paths_headers_and_pagination() {
        for fmt in ["openai", "openai-resp", "anthropic", "gemini"] {
            let (base, server) = local_server(2, move |page, request| {
                let prefix = if fmt == "gemini" { "/v1beta/models" } else { "/v1/models" };
                if page == 0 {
                    assert_eq!(request.url(), prefix);
                } else {
                    let query = match fmt {
                        "anthropic" => "?after_id=model-A",
                        "gemini" => "?pageToken=next-fake",
                        _ => "?after=model-A",
                    };
                    assert_eq!(request.url(), format!("{prefix}{query}"));
                }
                match fmt {
                    "anthropic" => {
                        assert_eq!(request_header(request, "x-api-key"), "fake-secret");
                        assert_eq!(request_header(request, "anthropic-version"), "2023-06-01");
                        assert_eq!(request_header(request, "Authorization"), "");
                    }
                    "gemini" => {
                        assert_eq!(request_header(request, "x-goog-api-key"), "fake-secret");
                        assert_eq!(request_header(request, "Authorization"), "");
                    }
                    _ => assert_eq!(request_header(request, "Authorization"), "Bearer fake-secret"),
                }
                let id = if page == 0 { "model-A" } else { "model-B" };
                let response = if fmt == "gemini" {
                    let mut v = json!({"models":[{"name":format!("models/{id}")}]});
                    if page == 0 { v["nextPageToken"] = json!("next-fake"); }
                    v
                } else {
                    json!({"data":[{"id":id}],"has_more":page == 0,"last_id":id})
                };
                (200, "application/json", response.to_string())
            });
            let suffix = if fmt == "openai-resp" { "/v1/responses/" }
                else if fmt == "anthropic" { "/v1/messages/" }
                else if fmt == "gemini" { "/v1beta/models/fake-model:generateContent/" } else { "" };
            let (ok, msg, models) = probe_models_for_endpoint(&format!("{base}{suffix}"), "fake-secret", fmt);
            server.join().unwrap();
            assert!(ok, "{fmt}：{msg}");
            assert_eq!(models, vec!["model-A", "model-B"]);
        }
    }

    #[test]
    fn local_http_model_discovery_distinguishes_empty_html_and_partial() {
        for (content_type, response, expected_state) in [
            ("text/html", "<html>模型页</html>".to_string(), "error"),
            ("application/json", json!({"error":{"message":"fake-secret rejected"}}).to_string(), "error"),
            ("application/json", json!({"data":[]}).to_string(), "empty"),
            ("application/json", "not JSON".into(), "error"),
            ("application/json", json!({"models":[]}).to_string(), "error"),
            ("application/json", json!({"data":[],"success":false}).to_string(), "error"),
            ("application/json", json!({"data":[],"code":401}).to_string(), "error"),
            ("application/json", json!({"data":[{"wrong":"model"}]}).to_string(), "error"),
            ("application/json", json!({"data":[],"has_more":true}).to_string(), "error"),
        ] {
            let (base, server) = local_server(1, move |_, _| (200, content_type, response.clone()));
            let result = discover_endpoint_models(&base, "fake-secret", "openai", &AtomicBool::new(false), 3);
            server.join().unwrap();
            assert_eq!(result.state, expected_state);
            assert!(!result.msg.contains("fake-secret"));
        }
        let (base, server) = local_server(2, |_, _| (200, "application/json",
            json!({"data":[{"id":"model-A"}],"has_more":true,"last_id":"model-A"}).to_string()));
        let result = discover_endpoint_models(&base, "fake-secret", "openai", &AtomicBool::new(false), 3);
        server.join().unwrap();
        assert_eq!(result.state, "error", "重复游标不能伪装成完整成功");
        assert_eq!(result.models, vec!["model-A"]);
    }

    #[test]
    fn endpoint_identity_merges_site_and_key_endpoints_without_format_collapse() {
        let site = json!({
            "base_urls":[{"format":"openai","url":" http://127.0.0.1:1/a/ "},
                {"format":"openai","url":"http://127.0.0.1:1/b"}],
            "keys":[{"base_url":"http://127.0.0.1:1/a","base_format":"openai"},
                {"base_url":"http://127.0.0.1:1/private","base_format":"anthropic"}]
        });
        let endpoints = matrix_endpoints(&site);
        assert_eq!(endpoints.len(), 3);
        assert_eq!(endpoints[2].fmt, "anthropic");
        assert_eq!(endpoints[0].id, "openai|http://127.0.0.1:1/a");
        assert_ne!(endpoints[0].id, endpoints[1].id);
        assert_eq!(endpoint_id("", " http://127.0.0.1/v1/responses/// "),
            "openai-resp|http://127.0.0.1/v1/responses");
        assert_eq!(endpoint_id(" anthropic ", " http://127.0.0.1/v1/ "),
            "anthropic|http://127.0.0.1/v1");
        let explicit = matrix_endpoints(&json!({"base_urls":[{
            "format":"openai","base_format":"anthropic","url":"http://127.0.0.1/v1"
        }]}));
        assert_eq!(explicit[0].fmt, "anthropic");
    }

    #[test]
    fn local_http_discovery_visits_each_key_and_key_only_endpoint() {
        let (base, server) = local_server(3, |i, request| {
            assert_eq!(request.url(), if i == 1 { "/private/v1/models" } else { "/site/v1/models" });
            assert_eq!(request_header(request, "Authorization"), if i < 2 { "Bearer fake-A" } else { "Bearer fake-B" });
            (200, "application/json", json!({"data":[{"id":format!("model-{i}")}]}).to_string())
        });
        let site = json!({"base_urls":[{"format":"openai","url":format!("{base}/site")}],
            "keys":[{"id":"A","key":"fake-A","base_url":format!("{base}/private"),"base_format":"openai"},
                {"id":"B","key":"fake-B"}]});
        let results = discover_site_models(&site, &AtomicBool::new(false));
        server.join().unwrap();
        assert_eq!(results.len(), 3);
        assert!(results.iter().all(|r| r["state"] == "ok"));
        assert_ne!(results[0]["endpoint_id"], results[1]["endpoint_id"]);
        assert_eq!(results[2]["key_id"], "B");
        assert_eq!(results[2]["endpoint_id"], results[0]["endpoint_id"]);
    }

    #[test]
    fn key_catalogs_never_borrow_site_or_another_endpoint_permissions() {
        let site = json!({"models_list":"site-model"});
        let key = json!({"models_list":"manual-model", "model_catalogs":[
            {"endpoint_id":"openai|one","state":"ok","models":["key-model"]},
            {"endpoint_id":"openai|empty","state":"empty","models":[]},
            {"endpoint_id":"openai|error","state":"error","models":["partial-model"]},
            {"endpoint_id":"openai|stale","state":"stale","models":["old-model"]}
        ]});
        assert_eq!(models_for_endpoint(&site, &key, "openai|one"), vec!["key-model"]);
        for endpoint in ["openai|empty", "openai|error", "openai|stale"] {
            assert!(models_for_endpoint(&site, &key, endpoint).is_empty());
        }
        assert_eq!(models_for_endpoint(&site, &key, "openai|unknown"), vec!["manual-model"]);
        assert!(models_for_endpoint(&site, &json!({}), "unknown").is_empty());
        assert_eq!(models_for(&site, &key), vec!["key-model", "manual-model"]);
        assert_eq!(models_for(&site, &json!({})), vec!["site-model"]);
        assert!(models_for(&site, &json!({"models_list":""})).is_empty());
        assert!(models_for(&site, &json!({"model_catalogs":[]})).is_empty());
        let repeated = json!({"model_catalogs":[
            {"endpoint_id":"openai|one","state":"ok","models":["old-model"]},
            {"endpoint_id":"openai|one","state":"error","models":[]}
        ]});
        assert!(models_for(&site, &repeated).is_empty());
        assert!(models_for_endpoint(&site, &repeated, "openai|one").is_empty());
    }

    #[test]
    fn key_edits_preserve_catalogs_but_invalidate_on_probe_input_change() {
        let old = json!({"id":"A","key":"fake-secret","base_url":"http://127.0.0.1/v1",
            "base_format":"openai","models_list":"manual-model","probe_revision":"old-revision",
            "model_catalogs":[{"endpoint_id":"openai|http://127.0.0.1/v1","state":"ok","models":["old-model"]}]});
        let edited = merge_key_edit(Some(&old), &json!({"label":"新名称","key":"","model_catalogs":[]}));
        assert_eq!(edited["model_catalogs"], old["model_catalogs"]);
        assert_eq!(edited["probe_revision"], "old-revision");
        assert_eq!(edited["key"], "fake-secret");
        for (field, value) in [("key", "fake-new-secret"), ("base_url", "http://127.0.0.1/other"),
            ("base_format", "anthropic"), ("models_list", "manual-new-model")]
        {
            let mut patch = json!({});
            patch[field] = json!(value);
            let edited = merge_key_edit(Some(&old), &patch);
            assert_ne!(edited["probe_revision"], old["probe_revision"]);
            assert_eq!(edited["model_catalogs"][0]["state"], "stale");
            assert_eq!(edited["models_list"], if field == "models_list" { "manual-new-model" } else { "manual-model" });
        }
    }

    #[test]
    fn empty_locked_or_cancelled_keys_never_send_anonymous_requests() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let base = format!("http://{}", server.server_addr().to_ip().unwrap());
        let site = json!({"base_urls":[{"format":"openai","url":base}],
            "keys":[{"id":"empty","key":""},{"id":"locked","key":"fake-secret","secret_locked":true}]});
        let results = discover_site_models(&site, &AtomicBool::new(false));
        assert_eq!(results.len(), 2);
        assert!(results.iter().all(|r| r["state"] == "error"));
        let cancelled_discovery = discover_site_models(&site, &AtomicBool::new(true));
        assert_eq!(cancelled_discovery.len(), 2);
        assert!(cancelled_discovery.iter().all(|r| r["state"] == "cancelled"));
        assert_ne!(probe_combo(&base, "", "fake-model", "openai", 1).state, "ok");
        let cancelled = probe_matrix_cancellable(vec![test_job(&base, 0)], 1, 6, Arc::new(AtomicBool::new(true)));
        assert_eq!(cancelled[0].state, "cancelled");
        assert_eq!(cancelled[0].endpoint_id, endpoint_id("openai", &base));
        assert_eq!(cancelled[0].url, base);
        assert_eq!(cancelled[0].model, "fake-model-0");
        assert!(server.recv_timeout(Duration::from_millis(30)).unwrap().is_none());
    }

    #[test]
    fn local_http_cancellation_stops_next_matrix_job_and_model_page() {
        let cancel = Arc::new(AtomicBool::new(false));
        let server_cancel = cancel.clone();
        let (base, server) = local_server(1, move |_, _| {
            server_cancel.store(true, Ordering::Release);
            (200, "application/json", chat_response("pong").to_string())
        });
        let cells = probe_matrix_cancellable((0..3).map(|i| test_job(&base, i)).collect(), 3, 1, cancel);
        server.join().unwrap();
        assert_eq!(cells[0].state, "ok");
        assert!(cells[1..].iter().all(|c| c.state == "cancelled"));
        let cancel = Arc::new(AtomicBool::new(false));
        let server_cancel = cancel.clone();
        let (base, server) = local_server(1, move |_, _| {
            server_cancel.store(true, Ordering::Release);
            (200, "application/json", json!({"models":[{"name":"models/model-A"}],"nextPageToken":"next"}).to_string())
        });
        let result = discover_endpoint_models(&base, "fake-secret", "gemini", &cancel, 3);
        server.join().unwrap();
        assert_eq!(result.state, "cancelled");
        assert_eq!(result.models, vec!["model-A"]);
    }

    #[test]
    fn local_http_matrix_caps_workers_at_six() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let base = format!("http://{}", server.server_addr().to_ip().unwrap());
        let cancel = Arc::new(AtomicBool::new(false));
        let server_cancel = cancel.clone();
        let thread = std::thread::spawn(move || {
            let mut requests = Vec::new();
            for _ in 0..6 {
                requests.push(server.recv_timeout(Duration::from_secs(5)).unwrap().expect("应有六个在途请求"));
            }
            assert!(server.recv_timeout(Duration::from_millis(30)).unwrap().is_none(), "不得出现第七个在途请求");
            server_cancel.store(true, Ordering::Release);
            for request in requests {
                request.respond(tiny_http::Response::from_string(chat_response("pong").to_string())
                    .with_header(tiny_http::Header::from_bytes("Content-Type", "application/json").unwrap())).unwrap();
            }
        });
        let results = probe_matrix_cancellable((0..12).map(|i| test_job(&base, i)).collect(), 10, 99, cancel);
        thread.join().unwrap();
        assert_eq!(results.iter().filter(|c| c.state == "ok").count(), 6);
        assert_eq!(results.iter().filter(|c| c.state == "cancelled").count(), 6);
        for (i, result) in results.iter().enumerate() {
            assert_eq!(result.key_id, format!("key-{i}"));
            assert_eq!(result.model, format!("fake-model-{i}"));
            assert!(!result.msg.contains("fake-key"));
        }
    }

    /// 上游 base 的形态五花八门（有的带 /v1、有的到 /v4、有的直接给了 /models），
    /// 「从上游获取」要能把这些都拼对，否则用户点了只会得到一个 404
    #[test]
    fn models_url_handles_common_base_shapes() {
        assert_eq!(models_url("").as_deref(), None);
        assert_eq!(models_url("   ").as_deref(), None);
        assert_eq!(models_url("http://127.0.0.1/models/").as_deref(), Some("http://127.0.0.1/models"));
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

    #[test]
    fn protocol_format_prefers_explicit_format_then_infers_url() {
        assert_eq!(protocol_format("OpenAI-Resp", "http://127.0.0.1/v1"), "openai-resp");
        assert_eq!(protocol_format("", "http://127.0.0.1/v1/responses/"), "openai-resp");
        assert_eq!(protocol_format("anthropic", "http://127.0.0.1/v1/responses"), "anthropic");
        assert_eq!(protocol_format("", "http://127.0.0.1/v1"), "openai");
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
            "key_denied"
        );
        assert_eq!(
            classify_failure(400, r#"{"error":{"message":"model gpt-x not found"}}"#),
            "model_mismatch"
        );
        assert_eq!(classify_failure(400, r#"{"error":{"message":"bad request"}}"#), "error");
        assert_eq!(classify_failure(400, "dispatch failed"), "error");
        assert_eq!(classify_failure(400, "site-wide request limit reached"), "error");
        assert_eq!(classify_failure(401, "unauthorized"), "key_denied");
        assert_eq!(classify_failure(403, "forbidden"), "key_denied");
        assert_eq!(classify_failure(429, "slow down"), "rate_limited");
        assert_eq!(classify_failure(404, ""), "error");
        assert_eq!(classify_failure(404, "model fake does not exist"), "model_mismatch");
        assert_eq!(classify_failure(403, "This key has been disabled"), "key_denied");
        assert_eq!(classify_failure(400, "This group: chat_completions_disabled"), "key_denied");
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

    /// 同格式多根地址必须保留；站点和 Key 自带地址始终合并。
    #[test]
    fn matrix_columns_keeps_all_distinct_endpoints() {
        let site = json!({"base_urls": [
            {"format": "openai-resp", "url": "https://a/v1/responses"},
            {"format": "openai",      "url": "https://a/v1"},
            {"format": "openai",      "url": "https://a/v1-duplicate"},
            {"format": "anthropic",   "url": "  "}
        ]});
        let cols = matrix_columns(&site);
        assert_eq!(cols.len(), 3, "同一格式的不同端点都占一列，空地址跳过");
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
        assert_eq!(models_for(&site, &json!({ "models_list": "b, c" })), vec!["b", "c"]);
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
