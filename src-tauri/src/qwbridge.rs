//! 千问办公（QwenWork）本地桥。
//!
//! 桌面端把模型请求走 `gateway.qwenwork.cn/algo/...`，那条路要客户端自己的 WASM 签名，
//! 第三方复刻不了。但网页版用的是 `wss://qwenwork.cn/api/chat-ws`，只要一枚 Bearer JWT，
//! 所以本地起一个 OpenAI 兼容外壳（`qwenwork-bridge`，默认 127.0.0.1:8765），
//! 由它拿桌面端的登录态去换成积分消耗。这里只负责：探活、读模型清单、报告接线状态。
//!
//! 桥没在跑就**如实报没上游**，不拿一个连不上的地址去进池子。

use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub const APPS: [&str; 1] = ["qwenwork"];
pub const PORT: u16 = 8765;
/// 桥是标准 OpenAI 兼容（自带 /v1），客户端发 `/v1/chat/completions` 正好接上。
pub const GATEWAY_BASE: &str = "http://127.0.0.1:8765/v1";

const CACHE_TTL: Duration = Duration::from_secs(30);

static CACHE: Mutex<Option<(Instant, Vec<Value>)>> = Mutex::new(None);

/// 模式的显示名：下拉里要看得懂，别只给个 key
fn mode_name(id: &str) -> String {
    match id {
        "flash" => "标准".to_string(),
        "pro" => "高级".to_string(),
        "qwen3.8-max-preview" => "Qwen3.8-Max".to_string(),
        _ => id.to_string(),
    }
}

/// 端口在监听就算桥在跑：桥是"起来就一直挂着"的本地服务，不做进程名匹配
pub fn running() -> bool {
    use std::net::{SocketAddr, TcpStream};
    let addr: SocketAddr = ([127, 0, 0, 1], current_port()).into();
    TcpStream::connect_timeout(&addr, Duration::from_millis(400)).is_ok()
}

const SVC_NAME: &str = "qwenwork-bridge";

/// 端口在监听 **且确实是我们的桥**。8765 这种好记的端口会被别的工具占
/// （2026-10-27 实录：agentfind.py serve 占了 8765，对任何路径回 not found，
/// 界面显示"桥在跑"、实际每条请求都是别人回的——0 个模型、测试全失败）。
/// 桥的 /health 会自报 `svc: "qwenwork-bridge"`，借这个特征区分"端口被占"和"桥在跑"。
pub fn running_verified() -> bool {
    verified_on_port(current_port())
}

fn verified_on_port(port: u16) -> bool {
    if !port_busy(port) {
        return false;
    }
    match ureq::get(&format!("http://127.0.0.1:{port}/health"))
        .timeout(Duration::from_secs(2))
        .call()
    {
        Ok(r) => r
            .into_json::<Value>()
            .ok()
            .and_then(|v| v["svc"].as_str().map(|s| s == SVC_NAME))
            .unwrap_or(false),
        Err(_) => false,
    }
}

/// 备用端口：首选口被别人占时退到这里
pub const ALT_PORT: u16 = 18765;

/// 当前生效端口：config 里存过（起桥时按实际选定端口写入）就用存的，否则首选 8765
pub fn current_port() -> u16 {
    crate::registry::load_config()["proxy"]["qw_port"]
        .as_u64()
        .unwrap_or(PORT as u64) as u16
}

/// 桥基址随 current_port 走（GATEWAY_BASE 常量只在旧配置读取兼容时用）
pub fn base() -> String {
    format!("http://127.0.0.1:{}/v1", current_port())
}

/// 指定端口有没有人听（不经过 config，探测谁就答谁）
fn port_busy(p: u16) -> bool {
    use std::net::{SocketAddr, TcpStream};
    let addr: SocketAddr = ([127, 0, 0, 1], p).into();
    TcpStream::connect_timeout(&addr, Duration::from_millis(400)).is_ok()
}

