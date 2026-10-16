// 官网个人主页的逐资源包额度。
//
// 为什么不能只读本机：客户端的 `/api/v2/quota/usage`（agenthub 那条）只给
// 「主额度 + 附加包」两个聚合数，附加包底下那二十几个资源包各自的到期日它不给。
// 官网个人主页（qoder.com.cn/account/usage）调的是 `/api/v2/me/usages/big_model_credits`，
// 每个资源包一行，带 limit_value / used_value / remaining_value / expires_at。
//
// 这条接口只认浏览器会话 cookie：登录令牌（auth.v1.dat 里的 Bearer）实测一律 401。
// 所以用一个驾驶舱自己的 WebView2 窗口负责登录，登录态存在数据根的 webprofile/ 里，
// 取额度时从那个窗口自己的 cookie 罐里拿会话，再由 Rust 侧直接调接口。
// 不读用户浏览器的档案，也不读产品自己的 Network/Cookies（那个文件被独占锁，读不了）。
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Duration;
use tauri::Manager;

static APP: std::sync::OnceLock<tauri::AppHandle> = std::sync::OnceLock::new();

pub fn set_app(h: tauri::AppHandle) {
    let _ = APP.set(h);
}

fn app_handle() -> Result<&'static tauri::AppHandle, String> {
    APP.get().ok_or_else(|| "程序还在启动中，稍等几秒再试".to_string())
}

pub struct Site {
    /// 官网域名，按优先级排。同一个产品的官网可能有两个域名在同时活着
    /// （Qoder CN 的 `qoder.cn` 与 `qoder.com.cn` DNS 都指向同一台 aliyun 主机），
    /// 但 **cookie 是按域分开的**——会话落在哪个域名下，就只能用哪个域名去调接口。
    /// 所以取会话时两个域都问一遍，谁那儿有 `qoder_session_cookie` 就用谁。
    hosts: &'static [&'static str],
}

impl Site {
    fn usage_page(&self, host: &str) -> String {
        format!("https://{host}/account/usage")
    }
    /// 登录页：登录后由 oauth_callback 跳回用量页
    fn sign_in(&self, host: &str) -> String {
        format!(
            "https://{host}/users/sign-in?oauth_callback={}",
            url_encode(&self.usage_page(host))
        )
    }
}

/// 只给 oauth_callback 这一个参数用：域名里只有字母数字和点，
/// 需要转义的只有 `:` 与 `/`（先转 `:`，否则会把刚插进去的 `%2F` 里的 `/` 也当成原字符）。
fn url_encode(s: &str) -> String {
    s.replace(':', "%3A").replace('/', "%2F")
}

const SITES: &[(&str, Site)] = &[
    (
        "qoder-cn",
        Site {
            hosts: &["qoder.com.cn", "qoder.cn"],
        },
    ),
    ("qoder", Site { hosts: &["qoder.com"] }),
];

pub fn site_for(store: &str) -> Option<&'static Site> {
    SITES.iter().find(|(s, _)| *s == store).map(|(_, v)| v)
}

/// 支持官网取数的驾驶舱产品 id（界面据此决定摆不摆这个按钮）
pub fn supports(app_id: &str) -> bool {
    crate::local_quota::store_for(app_id).and_then(|s| site_for(s)).is_some()
}

fn store_of(app_id: &str) -> Result<&'static str, String> {
    crate::local_quota::store_for(app_id)
        .ok_or_else(|| "这个程序还没有本机直读通道，也就没有对应的官网接口".to_string())
}

fn label_for(store: &str, slot: &str) -> String {
    format!("webq-{}-{}", store, slot)
}

/// 这张卡该用哪一份官网会话。
///
/// 以前整个产品只有一份 `webprofile/<store>/`，于是官网读数永远只属于当初登进那个浏览器
/// 档案的那一个号：另一张卡点刷新要么读不到、要么被防串号守卫挡回去。而官网那份是唯一
/// 给得出「每个积分包各自什么时候到期」的，卡上点一次刷新就该出这个号的真数，
/// 所以会话必须按账号隔离——不管这台机器的客户端现在登的是谁。
/// 键优先用绑死的 userId（UUID，稳定），第一次读还没有 userId 时才用账号名，
/// 读完立刻把实际用的这一格记进 `quota_web_slot`，之后再也不换（否则一绑定就把
/// 刚登好的会话丢在了别的目录下）。
fn slot_of(meta: &Value) -> String {
    let keep = meta["quota_web_slot"].as_str().unwrap_or("").trim().to_string();
    if !keep.is_empty() {
        return keep;
    }
    let raw = ["quota_user_id", "quota_account", "name"]
        .iter()
        .map(|k| meta[k].as_str().unwrap_or("").trim().to_string())
        .find(|s| !s.is_empty())
        .unwrap_or_else(|| "default".to_string());
    let clean: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '-'
            }
        })
        .collect();
    if clean == raw {
        clean
    } else {
        // 中文名整段被洗成连字符会撞车（两张卡共用一份档案），补一段原名指纹
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for b in raw.as_bytes() {
            h ^= *b as u64;
            h = h.wrapping_mul(0x1000_0000_01b3);
        }
        format!("{}-{:08x}", clean.trim_matches('-'), (h >> 32) as u32)
    }
}

/// 登录态存放处：数据根下的独立 WebView2 档案目录，和驾驶舱主窗口分开。
/// 一个账号一格，删掉某一格就等于只在这个号上退出登录。
fn web_profile(store: &str, slot: &str) -> PathBuf {
    let p = crate::registry::base_dir()
        .join("webprofile")
        .join(store)
        .join(slot);
    let _ = std::fs::create_dir_all(&p);
    p
}

