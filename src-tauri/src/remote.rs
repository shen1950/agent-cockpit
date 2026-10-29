// Web 远程控制（M1）：手机扫码进 mobile.html，看额度 / 一键刷新 / 每日签到。
// 设计与安全模型见 docs/WEB_REMOTE_DESIGN.md。四条铁律落在代码上：
//   1) 监听默认关闭，只在用户点了「开启」后 bind（重启不自动恢复，同 8788 反代的先例）；
//   2) 手机端能调的动作是编译期白名单（WEB_ACTIONS），换号/删除/搬家/反代管理一概不进来；
//   3) 出网状态是**字段白名单投影**（curate），不是桌面 state 原样——
//      桌面那份里有 settings.ai.key 明文和各家反代视图，那些不许出网；
//   4) 每个请求进审计日志；配对码 10 分钟一次性；token 存 DPAPI 保管箱。
use crate::commands::{self, AppState};
use crate::secrets;
use crate::state;
use serde_json::{json, Value};
use std::io::Read;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use tiny_http::{Header, Response, Server};

const PORT: u16 = 8790;
/// 配对码有效期：10 分钟。生成新的会顶掉旧的（同一时刻只有一个有效码）。
const PAIR_TTL_SECS: f64 = 600.0;
/// 无人访问 N 小时自动关闭。「无人」从开启时刻起算——开着没人用就不该一直监听。
const IDLE_OFF_HOURS: f64 = 6.0;
/// 请求体上限：action payload 都是几十字节的量级，64KB 绰绰有余
const MAX_BODY: usize = 64 * 1024;
/// 同一 IP 连续鉴权失败超过这个数（10 分钟窗口内）就拉黑，防对着 token 端点爆破
const FAIL_LIMIT: usize = 10;

/// 手机端可调用的动作白名单。**只读 + 刷新 + 签到 + 测活**。
/// 这张表是安全边界本身：往里加名字之前先想清楚它出网之后被任意设备
/// 连点会发生什么（写在这里的都应该是"连点一万次也没有破坏性后果"的）。
const WEB_ACTIONS: &[&str] = &[
    "refresh_all",        // 一键刷新全部额度（后台任务，连点会被告知"上一轮还在跑"）
    "quota_refresh",      // 单卡刷新额度
    "fetch_quota",        // 按 quota_url 抓一次
    "fetch_usage",        // CLI 本地会话用量
    "checkin_view",       // 签到页缓存
    "checkin_refresh",    // 全量查一轮签到状态
    "checkin_refresh_one", // 只重查一家
    "checkin_claim",      // 领取（写操作里最安全的一类：多领一次服务端会说已领）
    "checkin_auto",       // 勾/取消某家自动领
    "mark_seen",          // 消掉临期徽章
    "key_test",           // Key 测活（只读探测）
];

static APP: OnceLock<tauri::AppHandle> = OnceLock::new();
pub fn set_app(h: tauri::AppHandle) {
    let _ = APP.set(h);
}

fn now_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/* ---------------- 配对凭据随机数 ---------------- */

/// 系统密码学随机源失败时拒绝配对，不退回可预测的随机数。
#[cfg(windows)]
fn secure_random(bytes: &mut [u8]) -> Result<(), String> {
    #[link(name = "bcrypt")]
    unsafe extern "system" {
        fn BCryptGenRandom(algorithm: *mut std::ffi::c_void, buffer: *mut u8, len: u32, flags: u32) -> i32;
    }
    let len = u32::try_from(bytes.len()).map_err(|_| "随机数长度超限".to_string())?;
    // BCRYPT_USE_SYSTEM_PREFERRED_RNG：由系统选择密码学安全的随机源。
    let status = unsafe { BCryptGenRandom(std::ptr::null_mut(), bytes.as_mut_ptr(), len, 2) };
    if status < 0 { Err("系统安全随机数不可用，无法配对".into()) } else { Ok(()) }
}

#[cfg(unix)]
fn secure_random(bytes: &mut [u8]) -> Result<(), String> {
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(bytes))
        .map_err(|_| "系统安全随机数不可用，无法配对".into())
}

#[cfg(not(any(windows, unix)))]
fn secure_random(_bytes: &mut [u8]) -> Result<(), String> {
    Err("当前系统不支持安全随机数，无法配对".into())
}

fn random_hex(n: usize) -> Result<String, String> {
    let mut bytes = vec![0u8; n];
    secure_random(&mut bytes)?;
    Ok(bytes.iter().map(|b| format!("{b:02X}")).collect())
}

/// 配对码 128 位，设备 token 256 位；大写十六进制兼容既有扫码逻辑。
fn gen_code() -> Result<String, String> { random_hex(16) }
fn gen_token() -> Result<String, String> { random_hex(32) }

/// 逐字节比较，不短路——token 校验不给出"前几位对了"的时序信号
fn token_eq(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/* ---------------- 状态 ---------------- */

struct Device {
    id: String,
    name: String,
    token: String,
    created_at: f64,
    last_seen: f64,
}

struct Pairing {
    code: String,
    expires_at: f64,
}

struct Listener {
    server: Arc<Server>,
    active: Arc<AtomicBool>,
    worker: std::thread::JoinHandle<()>,
    address: std::net::SocketAddr,
}

struct Inner {
    listener: Mutex<Option<Listener>>,
    devices: Mutex<Vec<Device>>,
    loaded: AtomicBool,
    pair: Mutex<Option<Pairing>>,
    last_seen: Mutex<f64>,
    /// 配对失败与设备凭据失败分开统计，避免中继共享来源导致配对被误封。
    pair_fails: Mutex<Vec<(String, f64)>>,
    auth_fails: Mutex<Vec<(String, f64)>>,
    started_at: Mutex<f64>,
}

static REMOTE: OnceLock<Inner> = OnceLock::new();
fn r() -> &'static Inner {
    REMOTE.get_or_init(|| Inner {
        listener: Mutex::new(None),
        devices: Mutex::new(Vec::new()),
        loaded: AtomicBool::new(false),
        pair: Mutex::new(None),
        last_seen: Mutex::new(0.0),
        pair_fails: Mutex::new(Vec::new()),
        auth_fails: Mutex::new(Vec::new()),
        started_at: Mutex::new(0.0),
    })
}