/// 起桥前挑端口：自家的桥已在跑就不动；首选口空闲回首选（config 里的旧端口一并拉回来）；
/// 首选口被别人占了换备用口（返回的 note 非空时调用方要把新端口写进 config）。
/// 判断「首选口能不能用」要直接探测 8765 本身——沿用 config 里记的旧端口去探，
/// 会把「备用口空闲」误读成「首选口可用」，起在真正被占的 8765 上。
fn pick_port() -> Result<(u16, Option<String>), String> {
    if running_verified() {
        return Ok((current_port(), None));
    }
    if !port_busy(PORT) {
        return Ok((PORT, None));
    }
    if verified_on_port(PORT) {
        return Ok((PORT, None));
    }
    if verified_on_port(ALT_PORT) {
        return Ok((ALT_PORT, Some(format!("端口 {PORT} 被其它程序占用，使用已运行的 {ALT_PORT}"))));
    }
    if port_busy(ALT_PORT) {
        return Err(format!("端口 {PORT} 和备用端口 {ALT_PORT} 均被其它程序占用，请释放其中一个端口"));
    }
    Ok((ALT_PORT, Some(format!("端口 {PORT} 被其它程序占用，改用 {ALT_PORT}"))))
}

/// 桌面端登录态解不解得出来（不启动桥、不发网络）：member_options 判「能不能挂」用
pub fn cred_exists() -> bool {
    cred().is_some()
}

/// 模型清单缓存 30 秒：state 每 4 秒轮询一次，不该每次都打一次本地 HTTP
fn models() -> Vec<Value> {
    if let Ok(g) = CACHE.lock() {
        if let Some((at, ref v)) = *g {
            if at.elapsed() < CACHE_TTL && !v.is_empty() {
                return v.clone();
            }
        }
    }
    let url = format!("{}/models", base());
    let got: Vec<Value> = match ureq::get(&url).timeout(Duration::from_secs(3)).call() {
        Ok(r) => r
            .into_json::<Value>()
            .ok()
            .and_then(|v| {
                v["data"].as_array().map(|a| {
                    a.iter()
                        .filter_map(|m| m["id"].as_str())
                        .map(|id| json!({ "id": id, "name": mode_name(id) }))
                        .collect::<Vec<_>>()
                })
            })
            .unwrap_or_default(),
        Err(_) => Vec::new(),
    };
    if !got.is_empty() {
        if let Ok(mut g) = CACHE.lock() {
            *g = Some((Instant::now(), got.clone()));
        }
    }
    got
}

fn clear_cache() {
    if let Ok(mut g) = CACHE.lock() {
        *g = None;
    }
}

/// 桌面端登录态：只读 token / 到期时间，refreshToken 一概不碰
struct Cred {
    token: String,
    expires_at: String,
}

fn data_dir() -> PathBuf {
    PathBuf::from(std::env::var("APPDATA").unwrap_or_default()).join("QwenWorkCN")
}

fn master_key(dir: &Path) -> Option<[u8; 32]> {
    let txt = std::fs::read_to_string(dir.join("Local State")).ok()?;
    let v: Value = serde_json::from_str(&txt).ok()?;
    let raw = crate::secrets::b64_decode(v["os_crypt"]["encrypted_key"].as_str()?).ok()?;
    let body = raw.strip_prefix(b"DPAPI".as_ref())?;
    let plain = crate::secrets::dpapi_open(&crate::secrets::b64_encode(body)).ok()?;
    if plain.len() < 32 {
        return None;
    }
    let mut k = [0u8; 32];
    k.copy_from_slice(&plain[..32]);
    Some(k)
}

fn cred() -> Option<Cred> {
    let dir = data_dir();
    let blob = std::fs::read(dir.join("auth-v2.dat")).ok()?;
    if blob.len() < 31 || &blob[..3] != b"v10" {
        return None;
    }
    let key = master_key(&dir)?;
    let plain = crate::qproxy::gcm_open(&key, &blob[3..15], &blob[15..], &[])?;
    let v: Value = serde_json::from_slice(&plain).ok()?;
    let token = v["token"].as_str().unwrap_or("").trim().to_string();
    if token.is_empty() {
        return None;
    }
    Some(Cred {
        token,
        expires_at: v["expiresAt"].as_str().unwrap_or("").to_string(),
    })
}