/// 打开（或唤到前台）官网登录窗口。用户在这个窗口里用阿里云账号登录一次，
/// 会话就留在 webprofile/<产品>/<这个号> 里，之后取额度不用再登。
pub fn open_login(app_id: &str, meta: &Value) -> Result<String, String> {
    let store = store_of(app_id)?;
    let site = site_for(store).ok_or("这个产品没有登记官网地址")?;
    let app = app_handle()?.clone();
    let slot = slot_of(meta);
    let label = label_for(store, &slot);
    let who = meta["quota_account"]
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| meta["name"].as_str())
        .unwrap_or("这个账号")
        .to_string();
    if let Some(w) = app.get_webview_window(&label) {
        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();
        return Ok(format!("「{who}」的登录窗口已经在开着了，切到前台了。"));
    }
    // 这个号已经登过的域名优先：会话是按域分开的，重登要登在同一个域上，
    // 否则新会话落在另一个域名、原来那份读数就白读了。没登过才用第一个（主域名）。
    let host = preferred_host(store, &slot);
    let url: tauri::Url = site
        .sign_in(host)
        .parse()
        .map_err(|e| format!("登录地址解析失败：{e}"))?;
    let win = tauri::WebviewWindowBuilder::new(&app, &label, tauri::WebviewUrl::External(url))
        .title(&format!(
            "{} 官网登录 · {who}（驾驶舱专用窗口）",
            crate::registry::app_def(app_id).map(|a| a.name).unwrap_or_else(|| "Agent".into())
        ))
        .inner_size(1000.0, 780.0)
        .min_inner_size(520.0, 420.0)
        .data_directory(web_profile(store, &slot))
        .visible(true)
        .build()
        .map_err(|e| format!("登录窗口打不开：{e}"))?;
    let _ = win.set_focus();
    Ok(format!(
        "已打开「{who}」自己的官网登录窗口（{host}）：在里面用这个号的阿里云账号登录一次，\
         之后每次刷新额度都由它带着自己的会话去读，跟这台机器的客户端登的是谁无关。"
    ))
}

/// 这张卡的会话该用哪个域名：先看哪个域名的 cookie 罐里已经有登录会话，
/// 都没有就用候选里的第一个（主域名）。窗口还没建起来时一律回落到第一个。
fn preferred_host(store: &str, slot: &str) -> &'static str {
    let site = match site_for(store) {
        Some(s) => s,
        None => return "",
    };
    site.hosts
        .iter()
        .copied()
        .find(|h| cookie_for_host(store, slot, h).is_some())
        .unwrap_or_else(|| site.hosts.first().copied().unwrap_or(""))
}

/// 只问这一个域名的 cookie 罐（不建窗口、不发请求）：有 `qoder_session_cookie` 就返回它。
fn cookie_for_host(store: &str, slot: &str, host: &str) -> Option<String> {
    let app = app_handle().ok()?;
    let site = site_for(store)?;
    let label = label_for(store, slot);
    if app.get_webview_window(&label).is_none() {
        return None;
    }
    let list = once(app, &label, &site.usage_page(host)).ok()?;
    list.iter()
        .find(|(n, _)| n == "qoder_session_cookie")
        .map(|(_, v)| v.clone())
        .filter(|v| !v.trim().is_empty())
}

/// 关掉可见的登录窗口后重建一个隐藏的同名窗口，只为读 cookie 用。
/// 已经在建（比如刚点过登录又立刻取数）就直接复用。
fn ensure_hidden(app: &tauri::AppHandle, store: &str, slot: &str) -> Result<(), String> {
    let label = label_for(store, slot);
    if app.get_webview_window(&label).is_some() {
        return Ok(());
    }
    let site = site_for(store).ok_or("这个产品没有登记官网地址")?;
    let url: tauri::Url = site
        .usage_page(preferred_host(store, slot))
        .parse()
        .map_err(|e| format!("地址解析失败：{e}"))?;
    tauri::WebviewWindowBuilder::new(app, &label, tauri::WebviewUrl::External(url))
        .title("官网会话")
        .inner_size(900.0, 700.0)
        .data_directory(web_profile(store, slot))
        // 隐藏窗口也要能被 WebView2 建出来：先建再立刻隐藏比 visible(false) 稳，
        // 后者在某些版本上不会创建底层 webview，cookie 也就读不到
        .visible(true)
        .build()
        .map_err(|e| format!("会话窗口打不开：{e}"))?
        .hide()
        .map_err(|e| format!("会话窗口藏不起来：{e}"))?;
    Ok(())
}

/// 从那个窗口自己的 cookie 罐里取会话 cookie，连"它属于哪个域名"一起给出来。
/// 发起在 UI 线程（with_webview 的闭包），等待在这个工作线程——
/// 窗口刚建好时底层 webview 可能还没起来，所以带几次重试。
/// `ensure = false` 时只在窗口已经存在的前提下读，绝不顺手建隐藏窗口
/// （轮询"她登录上了没有"要走这条，否则关掉了登录窗口就会冒出一个看不见的窗口）。
///
/// 两个域名都要问：`qoder.cn` 与 `qoder.com.cn` 是同一套后端的两个域名，
/// 但会话 cookie 是按域分开的——只问主域名的话，会话落在另一个域上就永远"读不到"。
fn cookie_of(store: &str, slot: &str, ensure: bool) -> Result<(String, String), String> {
    let app = app_handle()?.clone();
    let site = site_for(store).ok_or("这个产品没有登记官网地址")?;
    let label = label_for(store, slot);
    if app.get_webview_window(&label).is_none() {
        if !ensure {
            return Err("登录窗口已经关了".to_string());
        }
        ensure_hidden(&app, store, slot)?;
    }
    let mut last = String::from("没读到 cookie");
    for attempt in 0..8 {
        if attempt > 0 {
            std::thread::sleep(Duration::from_millis(500));
        }
        // 每次重试都把两个域名重问一遍：窗口刚起来时第一次往往什么都拿不到
        let mut ok_hosts = 0usize;
        let mut retryable = false;
        for host in site.hosts {
            match once(&app, &label, &site.usage_page(host)) {
                Ok(list) => {
                    ok_hosts += 1;
                    if let Some(v) = list
                        .iter()
                        .find(|(n, _)| n == "qoder_session_cookie")
                        .map(|(_, v)| v.clone())
                        .filter(|v| !v.trim().is_empty())
                    {
                        return Ok(((*host).to_string(), v));
                    }
                }
                Err(e) => {
                    // 只有"窗口还没起来"值得重试；其它错误重试也是白试
                    if e.contains("还没就绪") || e.contains("超时") || e.contains("没回应") {
                        retryable = true;
                    } else {
                        return Err(e);
                    }
                    last = e;
                }
            }
        }
        // **每一个**域名都问到了、谁那儿都没有会话 cookie：这是"没登录"，不是"窗口没起来"，
        // 重试也变不出来——直接把该怎么做说清楚
        if ok_hosts == site.hosts.len() {
            return Err(format!(
                "这个号的官网会话还没登录：{} 的 cookie 罐里都没有登录会话。\
                 先在驾驶舱打开的官网窗口里登录一次（用阿里云账号）。",
                site.hosts.join(" / ")
            ));
        }
        if !retryable {
            break;
        }
    }
    Err(last)
}