fn devices_file() -> std::path::PathBuf {
    // 测试一律走临时目录：以前测试把「测试机」设备和 token 真写进了
    // 数据根的 remote_devices.json 和保管箱（审计日志里一排 10.0.0.9 就是它）
    #[cfg(test)]
    return std::env::temp_dir().join(format!("ac-remote-devices-{}.json", std::process::id()));
    #[cfg(not(test))]
    crate::registry::base_dir().join("remote_devices.json")
}

/// 设备名存数据根，token 存保管箱。保管箱拿不回 token 的设备视为已失效
/// （锁着的保管箱 / 换机器），开机后手机端会看到 401，重新扫码即可。
fn load_devices() {
    if r().loaded.swap(true, Ordering::Relaxed) {
        return;
    }
    let meta: Vec<Value> = std::fs::read_to_string(devices_file())
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| v["list"].as_array().cloned())
        .unwrap_or_default();
    let mut list = Vec::new();
    for m in meta {
        let id = m["id"].as_str().unwrap_or("").to_string();
        if id.is_empty() {
            continue;
        }
        let token = secrets::get(&format!("remote:dev:{id}")).unwrap_or_default();
        if token.is_empty() {
            continue;
        }
        list.push(Device {
            name: m["name"].as_str().unwrap_or("未命名设备").to_string(),
            created_at: m["created_at"].as_f64().unwrap_or(0.0),
            token,
            last_seen: m["last_seen"].as_f64().unwrap_or(0.0),
            id,
        });
    }
    *r().devices.lock().unwrap_or_else(|p| p.into_inner()) = list;
}

fn save_devices() {
    let g = r().devices.lock().unwrap_or_else(|p| p.into_inner());
    let list: Vec<Value> = g
        .iter()
        .map(|d| {
            json!({ "id": d.id, "name": d.name, "created_at": d.created_at, "last_seen": d.last_seen })
        })
        .collect();
    let _ = std::fs::write(
        devices_file(),
        serde_json::to_string_pretty(&json!({ "list": list })).unwrap_or_default(),
    );
}

/* ---------------- 审计 ---------------- */

fn audit_file() -> std::path::PathBuf {
    #[cfg(test)]
    return std::env::temp_dir().join(format!("ac-remote-audit-{}.log", std::process::id()));
    #[cfg(not(test))]
    crate::registry::base_dir().join("logs").join("remote-access.log")
}

fn audit(ev: &str, ip: &str, detail: &str) {
    let line = json!({
        "t": chrono::Local::now().format("%m-%d %H:%M:%S").to_string(),
        "ev": ev, "ip": ip, "d": detail,
    });
    if let Some(dir) = audit_file().parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(audit_file()) {
        use std::io::Write;
        let _ = writeln!(f, "{line}");
    }
}

fn audit_tail(n: usize) -> Vec<Value> {
    let text = std::fs::read_to_string(audit_file()).unwrap_or_default();
    text.lines()
        .rev()
        .take(n)
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .collect()
}

/* ---------------- 限速 ---------------- */

fn fail_in(fails: &Mutex<Vec<(String, f64)>>, ip: &str) {
    let now = now_secs();
    let mut g = fails.lock().unwrap_or_else(|p| p.into_inner());
    g.push((ip.to_string(), now));
    g.retain(|(_, t)| now - t < 600.0);
}

fn blocked_in(fails: &Mutex<Vec<(String, f64)>>, ip: &str) -> bool {
    let now = now_secs();
    let g = fails.lock().unwrap_or_else(|p| p.into_inner());
    g.iter().filter(|(i, t)| i == ip && now - t < 600.0).count() >= FAIL_LIMIT
}

/* ---------------- 服务生命周期 ---------------- */

// 统一串行远控监听和公网中继的启停；锁顺序为 CONTROL → listener → pair。
static CONTROL: Mutex<()> = Mutex::new(());

// tiny_http 的 unblock 只唤醒 recv_timeout，不会关闭其独立的 accept 线程。
// 保持生命周期锁直到 worker 退出、Server 被 drop 且端口可重新绑定，避免两次启停交错。
fn release_listener(listener: Listener) -> Result<(), String> {
    listener.active.store(false, Ordering::SeqCst);
    listener.server.unblock();
    let address = listener.address;
    let _ = listener.worker.join();
    drop(listener.server);
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    loop {
        match std::net::TcpListener::bind(address) {
            Ok(probe) => { drop(probe); return Ok(()); }
            Err(e) if std::time::Instant::now() >= deadline => {
                return Err(format!("关闭后端口 {} 尚未释放：{}", address.port(), e));
            }
            Err(_) => std::thread::sleep(Duration::from_millis(20)),
        }
    }
}

