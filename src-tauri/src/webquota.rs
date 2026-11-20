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

use chrono::TimeZone;

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
    /// 所以取会话时两个域都问一遍，谁那儿有 `session_cookie` 就用谁。
    hosts: &'static [&'static str],
    /// 判定"登录了没有"的那枚 cookie 名。Qoder 是 `qoder_session_cookie`；
    /// 妙手不靠单枚名字（整罐 cookie 复用，见 cookie_jar），这里只是兼容提示。
    session_cookie: &'static str,
    /// 登录页（在驾驶舱窗口里打开这个，用户登完 cookie 就落进这个档案）。
    /// 空表示用 usage_page 当登录页（有些站点直接在用量页上引导登录）。
    sign_in_path: &'static str,
    /// 用量页路径（也是 Referer 的来源）。
    usage_path: &'static str,
    /// 解析器：把这家官网的报文整理成驾驶舱那套「大组 + 逐笔」。
    /// 每家的报文结构完全不同，没法用一套通用逻辑，只能各写各的。
    parser: Parser,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Parser {
    /// Qoder：`/api/v2/me/usages/big_model_credits` 三节 quota_detail + histories 补过期
    Qoder,
    /// 妙手：`credit.catpaw.meituan.com` 的 /api/credit/balance + /credit/transactions
    Catpaw,
}

impl Site {
    fn usage_page(&self, host: &str) -> String {
        format!("https://{host}{}", self.usage_path)
    }
    /// 登录页：登录后由 oauth_callback 跳回用量页
    fn sign_in(&self, host: &str) -> String {
        if self.sign_in_path.is_empty() {
            return self.usage_page(host);
        }
        format!(
            "https://{host}{}?oauth_callback={}",
            self.sign_in_path,
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
            session_cookie: "qoder_session_cookie",
            sign_in_path: "/users/sign-in",
            usage_path: "/account/usage",
            parser: Parser::Qoder,
        },
    ),
    (
        "qoder",
        Site {
            hosts: &["qoder.com"],
            session_cookie: "qoder_session_cookie",
            sign_in_path: "/users/sign-in",
            usage_path: "/account/usage",
            parser: Parser::Qoder,
        },
    ),
    // 妙手（美团 CatPaw）：积分站点是独立的 credit.catpaw.meituan.com，
    // 登录走 passport.meituan.com。生产站点 withCredentials 发请求，
    // 会话 cookie 名不固定——取数时整罐 cookie 都带上（见 cookie_jar），
    // 这里的 token 只是历史兼容提示，不再当登录判据。
    (
        "catpaw",
        Site {
            hosts: &["credit.catpaw.meituan.com"],
            session_cookie: "token",
            // 这个站点没有独立登录页，直接在用量页上引导到 passport
            sign_in_path: "",
            usage_path: "/",
            parser: Parser::Catpaw,
        },
    ),
    // WorkBuddy 不登记官网站点：过期历史已实测可用客户端现有凭据直读
    // （/billing/meter/get-user-resource-* 加 OnlyExpiredPeriod 等参数，225 条全量），
    // 不需要网页登录。原先拼的 oauth_callback 登录地址也不符合官方入口
    // （官方用 platform/state/redirect_uri），留着只会报 invalid state。
];

pub fn site_for(store: &str) -> Option<&'static Site> {
    SITES.iter().find(|(s, _)| *s == store).map(|(_, v)| v)
}

/// Trae 旧组名 → 新档名（与 agenthub.py 的 TRAE_GROUP_ALIAS 同一套口径）。
/// 只返回需要改名的；同名映射返回 None。台账迁移组属性时用。
pub fn trae_group_alias(old: &str) -> Option<&'static str> {
    match old {
        "签到奖励" | "每日签到奖励" => Some("每日签到"),
        "每月登录积分" => Some("每月登录赠送"),
        "邀请好友奖励" | "TraeCode邀请好友奖励" => Some("TraeCode 邀请好友奖励"),
        "平台活动" => Some("积分返还"),
        _ => None,
    }
}

/// 支持官网取数的驾驶舱产品 id（界面据此决定摆不摆这个按钮）
pub fn supports(app_id: &str) -> bool {
    crate::local_quota::store_for(app_id).and_then(|s| site_for(s)).is_some()
}

fn store_of(app_id: &str) -> Result<&'static str, String> {
    crate::local_quota::store_for(app_id)
        .ok_or_else(|| "这个程序还没有本机直读通道，也就没有对应的官网接口".to_string())
}