/// 取会话，连它落在哪个域名一起带回来（调接口必须用同一个域名）。
fn session_cookie(store: &str, slot: &str) -> Result<(String, String), String> {
    cookie_of(store, slot, true)
}

/// 这个号自己的官网窗口里有没有登录会话——只翻 cookie 罐、不发任何请求。
/// 给「她登录成功 → 自动补一次取数」那条轮询用。
pub fn has_session(app_id: &str, meta: &Value) -> bool {
    match store_of(app_id) {
        Ok(s) => cookie_of(s, &slot_of(meta), false)
            .map(|(_, c)| !c.trim().is_empty())
            .unwrap_or(false),
        Err(_) => false,
    }
}

fn once(
    app: &tauri::AppHandle,
    label: &str,
    uri: &str,
) -> Result<Vec<(String, String)>, String> {
    let win = app
        .get_webview_window(label)
        .ok_or("登录窗口没建起来：先点「打开官网登录窗口」")?;
    let (tx, rx) = mpsc::channel::<Result<Vec<(String, String)>, String>>();
    let url = uri.to_string();
    win.with_webview(move |pw| {
        // 发起失败时 tx 随闭包一起析构，调用方会收到"断开"，按超时重试处理
        let _ = unsafe { start_cookie_read(pw.controller(), &url, tx) };
    })
    .map_err(|e| format!("读不到登录窗口：{e}"))?;
    rx.recv_timeout(Duration::from_secs(6))
        .map_err(|_| "等 cookie 超时（窗口还没起来）".to_string())?
        .map_err(|e| format!("取 cookie 失败：{e}"))
}

/// 只负责"发起"取 cookie，绝不在这里等结果。
/// WebView2 的完成回调要靠 UI 线程的消息泵派发，而 `with_webview` 的闭包正是跑在 UI 线程上——
/// 在闭包里等回调 = 自己等自己，必然超时（第一版就是这么错的）。
#[cfg(windows)]
unsafe fn start_cookie_read(
    controller: webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2Controller,
    uri: &str,
    tx: mpsc::Sender<Result<Vec<(String, String)>, String>>,
) -> Result<(), String> {
    use webview2_com::Microsoft::Web::WebView2::Win32::{
        ICoreWebView2_2, ICoreWebView2CookieList,
    };
    use webview2_com::{take_pwstr, GetCookiesCompletedHandler};
    use windows::core::{HSTRING, Interface, PCWSTR, PWSTR};

    let webview: ICoreWebView2_2 = controller
        .CoreWebView2()
        .map_err(|e| format!("窗口还没就绪：{e}"))?
        .cast()
        .map_err(|e| format!("这个 WebView2 版本取不到 cookie：{e}"))?;
    let manager = webview.CookieManager().map_err(|e| format!("CookieManager：{e}"))?;
    let h = HSTRING::from(uri);
    manager.GetCookies(
        PCWSTR::from_raw(h.as_ptr()),
        &GetCookiesCompletedHandler::create(Box::new(
            move |code, list: Option<ICoreWebView2CookieList>| {
                let out = (|| -> Result<Vec<(String, String)>, String> {
                    code.map_err(|e| format!("{e}"))?;
                    let list = list.ok_or("没有返回 cookie 列表")?;
                    let mut count: u32 = 0;
                    list.Count(&mut count).map_err(|e| format!("{e}"))?;
                    let mut v = Vec::with_capacity(count as usize);
                    for i in 0..count {
                        let c = list.GetValueAtIndex(i).map_err(|e| format!("{e}"))?;
                        let mut name = PWSTR::null();
                        c.Name(&mut name).map_err(|e| format!("{e}"))?;
                        let name = take_pwstr(name);
                        let mut value = PWSTR::null();
                        c.Value(&mut value).map_err(|e| format!("{e}"))?;
                        let value = take_pwstr(value);
                        v.push((name, value));
                    }
                    Ok(v)
                })();
                let _ = tx.send(out);
                Ok(())
            },
        )),
    )
    .map_err(|e| format!("GetCookies：{e}"))?;
    Ok(())
}

#[cfg(not(windows))]
unsafe fn start_cookie_read(
    _controller: (),
    _uri: &str,
    _tx: mpsc::Sender<Result<Vec<(String, String)>, String>>,
) -> Result<(), String> {
    Err("只有 Windows 上的 WebView2 能读登录窗口".to_string())
}

fn get_json(url: &str, cookie: &str, referer: &str) -> Result<Value, String> {
    let resp = ureq::get(url)
        .set("Cookie", cookie)
        .set("Accept", "application/json, text/plain, */*")
        .set("User-Agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36")
        .set("Referer", referer)
        .set("X-Requested-With", "XMLHttpRequest")
        .timeout(Duration::from_secs(20))
        .call();
    match resp {
        Ok(r) => r
            .into_json::<Value>()
            .map_err(|e| format!("{url} 返回的不是 JSON：{e}")),
        Err(ureq::Error::Status(401, _)) => {
            Err("官网说没登录（会话过期了）：重新打开登录窗口登一次".into())
        }
        Err(ureq::Error::Status(code, _)) => Err(format!("{url} HTTP {code}")),
        Err(e) => Err(format!("调 {url} 失败：{e}")),
    }
}