/// 桥有没有被挂进某个上游组，以及那条链路上能用的接入密钥
fn wiring() -> (bool, String) {
    let cfg = crate::registry::load_config();
    let pools = cfg["proxy"]["pools"].as_array().cloned().unwrap_or_default();
    let mut hit: Vec<String> = Vec::new();
    for p in &pools {
        let members = p["members"].as_array().cloned().unwrap_or_default();
        let mine = members.iter().any(|m| {
            m["src"]
                .as_str()
                .unwrap_or("")
                .starts_with("bridge:qwenwork/")
        });
        if !mine {
            continue;
        }
        if p["enabled"].as_bool().unwrap_or(true) {
            if let Some(id) = p["id"].as_str() {
                hit.push(id.to_string());
            }
        }
    }
    if hit.is_empty() {
        return (false, String::new());
    }
    let dist = cfg["proxy"]["dist"].as_array().cloned().unwrap_or_default();
    for d in dist {
        if !d["enabled"].as_bool().unwrap_or(true) {
            continue;
        }
        let ok = d["pools"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .iter()
            .any(|x| x.as_str().map(|s| hit.iter().any(|h| h == s)).unwrap_or(false));
        if ok {
            return (true, d["key"].as_str().unwrap_or("").to_string());
        }
    }
    (true, String::new())
}

pub fn view() -> Value {
    let up = running_verified();
    let ms = if up { models() } else { Vec::new() };
    let (in_pool, key) = wiring();
    // 单独确认首选口的身份；备用口上的桥运行中也不能隐藏首选口冲突。
    let squatter = port_busy(PORT) && !verified_on_port(PORT);
    json!({
        "app": "qwenwork",
        "port": current_port(),
        "base": base(),
        "running": up,
        "port_conflict": squatter,
        "port_conflict_msg": if squatter {
            if up { format!("端口 {} 被其它程序占用，千问桥当前使用端口 {}", PORT, current_port()) }
            else { format!("端口 {} 被其它程序占用（不是千问桥）：点「一键反代」会尝试备用端口 {}", PORT, ALT_PORT) }
        } else { String::new() },
        "models": ms,
        "in_pool": in_pool,
        "key": key,
        "dist": dist_view(),
        "accounts": accounts(up),
    })
}

/// 刷新 / 重新探测：清掉 30 秒缓存，立刻重读一次
pub fn probe() -> Value {
    clear_cache();
    view()
}

/// 给 proxy.rs /v1/models 用的公开出口：桥没在跑就空表（不在此处起桥，请求路径才有自愈）
pub fn models_view() -> Vec<Value> {
    if !running_verified() {
        return Vec::new();
    }
    models()
}

/// 面板「获取模型ID」用：桥没跑就先拉起来（起桥 2~5 秒，比"列不出模型"强）。
/// id 一律带 `qwenwork/` 前缀——`flash`、`pro` 这些裸名在反代里会漂到别家。
pub fn model_ids() -> Value {
    if !running_verified() {
        if cred().is_none() {
            return json!({ "ok": false, "msg": "桌面端登录态读不到：打开千问办公登录一次" });
        }
        let r = start();
        if r["ok"].as_bool() != Some(true) {
            return json!({ "ok": false, "msg": r["msg"].as_str().unwrap_or("起桥失败") });
        }
    }
    let ms = models();
    if ms.is_empty() {
        return json!({ "ok": false, "msg": "桥没回模型清单" });
    }
    let tagged = crate::proxy::tag_owner("qwenwork", &ms);
    json!({ "ok": true, "count": tagged.len(), "models": tagged })
}

/// 一键：验登录态 → 起桥 → 接线（上游组/接入密钥缺才建，已有的原样复用）→ 起反代。
/// 一个按钮从"没配过"走到"客户端能调"，不再有「启动桥」+「启动反代」两段式。
pub fn one_click(proxy: &std::sync::Arc<crate::proxy::Proxy>) -> Value {
    let mut steps: Vec<String> = Vec::new();

    if cred().is_none() {
        return json!({ "ok": false, "msg": "桌面端登录态读不到（%APPDATA%\\QwenWorkCN\\auth-v2.dat）：打开千问办公登录一次", "steps": steps });
    }
    steps.push("登录态可用".into());

    if !running_verified() {
        let r = start();
        if r["ok"].as_bool() != Some(true) {
            return json!({ "ok": false, "msg": r["msg"].clone(), "steps": steps });
        }
        if let Some(n) = r["msg"].as_str().and_then(|s| s.split('（').nth(1)) {
            steps.push(format!("本地桥已就绪（{}", n));
        } else {
            steps.push("本地桥已就绪".into());
        }
    } else {
        steps.push("本地桥已就绪".into());
    }

    // 桥成员的 src 是 bridge:qwenwork/<账号名>：跟 instances 里登记的名字对齐
    let name = accounts(false)
        .first()
        .and_then(|a| a["name"].as_str())
        .unwrap_or("古零")
        .to_string();
    let (_pid, key, wire_steps) = crate::gateway::ensure_wiring(
        "gp_qwbridge",
        "千问办公 上游组",
        &format!("bridge:qwenwork/{name}"),
        "dk_qwbridge",
        "千问办公 接入",
    );
    steps.extend(wire_steps);
    if !key.is_empty() {
        steps.push("接入密钥已就绪".into());
    }

    if !proxy.is_running() {
        let port = crate::qproxy::proxy_port();
        proxy.set_config(Some("https://api.openai.com"), None);
        let mut cfg = crate::registry::load_config();
        if !cfg["proxy"].is_object() {
            cfg["proxy"] = json!({});
        }
        cfg["proxy"]["port"] = json!(port);
        crate::registry::save_config(&cfg);
        let (o, m) = proxy.start(port);
        if !o {
            return json!({ "ok": false, "msg": format!("反代起不来：{m}"), "steps": steps });
        }
        steps.push(format!("反代已启动 127.0.0.1:{port}"));
    } else {
        steps.push("反代已在运行".into());
    }
    clear_cache();
    json!({
        "ok": true,
        "msg": format!("千问办公反代已可用（{} 个模型）", models().len()),
        "steps": steps,
    })
}

/// 桥目录：优先环境变量，其次本机这个固定位置（桥是本仓库的配套件）
fn bridge_dir() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("QWB_DIR") {
        let pb = PathBuf::from(p);
        if pb.join("bridge.py").exists() {
            return Some(pb);
        }
    }
    let cand = PathBuf::from(r"D:\Claude\人工智能产品测试\qwenwork-bridge");
    if cand.join("bridge.py").exists() {
        return Some(cand);
    }
    None
}