/// 不再支持官网取数的产品，把档案里残留的「官网没读成、去登录」提示清掉。
/// 入口删了提示还在，卡片就会指着一颗不存在的按钮（10-12 定过这条规矩）。
/// 启动时跑一次；只动 quota_web_error 这一个字段，走 save_instance 唯一写出口。
pub fn sweep_stale_web_errors() {
    for app in crate::registry::all_apps() {
        if supports(&app.id) {
            continue;
        }
        for mut m in crate::registry::load_instances(&app.id) {
            if m["quota_web_error"].as_str().unwrap_or("").trim().is_empty() {
                continue;
            }
            if let Some(o) = m.as_object_mut() {
                o.insert("quota_web_error".into(), json!(""));
            }
            crate::registry::save_instance(&app.id, &m);
        }
    }
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

/// 打开（或唤到前台）官网登录窗口。用户在这个窗口里用这个号的官网账号登录一次，
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
        "已打开「{who}」自己的官网登录窗口（{host}）：在里面用这个号的官网账号登录一次，\
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

/// 只问这一个域名的 cookie 罐（不建窗口、不发请求）：有该产品的会话 cookie 就返回它。
fn cookie_for_host(store: &str, slot: &str, host: &str) -> Option<String> {
    let app = app_handle().ok()?;
    let site = site_for(store)?;
    let label = label_for(store, slot);
    if app.get_webview_window(&label).is_none() {
        return None;
    }
    let list = once(app, &label, &site.usage_page(host)).ok()?;
    list.iter()
        .find(|(n, _)| n == site.session_cookie)
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

/// 妙手会话存档：官网会话 cookie 是 WebView2 的**内存态** cookie，
/// 窗口关掉或驾驶舱重启（强杀进程）就没了，cookie 库里只剩几枚统计 cookie。
/// 所以读到会话后立刻存一份到本地，下次取数先用它，顶住"重启就没会话"。
fn catpaw_session_file(root: &std::path::Path, store: &str, slot: &str) -> PathBuf {
    root.join("webprofile").join(store).join(slot).join("session-cookie.txt")
}

fn catpaw_session_file_default(store: &str, slot: &str) -> PathBuf {
    catpaw_session_file(&crate::registry::base_dir(), store, slot)
}

/// 读会话存档。过期太久（7 天）的当没有，省得拿一个早就失效的 cookie 去撞接口。
fn catpaw_session_saved(root: &std::path::Path, store: &str, slot: &str) -> Option<String> {
    let path = catpaw_session_file(root, store, slot);
    let text = std::fs::read_to_string(path).ok()?;
    let mut lines = text.lines();
    let stamp = lines.next()?.trim().to_string();
    let cookie = lines.next().unwrap_or("").trim().to_string();
    if cookie.is_empty() {
        return None;
    }
    let saved_at = chrono::NaiveDateTime::parse_from_str(&stamp, "%Y-%m-%d %H:%M:%S").ok()?;
    let age = chrono::Local::now().naive_local() - saved_at;
    if age < chrono::Duration::zero() || age > chrono::Duration::days(7) {
        return None;
    }
    Some(cookie)
}

/// 存档里的会话万一已经失效（官网回 401），清掉它，
/// 让下一次取数回到官网窗口重新读，别拿一个失效 cookie 反复撞接口。
fn catpaw_session_drop(root: &std::path::Path, store: &str, slot: &str) {
    let _ = std::fs::remove_file(catpaw_session_file(root, store, slot));
}

fn catpaw_session_save(root: &std::path::Path, store: &str, slot: &str, cookie: &str) {
    let path = catpaw_session_file(root, store, slot);
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let body = format!(
        "{}\n{}\n",
        chrono::Local::now().format("%Y-%m-%d %H:%M:%S"),
        cookie.trim()
    );
    let _ = std::fs::write(path, body);
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
                        .find(|(n, _)| n == site.session_cookie)
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
                 先在驾驶舱打开的官网窗口里登录一次。",
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
    let Ok(s) = store_of(app_id) else { return false };
    let slot = slot_of(meta);
    match site_for(s).map(|x| x.parser) {
        // 妙手：cookie 罐里有任何会话就算登了（具体认不认由取数时的身份接口判）
        Some(Parser::Catpaw) => cookie_jar(s, &slot, false)
            .map(|(_, j)| !j.trim().is_empty())
            .unwrap_or(false),
        _ => cookie_of(s, &slot, false)
            .map(|(_, c)| !c.trim().is_empty())
            .unwrap_or(false),
    }
}

/// 妙手用：读这个号窗口里**整罐** cookie（不猜单枚名字），拼成 Cookie 头。
/// 生产站点 withCredentials 发请求，会话落在哪枚 cookie 上由站点自己定，
/// 预先猜名字（早先猜的 `token`）会把已经登录的会话判成未登录。
fn cookie_jar(store: &str, slot: &str, ensure: bool) -> Result<(String, String), String> {
    let app = app_handle()?.clone();
    let site = site_for(store).ok_or("这个产品没有登记官网地址")?;
    let label = label_for(store, slot);
    let host = preferred_host(store, slot);
    let window_open = app.get_webview_window(&label).is_some();

    // 实时优先：窗口开着就读整罐 cookie，用户刚登完的新 cookie 立即生效。
    // 窗口开着的场景下不许旧存档先压哨，否则每次刷新都命中旧 cookie，
    // 用户以为"又在拿旧数据秒回"，其实窗口里可能已经换了号。
    if window_open {
        let mut last = String::from("没读到 cookie");
        for attempt in 0..8 {
            if attempt > 0 {
                std::thread::sleep(Duration::from_millis(500));
            }
            match once(&app, &label, &site.usage_page(host)) {
                Ok(list) => {
                    let pairs: Vec<String> = list
                        .iter()
                        .filter(|(_, v)| !v.trim().is_empty())
                        .map(|(n, v)| format!("{n}={v}"))
                        .collect();
                    if !pairs.is_empty() {
                        let jar = pairs.join("; ");
                        catpaw_session_save(&crate::registry::base_dir(), store, slot, &jar);
                        return Ok((host.to_string(), jar));
                    }
                    // cookie 罐是空的=这个窗口没登录：不建新窗口、不傻等，回落存档
                    break;
                }
                Err(e) => {
                    if e.contains("还没就绪") || e.contains("超时") || e.contains("没回应") {
                        last = e;
                        continue;
                    }
                    return Err(e);
                }
            }
        }
    }
    // 兜底存档：窗口关了、驾驶舱刚重启过，或者窗口开着但还没登录——
    // 用这份本地会话（最多 7 天）顶住，取数时接口 401 会把它清掉。
    if let Some(saved) = catpaw_session_saved(&crate::registry::base_dir(), store, slot) {
        return Ok((host.to_string(), saved));
    }
    if window_open {
        Err(format!(
            "这个号的官网会话还没登录：{host} 的 cookie 罐是空的。\
             先在驾驶舱打开的官网窗口里登录一次。"
        ))
    } else if ensure {
        // 窗口没开、也没有存档兜底：才建隐藏窗口去读（用户可能刚把窗口关了）
        ensure_hidden(&app, store, slot)?;
        let mut last = String::from("没读到 cookie");
        for attempt in 0..8 {
            if attempt > 0 {
                std::thread::sleep(Duration::from_millis(500));
            }
            match once(&app, &label, &site.usage_page(host)) {
                Ok(list) => {
                    let pairs: Vec<String> = list
                        .iter()
                        .filter(|(_, v)| !v.trim().is_empty())
                        .map(|(n, v)| format!("{n}={v}"))
                        .collect();
                    if !pairs.is_empty() {
                        let jar = pairs.join("; ");
                        catpaw_session_save(&crate::registry::base_dir(), store, slot, &jar);
                        return Ok((host.to_string(), jar));
                    }
                    return Err(format!(
                        "这个号的官网会话还没登录：{host} 的 cookie 罐是空的。\
                         先在驾驶舱打开的官网窗口里登录一次。"
                    ));
                }
                Err(e) => {
                    if e.contains("还没就绪") || e.contains("超时") || e.contains("没回应") {
                        last = e;
                        continue;
                    }
                    return Err(e);
                }
            }
        }
        Err(last)
    } else {
        Err("登录窗口已经关了".to_string())
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

/// 同 ms_to_text，但保留到秒——只用于到账时间（厂商给到秒就保留到秒）。
/// 到期日不用它：到期判定那几处解析器认的是分钟精度。
fn ms_to_text_full(ms: f64) -> String {
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
    d.format("%Y-%m-%d %H:%M:%S").to_string()
}

/// 官网的 source 枚举 → 驾驶舱那四大组的名字。
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
    v.get(k).and_then(num_of).unwrap_or(0.0)
}

/// 官网额度字段常以**字符串**返回（"2170.50"、"1000.00"、"-162.18"），
/// 数字与数字字符串都必须收。只按 `as_f64` 取会把整份读数当 0——
/// 2026-11-18 真机：balance/流水全是字符串，卡片因此显示"剩 0 / 共 0"。
fn num_of(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse::<f64>().ok(),
        _ => None,
    }
}

/// 整数字段同样有字符串形态（extJson.invType 真机是 "9"）。
fn int_of(v: &Value) -> Option<i64> {
    match v {
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
        Value::String(s) => s.trim().parse::<i64>().ok(),
        _ => None,
    }
}

/// 抓一次官网额度：逐资源包 + 身份 + 套餐 + 每日消耗热力图。
/// 用的是这张卡自己那份会话，所以读回来的一定是这个号的数。
/// 返回 {ok, account, user_id, plan, plan_expire, left, total, used, packs:[...], portions:[...]}
pub fn fetch(app_id: &str, meta: &Value) -> Value {
    let r = fetch_inner(app_id, meta);
    // 用完就关。官网 usage 页自带 JS 定时器/动画，隐藏窗口里照样全速跑——
    // 实测每个窗口的渲染进程持续吃 ~26% CPU、GPU 进程 ~17%，三张 web 卡合起来
    // 白白烧掉一整个核，整机滚动/缩放跟着卡。登录中的可见窗口（open_login 建的）
    // 必须留着让人操作，只关"我们为取数临时建的"那一扇：标签固定 webq- 前缀 +
    // 标题是「官网会话」，用户手动开的登录窗口标题不是这个，不会误伤。
    if let Ok(app) = app_handle() {
        let store_ok = store_of(app_id).ok();
        let slot = slot_of(meta);
        if let Some(store) = store_ok {
            let label = label_for(store, &slot);
            if let Some(w) = app.get_webview_window(&label) {
                let is_temp = w.title().map(|t| t == "官网会话").unwrap_or(false)
                    && !w.is_visible().unwrap_or(true);
                if is_temp {
                    let _ = w.destroy();
                }
            }
        }
    }
    r
}

fn fetch_inner(app_id: &str, meta: &Value) -> Value {
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
    // 拿 A 域的会话去调 B 域的接口只会 401）。
    // 妙手例外：生产站点 withCredentials 发请求，会话 cookie 名不固定，
    // 不能猜单枚名字——整罐 cookie 都带上，登没登录由 get-current-user 认。
    let (host, hdr) = match site.parser {
        Parser::Catpaw => match cookie_jar(store, &slot, true) {
            Ok((h, jar)) => (h, jar),
            Err(e) => {
                return json!({ "ok": false, "need_login": true, "slot": slot, "msg": e });
            }
        },
        _ => match session_cookie(store, &slot) {
            Ok((h, c)) => (h, format!("{}={}", site.session_cookie, c)),
            Err(e) => {
                return json!({ "ok": false, "need_login": true, "slot": slot, "msg": e });
            }
        },
    };
    let referer = site.usage_page(&host);
    // 各家的接口与报文结构完全不同，按 parser 分派。
    match site.parser {
        Parser::Qoder => fetch_qoder(host.as_str(), &hdr, &referer, &slot),
        Parser::Catpaw => fetch_catpaw(host.as_str(), &hdr, &referer, &slot),
    }
}

/// Qoder：三节 quota_detail + histories 补过期笔。
fn fetch_qoder(host: &str, hdr: &str, referer: &str, slot: &str) -> Value {
    let mut usage = match get_json(
        &format!("https://{}/api/v2/me/usages/big_model_credits", host),
        hdr,
        referer,
    ) {
        Ok(v) => v,
        Err(e) => return json!({ "ok": false, "msg": e }),
    };
    // 身份与套餐读不到不影响额度，单独降级成空
    let me = get_json(&format!("https://{}/api/v1/me", host), hdr, referer)
        .unwrap_or_else(|_| json!({}));
    let plan = get_json(&format!("https://{}/api/v1/me/userplan", host), hdr, referer)
        .unwrap_or_else(|_| json!({}));
    // 官网用量页那张「近一年每日 Credits 消耗」热力图就是这条接口给的：
    // 厂商自己把 371 天补零成连续序列，还顺带给四档分色阈值（levels）。
    // 读不到只少一张图，不许把额度本身打挂，所以跟 me/plan 一样降级成 error 装进报文。
    let heatmap = get_json(
        &format!("https://{}/api/v1/me/ai-conversations/credits-heatmap?days=371", host),
        hdr,
        referer,
    )
    .unwrap_or_else(|e| json!({ "error": e }));

    // 历史逐笔（含**已过期**的包）：官网用量页有这条接口，我们以前没接过，
    // 于是包一到期就从列表里消失。报文结构与 /api/v2/me/usages/big_model_credits
    // 同族（plan_quota / resource_package_quota 三节），只是多带已经作废的笔。
    // 读不到只影响「已过期」那一栏，不许把当期额度打挂。
    let histories = get_json(
        &format!("https://{}/api/v1/me/usages/big_model_credits/histories", host),
        hdr,
        referer,
    )
    .unwrap_or_else(|e| json!({ "error": e }));

    if let Some(o) = usage.as_object_mut() {
        o.insert("histories".into(), histories);
    }
    let mut out = parse_usage(&usage, &me, &plan, referer);
    if let Some(o) = out.as_object_mut() {
        o.insert("slot".into(), json!(slot));
        o.insert("host".into(), json!(host));
        o.insert("heatmap".into(), heatmap);
        o.insert("histories".into(), usage["histories"].clone());
    }
    out
}

/// 妙手：`credit.catpaw.meituan.com` 的余额 + 流水。
///
/// 官网契约（2026-11-17 核对生产脚本）：
///   余额 `/api/credit/balance` 的 `data.availableCredits` 是权威值；
///   流水 `/api/credit/transactions` 的 `logTab` 是**数字**：1=消耗 2=到账 3=过期；
///   数额读 `realCredit`，到账时间 `ctime`、消耗时间 `utime`，
///   到期/生效时间在 `extJson`（JSON 字符串）里的 expireTime / effectiveTime；
///   业务关联用 `logBizId`。消耗流水不是新积分包，过期扣除额也不是原始发放额。
fn catpaw_identity_valid(identity: &Value) -> bool {
    match identity.get("userId") {
        Some(Value::Number(n)) => n.as_u64().is_some_and(|id| id > 0),
        Some(Value::String(id)) => !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit())
            && id.bytes().any(|b| b != b'0'),
        _ => false,
    }
}

fn fetch_catpaw(host: &str, hdr: &str, referer: &str, slot: &str) -> Value {
    let base = format!("https://{host}/api");
    let bal = match get_json(&format!("{base}/credit/balance"), hdr, referer) {
        Ok(v) => v,
        Err(e) => {
            return json!({ "ok": false, "need_login": e.contains("没登录"), "msg": e });
        }
    };
    // 妙手接口的鉴权失败是 **HTTP 200 + body code=401**（实测：
    // {"code":401,"data":{"message":"未登录或登录失效，需要先登录"}}）。
    // 只看 HTTP 状态会把"没登录"当成"读到 0"，还会拿空读数覆盖掉好数据。
    if let Some(code) = bal.get("code").and_then(|v| v.as_i64()) {
        if code != 0 && code != 200 {
            let msg = bal
                .get("data")
                .and_then(|d| d.get("message"))
                .and_then(|m| m.as_str())
                .unwrap_or("官网接口拒绝（可能是会话失效）");
            if code == 401 {
                // 存档里的会话失效了：清掉，让下一次回到官网窗口重读
                catpaw_session_drop(&crate::registry::base_dir(), "catpaw", slot);
            }
            return json!({ "ok": false, "need_login": code == 401, "msg": msg });
        }
    }
    let me = get_json(&format!("{base}/passport/get-current-user"), hdr, referer)
        .unwrap_or_else(|_| json!({}));
    if let Some(code) = me.get("code").and_then(|v| v.as_i64()) {
        if code == 401 {
            // 存档里的会话失效了：清掉，让下一次回到官网窗口重读
            catpaw_session_drop(&crate::registry::base_dir(), "catpaw", slot);
            return json!({ "ok": false, "need_login": true,
                           "msg": "官网说没登录（会话失效）：先在这个号的官网窗口登录一次" });
        }
    }
    let identity = me.get("data").unwrap_or(&me);
    if !catpaw_identity_valid(identity) {
        return json!({"ok": false, "msg": "官网未返回账号身份，本次保留已有额度与账号绑定"});
    }
    // availableCredits 在 2026-11-18 真机上是字符串 "2170.50"，
    // 必须走 num_of；直接 as_f64 会把权威余额读成空。
    let available = bal.get("data").and_then(|d| d.get("availableCredits")).and_then(num_of);

    let mut errs: Vec<String> = Vec::new();
    // 三个页签都要取：早先只取了消耗/过期（还传错了字符串），漏了到账流水
    let http = CatpawHttp { base: &base, hdr: &hdr, referer: &referer };
    let (grants, grant_notes) = catpaw_pages(&http, 2);
    errs.extend(grant_notes.iter().map(|e| format!("到账流水: {e}")));
    let (consumes, consume_notes) = catpaw_pages(&http, 1);
    errs.extend(consume_notes.iter().map(|e| format!("消耗流水: {e}")));
    let (expireds, expire_notes) = catpaw_pages(&http, 3);
    errs.extend(expire_notes.iter().map(|e| format!("过期流水: {e}")));
    if grants.is_empty() && !errs.is_empty() {
        return json!({ "ok": false, "need_login": errs.iter().any(|e| e.contains("没登录")),
                       "msg": errs.join("；") });
    }
    // 空读数不许覆盖：没有余额也没有任何逐笔，只可能是"没登进来"或接口口径变了，
    // 拿它写档案会把卡上原本好端端的数抹成"未给出"
    if available.is_none() && grants.is_empty() && expireds.is_empty() {
        return json!({ "ok": false, "need_login": true,
                       "msg": "官网读数为空（多半是会话失效）：先在官网窗口登录一次，这次没有覆盖卡片上的旧数据" });
    }

    let mut packs: Vec<Value> = grants.iter().filter_map(catpaw_grant).collect();
    catpaw_apply_expired(&mut packs, &expireds);
    // 逐包消耗官方没有接口，但消费流水带时刻、每笔包有到期时间，
    // 可以按用户对账口径还原（2026-11-18 用户确认）：
    // 「第 1 个月 200 用完，后两个月没用，第 4 个月用了 37.82，
    //   注册 1000 用掉 29.5」——即每次消费扣**当时还在有效期、最早到期**的那一笔。
    // 这是推导值，不冒充厂商数据；界面标「推算」。
    let pack_used = catpaw_allocate_consumption(&packs, &consumes);
    for p in packs.iter_mut() {
        let biz = p["log_biz_id"].as_str().unwrap_or("");
        if !biz.is_empty() {
            let total = p["total"].as_f64().unwrap_or(0.0);
            // 没有消费记录的包也要写 0，否则界面拿不到分母，进度条不画
            let used = (pack_used.get(biz).copied().unwrap_or(0.0) * 100.0).round() / 100.0;
            p["used"] = json!(used);
            p["used_est"] = json!(true);
            let expired = p["expired"].as_bool().unwrap_or(false);
            if expired {
                // 过期包：剩余就是作废额，不重复算
                p["left"] = p["voided"].clone();
            } else {
                let l = ((total - used) * 100.0).round() / 100.0;
                p["left"] = json!(l);
            }
        }
    }
    // 消耗流水的 realCredit 是负数，顶层「已用」按正数展示。
    let used_sum: f64 = consumes.iter().map(|c| f(c, "realCredit").abs()).sum();
    // 官网没有独立热力图接口：用「积分消耗」页签的逐笔流水按天聚合，
    // 形状对齐 Qoder 的 heatmap，界面那条波形图就能直接复用。
    let heatmap = catpaw_trend(&consumes);

    let mut o = packs_to_shape(&packs, identity, &bal, referer, "妙手", "积分");
    if let Some(m) = o.as_object_mut() {
        // 权威余额来自 balance 接口，不是从不完整逐笔重新求和
        m.insert("left".into(), available.map(|v| json!(v)).unwrap_or(Value::Null));
        // 当前有效包的已用和 total 同口径；历史累计仅用于趋势。
        m.insert("lifetime_used".into(), json!(used_sum));
        m.insert("slot".into(), json!(slot));
        m.insert("host".into(), json!(host));
        m.insert("heatmap".into(), heatmap);
        m.insert("balance_note".into(),
                 json!("余额以官网 availableCredits 为准；官方不提供逐包消耗，逐笔已用按消费流水推算"));
        if !errs.is_empty() {
            m.insert("partial_errors".into(), json!(errs));
        }
    }
    o
}

/// 妙手流水请求的 URL。单独抽出来是为了离线测试钉住官网的时间戳契约，
/// 不连网也能验证 `startTime/endTime` 是毫秒数字而不是 `YYYY-MM-DD` 字符串
/// （传日期字符串会得到业务码 1500，2026-11-18 真机确认）。
fn catpaw_transactions_url(base: &str, page: i64, start_ms: i64, end_ms: i64, tab: u8) -> String {
    format!(
        "{base}/credit/transactions?pageNum={page}&pageSize=20&startTime={start_ms}&endTime={end_ms}&logTab={tab}"
    )
}

/// 妙手流水接口的一次请求。返回 (这一页的记录, 总条数)。
/// 妙手的业务码在 body 里（HTTP 仍 200）：401 = 未登录，1500 = 缺时间参数，
/// 1001 = 查询区间超出服务端允许范围（2026-11-18 真机：起点早于允许范围时
/// 三个页签全部 1001）。把服务端自己的报错文字带出来，别只丢一个数字。
fn catpaw_body_error(v: &Value) -> Option<String> {
    let code = v.get("code").and_then(|c| c.as_i64())?;
    if code == 0 || code == 200 {
        return None;
    }
    let detail = v
        .get("message")
        .and_then(|m| m.as_str())
        .or_else(|| {
            v.get("data")
                .and_then(|d| d.get("message"))
                .and_then(|m| m.as_str())
        })
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("");
    Some(if code == 401 {
        "官网说没登录（会话失效）".to_string()
    } else if detail.is_empty() {
        format!("流水接口返回 code {code}")
    } else {
        format!("流水接口返回 code {code}：{detail}")
    })
}

fn catpaw_transactions_page(
    base: &str,
    hdr: &str,
    referer: &str,
    tab: u8,
    page: i64,
    start_ms: i64,
    end_ms: i64,
) -> Result<(Vec<Value>, i64), String> {
    let url = catpaw_transactions_url(base, page, start_ms, end_ms, tab);
    let v = get_json(&url, hdr, referer)?;
    if let Some(msg) = catpaw_body_error(&v) {
        return Err(msg);
    }
    let d = v.get("data").cloned().unwrap_or_else(|| v.clone());
    let list = d
        .get("list")
        .and_then(|l| l.as_array())
        .cloned()
        .unwrap_or_default();
    let total = d.get("total").and_then(|t| t.as_i64()).unwrap_or(0);
    Ok((list, total))
}

/// 妙手流水全量读取：从今天往回按窗口滚，碰到服务端边界就停。
///
/// `startTime/endTime` 必须传**毫秒时间戳**（官网 `gu()` 用
/// `new Date(y,m,d).getTime()` 与 `new Date(y,m,d,23,59,59,999).getTime()` 生成）。
/// 抓包 `…/credit/transactions?pageNum=1&pageSize=20&startTime=1794412800000&
/// endTime=1795017599999&logTab=1`：pageSize 固定 20，窗口是官网 7d 预设
/// （当天 00:00 到今天 23:59:59.999）。
///
/// 方向很关键：**必须从今天往回滚**。正向从最早日期开始，第一个请求就落在
/// 服务端允许范围之外，整轮直接被 1001 挡掉（2026-11-18 真机）。
/// 往回滚碰到 1001 说明已经越过服务端能查的最早时间，把起点夹在那里，
/// 更早的记录官网自己也查不到，就到这里为止。
/// 一次流水请求：页码 + 时间窗，返回 (这一页的记录, 总条数)。
/// 抽成 trait 是为了能离线模拟服务端的分页与业务码，不用连网也能验窗口推进。
trait CatpawFetch {
    fn fetch(&self, tab: u8, page: i64, start_ms: i64, end_ms: i64) -> Result<(Vec<Value>, i64), String>;
}

/// 真的打官网那条接口。
struct CatpawHttp<'a> {
    base: &'a str,
    hdr: &'a str,
    referer: &'a str,
}

