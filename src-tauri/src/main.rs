#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
// state.rs 那份 instances 输出是一个大 json!，额度块又新加了几个字段，
// 默认 128 层递归展开不下
#![recursion_limit = "256"]

mod ai;
mod authpack;
mod checkin;
mod commands;
mod discover;
mod gateway;
mod icons;
mod identity;
mod inject;
mod inventory;
mod keys;
mod local_quota;
mod machine;
mod procs;
mod proxy;
mod quota;
mod registry;
mod secrets;
mod settings;
mod shelllink;
mod space;
mod state;
mod storage;
mod swap;
mod usage;
mod webquota;

use std::time::Duration;

use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::Manager;
use tauri_plugin_autostart::ManagerExt;
use tauri_plugin_notification::NotificationExt;

const TRAY_ID: &str = "cockpit-tray";

/// 命令行归位：`AgentCockpit.exe --rehome <产品id> <账号名> [--force]`。
/// release 产物是没有控制台的 GUI 程序，所以报告除了往 stdout 写一份，
/// 还追加到数据根的 rehome.log，跑完用退出码 0/1 表成败。
fn cli_rehome() -> Option<i32> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let at = args.iter().position(|a| a == "--rehome")?;
    let rest = &args[at + 1..];
    if rest.len() < 2 {
        report("用法：--rehome <产品id> <账号名> [--force]".into());
        return Some(2);
    }
    storage::ensure_first_run();
    secrets::load();
    let force = rest.iter().any(|a| a == "--force");
    let (ok, msg) = swap::rehome(&rest[0], &rest[1], force);
    report(format!("{}: {}", if ok { "OK" } else { "FAIL" }, msg));
    Some(if ok { 0 } else { 1 })
}

fn report(line: String) {
    println!("{line}");
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(registry::base_dir().join("rehome.log"))
    {
        let _ = writeln!(f, "{line}");
    }
}

fn main() {
    if let Some(code) = cli_rehome() {
        std::process::exit(code);
    }
    tauri::Builder::default()
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            Some(vec!["--minimized"]),
        ))
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_single_instance::init(|app, argv, _cwd| {
            // 已在运行：把窗口唤到前台；开机自启的静默实例不打扰
            if argv.iter().any(|a| a == "--minimized") {
                return;
            }
            show_main(app);
        }))
        .manage(commands::AppState {
            proxy: proxy::Proxy::shared(),
        })
        .setup(|app| {
            // 官网额度要开一个自己的登录窗口，得先拿到 AppHandle
            webquota::set_app(app.handle().clone());
            // 还没安家的话先按盘况挑位置（系统盘满了不该再往上堆十几 GB），再建目录
            storage::ensure_first_run();
            registry::ensure_dirs();
            // 保管箱先解锁到内存（没设主密码就是 DPAPI 直接解），再把散落的明文收进去
            secrets::load();
            commands::sweep_plaintext();
            // 开机先把「换号记录 vs 磁盘上的 junction」对一遍：两者脱节的后果不是显示错，
            // 是目标程序直接起不来（默认入口指着已删除的账号目录）。放在建托盘之前，
            // 让第一次 get_state 就能带上体检结果。
            swap::self_check();
            build_tray(app.handle())?;
            // 窗口配置为 visible:false，由这里显式决定显示还是静默驻留托盘：
            // 继承父进程 STARTUPINFO 会让某些启动方式下窗口以最小化出现
            let minimized = std::env::args().any(|a| a == "--minimized");
            if !minimized {
                if let Some(w) = app.get_webview_window("main") {
                    // 任务栏/窗口图标：不设置的话 Windows 会显示默认占位图
                    let _ = w.set_icon(tauri::include_image!("icons/128x128.png"));
                    let _ = w.show();
                    let _ = w.unminimize();
                    let _ = w.set_focus();
                }
            }
            let handle = app.handle().clone();
            std::thread::spawn(move || quota_watch_loop(handle));
            // 积分到账监测单独一条线程：30 积分那种服务端按天结算的，几十分钟一轮的
            // 自动取数根本追不上（2026-10-15 她报的正是"昨天 3:50 才到账"）
            let h2 = app.handle().clone();
            std::thread::spawn(move || checkin::arrival_watch_loop(h2));
            // 签到状态也单独一条：不能再挂在额度自动取数后面（见 checkin_loop 注释）
            let h3 = app.handle().clone();
            std::thread::spawn(move || checkin_loop(h3));
            // 便携 exe 不进开始菜单就搜不到，而搜不到就没人能进设置去点那个按钮——
            // 启动后自己在后台补一条（要起 PowerShell，不放启动路径上）
            std::thread::spawn(shelllink::ensure_start_menu);
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::get_state,
            commands::action,
            commands::get_meta,
            commands::get_key_plain,
            commands::add_app,
            commands::scan_inventory,
            commands::pick_folder,
            commands::remove_app,
            commands::get_settings,
            commands::set_settings,
            commands::set_autostart
        ])
        .run(tauri::generate_context!())
        .expect("Agent 驾驶舱启动失败");
}