/// 起桥用 pythonw：GUI 子系统，永远不会弹控制台窗口
fn python_exe(windowless: bool) -> String {
    if let Ok(p) = std::env::var("QWB_PY") {
        return p;
    }
    // 桥要 websockets + cryptography + 完整 stdlib。agenthub venv 的 base 是 WorkBuddy
    // 自带的 embeddable Python,stdlib 残缺（2026-10-28 实录：chat 路径报
    // No module named 'dataclasses'，命令行同一个 exe 却好好的），所以系统安装的
    // Python 优先，venv 只作后备。
    for base in [r"C:\Python314", r"C:\Python313", r"C:\Python312"] {
        let w = format!(r"{base}\pythonw.exe");
        let v = format!(r"{base}\python.exe");
        if windowless && std::path::Path::new(&w).exists() {
            return w;
        }
        if std::path::Path::new(&v).exists() {
            return v;
        }
    }
    let dir = r"C:\Users\LENOVO\.agenthub\venv\Scripts";
    if windowless {
        let w = format!(r"{dir}\pythonw.exe");
        if std::path::Path::new(&w).exists() {
            return w;
        }
    }
    let v = format!(r"{dir}\python.exe");
    if std::path::Path::new(&v).exists() {
        return v;
    }
    "python".to_string()
}