impl CatpawFetch for CatpawHttp<'_> {
    fn fetch(&self, tab: u8, page: i64, start_ms: i64, end_ms: i64) -> Result<(Vec<Value>, i64), String> {
        catpaw_transactions_page(self.base, self.hdr, self.referer, tab, page, start_ms, end_ms)
    }
}

/// 取一个时间窗内的全部流水（翻页取完）。返回 (记录, 备注)。
fn catpaw_window(
    fetcher: &dyn CatpawFetch,
    tab: u8,
    start_day: chrono::NaiveDate,
    end_day: chrono::NaiveDate,
) -> (Vec<Value>, Option<String>) {
    let start_ms = chrono::Local
        .from_local_datetime(&start_day.and_hms_opt(0, 0, 0).unwrap())
        .single()
        .map(|t| t.timestamp_millis())
        .unwrap_or(0);
    let end_ms = chrono::Local
        .from_local_datetime(&end_day.and_hms_milli_opt(23, 59, 59, 999).unwrap())
        .single()
        .map(|t| t.timestamp_millis())
        .unwrap_or(0);
    let mut out: Vec<Value> = vec![];
    let mut page = 1;
    let mut collected = 0usize;
    loop {
        match fetcher.fetch(tab, page, start_ms, end_ms) {
            Ok((list, total)) => {
                let n = list.len();
                collected += n;
                out.extend(list);
                if n == 0 || (total > 0 && (collected as i64) >= total) {
                    break;
                }
                page += 1;
                if page > 100 {
                    break;
                }
            }
            Err(e) => return (out, Some(e)),
        }
    }
    (out, None)
}