fn show_main(app: &tauri::AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();
    }
}

fn build_tray(app: &tauri::AppHandle) -> tauri::Result<()> {
    let open = MenuItem::with_id(app, "open", "打开驾驶舱", true, None::<&str>)?;
    let rescan = MenuItem::with_id(app, "rescan", "重新扫描本机程序", true, None::<&str>)?;
    let refresh = MenuItem::with_id(app, "refresh_quota", "立即刷新额度", true, None::<&str>)?;
    let proxy = CheckMenuItem::with_id(app, "proxy", "本地反代", true, false, None::<&str>)?;
    let data_dir = MenuItem::with_id(app, "data_dir", "打开数据目录", true, None::<&str>)?;
    let auto = CheckMenuItem::with_id(app, "autostart", "开机自启", true, false, None::<&str>)?;
    let sep = PredefinedMenuItem::separator(app)?;
    // 换了新版 exe、或程序自己卡住时，从这里重起一次最省事：不用去任务管理器找进程，
    // 也不会把"关窗＝收进托盘"那条习惯路径变成真的退出
    let restart = MenuItem::with_id(app, "restart", "重启应用", true, None::<&str>)?;
    let sep2 = PredefinedMenuItem::separator(app)?;
    let quit = MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;
    let menu = Menu::with_items(
        app,
        &[&open, &rescan, &refresh, &proxy, &data_dir, &sep, &auto, &sep2, &restart, &quit],
    )?;

    if let Ok(enabled) = app.autolaunch().is_enabled() {
        let _ = auto.set_checked(enabled);
    }
    if let Some(state) = app.try_state::<commands::AppState>() {
        let _ = proxy.set_checked(state.proxy.is_running());
    }

    let mut builder = TrayIconBuilder::with_id(TRAY_ID)
        .menu(&menu)
        .tooltip("Agent 驾驶舱 · 后台监控额度中\n左键单击打开窗口，右键打开菜单")
        .show_menu_on_left_click(false);
    if let Some(icon) = app.default_window_icon() {
        builder = builder.icon(icon.clone());
    }

    // 左键单击 = 打开主窗口；右键仍弹菜单
    let handle = app.clone();
    builder = builder.on_tray_icon_event(move |_tray, event| {
        if let tauri::tray::TrayIconEvent::Click {
            button: tauri::tray::MouseButton::Left,
            button_state: tauri::tray::MouseButtonState::Up,
            ..
        } = event
        {
            show_main(&handle);
        }
    });

    let auto_item = auto.clone();
    let proxy_item = proxy.clone();
    builder = builder.on_menu_event(move |app: &tauri::AppHandle, event| {
        match event.id().as_ref() {
            "open" => show_main(app),
            "rescan" => {
                std::thread::spawn(commands::do_rescan);
            }
            "refresh_quota" => {
                std::thread::spawn(autofetch_once);
            }
            "proxy" => {
                let state = app.state::<commands::AppState>();
                let p = state.proxy.clone();
                if p.is_running() {
                    let _ = p.stop();
                    let _ = proxy_item.set_checked(false);
                } else {
                    let port = settings::get()["proxy"]["port"]
                        .as_u64()
                        .unwrap_or(registry::PROXY_PORT as u64) as u16;
                    let (ok, _) = p.start(port);
                    let _ = proxy_item.set_checked(ok);
                }
            }
            "data_dir" => {
                let base = registry::base_dir();
                let _ = space::open_dir(base.to_string_lossy().as_ref());
            }
            "autostart" => {
                let plugin = app.autolaunch();
                let enabled = plugin.is_enabled().unwrap_or(false);
                let res = if enabled {
                    plugin.disable()
                } else {
                    plugin.enable()
                };
                if res.is_ok() {
                    let _ = auto_item.set_checked(!enabled);
                }
            }
            // 重启＝把当前 exe 原样再拉一遍：换过新版 exe、或者后台线程卡住时，
            // 这一下比"去任务管理器结束进程再双击"清楚得多
            "restart" => app.restart(),
            "quit" => app.exit(0),
            _ => {}
        }
    });
    builder.build(app)?;
    Ok(())
}