fn start_listener(slot: &Mutex<Option<Listener>>, address: std::net::SocketAddr) -> Result<Arc<AtomicBool>, String> {
    let mut slot = slot.lock().unwrap_or_else(|p| p.into_inner());
    if slot.as_ref().is_some_and(|l| l.active.load(Ordering::SeqCst)) {
        return Err("远程控制已在运行".into());
    }
    if let Some(old) = slot.take() { release_listener(old)?; }
    let server = Arc::new(Server::http(address).map_err(|e| format!("端口 {} 监听失败：{}", address.port(), e))?);
    let bound: std::net::SocketAddr = server.server_addr().to_string().parse()
        .map_err(|e| format!("获取监听地址失败：{e}"))?;
    let active = Arc::new(AtomicBool::new(true));
    let flag = active.clone();
    let worker_server = server.clone();
    let worker = std::thread::spawn(move || {
        while flag.load(Ordering::SeqCst) {
            match worker_server.recv_timeout(Duration::from_millis(500)) {
                Ok(Some(req)) => { std::thread::spawn(move || serve(req)); }
                Ok(None) => {}
                Err(_) => break,
            }
        }
        flag.store(false, Ordering::SeqCst);
    });
    *slot = Some(Listener { server, active: active.clone(), worker, address: bound });
    Ok(active)
}

// expected 限定只能关闭同一次启动，防止上一轮闲置计时器关闭新服务。
fn stop_listener(slot: &Mutex<Option<Listener>>, expected: Option<&Arc<AtomicBool>>, on_stop: impl FnOnce()) -> Result<bool, String> {
    let mut slot = slot.lock().unwrap_or_else(|p| p.into_inner());
    let matches = slot.as_ref().is_some_and(|l| expected.map_or(true, |e| Arc::ptr_eq(e, &l.active)));
    if !matches { return Ok(false); }
    let listener = slot.take().unwrap();
    let was_active = listener.active.load(Ordering::SeqCst);
    on_stop();
    release_listener(listener)?;
    Ok(was_active)
}

pub fn start() -> (bool, String) {
    let _control = CONTROL.lock().unwrap_or_else(|p| p.into_inner());
    load_devices();
    // 0.0.0.0：要的就是局域网里手机能连。私网地址在二维码里给手机看。
    let active = match start_listener(&r().listener, ([0, 0, 0, 0], PORT).into()) {
        Ok(flag) => flag,
        Err(e) => return (false, e),
    };
    let t = now_secs();
    *r().last_seen.lock().unwrap_or_else(|p| p.into_inner()) = t;
    *r().started_at.lock().unwrap_or_else(|p| p.into_inner()) = t;
    audit("start", "local", &format!("0.0.0.0:{}", PORT));

    // 防火墙放行放后台做：netsh 查询毫秒级，但缺规则时要等 UAC（用户可能半天不点），
    // 不能把 enable 那次 invoke 拖住。UAC 弹窗在开启后一两秒内出现。
    std::thread::spawn(|| {
        let (ok, msg) = fw_ensure();
        audit(if ok { "fw_ok" } else { "fw_fail" }, "local", &msg);
    });

    // 无人访问自动关闭：开着没人用就不该一直监听
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(Duration::from_secs(60));
            let _control = CONTROL.lock().unwrap_or_else(|p| p.into_inner());
            if !active.load(Ordering::SeqCst) {
                break;
            }
            // 用户显式开启公网远控后，需要能在离开电脑超过六小时后继续访问。
            // 只有公网中继活跃时保留服务，连接失败或手动停止后仍按闲置规则关闭。
            if matches!(crate::remote_tunnel::view()["state"].as_str(), Some("starting" | "ready" | "reconnecting")) {
                continue;
            }
            let last = *r().last_seen.lock().unwrap_or_else(|p| p.into_inner());
            if now_secs() - last > IDLE_OFF_HOURS * 3600.0 {
                if let Ok(true) = stop_listener(&r().listener, Some(&active), || {
                    *r().pair.lock().unwrap_or_else(|p| p.into_inner()) = None;
                }) {
                    let _ = crate::remote_tunnel::stop();
                    audit("idle_off", "local", "无人访问自动关闭");
                }
                break;
            }
        }
    });
    (true, format!("已开启 http://0.0.0.0:{}", PORT))
}

pub fn stop() -> (bool, String) {
    let _control = CONTROL.lock().unwrap_or_else(|p| p.into_inner());
    let _ = crate::remote_tunnel::stop();
    match stop_listener(&r().listener, None, || {
        *r().pair.lock().unwrap_or_else(|p| p.into_inner()) = None;
    }) {
        Ok(true) => {}
        Ok(false) => return (false, "远程控制没在运行".into()),
        Err(e) => return (false, e),
    }
    audit("stop", "local", "");
    (true, "已关闭".into())
}

pub fn is_running() -> bool {
    r().listener.lock().unwrap_or_else(|p| p.into_inner())
        .as_ref().is_some_and(|l| l.active.load(Ordering::SeqCst))
}

/// 在同一生命周期锁下检查监听并建立中继，避免关闭与启动交错。
pub fn start_tunnel(email: &str) -> Result<(), String> {
    let _control = CONTROL.lock().unwrap_or_else(|p| p.into_inner());
    if !is_running() { return Err("请先开启远程控制".into()); }
    crate::remote_tunnel::start(email)?;
    // 监听线程也可能因自身错误退出，避免中继指向已关闭的本地服务。
    if !is_running() {
        let _ = crate::remote_tunnel::stop();
        return Err("远程控制已关闭".into());
    }
    Ok(())
}

pub fn stop_tunnel() -> Result<(), String> {
    let _control = CONTROL.lock().unwrap_or_else(|p| p.into_inner());
    crate::remote_tunnel::stop()
}

/// 可用 IPv4 和网卡名。优先系统默认路由的本地地址，再提供其余网卡供用户选择。
/// UDP connect 只查询路由，不发送网络数据。
fn usable_ipv4(ip: std::net::Ipv4Addr) -> bool {
    let first = ip.octets()[0];
    first != 0 && first < 224 && !ip.is_loopback() && !ip.is_link_local() && !ip.is_broadcast()
}