/// 毫秒时间戳 → 和 agenthub 同一套三态写法，界面那层只认这一种：
/// 0 = 接口没给（"未给出"，不等于无限期）；9999 年哨兵 = 厂商说的"无限期"。
fn ms_to_text(ms: f64) -> String {
    use chrono::Datelike;
    if ms <= 0.0 {
        return "未给出".to_string();
    }
    let secs = (ms / 1000.0) as i64;
    let d = match chrono::DateTime::from_timestamp(secs, 0) {
        Some(d) => d.with_timezone(&chrono::Local),
        None => return "未给出".to_string(),
    };
    if d.year() > 9000 {
        return "无限期".to_string();
    }
    d.format("%Y-%m-%d %H:%M").to_string()
}

/// 官网的 source 枚举 → 驾驶舱/WorkBuddy 那四大组的名字。
/// 组名必须和现有分组口径一致，否则界面上会凭空多出几个组来。
fn group_name(src: &str) -> &'static str {
    let s = src.strip_prefix("RESOURCE_PACKAGE_SOURCE_").unwrap_or(src);
    match s {
        "PLAN" => "套餐基础积分",
        "BONUS" | "TRIAL" | "CARRY_OVER" => "平台奖励积分",
        "PURCHASE" | "BOUGHT" | "REDEMPTION_CODE" | "REDEMPTION" => "购买积分",
        _ => "其他",
    }
}

/// 组内这一笔叫什么：官网按包发积分，一笔一行，名字要说清是哪来的
fn item_label(src: &str) -> String {
    let s = src.strip_prefix("RESOURCE_PACKAGE_SOURCE_").unwrap_or(src);
    match s {
        "PLAN" => "套餐额度".into(),
        "BONUS" => "平台赠送".into(),
        "PURCHASE" | "BOUGHT" => "购买".into(),
        "REDEMPTION_CODE" | "REDEMPTION" => "兑换码".into(),
        "TRIAL" => "试用".into(),
        "CARRY_OVER" => "上期结转".into(),
        other => other.to_string(),
    }
}

fn f(v: &Value, k: &str) -> f64 {
    v.get(k)
        .and_then(|x| x.as_f64().or_else(|| x.as_i64().map(|i| i as f64)))
        .unwrap_or(0.0)
}

/// 抓一次官网额度：逐资源包 + 身份 + 套餐 + 每日消耗热力图。
/// 用的是这张卡自己那份会话，所以读回来的一定是这个号的数。
/// 返回 {ok, account, user_id, plan, plan_expire, left, total, used, packs:[...], portions:[...]}
pub fn fetch(app_id: &str, meta: &Value) -> Value {
    let store = match store_of(app_id) {
        Ok(s) => s,
        Err(e) => return json!({ "ok": false, "msg": e }),
    };
    let site = match site_for(store) {
        Some(s) => s,
        None => return json!({ "ok": false, "msg": "这个产品没有登记官网地址" }),
    };
    let slot = slot_of(meta);
    // 会话落在哪个域名下，这一轮就整条走哪个域名（cookie 是按域分开的，
    // 拿 A 域的会话去调 B 域的接口只会 401）
    let (host, cookie) = match session_cookie(store, &slot) {
        Ok(v) => v,
        Err(e) => {
            return json!({ "ok": false, "need_login": true, "slot": slot, "msg": e });
        }
    };
    let hdr = format!("qoder_session_cookie={}", cookie);
    let referer = site.usage_page(&host);
    let usage = match get_json(
        &format!("https://{}/api/v2/me/usages/big_model_credits", host),
        &hdr,
        &referer,
    ) {
        Ok(v) => v,
        Err(e) => return json!({ "ok": false, "msg": e }),
    };
    // 身份与套餐读不到不影响额度，单独降级成空
    let me = get_json(&format!("https://{}/api/v1/me", host), &hdr, &referer)
        .unwrap_or_else(|_| json!({}));
    let plan = get_json(&format!("https://{}/api/v1/me/userplan", host), &hdr, &referer)
        .unwrap_or_else(|_| json!({}));
    // 官网用量页那张「近一年每日 Credits 消耗」热力图就是这条接口给的：
    // 厂商自己把 371 天补零成连续序列，还顺带给四档分色阈值（levels）。
    // 读不到只少一张图，不许把额度本身打挂，所以跟 me/plan 一样降级成 error 装进报文。
    let heatmap = get_json(
        &format!("https://{}/api/v1/me/ai-conversations/credits-heatmap?days=371", host),
        &hdr,
        &referer,
    )
    .unwrap_or_else(|e| json!({ "error": e }));

    let mut out = parse_usage(&usage, &me, &plan, &referer);
    if let Some(o) = out.as_object_mut() {
        o.insert("slot".into(), json!(slot));
        o.insert("host".into(), json!(host));
        o.insert("heatmap".into(), heatmap);
    }
    out
}

/// 官网热力图 → 驾驶舱那份按天序列。单位、分档阈值一律照厂商给的，
/// 不许我们另按分位数重切（同一份数在官网和驾驶舱分色不同，用户会以为读数不一致）。
fn trend_of_heatmap(h: &Value) -> Option<Value> {
    let items = h["items"].as_array()?;
    let days: Vec<Value> = items
        .iter()
        .filter_map(|it| {
            let d = it["date"].as_str()?;
            Some(json!({ "date": d, "credits": it["value"].as_f64().unwrap_or(0.0) }))
        })
        .collect();
    if days.is_empty() {
        return None;
    }
    let mut peak: f64 = 0.0;
    let mut peak_date = String::new();
    for it in &days {
        let v = it["credits"].as_f64().unwrap_or(0.0);
        if v > peak {
            peak = v;
            peak_date = it["date"].as_str().unwrap_or("").to_string();
        }
    }
    Some(json!({
        "unit": h["unit"].as_str().unwrap_or("credits"),
        "days": days,
        "total": h["total"].as_f64(),
        "peak": peak,
        "peak_date": peak_date,
        "levels": h["levels"].as_array().cloned().unwrap_or_default(),
        "from": days.first().and_then(|v| v["date"].as_str()).unwrap_or(""),
        "to": days.last().and_then(|v| v["date"].as_str()).unwrap_or(""),
        "source": "官网热力图接口",
    }))
}