/// 后台循环：启动先扫一遍本机新装程序，再提醒临期额度，之后按设置里的间隔抓取 + 复查
fn quota_watch_loop(app: tauri::AppHandle) {
    if settings::get()["general"]["auto_scan"].as_bool().unwrap_or(true) {
        commands::do_rescan();
    }
    std::thread::sleep(Duration::from_secs(20));
    loop {
        let st = settings::get();
        autofetch_once();
        if settings::notify_alerts(&st) {
            notify_alerts(&app);
        }
        let mins = settings::auto_fetch_min(&st);
        // 分段睡眠，让改完间隔后下一轮就生效
        for _ in 0..mins * 4 {
            std::thread::sleep(Duration::from_secs(15));
        }
    }
}

/// 签到状态单独一条循环：**不再挂在额度自动取数后面**。
/// 额度那一轮里只要有一张卡走官网、会话又读不到，光那几次重试就能拖几分钟，
/// 签到状态跟着一起等——表现就是"进签到页要等半天才出状态"（2026-10-15 她报的第 4 条）。
/// 自己一条线程：开机 6 秒先查一次（界面正好起来），之后每 5 分钟一轮，
/// 与 `checkin::stale` 的 300 秒 TTL 对齐。自动领取与到点提醒仍由 `checkin::tick` 把关
/// （它自己看总开关、时间窗、"今天跑过没有"），所以这里只是让它有机会被更及时地调到。
fn checkin_loop(app: tauri::AppHandle) {
    std::thread::sleep(Duration::from_secs(6));
    loop {
        let _ = checkin::refresh();
        checkin::tick(&app);
        for _ in 0..20 {
            std::thread::sleep(Duration::from_secs(15));
        }
    }
}