fn lan_addresses() -> Vec<(String, String)> {
    let preferred = std::net::UdpSocket::bind("0.0.0.0:0")
        .and_then(|socket| { socket.connect("1.1.1.1:80")?; socket.local_addr() })
        .ok().map(|addr| addr.ip().to_string());
    let nets = sysinfo::Networks::new_with_refreshed_list();
    let mut out: Vec<(u8, String, String)> = Vec::new();
    for (name, data) in &nets {
        for ipn in data.ip_networks() {
            let std::net::IpAddr::V4(ip) = ipn.addr else { continue; };
            if !usable_ipv4(ip) { continue; }
            let s = ip.to_string();
            let rank = if preferred.as_deref() == Some(s.as_str()) {
                0
            } else if s.starts_with("192.168.") {
                1
            } else if s.starts_with("10.") {
                2
            } else {
                3
            };
            out.push((rank, s, name.to_string()));
        }
    }
    out.sort();
    out.dedup_by(|a, b| a.1 == b.1);
    out.into_iter().map(|(_, ip, name)| (ip, name)).collect()
}

fn lan_ips() -> Vec<String> {
    lan_addresses().into_iter().map(|(ip, _)| ip).collect()
}

fn lan_ip() -> String {
    lan_ips().into_iter().next().unwrap_or_else(|| "127.0.0.1".into())
}

fn pair_url(code: &str) -> String {
    format!("http://{}:{}/#t={}", lan_ip(), PORT, code)
}

fn pair_links(code: &str) -> Vec<Value> {
    pair_links_for(code, lan_addresses())
}

fn pair_links_for(code: &str, addresses: Vec<(String, String)>) -> Vec<Value> {
    addresses.into_iter().map(|(ip, interface)| {
        let url = format!("http://{}:{}/#t={}", ip, PORT, code);
        json!({ "ip": ip, "interface": interface, "qr_svg": qr_svg(&url), "url": url })
    }).collect()
}

/* ---------------- Windows 防火墙 ----------------
   三个配置文件默认 BlockInbound，驾驶舱又没有安装器帮你放行——第一次开远程
   的用户几乎必然撞上"手机扫码 → 网页不存在"（2026-10-28 实测就是这个）。
   本机自连不走入站防火墙，所以桌面端看着一切正常，更难排查。 */

const FW_RULE: &str = "AgentCockpit Remote 8790";

fn fw_rule_exists() -> bool {
    let out = std::process::Command::new("netsh")
        .args(["advfirewall", "firewall", "show", "rule", &format!("name={FW_RULE}")])
        .output();
    match out {
        Ok(o) => {
            // 中文系统的 netsh 输出是 GBK：规则名是 ASCII，逐字节找不受编码影响
            let text = String::from_utf8_lossy(&o.stdout);
            let lower = text.to_lowercase();
            lower.contains("agentcockpit") && !lower.contains("没有与指定标准相匹配") && !lower.contains("no rules match")
        }
        Err(_) => false,
    }
}

/// 没有放行规则就弹 UAC 添加一条（仅限私网网段）。返回 (是否已有/已成, 说明)
fn fw_ensure() -> (bool, String) {
    if fw_rule_exists() {
        return (true, "防火墙已有放行规则".into());
    }
    // 规则添加本身很轻，但提权必须过 UAC：单独起一个隐藏 PowerShell 完成，
    // 结果落临时文件回来核对（驱动器上没有它 = 用户拒绝了 UAC / 加载失败）
    let marker = std::env::temp_dir().join("ac-fw-8790-done.txt");
    let script = std::env::temp_dir().join("ac-fw-8790.ps1");
    let ps = format!(
        "try {{ netsh advfirewall firewall add rule name=\"{FW_RULE}\" dir=in action=allow protocol=TCP localport=8790 profile=any remoteip=10.0.0.0/8,172.16.0.0/12,192.168.0.0/16 | Out-Null; 'OK' | Set-Content -Path '{}' }} catch {{ $_.Exception.Message | Set-Content -Path '{}' }}\n",
        marker.display(),
        marker.display(),
    );
    if std::fs::write(&script, ps).is_err() {
        return (false, "写临时脚本失败，无法自动放行防火墙".into());
    }
    let _ = std::fs::remove_file(&marker);
    let inner = format!(
        "Start-Process powershell -Verb RunAs -Wait -WindowStyle Hidden -ArgumentList '-NoProfile','-ExecutionPolicy','Bypass','-File','{}'",
        script.display()
    );
    let spawn = std::process::Command::new("powershell")
        .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-Command", &inner])
        .output();
    match spawn {
        Ok(_) => {
            // -Wait 已经等 UAC 处理完；再核对一次规则真实状态（以系统为准，不看标记文件）
            if fw_rule_exists() {
                (true, "已自动添加防火墙放行规则（仅限私网）".into())
            } else {
                (false, "防火墙放行没成（UAC 被拒绝？）——手机会连不上；到 设置→远程控制 再试或手动放行 8790".into())
            }
        }
        Err(e) => (false, format!("起不来提权进程：{e}")),
    }
}

/* ---------------- 配对 ---------------- */

pub fn new_pair() -> Value {
    // 与 stop 共用生命周期锁，避免关闭后并发生成一个无效的配对码。
    let listener = r().listener.lock().unwrap_or_else(|p| p.into_inner());
    if !listener.as_ref().is_some_and(|l| l.active.load(Ordering::SeqCst)) {
        return json!({ "error": "远程控制尚未开启" });
    }
    let code = match gen_code() {
        Ok(code) => code,
        Err(error) => return json!({ "error": error }),
    };
    let p = Pairing { code, expires_at: now_secs() + PAIR_TTL_SECS };
    let url = pair_url(&p.code);
    let out = json!({
        "code": p.code, "url": url, "qr_svg": qr_svg(&url),
        "urls": pair_links(&p.code),
        "expires_in": PAIR_TTL_SECS,
    });
    *r().pair.lock().unwrap_or_else(|p| p.into_inner()) = Some(p);
    out
}