/// 妙手流水全量读取：从今天往回按窗口滚，把注册以来的到账/消耗/失效都拉出来。
///
/// `startTime/endTime` 必须传**毫秒时间戳**（官网 `gu()` 用
/// `new Date(y,m,d).getTime()` 与 `new Date(y,m,d,23,59,59,999).getTime()` 生成）。
/// 抓包 `…/credit/transactions?pageNum=1&pageSize=20&startTime=1794412800000&
/// endTime=1795017599999&logTab=1`：pageSize 固定 20，窗口是官网 7d 预设
/// （当天 00:00 到今天 23:59:59.999）。
///
/// 方向必须**从今天往回滚**：正向从最早日期开始，第一个请求往往落在没有记录的
/// 区间上，服务端回 1001，整轮就断了（2026-11-18 真机）。
///
/// 1001 的两种可能都照顾到：先按 30 天窗试，被拒就把这段拆成 7 天子窗重试
/// （7 天是官网默认预设，一定合法）；子窗也全被拒，说明这段真的没有记录，
/// 跳过继续往更早走。全程最多连续 5 段空窗口就收手（半年没动静的号到这儿够了）。
fn catpaw_pages(
    fetcher: &dyn CatpawFetch,
    tab: u8,
) -> (Vec<Value>, Vec<String>) {
    let today = chrono::Local::now().date_naive();
    let floor = today - chrono::Duration::days(400);
    let mut out: Vec<Value> = vec![];
    let mut notes: Vec<String> = vec![];
    let mut cursor_end = today;
    let mut guard = 0;
    let mut empty_streak = 0i64;
    while cursor_end >= floor && guard < 80 {
        guard += 1;
        let window_start = cursor_end - chrono::Duration::days(29);
        let (rows, err) = catpaw_window(fetcher, tab, window_start, cursor_end);
        if let Some(e) = err {
            if !e.contains("code 1001") {
                notes.push(e);
                break;
            }
            // 30 天窗被拒：拆成 7 天子窗重试（官网默认预设的宽度）
            let mut sub_rows: Vec<Value> = vec![];
            let mut sub_err: Option<String> = None;
            let mut sub_end = cursor_end;
            while sub_end >= window_start {
                let sub_start = if sub_end - chrono::Duration::days(6) < window_start {
                    window_start
                } else {
                    sub_end - chrono::Duration::days(6)
                };
                let (rows, err) = catpaw_window(fetcher, tab, sub_start, sub_end);
                sub_rows.extend(rows);
                if let Some(e) = err {
                    let is_empty_range = e.contains("code 1001");
                    sub_err = Some(e);
                    if !is_empty_range {
                        break;
                    }
                }
                if sub_start == window_start {
                    break;
                }
                sub_end = sub_start - chrono::Duration::days(1);
            }
            if sub_rows.is_empty() {
                empty_streak += 1;
                if let Some(e) = sub_err {
                    if !e.contains("code 1001") {
                        notes.push(e);
                        break;
                    }
                }
                if empty_streak > 5 {
                    break;
                }
            } else {
                empty_streak = 0;
                out.extend(sub_rows);
            }
        } else {
            empty_streak = 0;
            out.extend(rows);
        }
        cursor_end = window_start - chrono::Duration::days(1);
    }
    (out, notes)
}

/// extJson 是 JSON 字符串（到期/生效时间藏在里面），解不开就当空。
fn catpaw_ext(it: &Value) -> Value {
    it.get("extJson")
        .and_then(|v| v.as_str())
        .and_then(|s| serde_json::from_str::<Value>(s).ok())
        .unwrap_or_else(|| json!({}))
}

/// 时间字段：毫秒数字或字符串都收；没给就是空（不猜）。
fn catpaw_time(v: Option<&Value>) -> String {
    match v {
        Some(Value::Number(n)) => n.as_f64().map(ms_to_text_full).unwrap_or_default(),
        // 毫秒时间戳也会以字符串返回（extJson.expireTime = "1810223999000"），
        // 原样透出就成了数字串，卡片上会看不到日期。
        Some(Value::String(s)) if !s.trim().is_empty() => {
            match s.trim().parse::<f64>() {
                Ok(ms) if ms > 1_000_000_000_000.0 => ms_to_text_full(ms),
                _ => s.trim().to_string(),
            }
        }
        _ => String::new(),
    }
}

/// 官网 logType → 官方名称（站点脚本 `kS` 表原文照抄）。
/// 「赠送/购买」两栏也按官网口径：logType 2（购买专业版）与 20（购买加量包）
/// 算购买，其余算赠送（官网 `new Set([2,20])`）。
fn catpaw_log_type_name(log_type: i64) -> Option<&'static str> {
    Some(match log_type {
        1 => "登录奖励",
        2 => "购买专业版",
        3 => "运营活动",
        7 => "退款回收",
        8 => "过期",
        9 => "赠送基础积分",
        20 => "购买加量包",
        21 => "双倍积分赠送",
        31 => "老带新邀请人奖励",
        32 => "老带新被邀请人奖励",
        33 => "新用户注册奖励",
        41 => "Managed Agents产品消耗",
        42 => "CatPaw Office产品消耗",
        43 => "妙手产品消耗",
        44 => "CatPaw IDE产品消耗",
        91 => "积分管理后台发放",
        _ => return None,
    })
}