/// 把官网三个接口的原始返回整理成驾驶舱那套「大组 + 逐笔」。
/// 单独抽出来是为了能拿真实抓回来的报文跑离线测试，不靠连网。
pub fn parse_usage(usage: &Value, me: &Value, plan: &Value, page: &str) -> Value {
    // 官网把积分分成三节（套餐 / 资源包 / 专属资源包），但界面那层的分组口径是
    // agenthub 定的四大组。这里按 source 归到同四大组，跨节合并——
    // 不然同一份数据在界面上会出现两套组名。
    const ORDER: [&str; 4] = ["套餐基础积分", "平台奖励积分", "购买积分", "其他"];
    let mut by_group: Vec<Value> = ORDER
        .iter()
        .map(|n| json!({ "name": n, "unit": "credits", "left": 0.0, "total": 0.0, "used": 0.0, "items": [] }))
        .collect();
    let mut portions: Vec<Value> = Vec::new();
    // 套餐那一档官网从不给 expires_at——它是随计费周期刷新的，不是发下来的资源包。
    // 但同一份报文顶层的 nextResetAt（以及 userplan.next_refresh_date）就是那句
    // "将于 X 刷新配额"的 X，对月度配额来说刷新即到期（当期没用完不结转），补上它。
    let plan_reset = [f(usage, "nextResetAt"), f(plan, "next_refresh_date")]
        .into_iter()
        .find(|x| *x > 0.0)
        .unwrap_or(0.0);
    for key in [
        "plan_quota",
        "resource_package_quota",
        "dedicated_resource_package_quota",
    ] {
        let g = usage.get(key).cloned().unwrap_or_else(|| json!({}));
        let det = g
            .get("quota_detail")
            .and_then(|d| d.as_array())
            .cloned()
            .unwrap_or_default();
        for it in &det {
            let src = it["source"].as_str().unwrap_or("");
            let gname = group_name(src);
            let left = f(it, "remaining_value");
            let total = f(it, "limit_value");
            let used = {
                let u = f(it, "used_value");
                if u > 0.0 || total <= 0.0 { u } else { (total - left).max(0.0) }
            };
            let exp_raw = f(it, "expires_at");
            let is_plan = src
                .strip_prefix("RESOURCE_PACKAGE_SOURCE_")
                .unwrap_or(src)
                .ends_with("PLAN");
            let renews = is_plan && exp_raw <= 0.0 && plan_reset > 0.0;
            let exp = ms_to_text(if renews { plan_reset } else { exp_raw });
            let gi = ORDER.iter().position(|n| *n == gname).unwrap_or(3);
            let item = json!({
                "label": if renews {
                    format!("{}·周期刷新", item_label(src))
                } else {
                    item_label(src)
                },
                "pool": gname,
                "detail": src,
                "left": left,
                "total": total,
                "used": used,
                "expires_on": exp,
                "unit": it["unit"].as_str().unwrap_or("credits"),
                "status": it["status"].as_str().unwrap_or(""),
            });
            by_group[gi]["items"].as_array_mut().unwrap().push(item.clone());
            by_group[gi]["left"] = json!(f(&by_group[gi], "left") + left);
            by_group[gi]["total"] = json!(f(&by_group[gi], "total") + total);
            by_group[gi]["used"] = json!(f(&by_group[gi], "used") + used);
            let mut p = item.clone();
            p["category"] = json!(gname);
            p["label"] = json!(format!("{}·{}", gname, item_label(src)));
            portions.push(p);
        }
    }
    // 快到期的排前面：一屏之内先看会过期的；空组不摆出来
    let mut groups: Vec<Value> = Vec::new();
    for g in by_group.iter_mut() {
        let items = g["items"].as_array_mut().map(|a| std::mem::take(a)).unwrap_or_default();
        if items.is_empty() {
            continue;
        }
        let mut items = items;
        items.sort_by(|a, b| {
            let ea = a["expires_on"].as_str().unwrap_or("9999");
            let eb = b["expires_on"].as_str().unwrap_or("9999");
            ea.cmp(eb)
        });
        g["items"] = json!(items);
        g["n"] = json!(items.len());
        groups.push(g.clone());
    }
    let sum = usage
        .get("total_quota")
        .and_then(|t| t.get("quota_summary"))
        .cloned()
        .unwrap_or_else(|| json!({}));
    let uid = usage["user_id"].as_str().unwrap_or("").to_string();
    let name = me["name"].as_str().unwrap_or("").trim().to_string();
    let tier = plan["plan_tier_name"].as_str().unwrap_or("").to_string();
    json!({
        "ok": true,
        "user_id": uid,
        "account": if name.is_empty() { uid.clone() } else { name },
        "plan": tier.clone(),
        "plan_name": plan_name_cn(&tier),
        "plan_expire": ms_to_text(f(&plan, "end_date")),
        "next_reset": ms_to_text(f(&usage, "nextResetAt")),
        "left": f(&sum, "remaining_value"),
        "total": f(&sum, "limit_value"),
        "used": f(&sum, "used_value"),
        "quota_key": usage["quota_key"].as_str().unwrap_or(""),
        "groups": groups,
        "portions": portions,
        "page": page,
    })
}

/// 官网的套餐枚举 → 人话。认不出的原样给出来，不猜。
fn plan_name_cn(tier: &str) -> String {
    let s = tier.strip_prefix("PLAN_TIER_").unwrap_or(tier);
    match s {
        "FREE" => "体验版".into(),
        "PRO" => "专业版".into(),
        "PRO_PLUS" | "PLUS" => "专业版+".into(),
        "TEAM" | "ORG" => "团队版".into(),
        "" => String::new(),
        other => other.to_string(),
    }
}