/// 手机端 POST /api/pair {code, name}。码对且没过期 → 发 token 并作废码
fn pair_consume(code: &str, name: &str, ip: &str) -> Result<Value, (u16, String)> {
    let now = now_secs();
    let mut g = r().pair.lock().unwrap_or_else(|p| p.into_inner());
    let ok = matches!(
        g.as_ref(),
        Some(p) if token_eq(&p.code, &code.to_uppercase()) && now < p.expires_at
    );
    if !ok {
        // 错码不能作废仍有效的配对码，否则任何访客都能阻止用户配对。
        if g.as_ref().is_some_and(|p| now >= p.expires_at) { *g = None; }
        // 中继的所有手机共用回环 IP。拒绝过量的错误码，但仍允许正确码配对，
        // 避免一个设备的失败导致其他设备无法配对。
        if blocked_in(&r().pair_fails, ip) {
            return Err((429, "尝试太多次，等几分钟再来".into()));
        }
        fail_in(&r().pair_fails, ip);
        audit("pair_fail", ip, "");
        return Err((401, "配对码不对或已过期，去桌面端重新生成".into()));
    }
    let token = gen_token().map_err(|e| (500, e))?;
    let id = format!("d{}", random_hex(16).map_err(|e| (500, e))?);
    *g = None; // 一次性；只有系统随机数生成成功后才消费。
    let name: String = if name.trim().is_empty() { "手机".into() } else { name.trim().chars().take(24).collect() };
    r().devices.lock().unwrap_or_else(|p| p.into_inner()).push(Device {
        id: id.clone(),
        name: name.clone(),
        token: token.clone(),
        created_at: now,
        last_seen: now,
    });
    if secrets::available() {
        persist_device_secret(&id, &token);
    }
    save_devices();
    audit("pair", ip, &name);
    Ok(json!({ "ok": true, "token": token, "device": id, "name": name }))
}

/// 设备 token 只进真实保管箱；测试里是 no-op（测试设备不该留进 DPAPI 库）
#[cfg(not(test))]
fn persist_device_secret(id: &str, token: &str) {
    if secrets::put(&format!("remote:dev:{id}"), token) {
        let _ = secrets::flush();
    }
}
#[cfg(test)]
fn persist_device_secret(_id: &str, _token: &str) {}

/// Bearer 校验：通过则记 last_seen 并返回 token（调用方只关心有没有过）
fn auth(bearer: &str) -> Option<String> {
    let token = bearer.strip_prefix("Bearer ").or_else(|| bearer.strip_prefix("bearer "))?.trim();
    if token.is_empty() {
        return None;
    }
    let now = now_secs();
    let mut g = r().devices.lock().unwrap_or_else(|p| p.into_inner());
    let hit = g.iter_mut().find(|d| token_eq(&d.token, token))?;
    hit.last_seen = now;
    drop(g);
    *r().last_seen.lock().unwrap_or_else(|p| p.into_inner()) = now;
    Some(token.to_string())
}

pub fn revoke(id: &str) -> bool {
    let mut g = r().devices.lock().unwrap_or_else(|p| p.into_inner());
    let before = g.len();
    g.retain(|d| d.id != id);
    let removed = g.len() < before;
    drop(g);
    if removed {
        #[cfg(not(test))]
        if secrets::available() {
            secrets::remove(&format!("remote:dev:{id}"));
            let _ = secrets::flush();
        }
        save_devices();
        audit("revoke", "local", id);
    }
    removed
}

/* ---------------- 出网状态投影 ---------------- */