/// 这一笔的官方名称：logType 映射优先（与官网表格「描述」列一致），
/// 官网没给 logType 时才退回接口自带的 logTypeDesc。
fn catpaw_label(it: &Value) -> String {
    let log_type = it.get("logType").and_then(|v| v.as_i64()).unwrap_or(0);
    if let Some(name) = catpaw_log_type_name(log_type) {
        return name.to_string();
    }
    if let Some(s) = it
        .get("logTypeDesc")
        .or_else(|| it.get("title"))
        .or_else(|| it.get("desc"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
    {
        return s.to_string();
    }
    "积分".into()
}

/// 妙手一条**到账**流水 → 一笔额度。数额 realCredit、时间 ctime、到期 extJson.expireTime。
/// 逐笔已用/剩余官方不给：留 Null，界面标「未给出」，不能拿总消费分摊。
fn catpaw_grant(it: &Value) -> Option<Value> {
    let total = f(it, "realCredit");
    if total <= 0.0 {
        return None;
    }
    let ext = catpaw_ext(it);
    Some(json!({
        "label": catpaw_label(it),
        "total": total,
        "used": Value::Null,
        "left": Value::Null,
        "expires_on": catpaw_time(ext.get("expireTime")),
        // 官网「积分到账」表格：「生效时间」列读流水 ctime，
        // 「失效时间」列读 extJson.expireTime。照官网口径写。
        "granted_on": catpaw_time(it.get("ctime")),
        "granted_src": "vendor",
        "unit": "积分",
        "expired": false,
        "log_type": it.get("logType").and_then(|v| v.as_i64()).unwrap_or(0),
        "log_biz_id": it.get("logBizId").and_then(|v| v.as_str()).unwrap_or(""),
        // 到期流水与到账流水唯一的共同键（logBizId 对不上，见 fetch_catpaw）
        "inv_id": catpaw_inv_id(it),
    }))
}

/// 妙手库存 ID：数字或数字字符串都收。失效流水靠它认回原发放。
fn catpaw_inv_id(it: &Value) -> Option<i64> {
    it.get("invId").and_then(int_of)
}

/// 把「失效积分」页签的记录并回对应的到账包。
///
/// 2026-11-18 真机证实：失效流水的 logBizId 是 `expire_inv_1250371`，
/// 跟到账流水的 `F2026104138865630` 永远对不上；两边唯一共同的键是 invId。
/// 失效流水的 realCredit 是负数（-162.18），作废额按绝对值记。
fn catpaw_apply_expired(packs: &mut Vec<Value>, expireds: &[Value]) {
    let mut inv_idx: std::collections::HashMap<i64, usize> = std::collections::HashMap::new();
    for (i, p) in packs.iter().enumerate() {
        if let Some(inv) = p["inv_id"].as_i64() {
            inv_idx.insert(inv, i);
        }
    }
    for ex in expireds {
        let voided = f(ex, "realCredit").abs();
        let biz = ex.get("logBizId").and_then(|v| v.as_str()).unwrap_or("");
        if let Some(inv) = catpaw_inv_id(ex) {
            if let Some(&i) = inv_idx.get(&inv) {
                packs[i]["expired"] = json!(true);
                packs[i]["voided"] = json!(voided);
                continue;
            }
        }
        let ext = catpaw_ext(ex);
        // initialCredit 是原始发放额，单位 0.01（"20000" = 200.00）；
        // 缺失时留空，绝不拿失效扣除额冒充原发放额。
        let original = f(&ext, "initialCredit");
        let total = if original > 0.0 { json!(original / 100.0) } else { Value::Null };
        let inv_type = ext.get("invType").and_then(int_of).unwrap_or(0);
        let label = catpaw_log_type_name(inv_type)
            .map(str::to_string)
            .unwrap_or_else(|| catpaw_label(ex));
        packs.push(json!({
            "label": label,
            "total": total, "used": Value::Null, "left": Value::Null,
            "voided": voided,
            "log_biz_id": biz,
            "inv_id": catpaw_inv_id(ex),
            // 官网「失效积分」页的两列：生效时间 = extJson.effectiveTime，
            // 失效时间 = 这条失效流水的 ctime（作废发生的那一刻）。
            "expires_on": catpaw_time(ex.get("ctime")),
            "granted_on": catpaw_time(ext.get("effectiveTime")),
            "granted_src": "effective",
            "unit": "积分", "expired": true,
            "log_type": inv_type,
        }));
    }
}

/// 妙手「积分消耗」流水 → 按天消耗序列（形状对齐 Qoder 的 heatmap，
/// 让界面那条波形图直接复用）。消耗流水的 realCredit 是负数，按绝对值入账。
fn catpaw_trend(consumes: &[Value]) -> Value {
    use std::collections::BTreeMap;
    let mut by_day: BTreeMap<String, f64> = BTreeMap::new();
    for c in consumes {
        let spent = f(c, "realCredit").abs();
        if spent <= 0.0 {
            continue;
        }
        let Some(ctime) = c.get("ctime").and_then(|v| match v {
            Value::Number(n) => n.as_f64(),
            Value::String(s) => s.trim().parse::<f64>().ok(),
            _ => None,
        }) else {
            continue;
        };
        let secs = (ctime / 1000.0) as i64;
        let Some(dt) = chrono::DateTime::from_timestamp(secs, 0) else {
            continue;
        };
        let day = dt.with_timezone(&chrono::Local).format("%Y-%m-%d").to_string();
        *by_day.entry(day).or_insert(0.0) += spent;
    }
    if by_day.is_empty() {
        return json!({ "error": "官网没返回积分消耗流水" });
    }
    let days: Vec<Value> = by_day
        .iter()
        .map(|(date, credits)| json!({ "date": date, "value": credits }))
        .collect();
    let total: f64 = by_day.values().sum();
    let (peak_date, peak) = by_day
        .iter()
        .max_by(|a, b| a.1.partial_cmp(b.1).unwrap_or(std::cmp::Ordering::Equal))
        .map(|(d, v)| (d.clone(), *v))
        .unwrap_or_default();
    json!({
        "unit": "积分",
        "items": days,
        "total": total,
        "peak": peak,
        "peak_date": peak_date,
        "levels": [],
        "from": by_day.keys().next().cloned().unwrap_or_default(),
        "to": by_day.keys().next_back().cloned().unwrap_or_default(),
        "source": "官网积分消耗流水",
    })
}

/// 把每笔消费摊到具体的积分包上。
///
/// 官网不给「哪笔包被用了多少」，但消费流水带时刻、每笔包有到账与到期时间，
/// 按用户的对账口径还原（2026-11-18 用户确认）：
///   第 1 个月 200 用完、第 2/3 个月没用、第 4 个月用了 200−162.18=37.82、
///   注册 1000 用掉 29.5 —— 每次消费扣**当时还在有效期、最早到期**的那一笔。
/// 返回 logBizId → 已用额。这是推导值，界面标「推算」，不冒充厂商数据。
fn catpaw_allocate_consumption(packs: &[Value], consumes: &[Value]) -> std::collections::HashMap<String, f64> {
    use chrono::TimeZone;
    // 到账包按 (到期时刻, 到账时刻) 排序；解析不出到期时间的包不参与摊派
    let mut order: Vec<(i64, i64, String, f64)> = packs
        .iter()
        .filter_map(|p| {
            let exp_ms = chrono::NaiveDateTime::parse_from_str(
                p["expires_on"].as_str().unwrap_or(""),
                "%Y-%m-%d %H:%M:%S",
            )
            .ok()
            .and_then(|d| chrono::Local.from_local_datetime(&d).single())
            .map(|d| d.timestamp_millis())?;
            let grant_ms = match p["granted_on"].as_str().unwrap_or("") {
                s if s.is_empty() => 0,
                s => chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S")
                    .ok()
                    .and_then(|d| chrono::Local.from_local_datetime(&d).single())
                    .map(|d| d.timestamp_millis())
                    .unwrap_or(0),
            };
            let total = p["total"].as_f64()?;
            let biz = p["log_biz_id"].as_str().unwrap_or("").to_string();
            if biz.is_empty() {
                return None;
            }
            Some((exp_ms, grant_ms, biz, total))
        })
        .collect();
    order.sort_by_key(|(exp, grant, _, _)| (*exp, *grant));

    let mut used: std::collections::HashMap<String, f64> = std::collections::HashMap::new();
    // 消费流水按时刻升序，与官网「积分消耗」页顺序一致
    let mut rows: Vec<(i64, f64)> = consumes
        .iter()
        .filter_map(|c| {
            let spent = f(c, "realCredit");
            if spent >= 0.0 {
                return None; // 0 消耗（免费模型）与正数都不是扣减
            }
            let ctime = match c.get("ctime") {
                Some(Value::Number(n)) => n.as_f64(),
                Some(Value::String(s)) => s.trim().parse::<f64>().ok(),
                _ => None,
            }?;
            Some((ctime as i64, spent.abs()))
        })
        .collect();
    rows.sort_by_key(|(t, _)| *t);

    let mut remaining: Vec<f64> = order.iter().map(|(_, _, _, t)| *t).collect();
    for (ctime, spent) in rows {
        // 只扣「消费时刻仍在有效期」的包里最早到期的：到期时刻必须晚于消费时刻
        let mut rest = spent;
        for (i, (exp_ms, grant_ms, biz, _)) in order.iter().enumerate() {
            // 消费时刻必须落在这一包的 [到账, 到期) 区间里
            if *grant_ms > ctime || *exp_ms <= ctime || remaining[i] <= 0.0 {
                continue;
            }
            let take = rest.min(remaining[i]);
            remaining[i] -= take;
            *used.entry(biz.clone()).or_insert(0.0) += take;
            rest -= take;
            if rest <= 1e-9 {
                break;
            }
        }
        // rest 还剩说明流水超出可解释范围（比如发放记录不在可查区间），
        // 这部分不硬塞进任何包——对不上就是对不上，宁可总数对不齐也不编。
    }
    used
}

/// 把「逐笔」摊成驾驶舱那套「大组 + 逐笔」。
///
/// 各家官网的报文长得完全不一样，但整理完之后形状是一致的：一条额度 =
/// {label, total, used, left, expires_on, granted_on, unit, expired}。
/// 所以收口在这一处，省得每家都写一遍分组/汇总。
fn packs_to_shape(
    packs: &[Value],
    me: &Value,
    summary: &Value,
    referer: &str,
    product_name: &str,
    unit: &str,
) -> Value {
    const ORDER: [&str; 4] = ["套餐基础积分", "平台奖励积分", "购买积分", "其他"];
    let mut by_group: Vec<Value> = ORDER
        .iter()
        .map(|n| json!({ "name": n, "unit": unit, "left": 0.0, "total": 0.0,
                         "used": 0.0, "items": [] }))
        .collect();
    let mut portions: Vec<Value> = Vec::new();
    let now = chrono::Local::now().naive_local();
    for p in packs {
        let expired = p["expired"].as_bool().unwrap_or(false)
            || crate::quota::expiry(p["expires_on"].as_str().unwrap_or("")).0;
        // 分档按官网口径：logType 2（购买专业版）与 20（购买加量包）是购买，
        // 其余（赠送基础积分、新用户注册奖励、积分管理后台发放…）是赠送。
        let label = p["label"].as_str().unwrap_or("");
        let log_type = p["log_type"].as_i64().unwrap_or(0);
        let gname = if log_type == 2 || log_type == 20 {
            "购买积分"
        } else if log_type == 9 {
            // 妙手 logType 9 = 赠送基础积分，就是体验版每月刷新的 200，
            // 属于套餐基础积分；靠标签文字猜不到（里面没有"套餐"二字）。
            "套餐基础积分"
        } else if label.contains("购买") || label.contains("充值") || label.contains("订单") {
            "购买积分"
        } else if label.contains("套餐") || label.contains("订阅") {
            "套餐基础积分"
        } else {
            "平台奖励积分"
        };
        let gi = ORDER.iter().position(|n| *n == gname).unwrap_or(3);
        let item = json!({
            "label": label,
            "pool": gname,
            "detail": "",
            "pid": format!("{}|{}|{}", label, p["expires_on"], p["total"]),
            "left": p["left"].clone(),
            "total": p["total"].clone(),
            "used": p["used"].clone(),
            "expires_on": p["expires_on"].clone(),
            "granted_on": p["granted_on"].clone(),
            "voided": p["voided"],
            "used_est": p["used_est"].as_bool().unwrap_or(false),
            "resource_id": p["log_biz_id"],
            "unit": unit,
            "expired": expired,
        });
        by_group[gi]["items"].as_array_mut().unwrap().push(item.clone());
        if !expired {
            by_group[gi]["left"] = json!(f(&by_group[gi], "left") + p["left"].as_f64().unwrap_or(0.0));
            by_group[gi]["total"] = json!(f(&by_group[gi], "total") + p["total"].as_f64().unwrap_or(0.0));
            by_group[gi]["used"] = json!(f(&by_group[gi], "used") + p["used"].as_f64().unwrap_or(0.0));
        }
        let mut q = item;
        q["category"] = json!(gname);
        portions.push(q);
    }
    // 有效期内正序（最近到期在最上），已过期倒序（最新过期在最上）
    let mut groups: Vec<Value> = Vec::new();
    for g in by_group.iter_mut() {
        let mut items = g["items"].as_array_mut().map(|a| std::mem::take(a)).unwrap_or_default();
        if items.is_empty() {
            continue;
        }
        let dead_only = items.iter().all(|i| i["expired"].as_bool().unwrap_or(false));
        items.sort_by(|a, b| {
            let ea = a["expires_on"].as_str().unwrap_or("");
            let eb = b["expires_on"].as_str().unwrap_or("");
            if dead_only { eb.cmp(ea) } else { ea.cmp(eb) }
        });
        let live_n = items.iter().filter(|i| !i["expired"].as_bool().unwrap_or(false)).count();
        g["items"] = json!(items);
        g["n"] = json!(items.len());
        g["live_count"] = json!(live_n);
        g["expired_count"] = json!(items.len() - live_n);
        groups.push(g.clone());
    }
    let left: f64 = groups.iter().map(|g| f(g, "left")).sum();
    let total: f64 = groups.iter().map(|g| f(g, "total")).sum();
    let used: f64 = groups.iter().map(|g| f(g, "used")).sum();
    json!({
        "ok": true,
        "account": me.get("nickname").or_else(|| me.get("name")).or_else(|| me.get("userName"))
            .or_else(|| me.get("email")).and_then(|v| v.as_str()).unwrap_or(""),
        "user_id": me.get("id").or_else(|| me.get("userId"))
            .map(|v| v.to_string().trim_matches('"').to_string()).unwrap_or_default(),
        "plan": product_name,
        "plan_expire": "",
        "left": left, "total": total, "used": used,
        "packs": groups.len(),
        "groups": groups,
        "portions": portions,
        "referer": referer,
        "summary": summary.clone(),
        "read_at": now.format("%Y-%m-%d %H:%M:%S").to_string(),
    })
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
                // 每一笔的身份键：官网的包 id 优先（同一档里 30 多笔全叫「平台赠送」，
                // 光靠名字根本分不出是哪一笔，改名/改到期就会改到别人头上）；
                // 没有 id 的老数据退回到「来源+总额+到期」，也够唯一。
                "pid": if it["id"].as_str().unwrap_or("").is_empty() {
                    format!("{src}|{total}|{exp}")
                } else {
                    it["id"].as_str().unwrap_or("").to_string()
                },
                // 台账的稳定身份只认真正的厂商 ID（组合 pid 含来源名，不算）
                "resource_id": it["id"].as_str().unwrap_or(""),
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
    // 已过期但官网还记着的笔：从 `/api/v1/me/usages/big_model_credits/histories` 补回来。
    //
    // 主接口（v2 big_model_credits）只回**当前有效**的资源包，一笔到期就从列表里消失；
    // 但官网用量页那栏「剩余 26 credits · 有效期至 11月17日」还留着它——数据来自这条
    // histories。以前没接它，于是"过期那一刻用了多少"永远停在最后一次读到的数
    // （用户 11-17 报的"一开始 64、后来变 25"就是这个：主接口不给，台账只能停在旧读数）。
    //
    // histories 的报文与主接口同族（plan_quota / resource_package_quota 三节），
    // 区别是它多带已经作废的笔。这里把「主接口没有、histories 有」的那些补进对应大组。
    let histories = usage.get("histories").cloned().unwrap_or_else(|| json!({}));
    let mut paired = std::collections::HashSet::new();
    if histories.get("error").is_none() {
        for key in [
            "plan_quota",
            "resource_package_quota",
            "dedicated_resource_package_quota",
        ] {
            let det = histories
                .get(key)
                .and_then(|g| g.get("quota_detail"))
                .and_then(|d| d.as_array())
                .cloned()
                .unwrap_or_default();
            for it in &det {
                let pid = it["id"].as_str().unwrap_or("").to_string();
                let src = it["source"].as_str().unwrap_or("");
                let gname = group_name(src);
                let total = f(it, "limit_value");
                if total <= 0.0 {
                    continue;
                }
                let exp_raw = f(it, "expires_at");
                let exp = ms_to_text(exp_raw);
                // 只补已经过期的：还没过期的笔主接口本来就会给，重复补会让同一笔出现两次
                if !crate::quota::expiry(&exp).0 && !it["status"].as_str().is_some_and(|v| v.eq_ignore_ascii_case("expired")) {
                    continue;
                }
                let gi = ORDER.iter().position(|n| *n == gname).unwrap_or(3);
                // 主接口里已经有这一笔（按 pid，没 pid 就按"来源+总额+到期"）就跳过
                let ident = if pid.is_empty() {
                    format!("{src}|{total}|{exp}")
                } else {
                    pid.clone()
                };
                let existing = by_group[gi]["items"].as_array().and_then(|rows| rows.iter().enumerate().position(|(index, x)| {
                    let old_id = x["resource_id"].as_str().unwrap_or("");
                    if !pid.is_empty() && !old_id.is_empty() { return pid == old_id; }
                    x["from_history"] != true && !paired.contains(&(gi, index)) && x["detail"].as_str() == Some(src)
                        && x["expires_on"].as_str() == Some(&exp) && f(x, "total") == total
                }));
                let left = f(it, "remaining_value");
                let used = {
                    let u = f(it, "used_value");
                    if u > 0.0 || total <= 0.0 { u } else { (total - left).max(0.0) }
                };
                if let Some(index) = existing {
                    paired.insert((gi, index));
                    let row = &mut by_group[gi]["items"][index];
                    row["expired"] = json!(true);
                    if !pid.is_empty() { row["resource_id"] = json!(pid); }
                    if it["remaining_value"].is_number() { row["left"] = json!(left); }
                    if it["used_value"].is_number() { row["used"] = it["used_value"].clone(); }
                    continue;
                }
                by_group[gi]["items"].as_array_mut().unwrap().push(json!({
                    "label": item_label(src),
                    "pool": gname,
                    "detail": src,
                    "pid": ident,
                    "resource_id": pid,
                    "left": left,
                    "total": total,
                    "used": used,
                    "expires_on": exp,
                    "unit": it["unit"].as_str().unwrap_or("credits"),
                    "status": it["status"].as_str().unwrap_or(""),
                    "expired": true,
                    "from_history": true,
                }));
                // 过期的笔不进组层汇总（额度已作废），只留在明细里
            }
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
    // 走界面已有的那套分组管线：quota_groups → state 现算"有效期内/已过期" → 卡片画大组。
    // 先过一遍台账：官网一旦不再返回某一笔，它就从「已过期」里彻底消失了（Qoder 的包
    // 过期即下架，平台自己也不留记录）。台账把见过的笔都留着，过期后补回来。
    let app = meta.get("app").and_then(|v| v.as_str()).unwrap_or("");
    let acct = w["account"].as_str().unwrap_or("");
    obj.insert(
        "quota_groups".into(),
        crate::quota_ledger::merge(app, acct, &w["groups"]),
    );
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
                // 只算还没过期的包："最早到期"不能是一笔早就失效的历史档，
                // 否则卡片会显示一个过去的日期。
                .filter(|p| !p["expired"].as_bool().unwrap_or(false))
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

    /// 官网取数支持哪些产品、各自用哪枚 cookie 认会话。
    ///
    /// WorkBuddy 于 2026-11-18 移除：过期历史已实测可用客户端现有凭据直读
    /// （OnlyExpiredPeriod 等参数，225 条全量），网页登录入口（oauth_callback 拼法
    /// 也不符合官方 platform/state/redirect_uri）一并删掉。这条测试钉住它不许回来。
    #[test]
    fn every_supported_site_has_its_own_cookie_and_parser() {
        assert!(site_for("qoder-cn").is_some());
        assert!(site_for("qoder").is_some());
        // Qoder 系共用一个 cookie 名（同一套后端）
        assert_eq!(site_for("qoder-cn").unwrap().session_cookie, "qoder_session_cookie");
        assert_eq!(site_for("qoder").unwrap().session_cookie, "qoder_session_cookie");
        assert!(matches!(site_for("qoder-cn").unwrap().parser, Parser::Qoder));
        // 妙手：美团 passport 的 token
        let cp = site_for("catpaw").expect("妙手应该登记了官网站点");
        assert_eq!(cp.session_cookie, "token");
        assert!(matches!(cp.parser, Parser::Catpaw));
        // WorkBuddy 走客户端凭据直读，不再登记官网站点、不再需要网页登录
        assert!(site_for("workbuddy").is_none());
        assert!(!supports("workbuddy"));
        assert!(!supports("workbuddy-intl"));
        assert!(supports("qoder-cn"));
    }

    /// 妙手会话存档：存 → 读 → 过期 → 删除，四条路径都要对。
    /// 目录必须能注入（用临时目录测），否则单测会往用户的真实档案目录写文件。
    #[test]
    fn catpaw_session_save_read_expire_drop() {
        let root = std::env::temp_dir().join(format!(
            "webquota-catpaw-session-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&root);

        // 刚存的能读回来（cookie 只去首尾空白，不干扰登录态本身）
        let cookie = "xhsid=abc; token=def; uid=123";
        catpaw_session_save(&root, "catpaw", "slot-1", cookie);
        assert_eq!(
            catpaw_session_saved(&root, "catpaw", "slot-1").as_deref(),
            Some(cookie)
        );
        assert!(catpaw_session_file(&root, "catpaw", "slot-1").is_file());

        // 8 天前的存档算作没有（会话 cookie 默认 7 天过期）
        let stale = root.join("webprofile/catpaw/slot-1/session-cookie.txt");
        let stale_body = format!(
            "{}\n{}\n",
            chrono::Local::now()
                .naive_local()
                .checked_sub_signed(chrono::Duration::days(8))
                .unwrap()
                .format("%Y-%m-%d %H:%M:%S"),
            cookie
        );
        std::fs::write(&stale, stale_body).unwrap();
        assert!(catpaw_session_saved(&root, "catpaw", "slot-1").is_none());

        // 删除后读不到，文件也没了
        catpaw_session_drop(&root, "catpaw", "slot-1");
        assert!(catpaw_session_saved(&root, "catpaw", "slot-1").is_none());
        assert!(!stale.is_file());

        let _ = std::fs::remove_dir_all(&root);
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
        let a = json!({ "quota_user_id": "00000000-0000-4000-8000-000000000003" });
        let b = json!({ "quota_user_id": "0aaa0beb-0000-4000-8000-0000000000aa" });
        assert_ne!(slot_of(&a), slot_of(&b));
        assert_eq!(slot_of(&a), "00000000-0000-4000-8000-000000000003");
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
        let m = json!({ "quota_web_slot": "账号2", "quota_user_id": "demo-user-id" });
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
          "user_id": "00000000-0000-4000-8000-000000000003",
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
        let me = json!({"id":"00000000-0000-4000-8000-000000000003","name":"demo_account_intl"});
        let plan = json!({"user_id":"00000000-0000-4000-8000-000000000003",
                          "plan_tier_name":"PLAN_TIER_FREE","end_date":253402214400000.0});
        let w = parse_usage(&usage, &me, &plan, "https://qoder.com.cn/account/usage");
        assert_eq!(w["account"], "demo_account_intl");
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
        // 每一笔都要带自己的身份键：Qoder 那 30 多笔全叫「平台赠送」，编辑改名/改到期
        // 只能靠这个键认人，靠名字会改到别人头上
        // 组内是按到期日升序排的（b2 比 b1 早到期，所以在前），这里只比集合不比顺序
        let mut ids: Vec<&str> = g[1]["items"].as_array().unwrap().iter()
            .map(|x| x["pid"].as_str().unwrap_or("")).collect();
        ids.sort_unstable();
        assert_eq!(ids, vec!["b1", "b2"], "官网给的包 id 要原样带上来：{ids:?}");
        assert_eq!(g[0]["items"][0]["pid"], "p1");
        assert_eq!(g[2]["items"][0]["pid"], "b3");
        assert_eq!(w["portions"][1]["category"], "平台奖励积分");
        let m = apply(&json!({ "name": "demo_account_intl", "app": "qoder-cn" }), &w);
        assert_eq!(m["quota_user_id"], "00000000-0000-4000-8000-000000000003");
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
          "user_id": "0aaa0beb-0000-4000-8000-0000000000aa",
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
        let w = parse_usage(&usage, &json!({"name": "demo_account"}), &plan, "");
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

    #[test]
    fn qoder_history_corrects_usage_and_keeps_twins() {
        let row = json!({"id":"a","source":"BONUS","limit_value":100,"used_value":10,
            "remaining_value":90,"expires_at":1000000000000_i64});
        let mut history = row.clone();
        history["used_value"] = json!(20);
        history["remaining_value"] = json!(80);
        let mut twin = history.clone();
        twin["id"] = json!("b");
        let out = parse_usage(&json!({"resource_package_quota":{"quota_detail":[row]},
            "histories":{"resource_package_quota":{"quota_detail":[history,twin]}}}), &json!({}), &json!({}), "");
        let items = out["groups"][0]["items"].as_array().unwrap();
        assert_eq!(items.len(), 2);
        assert!(items.iter().all(|i| i["expired"] == true && i["used"].as_f64() == Some(20.0)));
    }

    /// 官网抓包（2026-11-18）：妙手流水接口的时间参数是**毫秒时间戳**，
    /// 例如 `startTime=1794412800000&endTime=1795017599999&logTab=2`。
    /// 之前用 `YYYY-MM-DD` 日期字符串，接口一直回业务码 1500。这条钉住 URL 形状。
    #[test]
    fn catpaw_transactions_url_uses_millisecond_epoch_matching_official_packets() {
        let url = catpaw_transactions_url("https://credit.catpaw.meituan.com/api", 1, 1794412800000, 1795017599999, 2);
        assert!(
            url.contains("startTime=1794412800000")
                && url.contains("endTime=1795017599999")
                && url.contains("logTab=2")
                && url.contains("pageSize=20"),
            "官网按 pageSize=20 分页，抓包 URL：{url}"
        );
        assert!(
            !url.contains("startTime=2026-"),
            "日期字符串会触发业务码 1500，不能回退：{url}"
        );
    }

    /// 官网表格的「生效时间」列读 `extJson.effectiveTime`，「失效时间」读 `extJson.expireTime`。
    fn sample_catpaw_grant() -> Value {
        json!({
            "logBizId": "biz-1",
            "realCredit": 1000,
            "ctime": 1788305243000_i64,
            "logType": 91,
            "logTypeDesc": "积分管理后台发放",
            "extJson": "{\"expireTime\":1794095999999,\"effectiveTime\":1788305243000}"
        })
    }

    #[test]
    fn catpaw_grant_reads_effective_and_expiry_from_extjson() {
        let it = catpaw_grant(&sample_catpaw_grant()).expect("1000 积分到账应转成一笔额度");
        assert_eq!(it["log_biz_id"], "biz-1");
        assert_eq!(it["total"], 1000.0);
        assert_eq!(it["expires_on"], "2026-11-08 07:59:59");
        assert_eq!(it["granted_on"], "2026-09-02 07:27:23");
        assert_eq!(it["granted_src"], "vendor");
    }

    /// 模拟官网服务端：只有「注册奖励那一段」有记录，其余区间回 1001。
    /// 用来盯住窗口推进的两种情形：
    /// ① 最近的窗口没有记录 → 不能整轮中止，要继续往更早走；
    /// ② 更早的窗口里有记录 → 必须读到，一笔都不许丢。
    struct FakeCatpaw {
        /// 有记录的那段区间（含）的起止毫秒
        hit_start_ms: i64,
        hit_end_ms: i64,
        hits: Vec<Value>,
        calls: std::cell::RefCell<Vec<(i64, i64)>>,
    }

    impl CatpawFetch for FakeCatpaw {
        fn fetch(&self, _tab: u8, _page: i64, start_ms: i64, end_ms: i64) -> Result<(Vec<Value>, i64), String> {
            self.calls.borrow_mut().push((start_ms, end_ms));
            if start_ms <= self.hit_start_ms && end_ms >= self.hit_end_ms {
                Ok((self.hits.clone(), self.hits.len() as i64))
            } else {
                Err("流水接口返回 code 1001".to_string())
            }
        }
    }

    #[test]
    fn catpaw_pages_reads_older_records_despite_empty_recent_windows() {
        // 「注册奖励」那一笔落在 2026-07-28 前后
        let hit_start = chrono::Local
            .with_ymd_and_hms(2026, 7, 27, 0, 0, 0)
            .single()
            .unwrap()
            .timestamp_millis();
        let hit_end = chrono::Local
            .with_ymd_and_hms(2026, 7, 29, 23, 59, 59)
            .single()
            .unwrap()
            .timestamp_millis();
        let fake = FakeCatpaw {
            hit_start_ms: hit_start,
            hit_end_ms: hit_end,
            hits: vec![json!({"logBizId": "biz-old", "realCredit": 1000, "logType": 33})],
            calls: std::cell::RefCell::new(Vec::new()),
        };
        let (rows, notes) = catpaw_pages(&fake, 2);
        assert_eq!(rows.len(), 1, "更早那段有记录就必须读到：notes={notes:?}");
        assert_eq!(rows[0]["logBizId"], "biz-old");
        assert!(notes.is_empty(), "不该报错：{notes:?}");
        assert!(
            fake.calls.borrow().len() >= 2,
            "至少要试过最近的窗口和更早的窗口，实际 {} 次",
            fake.calls.borrow().len()
        );
    }

    #[test]
    fn catpaw_pages_stops_when_a_range_is_entirely_empty() {
        // 全区间都没有记录 → 不许无限循环，也不许报错刷屏
        let fake = FakeCatpaw {
            hit_start_ms: i64::MAX,
            hit_end_ms: i64::MAX,
            hits: Vec::new(),
            calls: std::cell::RefCell::new(Vec::new()),
        };
        let (rows, notes) = catpaw_pages(&fake, 2);
        assert!(rows.is_empty());
        assert!(notes.is_empty(), "整段没记录不算错误：{notes:?}");
        assert!(fake.calls.borrow().len() <= 80, "窗口数要有上限，实际 {}", fake.calls.borrow().len());
    }

    /// 官网 logType 名称表（站点脚本 `kS` 原文）与「赠送/购买」判定：
    /// logType 2（购买专业版）与 20（购买加量包）算购买，其余算赠送。
    #[test]
    fn catpaw_log_type_names_match_official_table() {
        assert_eq!(catpaw_log_type_name(9), Some("赠送基础积分"));
        assert_eq!(catpaw_log_type_name(33), Some("新用户注册奖励"));
        assert_eq!(catpaw_log_type_name(91), Some("积分管理后台发放"));
        assert_eq!(catpaw_log_type_name(20), Some("购买加量包"));
        assert_eq!(catpaw_log_type_name(2), Some("购买专业版"));
        assert_eq!(catpaw_log_type_name(999), None);
        let row = json!({"logType": 9, "logTypeDesc": "赠送"});
        assert_eq!(catpaw_label(&row), "赠送基础积分");
        let unknown = json!({"logType": 999, "logTypeDesc": "别的说法"});
        assert_eq!(catpaw_label(&unknown), "别的说法");
    }

    /// 官网「积分到账」页两列：生效时间 = 流水 ctime，失效时间 = extJson.expireTime。
    /// 官网「失效积分」页反过来：生效时间 = extJson.effectiveTime，失效时间 = 流水 ctime。
    #[test]
    fn catpaw_time_columns_follow_official_table() {
        let it = catpaw_grant(&sample_catpaw_grant()).expect("到账流水要转成一笔额度");
        assert_eq!(it["granted_on"], "2026-09-02 07:27:23", "到账页生效时间读 ctime");
        assert_eq!(it["expires_on"], "2026-11-08 07:59:59", "到账页失效时间读 extJson.expireTime");
        assert_eq!(it["log_type"], 91);
    }

    /// 2026-11-18 真机报文：额度字段全是**字符串**。
    /// 之前 `as_f64()` 取不到字符串，余额读成 0、到账笔全被丢弃，
    /// 只剩失效页签的记录被当成积分包，界面就显示"剩 0 / 共 0 · 3 笔全部过期"。
    #[test]
    fn catpaw_real_string_payload_reads_balance_and_grant() {
        // balance 原文
        let bal = json!({
            "code": 0,
            "data": {"availableCredits": "2170.50", "totalCredits": "2170.50"}
        });
        assert_eq!(
            bal["data"]["availableCredits"].as_str().and_then(|s| s.parse::<f64>().ok()),
            Some(2170.5)
        );
        assert_eq!(num_of(&bal["data"]["availableCredits"]), Some(2170.5));
        assert_eq!(num_of(&json!(2170.5)), Some(2170.5));
        assert_eq!(num_of(&json!("")), None);

        // 到账流水原文（2026-11-14 积分管理后台发放）
        let g = json!({
            "logBizId": "admin_grant_4138865630_1794625083976",
            "logType": 91,
            "realCredit": "1000.00",
            "ctime": 1794625084000_i64,
            "invId": 1344220_i64,
            "extJson": "{\"expireTime\":\"1810223999000\"}"
        });
        let pack = catpaw_grant(&g).expect("字符串金额必须能读成一笔到账");
        assert_eq!(pack["total"], 1000.0);
        assert_eq!(pack["granted_on"], "2026-11-14 10:58:04");
        assert_eq!(pack["expires_on"], "2027-05-13 23:59:59", "字符串毫秒要格式化成日期");
        assert_eq!(pack["inv_id"], 1344220);
    }

    /// 失效流水靠 invId 认回原发放（logBizId 永远对不上）。
    /// 失效金额是负数，作废额必须按绝对值展示为 162.18，与官网「失效积分」页一致。
    #[test]
    fn catpaw_expired_pairs_by_inv_id_and_uses_absolute_amount() {
        let grant = json!({
            "logBizId": "F2026104138865630",
            "logType": 9,
            "realCredit": "200.00",
            "ctime": 1790812829000_i64,
            "invId": 1250371_i64,
            "extJson": "{\"expireTime\":\"1793462399000\"}"
        });
        let expired = json!({
            "logBizId": "expire_inv_1250371",
            "logType": 8,
            "realCredit": "-162.18",
            "ctime": 1793462947000_i64,
            "invId": 1250371_i64,
            "extJson": "{\"invType\":\"9\",\"initialCredit\":\"20000\",\"effectiveTime\":\"1790784000000\"}"
        });
        let mut packs = vec![catpaw_grant(&grant).unwrap()];
        catpaw_apply_expired(&mut packs, &[expired]);
        assert_eq!(packs.len(), 1, "失效记录必须并回原包，不能再多出一个'积分包'");
        assert_eq!(packs[0]["expired"], true);
        assert_eq!(packs[0]["voided"], 162.18);
        assert_eq!(packs[0]["total"], 200.0);
    }

    /// 万一原发放记录不在可查范围内：单列一条，但原发放额必须从
    /// extJson.initialCredit（单位 0.01）还原，不能拿失效扣除额冒充。
    #[test]
    fn catpaw_unmatched_expired_uses_initial_credit() {
        let expired = json!({
            "logBizId": "expire_inv_999",
            "logType": 8,
            "realCredit": "-162.18",
            "ctime": 1793462947000_i64,
            "invId": 999_i64,
            "extJson": "{\"invType\":\"9\",\"initialCredit\":\"20000\",\"effectiveTime\":\"1790784000000\"}"
        });
        let mut packs: Vec<Value> = Vec::new();
        catpaw_apply_expired(&mut packs, &[expired]);
        assert_eq!(packs.len(), 1);
        assert_eq!(packs[0]["total"], 200.0);
        assert_eq!(packs[0]["voided"], 162.18);
        assert_eq!(packs[0]["label"], "赠送基础积分");
        assert_eq!(packs[0]["granted_on"], "2026-10-01 00:00:00");
        assert_eq!(packs[0]["expires_on"], "2026-11-01 00:09:07");
    }

    /// 消耗流水是负数（"-229.50"），按天聚合后「已用」应是正数。
    #[test]
    fn catpaw_trend_reads_negative_consumptions_as_positive_spend() {
        let consumes = vec![
            json!({"realCredit": "-229.50", "ctime": 1785518022000_i64}),
            json!({"realCredit": "-37.82", "ctime": 1793462947000_i64}),
            json!({"realCredit": "0.00", "ctime": 1793462947000_i64}),
        ];
        let h = catpaw_trend(&consumes);
        assert_eq!(h["unit"], "积分");
        let total = h["total"].as_f64().unwrap();
        assert!((total - 267.32).abs() < 0.001, "消耗合计应为 267.32，实际 {total}");
        assert_eq!(h["items"].as_array().unwrap().len(), 2, "零消耗当天不占一天");
        let used: f64 = consumes.iter().map(|c| f(c, "realCredit").abs()).sum();
        assert!((used - 267.32).abs() < 0.001);
        // 空流水必须明确说没读到，不能伪造一条 0 的图
        assert!(catpaw_trend(&[]).get("error").is_some());
    }

    /// 每月 200 的「赠送基础积分」(logType 9) 属于套餐基础积分，
    /// 不能因为标签里没有"套餐"二字就永远落进平台奖励积分。
    #[test]
    fn catpaw_monthly_bonus_lands_in_plan_group() {
        let packs = vec![
            json!({"label": "赠送基础积分", "total": 200.0, "used": null, "left": null,
                   "expires_on": "2026-11-30 23:59:59", "granted_on": "2026-11-01 00:00:30",
                   "expired": false, "log_type": 9, "log_biz_id": "F2026114138865630"}),
            json!({"label": "积分管理后台发放", "total": 1000.0, "used": null, "left": null,
                   "expires_on": "2027-05-13 23:59:59", "granted_on": "2026-11-14 10:58:04",
                   "expired": false, "log_type": 91, "log_biz_id": "admin"}),
        ];
        let out = packs_to_shape(&packs, &json!({}), &json!({}), "https://x", "妙手", "积分");
        let groups = out["groups"].as_array().unwrap();
        let plan = groups.iter().find(|g| g["name"] == "套餐基础积分").expect("200 应进套餐基础积分");
        assert_eq!(plan["items"].as_array().unwrap().len(), 1);
        assert_eq!(plan["items"][0]["label"], "赠送基础积分");
        let reward = groups.iter().find(|g| g["name"] == "平台奖励积分").expect("1000 应进平台奖励积分");
        assert_eq!(reward["items"].as_array().unwrap().len(), 1);
    }

    /// 用户 2026-11-18 核对过的摊派口径：
    /// 第 1 个月 200 用完、第 2/3 个月没用、第 4 个月用了 37.82、
    /// 注册 1000 用掉 29.5（7 月消费 229.50 超出第一个月 200 的部分）。
    #[test]
    fn catpaw_allocates_consumption_to_earliest_expiring_valid_pack() {
        let pack = |biz: &str, total: f64, granted: &str, expires: &str| {
            json!({"log_biz_id": biz, "total": total, "granted_on": granted, "expires_on": expires})
        };
        let packs = vec![
            pack("jul", 200.0, "2026-07-28 00:49:27", "2026-07-31 23:59:59"),
            pack("aug", 200.0, "2026-08-01 00:06:02", "2026-08-31 23:59:59"),
            pack("sep", 200.0, "2026-09-01 00:00:32", "2026-09-30 23:59:59"),
            pack("oct", 200.0, "2026-10-01 00:00:29", "2026-10-31 23:59:59"),
            pack("nov", 200.0, "2026-11-01 00:00:30", "2026-11-30 23:59:59"),
            pack("register", 1000.0, "2026-07-28 00:49:27", "2027-07-28 23:59:59"),
            pack("admin", 1000.0, "2026-11-14 10:58:04", "2027-05-13 23:59:59"),
        ];
        let consumes = vec![
            json!({"realCredit": "-229.50", "ctime": 1785384000000_i64}),
            json!({"realCredit": "-37.82", "ctime": 1792036800000_i64}),
            json!({"realCredit": "0.00", "ctime": 1794283200000_i64}),
        ];
        let used = catpaw_allocate_consumption(&packs, &consumes);
        assert_eq!(used.get("jul"), Some(&200.0), "第一个月 200 用完");
        assert_eq!(used.get("register"), Some(&29.5), "超出第一个月的 29.5 由注册奖励承担");
        assert_eq!(used.get("oct"), Some(&37.82), "第四个月用掉 200−162.18");
        for k in ["aug", "sep", "nov", "admin"] {
            assert_eq!(used.get(k), None, "{k} 不该被扣：只有消费时刻仍在有效期且最早到期的包先扣");
        }
        let sum: f64 = used.values().sum();
        assert!((sum - 267.32).abs() < 0.001, "摊派合计要等于消耗流水 {sum}");
    }

    #[test]
    fn catpaw_rejects_empty_or_invalid_identity() {
        for id in [Value::Null, json!(""), json!(" "), json!(false), json!([]), json!(0), json!("000"), json!(-1)] {
            assert!(!catpaw_identity_valid(&json!({"userId": id})));
        }
        assert!(!catpaw_identity_valid(&json!({})));
        for id in [json!(12345), json!("12345")] {
            let me = json!({"userId": id, "userName": "测试账号"});
            assert!(catpaw_identity_valid(&me));
            let out = packs_to_shape(&[], &me, &json!({}), "", "妙手", "积分");
            assert_eq!(out["user_id"], "12345");
            assert_eq!(out["account"], "测试账号");
        }
    }

    /// 真机取数诊断（默认不跑）：用本机已保存的妙手会话直连官网，
    /// 跑完整的 balance + 三页签 + 摊派流程，核对古零账号的已知数。
    /// 手动触发：
    ///   agsync build --test-cmd "cargo test --manifest-path src-tauri/Cargo.toml live_catpaw_smoke -- --ignored --nocapture"
    #[test]
    #[ignore]
    fn live_catpaw_smoke() {
        let path = crate::registry::base_dir()
            .join("webprofile")
            .join("catpaw")
            .join("4138865630")
            .join("session-cookie.txt");
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("读不到本机会话存档 {}：{e}", path.display()));
        let hdr = text.lines().nth(1).expect("会话存档第二行是 Cookie").trim().to_string();
        let out = fetch_catpaw(
            "credit.catpaw.meituan.com",
            &hdr,
            "https://credit.catpaw.meituan.com/",
            "4138865630",
        );
        eprintln!("妙手取数：ok={} total={} left={} used={} groups={}",
            out["ok"], out["total"], out["left"], out["used"], out["groups"].as_array().map_or(0, |g| g.len()));
        assert_eq!(out["ok"], true, "取数失败：{out}");
        assert!((out["left"].as_f64().unwrap_or(0.0) - 2170.5).abs() < 0.01);
        assert!((out["total"].as_f64().unwrap_or(0.0) - 2200.0).abs() < 0.01);
        assert!((out["used"].as_f64().unwrap_or(0.0) - 29.5).abs() < 0.01);
        assert_eq!(out["user_id"], "4138865630");
        assert_eq!(out["account"], "古零");
        let groups = out["groups"].as_array().expect("要有分组");
        let plan = groups.iter().find(|g| g["name"] == "套餐基础积分").expect("要有套餐基础积分");
        let reward = groups.iter().find(|g| g["name"] == "平台奖励积分").expect("要有平台奖励积分");
        assert!((plan["left"].as_f64().unwrap_or(0.0) - 200.0).abs() < 0.01, "套餐档：{plan}");
        assert!((reward["left"].as_f64().unwrap_or(0.0) - 1970.5).abs() < 0.01, "平台档：{reward}");
        // 逐包：10 月包用 37.82；7 月包用完；注册奖励用 29.5
        let items: Vec<&Value> = plan["items"].as_array().unwrap().iter()
            .chain(reward["items"].as_array().unwrap().iter()).collect();
        let oct = items.iter().find(|i| i["label"] == "赠送基础积分" && i["total"] == 200.0
            && i["expires_on"].as_str().unwrap_or("").starts_with("2026-10-31")).expect("找到 10 月包");
        assert!((oct["used"].as_f64().unwrap_or(0.0) - 37.82).abs() < 0.01, "10 月包：{oct}");
        let reg = items.iter().find(|i| i["label"] == "新用户注册奖励").expect("找到注册奖励");
        assert!((reg["used"].as_f64().unwrap_or(0.0) - 29.5).abs() < 0.01, "注册奖励：{reg}");
        // 每日趋势来自消耗流水
        assert!(out["heatmap"]["items"].as_array().map_or(false, |a| !a.is_empty()));
        assert!((out["heatmap"]["total"].as_f64().unwrap_or(0.0) - 267.32).abs() < 0.01);
    }

    /// 业务码在 body 里（HTTP 仍 200）：401 = 未登录，1001 = 查询区间超出
    /// 服务端允许范围。报错要带上服务端自己的文字，不能只丢一个数字。
    #[test]
    fn catpaw_body_error_reports_business_code_with_server_message() {
        assert_eq!(catpaw_body_error(&json!({"code": 0, "data": {}})), None);
        assert_eq!(catpaw_body_error(&json!({"code": 200})), None);
        let e = catpaw_body_error(&json!({"code": 401, "data": {"message": "未登录或登录失效，需要先登录"}}))
            .expect("401 要当没登录处理");
        assert!(e.contains("没登录"), "{e}");
        let e = catpaw_body_error(&json!({"code": 1001, "message": "查询时间范围超出限制"}))
            .expect("1001 要报出来");
        assert!(e.contains("1001") && e.contains("查询时间范围超出限制"), "{e}");
        // 服务端没给文字时也不能丢掉业务码本身
        let e = catpaw_body_error(&json!({"code": 1500})).expect("1500 要报出来");
        assert!(e.contains("1500"), "{e}");
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