fn autofetch_once() {
    for app_def in registry::all_apps() {
        // 能本机直读的产品优先走本机：不联网、拿到的就是客户端里那个真数
        if let Some(store) = local_quota::store_for(&app_def.id) {
            for meta in registry::load_instances(&app_def.id) {
                // 只有"累计已用"的产品不进后台自动刷：它是全机共享的，自动写会把
                // 同一个累计数灌进每个账号档案，多账号用户会被误导。要点就点。
                let data_dir = meta["data_dir"].as_str().unwrap_or("");
                let bound = meta["quota_user_id"].as_str().unwrap_or("").trim().to_string();
                // 没绑定过的档案不自动写：否则本机当前登录的那个号会被灌进所有空档案，
                // 看起来像"每个账号余额都一样"。首次绑定必须由用户手动点「从本机读取」。
                if bound.is_empty() {
                    continue;
                }
                // 这张卡的明细本来是官网逐包读进来的，后台就继续走官网：本机那条接口对
                // Qoder 只给「主额度 + 附加包」两个汇总数，每半小时一轮就会把二十几笔逐包
                // 压回一行"到期时间未给出"。守卫挡得住覆盖，挡不住"明细从此再也不更新"。
                // 官网会话掉了才退回本机读数刷汇总数，并把原因记下来让界面提示重新登录。
                if meta["quota_detail_source"].as_str() == Some("web")
                    && webquota::supports(&app_def.id)
                {
                    // 起线程再 join：官网取数要建隐藏窗口、调三次接口，万一在这儿 panic，
                    // 直接跑在循环线程上会把整轮自动取数和临期提醒一起带走
                    let id = app_def.id.clone();
                    let mm = meta.clone();
                    let w = std::thread::spawn(move || webquota::fetch(&id, &mm))
                        .join()
                        .unwrap_or_else(|_| serde_json::json!({ "ok": false, "msg": "官网取数崩了一次" }));
                    if w["ok"].as_bool() == Some(true) {
                        // 防串号同一条规矩：这个号的会话里登成了别人，就不能写进这张卡。
                        // 光跳过不行——卡片会一声不响地停在旧数上，得把原因记下来让它说得出话
                        if w["user_id"].as_str().unwrap_or("") == bound {
                            registry::save_instance(&app_def.id, &webquota::apply(&meta, &w));
                            continue;
                        }
                        let mut m = meta.clone();
                        m["quota_web_error"] = serde_json::json!(format!(
                            "这个号自己的官网窗口里登的是「{}」，不是卡上绑的号",
                            w["account"].as_str().unwrap_or("?")
                        ));
                        registry::save_instance(&app_def.id, &m);
                    } else {
                        let mut m = meta.clone();
                        m["quota_web_error"] = serde_json::json!(
                            w["msg"].as_str().unwrap_or("官网读不到")
                        );
                        registry::save_instance(&app_def.id, &m);
                    }
                }
                // 实例目录里没有它自己的登录数据时读本机当前登录（Qoder 的登录态在 %APPDATA%），
                // 能不能写给这张卡由下面的 userId 校验把关——对不上号就 continue，不会串号
                let q = match local_quota::fetch_for_account(store, data_dir) {
                    Ok(q) => q,
                    Err(_) => continue,   // 这台机器上一个号都没登过 → 不写
                };
                if q.is_usage_only() {
                    break;
                }
                // 档案里登录的不是绑定的那个号：跳过，绝不把 A 号的余额记到 B 号头上
                if bound != q.user_id {
                    continue;
                }
                registry::save_instance(&app_def.id, &local_quota::apply(&meta, &q));
            }
            continue;
        }
        for meta in registry::load_instances(&app_def.id) {
            let mut m = meta.clone();
            let mut changed = false;
            let has_source = !meta["quota_adapter"].as_str().unwrap_or("").trim().is_empty()
                || !meta["quota_url"].as_str().unwrap_or("").trim().is_empty();
            if has_source {
                let (left, total, e) = quota::fetch_quota(&meta);
                if e.is_none() {
                    m["credits_left"] = serde_json::json!(left);
                    if total.is_some() {
                        m["credits_total"] = serde_json::json!(total);
                    }
                    m["quota_fetched_at"] = serde_json::json!(now_ts());
                    changed = true;
                }
            }
            // CLI 工具的本地会话用量（Codex 等）：不依赖官方配额接口
            if app_def.id == "codex" {
                if let Some(u) = usage::compute(&app_def.id, meta["data_dir"].as_str().unwrap_or("")) {
                    m["usage"] = u;
                    m["usage_fetched_at"] = serde_json::json!(now_ts());
                    changed = true;
                }
            }
            if changed {
                registry::save_instance(&app_def.id, &m);
            }
        }
    }
    // 官网/中转站 Key 的余额也参与后台自动抓取（两级结构：站点 → 多个 key）
    for (site_id, k) in keys::all_keys() {
        let has_source = !k["quota_adapter"].as_str().unwrap_or("").trim().is_empty()
            || !k["quota_url"].as_str().unwrap_or("").trim().is_empty();
        if !has_source {
            continue;
        }
        let (left, total, e) = keys::fetch_key_quota(&k);
        if e.is_some() {
            continue;
        }
        let mut updated = k.clone();
        updated["left"] = serde_json::json!(left);
        if total.is_some() {
            updated["total"] = serde_json::json!(total);
        }
        updated["fetched_at"] = serde_json::json!(now_ts());
        keys::upsert_key(&site_id, &updated);
    }
}

fn now_ts() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

fn notify_alerts(app: &tauri::AppHandle) {
    let proxy = app.state::<commands::AppState>().proxy.clone();
    let st = state::build_state(&proxy);
    let alerts = st["alerts"].as_array().cloned().unwrap_or_default();
    if alerts.is_empty() {
        return;
    }
    let worst = &alerts[0];
    let body = format!(
        "{} 笔赠送额度临期，最早 {} 天后过期（{}/{}）",
        alerts.len(),
        worst["days"].as_i64().unwrap_or(0),
        worst["app"].as_str().unwrap_or(""),
        worst["name"].as_str().unwrap_or(""),
    );
    let _ = app
        .notification()
        .builder()
        .title("Agent 驾驶舱")
        .body(body)
        .show();
}