/// 起桥：脱离父进程，桥自己带 `--idle-exit`，不会空跑通宵，也不弹窗。
/// 首选端口被**别人**占着就换备用口并把选择写进 config（端口是桥的启动参数，
/// 不是能事后改的，所以必须在 spawn 前定下来）。
pub fn start() -> Value {
    if running_verified() {
        return json!({ "ok": true, "msg": "桥已经在跑" });
    }
    let Some(dir) = bridge_dir() else {
        return json!({ "ok": false, "msg": "没找到 qwenwork-bridge 目录（可用环境变量 QWB_DIR 指定）" });
    };
    let (port, note) = match pick_port() {
        Ok(choice) => choice,
        Err(msg) => return json!({ "ok": false, "msg": msg }),
    };
    {
        // 起桥就把实际端口写回 config：current_port() 是 running()/base()/反代自愈的唯一事实源。
        // 只在换口时写的话，config 里残留的旧端口会让系统盯着一个没人听的口——
        // 桥明明起来了却处处判「没跑」，每次请求都再 spawn 一条（2026-10-28 实录：8765 双桥）。
        let mut cfg = crate::registry::load_config();
        if !cfg["proxy"].is_object() {
            cfg["proxy"] = json!({});
        }
        cfg["proxy"]["qw_port"] = json!(port);
        crate::registry::save_config(&cfg);
        let _ = &note;
    }
    if running_verified() {
        clear_cache();
        return json!({ "ok": true, "msg": note.unwrap_or_else(|| "桥已经在跑".to_string()) });
    }
    let mut c = Command::new(python_exe(true));
    c.current_dir(&dir)
        .args([
            "bridge.py",
            "--port",
            &port.to_string(),
            "--idle-exit",
            "120",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // DETACHED_PROCESS + CREATE_NO_WINDOW：既脱离父进程，也不要控制台
        c.creation_flags(0x0000_0008 | 0x0800_0000);
    }
    match c.spawn() {
        Ok(_) => {
            for _ in 0..50 {
                std::thread::sleep(Duration::from_millis(200));
                if running_verified() {
                    clear_cache();
                    let mut msg = "桥已启动".to_string();
                    if let Some(n) = note {
                        msg.push_str(&format!("（{n}）"));
                    }
                    return json!({ "ok": true, "msg": msg });
                }
            }
            json!({ "ok": false, "msg": format!("进程起来了但端口 {port} 没响应，看 qwenwork-bridge 里的报错") })
        }
        Err(e) => json!({ "ok": false, "msg": format!("启动失败：{e}") }),
    }
}

/// 停桥：交给脚本自己停（它按 pid + /health 核对身份，认不出就不动）
pub fn stop() -> Value {
    let Some(dir) = bridge_dir() else {
        return json!({ "ok": false, "msg": "没找到 qwenwork-bridge 目录" });
    };
    let mut c = Command::new(python_exe(false));
    c.current_dir(&dir)
        .args(["bridge.py", "--stop"])
        .stdin(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(0x0800_0000); // CREATE_NO_WINDOW：这一下也不能闪黑框
    }
    let out = c.output();
    clear_cache();
    match out {
        Ok(o) => {
            let msg = String::from_utf8_lossy(&o.stdout).trim().to_string();
            json!({ "ok": !running(), "msg": if msg.is_empty() { "已停止".into() } else { msg } })
        }
        Err(e) => json!({ "ok": false, "msg": format!("停止失败：{e}") }),
    }
}

/// 接入密钥：只在这条链路上取，取不到就空
pub fn reveal() -> String {
    wiring().1
}

/// 拿桥发一次极小请求，确认积分真的能花出去
pub fn test(model: &str) -> Value {
    if !running_verified() {
        if running() {
            return json!({ "ok": false, "error": format!("端口 {} 被其它程序占用（不是千问桥）：点「一键反代」换备用端口", PORT) });
        }
        return json!({ "ok": false, "error": "桥没在跑：点「一键反代」会自动拉起" });
    }
    let m = if model.trim().is_empty() {
        "flash"
    } else {
        model.trim()
    };
    let body = json!({
        "model": m,
        "messages": [{ "role": "user", "content": "只回复两个字：可用" }],
        "stream": false,
    });
    let t0 = Instant::now();
    let url = format!("{}/chat/completions", base());
    match ureq::post(&url)
        .timeout(Duration::from_secs(120))
        .send_json(body)
    {
        Ok(r) => {
            let v: Value = r.into_json().unwrap_or(json!({}));
            let text = v["choices"][0]["message"]["content"]
                .as_str()
                .unwrap_or("")
                .to_string();
            json!({
                "ok": text.to_lowercase().contains("可用"),
                "text": text,
                "ms": t0.elapsed().as_millis() as u64,
                "model_used": v["model"].as_str().unwrap_or(m),
                "usage": v["usage"].clone(),
                "error": if text.is_empty() { "桥回了空内容" } else { "" },
            })
        }
        Err(e) => json!({ "ok": false, "error": format!("{e}") }),
    }
}

fn dist_view() -> Value {
    let cfg = crate::registry::load_config();
    let dist = cfg["proxy"]["dist"].as_array().cloned().unwrap_or_default();
    for d in dist {
        if d["id"].as_str().unwrap_or("") != "dk_qwbridge" {
            continue;
        }
        let key = d["key"].as_str().unwrap_or("");
        let masked = if key.len() > 8 {
            format!("{}…{}", &key[..5], &key[key.len() - 4..])
        } else {
            String::new()
        };
        return json!({
            "id": "dk_qwbridge",
            "name": d["name"].as_str().unwrap_or(""),
            "masked": masked,
            "calls": d["calls"].as_u64().unwrap_or(0),
        });
    }
    Value::Null
}

/// 账号表：令牌前缀与剩余天数照着 WorkBuddy 那几列的口径给
fn accounts(up: bool) -> Vec<Value> {
    let calls = dist_view()["calls"].as_u64().unwrap_or(0);
    let cre = cred();
    let tok_mask = cre
        .as_ref()
        .map(|c| {
            let t = &c.token;
            format!("{}…{}", &t[..6.min(t.len())], &t[t.len().saturating_sub(3)..])
        })
        .unwrap_or_default();
    let days = cre
        .as_ref()
        .and_then(|c| crate::qproxy::days_left(&c.expires_at))
        .unwrap_or(-1);
    let exp = cre.as_ref().map(|c| c.expires_at.clone()).unwrap_or_default();

    let mut out = Vec::new();
    let dir = crate::registry::base_dir().join("instances").join("qwenwork");
    let Ok(rd) = std::fs::read_dir(&dir) else {
        return out;
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let Ok(txt) = std::fs::read_to_string(&p) else {
            continue;
        };
        let Ok(v) = serde_json::from_str::<Value>(&txt) else {
            continue;
        };
        let name = v["name"].as_str().unwrap_or("").to_string();
        if name.is_empty() {
            continue;
        }
        out.push(json!({
            "name": name,
            "left": v["credits_left"].clone(),
            "expires_on": v["expires_on"].as_str().unwrap_or("").to_string(),
            "region": "国内",
            "state": if up { "ok" } else { "off" },
            "text": if up { "可用" } else { "桥没开" },
            "token_mask": tok_mask,
            "token_days": days,
            "token_exp": exp,
            "calls": calls,
        }));
    }
    out
}