/// 字段白名单投影。桌面那份 state 里 settings.ai.key 是明文、proxy.qproxy 等视图
/// 带各家账号会话细节——都**不许**出网。这里只挑 mobile 要的字段。
fn curate(st: &Value) -> Value {
    let instances: Vec<Value> = st["instances"]
        .as_array()
        .map(|list| {
            list.iter()
                .map(|i| {
                    json!({
                        "app": i["app"], "app_name": i["app_name"], "kind": i["kind"],
                        "name": i["name"], "running": i["running"],
                        "checkin": i["checkin"],
                        "usage": i["usage"],
                        "quota": {
                            "total": i["quota"]["total"], "left": i["quota"]["left"],
                            "expires_on": i["quota"]["expires_on"], "days_left": i["quota"]["days_left"],
                            "credits": i["quota"]["credits"], "plan_name": i["quota"]["plan_name"],
                            "source": i["quota"]["source"], "stale": i["quota"]["stale"],
                            "live_error": i["quota"]["live_error"], "web_error": i["quota"]["web_error"],
                            "fetched_at": i["quota"]["fetched_at"],
                        },
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    // Key 台账：连 masked 串都不带 key 原文字段，这里再显式挑一遍，谁以后往
    // list_masked 里加字段也不会意外漏到手机上
    let sites: Vec<Value> = st["keys"]
        .as_array()
        .map(|list| {
            list.iter()
                .map(|p| {
                    let keys: Vec<Value> = p["keys"]
                        .as_array()
                        .map(|ks| {
                            ks.iter()
                                .map(|k| {
                                    json!({
                                        "id": k["id"], "masked": k["masked"], "label": k["label"],
                                        "left": k["left"], "total": k["total"],
                                        "expires_on": k["expires_on"], "days_left": k["days_left"],
                                    })
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    json!({
                        "id": p["id"], "name": p["name"], "key_count": p["key_count"], "keys": keys,
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    json!({
        "now": st["now"], "version": st["app_version"],
        "stats": st["stats"],
        "alerts": st["alerts"],
        "instances": instances,
        "checkin": st["checkin"],
        "alljob": st["alljob"],
        "keys": sites,
        "proxy": {
            "running": st["proxy"]["running"], "port": st["proxy"]["port"],
            "base_url": st["proxy"]["base_url"],
            // 最近请求日志（谁走了哪个上游）：只带 10 条，手机「更多」页看个风向
            "logs": st["proxy"]["logs"].as_array()
                .map(|a| a.iter().take(10).cloned().collect::<Vec<_>>())
                .unwrap_or_default(),
        },
        // 保管箱只报状态不报内容（file 路径、提示语都不出网）
        "vault": {
            "supported": st["vault"]["supported"], "locked": st["vault"]["locked"],
            "has_password": st["vault"]["has_password"], "count": st["vault"]["count"],
        },
        "remote": { "idle_off_h": IDLE_OFF_HOURS },
    })
}

fn web_state() -> Value {
    match APP.get() {
        Some(h) => {
            use tauri::Manager;
            let s = h.state::<AppState>();
            curate(&state::build_state(&s.proxy))
        }
        None => json!({ "error": "应用还没就绪" }),
    }
}

/// 桌面端「远程控制」卡片要看的全部信息
pub fn view() -> Value {
    load_devices();
    let current_pair = {
        let mut pair = r().pair.lock().unwrap_or_else(|p| p.into_inner());
        if pair.as_ref().is_some_and(|p| p.expires_at <= now_secs()) { *pair = None; }
        pair.as_ref().map(|p| (p.code.clone(), p.expires_at))
    };
    let pair = current_pair.map(|(code, expires_at)| {
            let url = pair_url(&code);
            let qr = qr_svg(&url);
            let urls = pair_links(&code);
            json!({
                "code": code, "url": url, "qr_svg": qr,
                "urls": urls,
                "expires_in": (expires_at - now_secs()).max(0.0) as i64,
            })
        });
    let devices: Vec<Value> = r()
        .devices
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .iter()
        .map(|d| {
            json!({
                "id": d.id, "name": d.name, "created_at": d.created_at,
                "last_seen_ago": (now_secs() - d.last_seen).max(0.0),
            })
        })
        .collect();
    json!({
        "enabled": is_running(), "port": PORT, "lan_ip": lan_ip(),
        "lan_ips": lan_ips(),
        "fw_ok": fw_rule_exists(),
        "idle_off_h": IDLE_OFF_HOURS,
        "url": format!("http://{}:{}", lan_ip(), PORT),
        "pair": pair, "devices": devices,
        "audit": audit_tail(8),
        "whitelist": WEB_ACTIONS,
    })
}

/* ---------------- 二维码 ---------------- */

pub(crate) fn qr_svg(text: &str) -> String {
    let code = match qrcode::QrCode::with_error_correction_level(
        text.as_bytes(),
        qrcode::EcLevel::M,
    ) {
        Ok(c) => c,
        Err(_) => return String::new(),
    };
    let colors = code.to_colors();
    let w = code.width();
    let mut rects = String::new();
    for y in 0..w {
        for x in 0..w {
            if colors[y * w + x].select(0x00, 0xff) == 0x00 {
                rects.push_str(&format!("<rect x=\"{x}\" y=\"{y}\" width=\"1\" height=\"1\"/>"));
            }
        }
    }
    // 白底黑码：手机扫码器对深底浅码的识别差得多，二维码卡片不跟主题走
    format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 {w} {w}\" shape-rendering=\"crispEdges\"><rect width=\"{w}\" height=\"{w}\" fill=\"#ffffff\"/><g fill=\"#111111\">{rects}</g></svg>"
    )
}

/* ---------------- HTTP ---------------- */

fn hdr_json() -> Header {
    Header::from_bytes(&b"Content-Type"[..], &b"application/json; charset=utf-8"[..]).unwrap()
}
fn hdr_nosniff() -> Header {
    Header::from_bytes(&b"X-Content-Type-Options"[..], &b"nosniff"[..]).unwrap()
}
fn hdr_nostore() -> Header {
    Header::from_bytes(&b"Cache-Control"[..], &b"no-store"[..]).unwrap()
}

fn reply(req: tiny_http::Request, status: u16, body: String, ct: Header) {
    // ascii 那套 as_str 别链着调（AsciiString::as_str 给 &str，再 as_str 就是
    // unstable 的 str_as_str）——单层 + to_string，同 proxy.rs 的写法
    let is_json = ct.value.as_str().to_string().contains("json");
    let mut resp = Response::from_string(body)
        .with_status_code(status)
        .with_header(ct)
        .with_header(hdr_nosniff());
    if is_json {
        resp = resp.with_header(hdr_nostore());
    }
    let _ = req.respond(resp);
}

fn err_json(req: tiny_http::Request, status: u16, msg: &str) {
    reply(req, status, json!({ "ok": false, "msg": msg }).to_string(), hdr_json());
}

const MOBILE_HTML: &str = include_str!("../../ui/mobile.html");
const MOBILE_JS: &str = include_str!("../../ui/mobile.js");

fn serve(mut req: tiny_http::Request) {
    let ip = req
        .remote_addr()
        .map(|a| a.ip().to_string())
        .unwrap_or_else(|| "?".into());
    let method = req.method().as_str().to_string();
    let path = req.url().split('?').next().unwrap_or("/").to_string();

    // 配对端点：唯一不需要 token 的入口（有码本身就是凭证）
    if method == "POST" && path == "/api/pair" {
        let mut body = String::new();
        if req.as_reader().take(MAX_BODY as u64 + 1).read_to_string(&mut body).is_err() || body.len() > MAX_BODY {
            return err_json(req, 413, "请求体太大");
        }
        let v: Value = match serde_json::from_str(&body) {
            Ok(v) => v,
            Err(_) => return err_json(req, 400, "不是合法的 JSON"),
        };
        let code = v["code"].as_str().unwrap_or("").trim().to_uppercase();
        let name = v["name"].as_str().unwrap_or("").to_string();
        return match pair_consume(&code, &name, &ip) {
            Ok(v) => reply(req, 200, v.to_string(), hdr_json()),
            Err((s, m)) => err_json(req, s, &m),
        };
    }

    // 其余 /api/* 全要 token
    if path.starts_with("/api/") {
        let bearer = req
            .headers()
            .iter()
            .find(|h| h.field.as_str().as_str().eq_ignore_ascii_case("authorization"))
            .map(|h| h.value.as_str().to_string())
            .unwrap_or_default();
        if auth(&bearer).is_none() {
            // 隧道连接可能共享回环源地址；不能因陌生人的失败封锁已配对设备。
            if blocked_in(&r().auth_fails, &ip) {
                return err_json(req, 429, "尝试太多次，等几分钟再来");
            }
            fail_in(&r().auth_fails, &ip);
            audit("401", &ip, &path);
            return err_json(req, 401, "先在桌面端扫码配对");
        }
        if method == "GET" && (path == "/api/state" || path == "/api/ping") {
            let v = if path == "/api/ping" { json!({ "ok": true, "now": now_secs() }) } else { web_state() };
            return reply(req, 200, v.to_string(), hdr_json());
        }
        if method == "POST" && path == "/api/action" {
            let mut body = String::new();
            if req.as_reader().take(MAX_BODY as u64 + 1).read_to_string(&mut body).is_err() || body.len() > MAX_BODY {
                return err_json(req, 413, "请求体太大");
            }
            let v: Value = match serde_json::from_str(&body) {
                Ok(v) => v,
                Err(_) => return err_json(req, 400, "不是合法的 JSON"),
            };
            let act = v["action"].as_str().unwrap_or("").to_string();
            if !WEB_ACTIONS.contains(&act.as_str()) {
                audit("deny", &ip, &act);
                return err_json(req, 403, "这个操作不允许从手机上做，回桌面端来");
            }
            let h = match APP.get() {
                Some(h) => h.clone(),
                None => return err_json(req, 500, "应用还没就绪"),
            };
            audit("action", &ip, &act);
            let out = commands::service_action(&h, &v).unwrap_or_else(|e| json!({ "ok": false, "msg": e }));
            return reply(req, 200, out.to_string(), hdr_json());
        }
        return err_json(req, 404, "没有这个接口");
    }

    // 静态：只放 mobile 两件套，桌面那套 UI 不从这儿出
    let (body, ct): (&str, &[u8]) = match (method.as_str(), path.as_str()) {
        ("GET", "/") | ("GET", "/mobile.html") => (MOBILE_HTML, &b"text/html; charset=utf-8"[..]),
        ("GET", "/mobile.js") => (MOBILE_JS, &b"text/javascript; charset=utf-8"[..]),
        _ => return err_json(req, 404, "没有这个页面"),
    };
    let h = Header::from_bytes(&b"Content-Type"[..], ct).unwrap();
    let csp = Header::from_bytes(
        &b"Content-Security-Policy"[..],
        &b"default-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; script-src 'self'"[..],
    )
    .unwrap();
    let _ = req.respond(
        Response::from_string(body.to_string())
            .with_header(h)
            .with_header(csp)
            .with_header(hdr_nosniff()),
    );
}

/* ---------------- 测试 ---------------- */

#[cfg(test)]
mod tests {
    use super::*;

    // 这两项测试共享真实模块的配对状态，串行执行以免互相替换配对码或清理失败记录。
    static PAIRING_TEST_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn listener_can_stop_restart_and_ignore_a_stale_timer() {
        // 使用独立状态和系统临时端口，不启动真实远控或修改防火墙。
        let slot = Mutex::new(None);
        let first = start_listener(&slot, ([127, 0, 0, 1], 0).into()).unwrap();
        let address = slot.lock().unwrap().as_ref().unwrap().address;
        assert!(std::net::TcpStream::connect(address).is_ok());
        assert!(stop_listener(&slot, Some(&first), || {}).unwrap());
        assert!(!first.load(Ordering::SeqCst));
        // stop 已等待 tiny_http 的后台 accept 线程释放监听。
        drop(std::net::TcpListener::bind(address).expect("停止后端口应可立即重用"));
        let second = start_listener(&slot, address).unwrap();
        assert!(!stop_listener(&slot, Some(&first), || panic!("旧计时器不应关掉新服务")).unwrap());
        assert!(second.load(Ordering::SeqCst));
        assert!(std::net::TcpStream::connect(address).is_ok());
        assert!(stop_listener(&slot, Some(&second), || {}).unwrap());
    }

    #[test]
    fn concurrent_starts_have_only_one_listener() {
        let slot = Arc::new(Mutex::new(None));
        let barrier = Arc::new(std::sync::Barrier::new(3));
        let workers: Vec<_> = (0..2).map(|_| {
            let slot = slot.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                start_listener(&slot, ([127, 0, 0, 1], 0).into()).is_ok()
            })
        }).collect();
        barrier.wait();
        assert_eq!(workers.into_iter().map(|w| w.join().unwrap()).filter(|ok| *ok).count(), 1);
        assert!(stop_listener(&slot, None, || {}).unwrap());
    }

    #[test]
    fn pairing_links_cover_usable_addresses_with_the_same_code() {
        for bad in ["127.0.0.1", "169.254.1.1", "0.0.0.0", "224.0.0.1", "255.255.255.255"] {
            assert!(!usable_ipv4(bad.parse().unwrap()));
        }
        for good in ["10.100.174.71", "192.168.1.20", "100.100.10.1"] {
            assert!(usable_ipv4(good.parse().unwrap()));
        }
        let links = pair_links_for("AB23CD45", vec![
            ("10.100.174.71".into(), "WLAN".into()),
            ("172.29.208.1".into(), "WSL".into()),
        ]);
        assert_eq!(links.len(), 2);
        assert_eq!(links[0]["interface"], "WLAN");
        assert_eq!(links[1]["url"], "http://172.29.208.1:8790/#t=AB23CD45");
        assert!(links.iter().all(|v| v["url"].as_str().unwrap().ends_with("#t=AB23CD45")
            && v["qr_svg"].as_str().unwrap().starts_with("<svg")));
    }

    /// 白名单里绝不许出现破坏性动作——这是把驾驶舱放上网络的安全底线
    #[test]
    fn whitelist_has_no_destructive_actions() {
        let banned = [
            "auth_switch", "swap_set", "swap_clear", "rehome", "delete", "key_delete",
            "key_delete_batch", "key_delete_key", "kill", "kill_pids", "storage_migrate",
            "storage_rollback", "vault_", "proxy_start", "proxy_stop", "inject_apply",
        ];
        for a in WEB_ACTIONS {
            for b in banned {
                assert!(
                    !a.starts_with(b),
                    "白名单里混进了破坏性动作：{a}"
                );
            }
        }
    }

    /// 出网投影不许带 settings（ai.key 明文在里面）和各家反代视图
    #[test]
    fn curate_never_leaks_settings_or_proxy_views() {
        let st = json!({
            "now": "12:00", "app_version": "test",
            "settings": { "ai": { "key": "sk-PLAINTEXT" } },
            "proxy": { "running": true, "port": 8788, "base_url": "http://x",
                       "qproxy": { "secret": "x" }, "wbproxy": { "secret": "y" } },
            "keys": [ { "id": "s1", "name": "站", "key_count": 1,
                        "keys": [ { "id": "k1", "key": "sk-PLAIN", "masked": "sk-***AIN",
                                    "label": "用途", "left": 1, "total": 2,
                                    "expires_on": "2026-11-01", "days_left": 6 } ] } ],
            "instances": [], "checkin": {}, "alljob": null, "alerts": [], "stats": {},
        });
        let out = curate(&st).to_string();
        assert!(!out.contains("PLAINTEXT"), "settings 明文漏出去了");
        assert!(!out.contains("qproxy"), "qproxy 视图漏出去了");
        assert!(!out.contains("wbproxy"), "wbproxy 视图漏出去了");
        assert!(!out.contains("\"key\":\"sk-PLAIN\"") && !out.contains("sk-PLAIN\""), "key 原文字段漏出去了");
        assert!(out.contains("sk-***AIN"), "打码串应该在");
        assert!(out.contains("\"masked\""), "站点与 key 的安全字段应该在");
    }

    /// 配对码：一次性 + 过期即作废 + 连错计数
    #[test]
    fn pairing_code_is_single_use_and_rate_limited() {
        let _guard = PAIRING_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let code = gen_code().unwrap();
        assert_eq!(code.len(), 32);
        assert!(code.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_eq!(gen_token().unwrap().len(), 64);
        *r().pair.lock().unwrap() = Some(Pairing { code: code.clone(), expires_at: now_secs() + 60.0 });
        let ip = "10.0.0.9";
        assert!(pair_consume("WRONG", "测试机", ip).is_err());
        assert_eq!(r().pair.lock().unwrap().as_ref().map(|p| p.code.clone()), Some(code.clone()), "错误配对不能使有效码失效");
        // 码对 → 成功，且立刻作废
        assert!(pair_consume(&code, "测试机", ip).is_ok());
        assert!(pair_consume(&code, "测试机", ip).is_err(), "一次性码不能用第二次");
        // 错码连打 → 计失败；FAIL_LIMIT 次后该 IP 被挡
        for i in 0..FAIL_LIMIT {
            *r().pair.lock().unwrap() = Some(Pairing { code: "XXXXXXXX".into(), expires_at: now_secs() + 60.0 });
            let _ = pair_consume("WRONGWRONG", &format!("10.1.1.{i}"), ip);
        }
        assert!(blocked_in(&r().pair_fails, ip), "连错太多次的 IP 要被挡住");
        assert_eq!(pair_consume("WRONGWRONG", "测试机", ip).unwrap_err().0, 429);
        let new_code = gen_code().unwrap();
        *r().pair.lock().unwrap() = Some(Pairing { code: new_code.clone(), expires_at: now_secs() + 60.0 });
        assert!(pair_consume(&new_code, "测试机", ip).is_ok(), "共享中继地址被限速后仍应允许正确配对码");
        // 恢复现场，别影响别的用例
        r().pair_fails.lock().unwrap().retain(|(failed_ip, _)| failed_ip != ip);
        r().devices.lock().unwrap().retain(|d| d.name != "测试机");
    }

    /// 过期的码不认
    #[test]
    fn expired_code_is_rejected() {
        let _guard = PAIRING_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        *r().pair.lock().unwrap() = Some(Pairing { code: "AAAAAAAA".into(), expires_at: now_secs() - 1.0 });
        assert!(pair_consume("AAAAAAAA", "", "10.2.2.2").is_err());
        r().pair_fails.lock().unwrap().retain(|(failed_ip, _)| failed_ip != "10.2.2.2");
    }

    /// token 比较不短路、长度不等直接 false
    #[test]
    fn token_compare() {
        assert!(token_eq("abc", "abc"));
        assert!(!token_eq("abc", "abd"));
        assert!(!token_eq("abc", "abcd"));
    }

    /// 二维码能生成出合法 SVG（顺带保证 qrcode crate 的 Color 用法没写错）
    #[test]
    fn qr_svg_builds() {
        let s = qr_svg("http://192.168.1.23:8790/#t=AB23CD45");
        assert!(s.starts_with("<svg"));
        assert!(s.contains("<rect"));
    }
}