/// 把官网读数写进实例档案：逐笔积分直接用资源包明细，
// 这样卡片、顶栏「临期」和到期提醒全部沿用同一套口径，不再另立一套数。
pub fn apply(meta: &Value, w: &Value) -> Value {
    let mut m = meta.clone();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    let obj = match m.as_object_mut() {
        Some(o) => o,
        None => return m,
    };
    obj.insert("credits_left".into(), json!(w["left"]));
    obj.insert("credits_total".into(), json!(w["total"]));
    obj.insert("credits".into(), w["portions"].clone());
    // 走界面已有的那套分组管线：quota_groups → state 现算"有效期内/已过期" → 卡片画大组
    obj.insert("quota_groups".into(), w["groups"].clone());
    obj.insert("quota_plan_name".into(), json!(w["plan_name"]));
    obj.insert("quota_user_id".into(), json!(w["user_id"]));
    // 这份会话存在哪一格，就在卡片上钉死哪一格：第一次读还没有 userId，
    // 目录是按账号名开的，读完就有了——不钉住的话下次刷新会去找一个空目录，
    // 等于刚登好的会话被丢在原地，又让她重登一次。
    if let Some(s) = w["slot"].as_str() {
        if !s.trim().is_empty() {
            obj.insert("quota_web_slot".into(), json!(s));
        }
    }
    obj.insert("quota_account".into(), json!(w["account"]));
    obj.insert("quota_plan".into(), json!(w["plan"]));
    obj.insert("quota_local".into(), json!(true));
    obj.insert("quota_source".into(), json!("web"));
    obj.insert("quota_live_error".into(), json!(""));
    obj.insert("quota_web_error".into(), json!(""));
    // 这份明细是全站最细的一层（逐资源包 + 各自到期日），来源与时间单独记一份：
    // 之后本机那条只有汇总数的读数进不来（见 local_quota::stored_rank 那道守卫），
    // 但汇总数会照常更新，所以界面要能说清"大字是刚才的、明细是这次的"。
    obj.insert("quota_detail_source".into(), json!("web"));
    obj.insert("quota_detail_stale".into(), json!(false));
    obj.insert("quota_detail_at".into(), json!(now));
    obj.insert("quota_fetched_at".into(), json!(now));
    let earliest = w["portions"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|p| p["expires_on"].as_str())
                .filter(|s| !s.is_empty() && *s != "未给出" && *s != "无限期")
                .min()
                .unwrap_or("")
                .to_string()
        })
        .unwrap_or_default();
    if !earliest.is_empty() {
        obj.insert("expires_on".into(), json!(earliest));
    }
    // 按天消耗序列（波形图与热力图）
    match trend_of_heatmap(&w["heatmap"]) {
        Some(t) => {
            obj.insert("quota_trend".into(), t);
        }
        None => {
            // 这一轮没读到：档案里已经有图就留着，一张都没有才记下原因——
            // 一次网络抖动不该把能用的图换成一句报错
            if obj.get("quota_trend").map_or(true, |v| v.is_null()) {
                let e = w["heatmap"]["error"]
                    .as_str()
                    .unwrap_or("官网没返回每日消耗序列");
                obj.insert("quota_trend".into(), json!({ "error": e }));
            }
        }
    }
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_qoder_family_has_a_known_usage_page() {
        assert!(site_for("qoder-cn").is_some());
        assert!(site_for("qoder").is_some());
        assert!(site_for("workbuddy").is_none());
        assert!(supports("qoder-cn"));
    }

    /// Qoder CN 的官网有两个域名同时在活着（DNS 都指向同一台 aliyun 主机），
    /// 但会话 cookie 是按域分开的：只问主域名，会话落在另一个域上就永远"读不到"。
    /// 这条钉的是两个域名都在候选里、且拼出来的地址与 oauth_callback 转义都对。
    #[test]
    fn qoder_cn_has_both_live_hosts_and_a_well_formed_sign_in() {
        let s = site_for("qoder-cn").unwrap();
        assert_eq!(s.hosts, &["qoder.com.cn", "qoder.cn"]);
        assert_eq!(s.usage_page("qoder.cn"), "https://qoder.cn/account/usage");
        assert_eq!(
            s.sign_in("qoder.cn"),
            "https://qoder.cn/users/sign-in?oauth_callback=https%3A%2F%2Fqoder.cn%2Faccount%2Fusage"
        );
        // 转义顺序不能反：先 `:` 再 `/`，否则会把刚插进去的 %2F 里的 `/` 也吃掉
        assert_eq!(
            url_encode("https://qoder.com.cn/account/usage"),
            "https%3A%2F%2Fqoder.com.cn%2Faccount%2Fusage"
        );
        // 国际版只有一个域名：不能把 CN 站当成它的回落（那是另一套账号体系）
        assert_eq!(site_for("qoder").unwrap().hosts, &["qoder.com"]);
    }

    /// 两个号必须落在两个格子里：共用一份会话时官网读数永远只属于登进去的那一个号，
    /// 另一张卡点刷新要么读不到、要么被防串号守卫挡回去（她 10-14 就是撞在这上面）。
    #[test]
    fn each_account_gets_its_own_web_session_slot() {
        let a = json!({ "quota_user_id": "019e8247-2e21-7aa6-9e87-b0720003b387" });
        let b = json!({ "quota_user_id": "01a05beb-b936-746a-93ae-371662bc1b57" });
        assert_ne!(slot_of(&a), slot_of(&b));
        assert_eq!(slot_of(&a), "019e8247-2e21-7aa6-9e87-b0720003b387");
        // 中文账号名整段会被洗成连字符，不补指纹两张卡会共用一份档案
        let c = slot_of(&json!({ "name": "你鸭神永远是你鸭神" }));
        let d = slot_of(&json!({ "name": "你鸭神也是你鸭神" }));
        assert_ne!(c, d);
        // 目录名与窗口 label 都不许带斜杠、空格这类禁忌字符
        for raw in ["a/b c", "你鸭神永远是你鸭神", ""] {
            let s = slot_of(&json!({ "name": raw }));
            assert!(
                s.chars().all(|x| x.is_ascii_alphanumeric() || matches!(x, '-' | '_' | '.')),
                "「{raw}」洗出来还是不能当目录名：{s}"
            );
        }
        assert_eq!(slot_of(&json!({ "name": "a-b.c" })), "a-b.c");
    }

    /// 格子一旦记下就以它为准：第一次读之前目录是按账号名开的，读完就绑上了 userId，
    /// 不钉住的话下一次刷新会去找一个空目录，等于刚登好的会话被丢在原地又要重登。
    #[test]
    fn recorded_slot_wins_over_a_newly_bound_user_id() {
        let m = json!({ "quota_web_slot": "账号2", "quota_user_id": "019e8247-2e21" });
        assert_eq!(slot_of(&m), "账号2");
    }

    #[test]
    fn expiry_keeps_the_three_states_apart() {
        // 接口没给 ≠ 无限期：混起来就是把"不知道"冒充成承诺
        assert_eq!(ms_to_text(0.0), "未给出");
        assert_eq!(ms_to_text(253402214400000.0), "无限期");
        let d = ms_to_text(1794287609904.0);
        assert!(d.starts_with("2026-11-"), "实际 {d}");
        assert!(d.contains(' '), "要精确到分，和 agenthub 同一套写法：{d}");
    }

    #[test]
    fn sources_fold_into_the_same_four_groups_as_agenthub() {
        assert_eq!(group_name("PLAN"), "套餐基础积分");
        assert_eq!(group_name("RESOURCE_PACKAGE_SOURCE_BONUS"), "平台奖励积分");
        assert_eq!(group_name("RESOURCE_PACKAGE_SOURCE_PURCHASE"), "购买积分");
        assert_eq!(group_name("RESOURCE_PACKAGE_SOURCE_WEIRD"), "其他");
        assert_eq!(plan_name_cn("PLAN_TIER_FREE"), "体验版");
        assert_eq!(plan_name_cn("PLAN_TIER_WEIRD"), "WEIRD", "认不出就原样给出来，不编");
    }

    /// 2026-10-11 从 qoder.com.cn/account/usage 真实抓回来的报文（截了 3 笔）。
    /// 这条测试钉的是：官网逐资源包的额度/用量/到期一个都不能在读数路上丢掉。
    #[test]
    fn parses_the_real_captured_usage_payload() {
        let usage = json!({
          "user_id": "019e8247-2e21-7aa6-9e87-b0720003b387",
          "quota_key": "big_model_credits",
          "status": "active",
          "plan_quota": {"quota_summary": {"used_value":0,"limit_value":0,"remaining_value":0,"unit":"credits"},
            "quota_detail": [{"id":"p1","limit_value":0.0,"used_value":0.0,"remaining_value":0.0,
                              "unit":"credits","is_active":true,"expires_at":0,
                              "source":"PLAN","status":"ACTIVE"}]},
          "resource_package_quota": {"quota_summary": {"used_value":649.0,"limit_value":2400.0,"remaining_value":1751.0,"unit":"credits"},
            "quota_detail": [
              {"id":"b1","limit_value":100.0,"used_value":0.0,"remaining_value":100.0,"unit":"credits",
               "is_active":true,"expires_at":1794287609904.0,"source":"RESOURCE_PACKAGE_SOURCE_BONUS","status":"ACTIVE"},
              {"id":"b2","limit_value":100.0,"used_value":49.0,"remaining_value":51.0,"unit":"credits",
               "is_active":true,"expires_at":1792842793833.0,"source":"RESOURCE_PACKAGE_SOURCE_BONUS","status":"ACTIVE"},
              {"id":"b3","limit_value":200.0,"used_value":200.0,"remaining_value":0.0,"unit":"credits",
               "is_active":true,"expires_at":1792754659727.0,"source":"RESOURCE_PACKAGE_SOURCE_PURCHASE","status":"ACTIVE"}]},
          "dedicated_resource_package_quota": {"quota_summary":{"used_value":0.0,"limit_value":0.0,"remaining_value":0.0},
            "quota_detail": null},
          "total_quota": {"quota_summary": {"used_value":649.0,"limit_value":2400.0,"remaining_value":1751.0,"unit":"credits"}},
          "lastResetAt": 0, "nextResetAt": 0
        });
        let me = json!({"id":"019e8247-2e21-7aa6-9e87-b0720003b387","name":"shen1950"});
        let plan = json!({"user_id":"019e8247-2e21-7aa6-9e87-b0720003b387",
                          "plan_tier_name":"PLAN_TIER_FREE","end_date":253402214400000.0});
        let w = parse_usage(&usage, &me, &plan, "https://qoder.com.cn/account/usage");
        assert_eq!(w["account"], "shen1950");
        assert_eq!(w["plan_name"], "体验版");
        assert_eq!(w["left"], 1751.0);
        assert_eq!(w["total"], 2400.0);
        assert_eq!(w["plan_expire"], "无限期", "9999 年哨兵要说成人话");
        // 套餐那笔 expires_at=0 → 未给出，不许被当成"今天到期"
        let g: Vec<&Value> = w["groups"].as_array().unwrap().iter().collect();
        assert_eq!(g.len(), 3, "空组（其他）不该摆出来： {:?}", g.iter().map(|x| &x["name"]).collect::<Vec<_>>());
        assert_eq!(g[0]["name"], "套餐基础积分");
        assert_eq!(g[0]["items"][0]["expires_on"], "未给出");
        assert_eq!(g[1]["name"], "平台奖励积分");
        assert_eq!(g[1]["n"], 2);
        assert_eq!(g[1]["left"], 151.0);
        assert_eq!(g[1]["used"], 49.0);
        // 组内按到期日升序：先看会过期的
        let e0 = g[1]["items"][0]["expires_on"].as_str().unwrap().to_string();
        let e1 = g[1]["items"][1]["expires_on"].as_str().unwrap().to_string();
        assert!(e0 < e1, "{e0} 应排在 {e1} 前面");
        assert_eq!(g[2]["name"], "购买积分");
        assert_eq!(g[2]["left"], 0.0);
        // 逐笔明细同时进 credits，卡片与临期提醒共用一套
        assert_eq!(w["portions"].as_array().unwrap().len(), 4);
        assert_eq!(w["portions"][1]["category"], "平台奖励积分");
        let m = apply(&json!({ "name": "shen1950", "app": "qoder-cn" }), &w);
        assert_eq!(m["quota_user_id"], "019e8247-2e21-7aa6-9e87-b0720003b387");
        assert_eq!(m["quota_source"], "web");
        // 明细的来源与时间要单独记：本机那条只有汇总数的读数靠这个判断该不该让路
        assert_eq!(m["quota_detail_source"], "web");
        assert_eq!(m["quota_detail_stale"], false);
        assert!(m["quota_detail_at"].as_f64().unwrap_or(0.0) > 0.0);
        assert_eq!(m["credits"].as_array().unwrap().len(), 4);
        assert_eq!(m["quota_groups"].as_array().unwrap().len(), 3);
        assert_eq!(m["quota_plan_name"], "体验版");
        let exp = m["expires_on"].as_str().unwrap().to_string();
        assert!(exp.starts_with("2026-10-2"), "卡上的到期日取最早那笔真日期，实际 {exp}");
    }

    /// 2026-10-11 晚真实抓包（专业版账号）：套餐那一档 expires_at 恒为 0，
    /// 官网页面上那句"将于 2026-10-20 19:28:32 刷新配额"来自顶层 nextResetAt。
    /// 钉住：套餐积分必须拿刷新时间当到期时间，别再在界面上显示"未给出"。
    #[test]
    fn plan_quota_falls_back_to_the_cycle_refresh_time() {
        let usage = json!({
          "user_id": "01a05beb-b936-746a-93ae-371662bc1b57",
          "quota_key": "big_model_credits", "status": "active",
          "plan_quota": {"quota_summary": {"used_value":0,"limit_value":2000,"remaining_value":2000,"unit":"credits"},
            "quota_detail": [{"id":"p1","limit_value":2000.0,"used_value":0.0,"remaining_value":2000.0,
                              "unit":"credits","is_active":true,"expires_at":0,
                              "source":"PLAN","status":"ACTIVE"}]},
          "resource_package_quota": {"quota_summary": {"used_value":100,"limit_value":2100,"remaining_value":2000,"unit":"credits"},
            "quota_detail": [{"id":"b1","limit_value":100.0,"used_value":100.0,"remaining_value":0.0,"unit":"credits",
                              "is_active":true,"expires_at":1792424439538.0,
                              "source":"RESOURCE_PACKAGE_SOURCE_BONUS","status":"ACTIVE"}]},
          "total_quota": {"quota_summary": {"used_value":100,"limit_value":4100,"remaining_value":4000,"unit":"credits"}},
          "lastResetAt": 1788248570506.0, "nextResetAt": 1792495712958.0
        });
        let plan = json!({"plan_tier_name": "PLAN_TIER_PRO",
                          "end_date": 1792495712958.0, "next_refresh_date": 1792495712958.0});
        let w = parse_usage(&usage, &json!({"name": "nick9994374911"}), &plan, "");
        let g: Vec<&Value> = w["groups"].as_array().unwrap().iter().collect();
        assert_eq!(g[0]["name"], "套餐基础积分");
        assert_eq!(g[0]["items"][0]["expires_on"], "2026-10-20 19:28");
        assert_eq!(g[0]["items"][0]["label"], "套餐额度·周期刷新");
        // 组级到期由 state.rs 从逐笔现算，这里只保证逐笔日期已经带上（portions 同一条）
        assert_eq!(w["portions"][0]["expires_on"], "2026-10-20 19:28");
        // 顶层 nextResetAt 缺失时退到 userplan.next_refresh_date，两条路都得通
        let mut no_top = usage.clone();
        no_top["nextResetAt"] = json!(0);
        let w2 = parse_usage(&no_top, &json!({}), &plan, "");
        assert_eq!(w2["groups"][0]["items"][0]["expires_on"], "2026-10-20 19:28");
        // 两个都没有（体验版）才允许写"未给出"
        let w3 = parse_usage(&no_top, &json!({}), &json!({"plan_tier_name": "PLAN_TIER_FREE"}), "");
        assert_eq!(w3["groups"][0]["items"][0]["expires_on"], "未给出");
        assert_eq!(w3["groups"][0]["items"][0]["label"], "套餐额度");
    }

    /// 官网热力图那份真实报文（厂商自己把 371 天补零、还给了四档分色阈值）转成按天序列。
    /// 阈值必须原样带上：同一份数在官网和驾驶舱分色不一样，用户只会认为读数不一致。
    #[test]
    fn heatmap_payload_becomes_a_daily_trend() {
        let h = json!({
            "year": 2026, "unit": "credits",
            "levels": [29.35, 206.69, 879.14, 1190.59],
            "total": 9836.16,
            "items": [{"date": "2025-10-07", "value": 0},
                      {"date": "2026-10-05", "value": 36.0456504078},
                      {"date": "2026-10-10", "value": 1190.5914247282}],
        });
        let t = trend_of_heatmap(&h).expect("这份形状要能聚出序列");
        assert_eq!(t["unit"], "credits");
        assert_eq!(t["days"].as_array().unwrap().len(), 3);
        assert_eq!(t["peak"], 1190.5914247282);
        assert_eq!(t["peak_date"], "2026-10-10");
        assert_eq!(t["levels"].as_array().unwrap().len(), 4, "厂商给的分档阈值不许丢");
        assert_eq!(t["from"], "2025-10-07");
        assert_eq!(t["to"], "2026-10-10");
        assert!(trend_of_heatmap(&json!({ "items": [] })).is_none());
        assert!(trend_of_heatmap(&json!({ "error": "401" })).is_none());
        let m = apply(
            &json!({ "name": "Qoder CN" }),
            &json!({ "left": 100, "total": 200, "portions": [], "groups": [], "heatmap": h }),
        );
        assert_eq!(m["quota_trend"]["days"].as_array().unwrap().len(), 3);
        // 这一轮读挂了、档案里已有图：留着上一轮的，不许换成报错
        let bad = apply(
            &m,
            &json!({ "left": 100, "total": 200, "portions": [], "groups": [],
                     "heatmap": { "error": "官网会话掉了" } }),
        );
        assert_eq!(bad["quota_trend"]["days"].as_array().unwrap().len(), 3);
    }
}
