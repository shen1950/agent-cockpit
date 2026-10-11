// Tauri 命令层：前端 invoke('action', {payload}) 的动作分发
use crate::inject;
use crate::keys;
use crate::secrets;
use crate::proxy::Proxy;
use crate::quota::fetch_quota;
use crate::registry::{self as reg, AppDef};
use crate::space;
use crate::state;
use crate::{procs, swap};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub struct AppState {
    pub proxy: Arc<Proxy>,
}

/// 账本的读-改-写必须整段串行。以前每条命令都在主线程上跑，天然排队；
/// 把带网络请求的 action 挪到工作线程之后，不自己上一把锁就会出现
/// "一个动作还拿着旧账本改，另一个已经把新的写回去了"——丢的是换号记录和账号目录指向。
/// **只有命令入口能拿这把锁**：run_action 内部 swap/state/gateway 那些写入跟它在
/// 同一个调用栈上，再拿一次就是自死锁。
static CMD_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn cmd_guard() -> std::sync::MutexGuard<'static, ()> {
    CMD_LOCK.lock().unwrap_or_else(|p| p.into_inner())
}

static SCAN_AT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub fn scan_stamp() -> String {
    let t = SCAN_AT.load(std::sync::atomic::Ordering::Relaxed);
    if t == 0 {
        return String::new();
    }
    chrono::DateTime::from_timestamp(t as i64, 0)
        .map(|d| d.with_timezone(&chrono::Local).format("%H:%M").to_string())
        .unwrap_or_default()
}

/// 扫一遍开始菜单/桌面，把新装的 AI 程序并入列表；手工适配过的不覆盖
pub fn do_rescan() -> usize {
    let skip = reg::manual_claimed_names();
    let list = crate::discover::discover(&skip);
    let n = list.len();
    reg::set_discovered(list);
    // 顺手抽出各程序真实图标，侧栏要用
    let pairs: Vec<(String, String)> = reg::all_apps()
        .iter()
        .filter_map(|a| reg::resolve_exe(a).map(|p| (a.id.clone(), p.to_string_lossy().into_owned())))
        .collect();
    crate::icons::refresh(&pairs);
    SCAN_AT.store(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        std::sync::atomic::Ordering::Relaxed,
    );
    // 重扫会改写各产品的档案目录判定，档案入口可能因此和换号记录对不上，再对一遍
    swap::self_check();
    n
}

fn ok(msg: impl Into<String>) -> Value {
    json!({ "ok": true, "msg": msg.into() })
}
fn err(msg: impl Into<String>) -> Value {
    json!({ "ok": false, "msg": msg.into() })
}

fn num_of(v: &Value) -> Option<f64> {
    match v {
        Value::Null => None,
        Value::Bool(_) => None,
        Value::Number(_) => v.as_f64(),
        Value::String(s) => s.trim().parse::<f64>().ok(),
        _ => None,
    }
}

/// 在两级结构里定位某个站点下的 key
fn find_key(site_id: &str, key_id: &str) -> Option<Value> {
    keys::sites()
        .iter()
        .find(|p| p["id"].as_str() == Some(site_id))
        .and_then(|p| p["keys"].as_array())
        .and_then(|ks| {
            ks.iter()
                .find(|k| k["id"].as_str() == Some(key_id))
                .cloned()
        })
}

/// 连同所属站点一起取回来：Base URL 现在挂在站点上，
/// 测活/拉模型/抓额度都得先看 key 自己有没有覆盖，没有就用站点的接入点。
fn find_pair(site_id: &str, key_id: &str) -> Option<(Value, Value)> {
    let site = keys::sites()
        .into_iter()
        .find(|p| p["id"].as_str() == Some(site_id))?;
    let key = site["keys"]
        .as_array()?
        .iter()
        .find(|k| k["id"].as_str() == Some(key_id))
        .cloned()?;
    Some((site, key))
}

#[tauri::command]
pub fn get_meta(app: String, name: String) -> Value {
    let m = match reg::find_instance(&app, &name) {
        Some(m) => m,
        None => return json!({}),
    };
    let mut out = serde_json::Map::new();
    for k in [
        "token",
        "quota_url",
        "quota_path",
        "quota_headers",
        "quota_adapter",
    ] {
        out.insert(
            k.into(),
            m.get(k).cloned().unwrap_or(Value::String(String::new())),
        );
    }
    Value::Object(out)
}

/// 手动登记一个本机程序：自动扫描认不出的（绿色版 / 没快捷方式 / 名字不含 AI 关键词）
#[tauri::command]
pub async fn add_app(exe: String, name: Option<String>) -> Value {
    let _g = cmd_guard();
    let exe = exe.trim().trim_matches('"').replace('/', "\\");
    let p = Path::new(&exe);
    if !p.is_file() {
        return json!({ "ok": false, "msg": format!("找不到这个文件：{}", exe) });
    }
    let name = name
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| p.file_stem().map(|s| s.to_string_lossy().into_owned()))
        .unwrap_or_default();
    if name.is_empty() {
        return json!({ "ok": false, "msg": "给这个程序起个名字" });
    }
    let base = p
        .file_name()
        .map(|s| s.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    if let Some(dup) = reg::all_apps_raw()
        .into_iter()
        .find(|a| reg::exe_name(a).to_lowercase() == base)
    {
        return json!({ "ok": false, "msg": format!("「{}」已经在列表里了（{}）", dup.name, dup.id) });
    }
    let def = reg::make_custom(&name, &exe);
    reg::save_custom(&def);
    json!({
        "ok": true,
        "app": def.id,
        "msg": format!(
            "已加入「{}」——{}",
            def.name,
            if def.isolatable { "识别为 Chromium 内核，可以多开隔离" } else { "没认出 Chromium 内核，只能整目录换号" }
        ),
    })
}

/// 一键盘点本机 Agent：三路查安装痕迹 + 与上次快照比对，报新装/卸载/内置路径失效
#[tauri::command]
pub fn scan_inventory() -> Value {
    crate::inventory::scan()
}

/// 选目录对话框：前端拿 {ok, path} 回填输入框
#[tauri::command]
pub fn pick_folder(current: Option<String>) -> Value {
    let (o, m) = space::pick_folder(&current.unwrap_or_default());
    if o {
        json!({ "ok": true, "path": m })
    } else {
        json!({ "ok": false, "msg": m })
    }
}

/// 移除手动添加的条目；自动扫描到的走「隐藏此程序」，两者都不碰账号目录
#[tauri::command]
pub async fn remove_app(app: String) -> Value {
    let _g = cmd_guard();
    if !app.starts_with("custom-") {
        return json!({ "ok": false, "msg": "只有手动添加的程序能移除，自动发现的请用右键「隐藏此程序」" });
    }
    if reg::remove_custom(&app) {
        json!({ "ok": true, "msg": "已从列表移除，账号目录原样保留" })
    } else {
        json!({ "ok": false, "msg": "没找到这条手动添加的记录" })
    }
}

#[tauri::command]
pub fn get_settings() -> Value {
    crate::settings::get()
}

/// 设置改动会写 config.json，跟 action 抢同一份账本 → 一起排队（async 才不会把窗口卡住）
#[tauri::command]
pub async fn set_settings(patch: Value) -> Value {
    let _g = cmd_guard();
    crate::settings::update(&patch)
}

/// 开机自启：交给系统插件写注册表，同时记进设置，保证两处状态一致
#[tauri::command]
pub async fn set_autostart(app: tauri::AppHandle, enable: bool) -> Value {
    use tauri_plugin_autostart::ManagerExt;
    let _g = cmd_guard();
    let plugin = app.autolaunch();
    let r = if enable { plugin.enable() } else { plugin.disable() };
    match r {
        Ok(_) => {
            crate::settings::update(&json!({ "general": { "autostart": enable } }));
            json!({ "ok": true, "msg": if enable { "已开启开机自启" } else { "已关闭开机自启" } })
        }
        Err(e) => json!({ "ok": false, "msg": format!("设置失败：{}", e) }),
    }
}

/// Key 原文只在复制/编辑时按需取，列表接口一律打码
#[tauri::command]
pub fn get_key_plain(site: String, id: String) -> String {
    keys::get_key_plain(&site, &id)
}

#[tauri::command]
pub fn get_state(state_ref: tauri::State<AppState>) -> Value {
    state::build_state(&state_ref.proxy)
}

/// 同步命令跑在主线程上，任何耗时实现都会把整个窗口冻住（标题栏挂「未响应」，
/// 网络请求一等就是几十秒）。带网络/磁盘等待的动作一律 async：Tauri 会把它们
/// 丢给工作线程执行，主线程只管画界面。
/// （Tauri 规定：入参带引用的 async 命令必须返回 Result，所以错误一律包在 Ok 里，
/// 业务失败还是走 `{ok:false,msg}` 那套，前端不用改）
#[tauri::command]
pub async fn action(
    payload: Value,
    state_ref: tauri::State<'_, AppState>,
) -> Result<Value, String> {
    let _g = cmd_guard();
    let act = payload["action"].as_str().unwrap_or("").to_string();
    let app_id = payload["app"].as_str().unwrap_or("").to_string();
    let name = payload["name"].as_str().unwrap_or("").to_string();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        run_action(&act, &app_id, &name, &payload, &state_ref)
    }));
    Ok(match result {
        Ok(v) => v,
        Err(_) => err("内部错误：动作执行失败，详情见控制台"),
    })
}

fn run_action(act: &str, app_id: &str, name: &str, payload: &Value, st: &AppState) -> Value {
    // 台账 / AI / 福利 / 反代 / 数据根 / 保管箱这些跟产品无关的动作先分流走，
    // 走到下面这道门的，分支里必然要用 AppDef 或实例 meta，才真的要求选中 Agent
    if let Some(v) = app_free(act, payload, st) {
        return v;
    }
    let app: AppDef = match reg::app_def(app_id) {
        Some(a) => a,
        None => return err("未选择 Agent：这一步是针对某个 Agent 的账号的，先在左边选中它"),
    };
    let meta = reg::find_instance(app_id, name);

    match act {
        "seed_sources" => {
            // 给向导「数据来源」那一步用。这里的原则是**只给认得出的信息**：
            // 光摆路径 + 体积，用户分不清哪份是自己天天在用的，等于没问。
            let mut sources = Vec::new();
            let mut default_claimed_by = String::new();
            if let Some(p) = reg::profile_dir(app_id) {
                let junction = reg::is_reparse_point(&p);
                let has = p.is_dir() && space::dir_has_data(&p);
                // 这份默认档案已经被哪张卡认领了（原地账号就是挂在它上面的）
                default_claimed_by = reg::bound_instance_name(app_id).unwrap_or_default();
                let mut facts = profile_facts(app_id, &p, Some(&default_claimed_by));
                facts["mb"] = json!(if has { space::human_mb(space::dir_size(&p)) } else { 0 });
                let note = if junction {
                    "现在被换号接管着（它是个 junction）——先「还原默认」才能接管".to_string()
                } else if !p.is_dir() {
                    "这个路径还不存在，接管不了".to_string()
                } else if !has {
                    "里面是空的，没有可带过去的对话与记忆".to_string()
                } else if !default_claimed_by.is_empty() {
                    format!("已经被账号「{}」认领了", default_claimed_by)
                } else {
                    "程序现在打开的就是这一份".to_string()
                };
                sources.push(json!({
                    "origin": "adopt", "kind": "profile",
                    "title": "你一直在用的那份",
                    "label": "本机的原有档案",
                    "path": p.to_string_lossy(),
                    "junction": junction, "has_data": has,
                    "facts": facts,
                    "note": note,
                }));
            }
            for m in reg::load_instances(app_id) {
                let p = PathBuf::from(m["data_dir"].as_str().unwrap_or(""));
                let has = p.is_dir() && space::dir_has_data(&p);
                let nm = m["name"].as_str().unwrap_or("?").to_string();
                let mut facts = profile_facts(app_id, &p, Some(&nm));
                // 卡片上记过的账号名比现读更可靠（这张卡可能已经不是本机当前登录的那个号）
                if facts["who"].as_str().unwrap_or("").is_empty() {
                    facts["who"] = json!(m["quota_account"].as_str().unwrap_or(""));
                }
                facts["mb"] = json!(if has { space::human_mb(space::dir_size(&p)) } else { 0 });
                sources.push(json!({
                    "origin": "copy", "kind": "instance",
                    "title": format!("账号「{}」", nm),
                    "label": format!("已有账号「{}」", nm),
                    "name": nm, "path": p.to_string_lossy(),
                    "junction": false, "has_data": has,
                    "facts": facts,
                    "note": if has { "复制一份过来：对话与记忆都带过来，它本身不动" } else { "里面是空的" },
                }));
            }
            let (rec, rec_src) = resolve_seed(app_id, &app.kind, "auto", None);
            json!({
                "ok": true,
                "sources": sources,
                "recommend": rec,
                "default_claimed_by": default_claimed_by,
                "recommend_from": rec_src.map(|p| p.to_string_lossy().into_owned()).unwrap_or_default(),
            })
        }

        "new_instance" => {
            if reg::resolve_exe(&app).is_none() {
                return err(format!("未找到 {} 可执行文件", app.name));
            }
            if app.kind == "plain" {
                return err(format!(
                    "{} 不是 Chromium/Electron 程序，无法用独立数据目录多开；点「启动」可直接打开主程序",
                    app.name
                ));
            }
            let existing: Vec<String> = reg::load_instances(app_id)
                .iter()
                .map(|m| m["name"].as_str().unwrap_or("").to_string())
                .collect();
            // 向导会指定名字；没指定就自动取 账号N
            let wanted = payload["name"].as_str().unwrap_or("").trim().to_string();
            if !wanted.is_empty()
                && (wanted.contains("..")
                    || wanted.contains('/')
                    || wanted.contains('\\')
                    || wanted.contains(':'))
            {
                return err("账号名不能含路径分隔符（/ \\ : ..）");
            }
            let nm = if !wanted.is_empty() {
                if existing.contains(&wanted) {
                    return err(format!("账号「{}」已存在，换个名字", wanted));
                }
                wanted
            } else {
                let mut n = String::from("账号1");
                for i in 1..9999 {
                    let cand = format!("账号{}", i);
                    if !existing.contains(&cand) {
                        n = cand;
                        break;
                    }
                }
                n
            };
            // 数据来源：默认档案里那份"我一直在用的" > 已有账号里最近活跃的 > 空档。
            // 以前一律建空目录，于是登进去是个没有对话也没有记忆的新号。
            let origin_want = payload["origin"].as_str().unwrap_or("auto").trim().to_string();
            let from_arg = payload["from"].as_str().map(PathBuf::from);
            let (origin, src) = resolve_seed(app_id, &app.kind, &origin_want, from_arg);
            // 接管＝原地登记：账号目录就是这个默认档案目录本身，不搬进 instances/。
            // 用户天天用的档案留在它本来的位置，驾驶舱只是给它起了个账号名。
            let data_dir = instance_data_dir(
                &origin,
                src.as_ref(),
                &reg::instances_dir(app_id),
                &nm,
            );
            // 认错目录（比如把别的程序的档案认成自己的）会让这张卡"认领"一个不属于它的目录，
            // 之后换号就是拿那个目录开刀。动手前把绝对路径摊出来问一句。
            if adopt_needs_confirm(&origin, src.as_ref(), payload["confirm_adopt"].as_bool()) {
                let s = src.as_ref().cloned().unwrap_or_default();
                return json!({
                    "ok": false,
                    "need_confirm": "adopt",
                    "source": s.to_string_lossy(),
                    "target": data_dir.to_string_lossy(),
                    "mb": if s.is_dir() { space::human_mb(space::dir_size(&s)) } else { 0 },
                    "msg": "接管原有档案：这个账号直接用默认档案目录本身，不搬也不复制",
                });
            }
            let (used, note) = match space::seed_instance(&app_id, &data_dir, &origin, src.as_deref()) {
                Ok(v) => v,
                Err(e) => return err(e),
            };
            let mut meta = json!({
                "name": nm, "app": app_id,
                "workdir": payload["workdir"].as_str().unwrap_or(&dirs_doc_home()),
                "data_dir": data_dir.to_string_lossy(),
                "created": std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs_f64())
                    .unwrap_or(0.0),
                "pid": 0,
                "origin": used,
            });
            if let Some(p) = &src {
                meta["seeded_from"] = json!(p.to_string_lossy());
            }
            // 向导在建卡之前就把额度读过了（那时还没有卡片可写），建卡这一步一起落账。
            // 不这么做的话，新卡建出来是空的，得再点一次"读余额"才像话——顺序就白排了。
            for k in [
                "credits",
                "credits_left",
                "credits_total",
                "expires_on",
                "quota_user_id",
                "quota_account",
                "quota_plan",
                "quota_plan_name",
                "quota_groups",
                "quota_source",
                "quota_live_error",
                "quota_detail_source",
                "quota_detail_at",
                "quota_detail_stale",
            ] {
                if let Some(v) = payload.get(k) {
                    if !v.is_null() {
                        meta[k] = v.clone();
                    }
                }
            }
            let now_ts = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs_f64())
                .unwrap_or(0.0);
            if meta.get("quota_source").is_some() {
                meta["quota_fetched_at"] = json!(now_ts);
            }
            // 向导带进来的逐笔明细也得记上它自己的来源与时间：后台自动取数靠
            // quota_detail_source == "web" 判断"这张卡的明细要继续走官网"，
            // 漏了这一步，新卡下一轮就被本机那两个汇总数占住、明细再也不更新了。
            if meta.get("quota_groups").is_some() && meta.get("quota_detail_source").is_none() {
                meta["quota_detail_source"] = meta["quota_source"].clone();
                meta["quota_detail_at"] = json!(now_ts);
                meta["quota_detail_stale"] = json!(false);
            }
            // 单窗口换号的产品这台机器上只有一份档案：新卡认领它时，原来那张必须让位，
            // 否则两张卡抢同一个目录，"当前账号是谁"又含混了。
            // 让位只是打个标记（那张卡的历史额度还留着能看），不碰任何档案文件。
            if let Some(old) = payload["take_over"].as_str().map(str::trim).filter(|s| !s.is_empty()) {
                if old != nm {
                    if let Some(mut o) = reg::find_instance(app_id, old) {
                        o["released"] = json!(true);
                        o["released_to"] = json!(nm);
                        reg::save_instance(app_id, &o);
                    }
                }
            }
            reg::save_instance(app_id, &meta);
            return json!({
                "ok": true, "origin": used,
                "msg": format!("已新建 {}「{}」。{}。", app.name, nm, note),
            });
        }

        "launch" => {
            let meta = match meta {
                Some(m) => m,
                None => return err("实例不存在"),
            };
            let exe = match reg::resolve_exe(&app) {
                Some(e) => e,
                None => return err(format!("未找到 {} 可执行文件", app.name)),
            };
            let mut workdir = meta["workdir"].as_str().unwrap_or("").to_string();
            if workdir.is_empty() || !Path::new(&workdir).exists() {
                workdir = reg::home().to_string_lossy().into_owned();
            }
            let data_dir = Path::new(meta["data_dir"].as_str().unwrap_or(""));
            let _ = std::fs::create_dir_all(data_dir);

            // 已知不能多开、而且现在挂着的不是这个账号：点「启动」没有任何意义——
            // 独立数据目录不生效（Qoder 0.4.x 直接忽略 --user-data-dir），
            // 结果只会是等 8 秒再报"转交给了已开窗口"。当场说清该走换号。
            if app.kind == "electron"
                && !crate::state::isolatable(app_id)
                && reg::swaps_of(&reg::load_config())
                    .get(app_id)
                    .and_then(|v| v["instance"].as_str())
                    != Some(name)
            {
                return err(format!(
                    "{} 不能多开：它只认自己的默认档案目录，给子进程加独立目录不生效。\n\
                     要打开「{}」这个账号，先点「设为当前」换号（零额外空间、随时还原），再启动。",
                    app.name, name
                ));
            }

            if app.kind == "electron" {
                // 防串号：实例目录里的产品标识须与本 Agent 同族，否则官方客户端自检报错
                if app_id.starts_with("qoder") {
                    let st_file = data_dir.join(".qoder-app-status.json");
                    if let Ok(s) = std::fs::read_to_string(&st_file) {
                        if let Ok(v) = serde_json::from_str::<Value>(&s) {
                            // 官方写的是 "qoder" / "qodercn"（没有连字符），别拿 app id 直接比
                            let norm = |s: &str| s.to_lowercase().replace(['-', '_'], "");
                            let prod = v["product"].as_str().unwrap_or("").to_string();
                            let want = if app_id.starts_with("qoder-cn") { "qodercn" } else { "qoder" };
                            if !prod.is_empty() && norm(&prod) != want {
                                return err(format!(
                                    "实例目录身份是 {}，与 {} 不符，混用会触发官方自检报错，请删除该实例重建",
                                    prod, app.name
                                ));
                            }
                        }
                    }
                }
                match procs::launch_electron(&exe, data_dir, &workdir) {
                    Err(e) => err(format!("启动失败：{}", e)),
                    Ok(_) => {
                        // 关键校验：真的出现「带本实例目录」的进程才算多开成功。
                        // 很多产品（如 Qoder 系）会把第二次启动转交给已开窗口后自己退出，
                        // 参数被丢弃，此时必须如实说明并引导改用换号，而不是假报"已启动"。
                        let mut verified = false;
                        let cands = reg::account_candidates(
                            app_id,
                            &meta,
                            &reg::swaps_of(&reg::load_config()),
                        );
                        for _ in 0..14 {
                            std::thread::sleep(std::time::Duration::from_millis(600));
                            let snap = procs::snapshot_fresh();
                            if !procs::instance_pids(&app, &cands, &snap).is_empty() {
                                verified = true;
                                break;
                            }
                        }
                        if verified {
                            ok(format!("已启动 {}「{}」，请在窗口内登录该账号", app.name, name))
                        } else {
                            state::mark_not_isolatable(app_id);
                            err(format!(
                                "{} 把这次启动转交给了已开着的窗口，独立数据目录没有生效——\
                                 该产品不支持同时多开。要使用不同账号请用「设为当前」换号（一次一个账号，零额外空间）。",
                                app.name
                            ))
                        }
                    }
                }
            } else {
                match procs::launch_cli(&app, &exe, data_dir, &workdir) {
                    Ok(pid) => {
                        let mut m = meta.clone();
                        m["pid"] = json!(pid);
                        reg::save_instance(app_id, &m);
                        ok(format!(
                            "已在新终端启动 {}「{}」（{}，PID {}）",
                            app.name, name, app.env, pid
                        ))
                    }
                    Err(e) => err(format!("启动失败：{}", e)),
                }
            }
        }

        "launch_app" => {
            let exe = match reg::resolve_exe(&app) {
                Some(e) => e,
                None => return err(format!("未找到 {} 可执行文件", app.name)),
            };
            let r = if app.kind == "cli" {
                procs::launch_terminal(&exe).map(|_| ())
            } else {
                procs::launch_plain(&exe).map(|_| ())
            };
            match r {
                Ok(_) if app.kind == "plain" => ok(format!(
                    "已打开 {}（该程序不支持独立数据目录，多开无效）", app.name)),
                Ok(_) => ok(format!("已打开 {}", app.name)),
                Err(e) => err(format!("启动失败：{}", e)),
            }
        }

        // 点开看过一次 → 摘掉侧栏的「新」标记（静默动作，不弹 toast）
        "mark_seen" => {
            reg::mark_seen(app_id);
            ok("已看过")
        }

        "kill" => {
            if app.kind == "cli" {
                let pid = meta
                    .as_ref()
                    .and_then(|m| m["pid"].as_u64())
                    .unwrap_or(0) as u32;
                let snap = procs::snapshot_fresh();
                if pid != 0 && procs::alive(&snap, pid) {
                    procs::kill_pids(&[pid]);
                    return ok(format!("已结束 PID {}", pid));
                }
                return err("该 CLI 实例未在运行");
            }
            let meta = match meta {
                Some(m) => m,
                None => return err("实例不存在"),
            };
            let snap = procs::snapshot_fresh();
            let swaps = reg::swaps_of(&reg::load_config());
            let cands = reg::account_candidates(app_id, &meta, &swaps);
            let mut pids = procs::instance_pids(&app, &cands, &snap);
            // 换号账号的进程命令行里出现的是产品默认档案路径，匹配不到实例目录；
            // 这里必须与 state.rs 的"运行中"判定同一套口径，否则卡片显示运行中却点不动。
            let program_level = pids.is_empty()
                && swaps
                    .get(app_id)
                    .and_then(|s| s["instance"].as_str())
                    == Some(name);
            if program_level {
                pids = procs::program_pids(&reg::exe_name(&app).to_lowercase(), &snap);
            }
            if pids.is_empty() {
                return err("该实例没有运行中的窗口（整个程序级结束请用进程表「结束全部」）");
            }
            procs::kill_pids(&pids);
            ok(if program_level {
                format!(
                    "已结束「{}」的 {} 个进程——该程序不能多开，这是整个程序的进程树",
                    name,
                    pids.len()
                )
            } else {
                format!(
                    "已结束「{}」的 {} 个窗口进程，不影响其他实例",
                    name,
                    pids.len()
                )
            })
        }

        "delete" => {
            let meta = match meta {
                Some(m) => m,
                None => return err("实例不存在"),
            };
            let data_dir = Path::new(meta["data_dir"].as_str().unwrap_or(""));
            // 只删驾驶舱自己落盘范围内的目录：账号目录在 instances/ 下，归位或迁移之后
            // 留底会落在数据根的 backups/ 下，那种卡也得能删掉，不然成了删不掉的僵尸记录。
            // 数据根之外一律拒绝，防手改 config 后越界删盘。
            if data_dir.parent() != Some(reg::instances_dir(app_id).as_path()) {
                if reg::is_in_place(app_id, data_dir) {
                    return err(format!(
                        "「{}」是原地账号：数据就在这个程序的默认档案位置 {}，\
                         删它等于把这个程序的本机档案整个清掉。要去登录请直接在程序里操作。",
                        name,
                        data_dir.display()
                    ));
                }
                if !data_dir.starts_with(reg::base_dir()) {
                    return err("实例数据目录路径异常，已拒绝删除");
                }
            }
            // 「这个账号正被挂成默认档案」以磁盘上的 junction 为准，不看 config 的换号记录。
            // 记录一旦脱节（手删过目录、换号中途失败），照记录放行就会把默认档案删成断链：
            // Qoder 国际版就这么坏过一次——账号删了，com.qoder.app.stable 还指着它，
            // 程序报「无法创建或写入本地数据目录」，开始菜单图标也变白纸。
            let bound = reg::is_bound_to(app_id, data_dir);
            let rec = reg::swaps_of(&reg::load_config())
                .get(app_id)
                .and_then(|s| s["instance"].as_str())
                .unwrap_or("")
                .to_string();
            if bound {
                let (o, m) = swap::swap_clear(app_id);
                if !o {
                    return err(format!(
                        "这个账号现在是 {} 的默认档案，得先还原默认才能删。\n还原没做成：{}",
                        app.name, m
                    ));
                }
            } else if rec == name {
                // 记录说它是当前账号、磁盘上却没挂着它：记录是脏的，顺手纠正，
                // 否则下次换号会以为"已经是它"而不去修那个坏链接。
                let mut cfg = reg::load_config();
                reg::set_swap(&mut cfg, app_id, None);
                reg::save_config(&cfg);
            }
            // 接管（adopt）进来的账号里那份数据是**从默认档案搬过来的唯一一份**，
            // 删掉就真没了。所以默认先打一个 zip 备份再删；想跳过就显式传 backup:false。
            let want_backup = payload.get("backup").and_then(|v| v.as_bool()).unwrap_or(true);
            let mut bk_note = String::new();
            if want_backup {
                // 只有"这个账号自己正在跑"才拦：备份要读它的文件，读到写一半的会打出坏档。
                // 按整个产品判会把无关窗口算进来——你正在用 A 号聊天，就不许删从没开过的 B 号。
                let cands = reg::account_candidates(app_id, &meta, &reg::swaps_of(&reg::load_config()));
                let busy = procs::instance_pids(&app, &cands, &procs::snapshot_fresh());
                if !busy.is_empty() {
                    return err(format!(
                        "「{}」这个账号自己正开着（{} 个进程），先关掉它的窗口再删。\n\
                         其它账号开着不影响，这次删的是这个账号的档案。",
                        name,
                        busy.len()
                    ));
                }
                let (bo, bm) = space::backup(app_id, name);
                if !bo {
                    return err(format!(
                        "删除前先备份失败了：{bm}\n\
                         没删任何东西。想不要备份直接删，就再点一次「删除」并选「不备份直接删」。"
                    ));
                }
                bk_note = format!("\n（删除前已自动备份：{bm}）");
            }
            let _ = std::fs::remove_dir_all(data_dir);
            if let Some(f) = meta["_file"].as_str() {
                let _ = std::fs::remove_file(f);
            }
            // 兜底：万一上面判断被绕过（例如账号目录刚被别处改名），删完发现默认入口
            // 成了断链，就地摘掉这个链接。留着它，程序连"新建一份档案"的机会都没有。
            let healed = swap::repair_dangling(app_id);
            ok(format!(
                "已删除实例「{}」{}{}",
                name,
                bk_note,
                if healed {
                    "\n（顺带发现该程序的默认档案入口是个断链，已摘掉；下次打开会新建一份干净档案）"
                } else {
                    ""
                }
            ))
        }

        "rename_instance" => {
            let (o, m) = space::rename_instance(app_id, name, payload["to"].as_str().unwrap_or(""));
            if o { ok(m) } else { err(m) }
        }

        // 本机这份档案换登成另一个号了：把这张卡改绑到当前登录的号，而不是再建一张重复卡。
        // 账号名与 userId 一律以后端现读的为准，界面只负责说"我要改绑"。
        "rebind_instance" => {
            let _meta = match meta {
                Some(m) => m,
                None => return err("实例不存在"),
            };
            let l = crate::machine::refresh_now(app_id);
            if l["ok"].as_bool() != Some(true) {
                return err(format!(
                    "读不到本机现在登的是谁：{}",
                    l["err"].as_str().unwrap_or("未知原因")
                ));
            }
            let acct = l["account"].as_str().unwrap_or("").trim().to_string();
            let want = payload["to"].as_str().unwrap_or("").trim().to_string();
            let want = if want.is_empty() { acct.clone() } else { want };
            let mut note = String::new();
            if !want.is_empty() && want != name {
                let (o, m) = space::rename_instance(app_id, name, &want);
                note = if o {
                    format!("卡片同时改名成「{}」。", want)
                } else {
                    format!("（没改名：{}）", m)
                };
            }
            let cur = reg::find_instance(app_id, &want).or_else(|| reg::find_instance(app_id, name));
            let mut m = match cur {
                Some(x) => x.clone(),
                None => return err("改绑中途找不到这张卡了，请刷新后重试"),
            };
            m["quota_user_id"] = l["user_id"].clone();
            m["quota_account"] = json!(acct);
            m["quota_plan"] = l["user_type"].clone();
            m["quota_source"] = l["source"].clone();
            m["quota_live_error"] = l["live_error"].clone();
            m["credits_left"] = l["left"].clone();
            m["credits_total"] = l["total"].clone();
            // 同一条守卫：改绑只是把这张卡对到当前登录的号上，不该顺手把更细的逐笔明细压平。
            // 本机这份读数只有汇总数时留着原来的明细，只把"明细比汇总旧"标出来。
            let inc = crate::local_quota::detail_rank(&[], &l["portions"].as_array().cloned().unwrap_or_default());
            if crate::local_quota::stored_rank(&m) > inc {
                m["quota_detail_stale"] = json!(true);
            } else {
                m["credits"] = l["portions"].clone();
                m["quota_detail_stale"] = json!(false);
                m["quota_detail_source"] = l["source"].clone();
                m["quota_detail_at"] = l["at"].clone();
            }
            m["quota_auth_from"] = l["auth_from"].clone();
            if let Some(e) = l["expires_on"].as_str().filter(|s| !s.is_empty()) {
                m["expires_on"] = json!(e);
            }
            m["quota_fetched_at"] = json!(std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs_f64())
                .unwrap_or(0.0));
            let label = m["name"].as_str().unwrap_or("").to_string();
            reg::save_instance(app_id, &m);
            ok(format!(
                "这张卡已改绑到本机当前登录的「{}」（{}），额度按当前读数重写。{}",
                label,
                if m["quota_plan"].as_str().unwrap_or("").is_empty() {
                    "套餐未给出".to_string()
                } else {
                    format!("套餐 {}", m["quota_plan"].as_str().unwrap_or(""))
                },
                note
            ))
        }

        "backup" => {
            let (o, m) = space::backup(app_id, name);
            if o {
                ok(m)
            } else {
                err(m)
            }
        }

        "restore" => {
            let (o, m) = space::restore(app_id, name);
            if o {
                ok(m)
            } else {
                err(m)
            }
        }

        "open_dir" => {
            let meta = match meta {
                Some(m) => m,
                None => return err("实例不存在"),
            };
            let (o, m) = space::open_dir(meta["data_dir"].as_str().unwrap_or(""));
            if o {
                ok(m)
            } else {
                err(m)
            }
        }

        "save_meta" => {
            let meta = match meta {
                Some(m) => m,
                None => return err("实例不存在"),
            };
            let mut m = meta.clone();
            let obj = m.as_object_mut().unwrap();
            for key in [
                "workdir",
                "expires_on",
                "token",
                "quota_url",
                "quota_headers",
                "quota_path",
                "quota_adapter",
            ] {
                if let Some(v) = payload.get(key) {
                    obj.insert(key.into(), json!(v.as_str().unwrap_or("")));
                }
            }
            for key in ["credits_total", "credits_left"] {
                if let Some(v) = payload.get(key) {
                    obj.insert(key.into(), match num_of(v) {
                        Some(n) => json!(n),
                        None => Value::Null,
                    });
                }
            }
            // 多笔积分：截图识别 / 向导登记走这里（界面上已经没有手填入口了）。
            // 一旦显式写了逐笔，就把 agenthub 那套分组与逐笔明细清掉——留着上一次的分组，
            // 界面会拿旧分组当现值显示，新登记的那几笔反而看不见。
            if let Some(cr) = payload.get("credits") {
                let arr = cr.as_array().cloned().unwrap_or_default();
                let mut clean: Vec<Value> = Vec::new();
                for c in arr {
                    let label = c["label"].as_str().unwrap_or("").trim().to_string();
                    let left = num_of(&c["left"]);
                    if label.is_empty() && left.is_none() {
                        continue;
                    }
                    clean.push(json!({
                        "label": label,
                        "left": left,
                        "total": num_of(&c["total"]),
                        "category": c["category"].as_str().unwrap_or("").trim().to_string(),
                        "unit": c["unit"].as_str().unwrap_or("credits").trim().to_string(),
                        "expires_on": c["expires_on"].as_str().unwrap_or("").trim().to_string(),
                        "note": c["note"].as_str().unwrap_or("").trim().to_string(),
                    }));
                }
                obj.remove("quota_groups");
                if clean.is_empty() {
                    obj.remove("credits");
                } else {
                    obj.insert("credits".into(), json!(clean));
                }
            }
            reg::save_instance(app_id, &m);
            ok("已保存")
        }

        "fetch_quota" => {
            let meta = match meta {
                Some(m) => m,
                None => return err("实例不存在"),
            };
            let (left, total, e) = fetch_quota(&meta);
            if let Some(e) = e {
                return err(e);
            }
            let mut m = meta.clone();
            m["credits_left"] = json!(left);
            if total.is_some() {
                m["credits_total"] = json!(total);
            }
            m["quota_fetched_at"] = json!(std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs_f64())
                .unwrap_or(0.0));
            reg::save_instance(app_id, &m);
            match (left, total) {
                (Some(l), Some(t)) => ok(format!("已更新剩余：{} / 总额 {}", gfmt(l), gfmt(t))),
                (Some(l), None) => ok(format!("已更新剩余：{}", gfmt(l))),
                _ => ok("已更新"),
            }
        }

        "pull_local" => {
            let meta = match meta {
                Some(m) => m,
                None => return err("实例不存在"),
            };
            let store = match crate::local_quota::store_for(app_id) {
                Some(s) => s,
                // 清单从 LOCAL_STORES 现拼：写死名字表是第二份真值，加一家忘一家（这次就过期了）
                None => return err(format!(
                    "这个程序还没有本机直读通道（目前只支持：{}）",
                    crate::local_quota::locally_readable_names()
                )),
            };
            // 读这个账号自己的档案目录（换号产品每个账号的数据就在自己的 data_dir 里）。
            // Qoder 系例外：登录态存在 %APPDATA%\QoderCN，实例目录空着时按目录读只会报"没登录过"，
            // 这时回落读本机当前登录，由下面的 userId 校验决定能不能写进这张卡。
            let data_dir = meta["data_dir"].as_str().unwrap_or("").to_string();
            let from_default = crate::local_quota::falls_back_to_default(store, &data_dir);
            let q = match crate::local_quota::fetch_for_account(store, &data_dir) {
                Ok(q) => q,
                Err(e) => return err(e),
            };
            // 只统计"已用"的产品：读的是这个账号自己档案目录里的那份库（换号产品各账号各一份），
            // 记累计消耗，不动手填的剩余额度
            if q.is_usage_only() {
                let fresh = crate::local_quota::apply(&meta, &q);
                reg::save_instance(app_id, &fresh);
                return ok(format!(
                    "这个账号的档案累计已用 {} credits（{} 个会话，最近活跃 {}）。\n\
                     WorkBuddy 的余额要官网接口才能查、本地没有凭据，剩余额度仍按你手填的为准。",
                    gfmt(q.used),
                    q.sessions,
                    if q.last_active.is_empty() { "未知" } else { &q.last_active }
                ));
            }
            // 防串号：Qoder 这类单实例产品靠 junction 换号，本机登录态随时可能是别的号。
            // 第一次读把 userId 绑死在这个账号档案上，之后每次比对，不一致就不写。
            let bound = meta["quota_user_id"].as_str().unwrap_or("").trim().to_string();
            let bound_name = meta["quota_account"].as_str().unwrap_or("").to_string();
            let force = payload.get("force").and_then(|v| v.as_bool()).unwrap_or(false);
            if !bound.is_empty() && bound != q.user_id && !force {
                return json!({
                    "ok": false, "mismatch": true,
                    "msg": format!(
                        "本机 Qoder 现在登的是「{}」，而这个档案绑的是「{}」。\n\
                         换号模式下这说明当前挂着的不是这个账号——要么先去驾驶舱把它换成号，\
                         要么确认这台机器上这个号就是这个档案。",
                        q.account, if bound_name.is_empty() { &q.user_id } else { &bound_name }),
                    "found": {
                        "account": q.account, "user_id": q.user_id,
                        "left": q.left, "total": q.total, "expires_on": q.expires_on,
                    },
                });
            }
            // 一个登录态只能绑一个档案：绑到两个上，两个账号会显示同一份余额，
            // 看起来就是"串号又回来了"。首绑时先查同程序的其它档案有没有占着这个 userId。
            if bound.is_empty() && !force {
                let taken = reg::load_instances(app_id).into_iter().find(|other| {
                    other["name"].as_str().unwrap_or("") != name
                        && other["quota_user_id"].as_str().unwrap_or("") == q.user_id
                });
                if let Some(other) = taken {
                    return json!({
                        "ok": false, "already_bound": true,
                        "msg": format!(
                            "本机当前登录的「{}」已经绑在「{}」上了。
再绑到「{}」会让两个账号显示同一份余额。
                             要么先在驾驶舱把这个档案换成号再回来读，要么确认它们本来就是同一个号。",
                            q.account, other["name"].as_str().unwrap_or("?"), name),
                        "found": {
                            "account": q.account, "user_id": q.user_id,
                            "left": q.left, "total": q.total, "expires_on": q.expires_on,
                        },
                    });
                }
            }
            let first_bind = bound.is_empty();
            let fresh = crate::local_quota::apply(&meta, &q);
            reg::save_instance(app_id, &fresh);
            // 名字还是「账号N」这种自动占位符时，把读到的用户名提为账号名：
            // 多账号一眼能对上号，省掉事后手动改名
            let acct = q.account.trim().to_string();
            let placeholder = name
                .strip_prefix("账号")
                .map(|s| !s.is_empty() && s.chars().all(|c| c.is_ascii_digit()))
                .unwrap_or(false);
            let usable = !acct.is_empty()
                && acct.chars().count() <= 40
                && acct
                    .chars()
                    .all(|c| c.is_alphanumeric() || matches!(c, '.' | '_' | '-' | '@' | ' '));
            // 这张卡是不是"本机这份档案"的认领者。是的话它的名字就该跟着现在登录的号走——
            // 单窗口换号的产品只有一份档案，卡片记的就是"这份档案此刻登的是谁"，
            // 人在客户端里换了号，卡片还叫旧名字就是错的（她手动改过一次就是这个感受）
            let bound_here = reg::bound_instance_name(app_id).as_deref() == Some(name);
            let taken = reg::load_instances(app_id)
                .iter()
                .any(|o| o["name"].as_str().unwrap_or("") == acct.as_str() && acct != name);
            let mut renamed = String::new();
            let mut rename_note = String::new();
            if usable && !taken && (placeholder || bound_here) {
                let (o, m) = space::rename_instance(app_id, name, &acct);
                if o {
                    renamed = acct.clone();
                } else {
                    rename_note = format!("\n（没自动改名：{}）", m);
                }
            } else if taken && (placeholder || bound_here) {
                rename_note = format!("\n（没改名：已经有一张卡叫「{}」了）", acct);
            } else if placeholder && !acct.is_empty() && !usable {
                rename_note = "\n（用户名含不适合当目录名的字符，没自动改名）".to_string();
            }
            let mut msg = format!(
                "读到「{}」{}：剩 {} / {} {}，到期 {}",
                q.account,
                if q.user_type.is_empty() { String::new() } else { format!("（{}）", q.user_type) },
                gfmt_opt(q.left),
                gfmt_opt(q.total),
                q.unit(),
                if q.expires_on.is_empty() { "未给出" } else { &q.expires_on }
            );
            if q.exceeded {
                msg.push_str(" · 已标记超额");
            }
            if from_default {
                msg.push_str("\n来源：本机当前登录的档案（这个账号目录里还没有它自己的登录数据）");
            }
            // 服务端没读成、只剩旧缓存：必须说清"这不是实时数"，否则用户会拿 8 月的余额做判断
            let stale_hint = q.stale_hint();
            if !stale_hint.is_empty() {
                msg = format!("⚠ {}\n{}", stale_hint, msg);
            }
            if first_bind {
                msg.push_str(&format!(" · 已把 userId {}… 绑定为本账号", q.user_id.chars().take(8).collect::<String>()));
            }
            if !renamed.is_empty() {
                msg.push_str(&format!("\n账号名已从「{}」改成「{}」", name, renamed));
            }
            msg.push_str(&rename_note);
            json!({
                "ok": true, "msg": msg, "renamed": renamed,
                // 让前端也能标出来源，而不仅仅是弹一条消息
                "source": q.source, "stale": q.is_stale(), "live_error": q.live_error,
            })
        }

        "fetch_usage" => {
            let meta = match meta {
                Some(m) => m,
                None => return err("实例不存在"),
            };
            match crate::usage::compute(app_id, meta["data_dir"].as_str().unwrap_or("")) {
                Some(u) => {
                    let mut m = meta.clone();
                    m["usage"] = u.clone();
                    m["usage_fetched_at"] = json!(std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs_f64())
                        .unwrap_or(0.0));
                    reg::save_instance(app_id, &m);
                    ok(format!(
                        "已汇总本地用量：入 {} / 出 {} / 共 {} tokens（{} 个会话）",
                        gfmt(u["input"].as_f64().unwrap_or(0.0)),
                        gfmt(u["output"].as_f64().unwrap_or(0.0)),
                        gfmt(u["total"].as_f64().unwrap_or(0.0)),
                        u["files"].as_i64().unwrap_or(0),
                    ))
                }
                None => err("没有可统计的本地会话日志（目前支持 Codex）"),
            }
        }

        "swap_set" => {
            let (o, m) = swap::swap_set(app_id, name, payload["force"].as_bool().unwrap_or(false));
            if o {
                ok(m)
            } else {
                err(m)
            }
        }

        "swap_clear" => {
            let (o, m) = swap::swap_clear(app_id);
            if o {
                ok(m)
            } else {
                err(m)
            }
        }

        // 把早期 adopt 搬进 instances/ 的档案搬回原生位置（同盘改名，不复制）
        "rehome" => {
            let (o, m) = swap::rehome(app_id, name, payload["force"].as_bool().unwrap_or(false));
            if o {
                ok(m)
            } else {
                err(m)
            }
        }

        "calc_size" => {
            let meta = match meta {
                Some(m) => m,
                None => return err("实例不存在"),
            };
            let mb = space::human_mb(space::dir_size(Path::new(
                meta["data_dir"].as_str().unwrap_or(""),
            )));
            ok(format!("「{}」占用 {} MB", name, mb))
        }

        "clean_cache" => {
            let (o, m) = space::clean_cache(app_id, name);
            if o {
                ok(m)
            } else {
                err(m)
            }
        }

        "hide_app" => {
            reg::set_hidden(app_id, true);
            ok("已从列表隐藏；要恢复在 设置 → 高级 → 已隐藏的程序")
        }

        "unhide_app" => {
            reg::set_hidden(app_id, false);
            ok("已恢复显示")
        }

        // 左侧列表手动排序：置顶 / 上移 / 下移，次序存进 settings.app_order
        "app_move" => {
            let mut ids: Vec<String> = {
                let mut l = reg::all_apps();
                reg::sort_apps(&mut l);
                l.iter().map(|a| a.id.clone()).collect()
            };
            let i = match ids.iter().position(|x| x == app_id) {
                Some(i) => i,
                None => return err("列表里没有这个程序"),
            };
            let last = ids.len().saturating_sub(1);
            let j = match payload["op"].as_str().unwrap_or("") {
                "top" => 0,
                "up" => i.saturating_sub(1),
                "down" => (i + 1).min(last),
                "bottom" => last,
                _ => return err("未知的移动方向"),
            };
            if i == j {
                return ok(if j == 0 { "已经在最上面了" } else { "已经在最下面了" });
            }
            let id = ids.remove(i);
            ids.insert(j, id);
            reg::set_app_order(&ids);
            ok("已调整顺序")
        }

        "forget_app" => {
            if reg::forget_discovered(app_id) {
                ok("已清理这条卸载记录；账号目录原样保留")
            } else {
                err("没找到这条自动发现记录")
            }
        }

        _ => err(format!("未知动作 {}", act)),
    }
}

/// 与具体产品无关的动作：API 台账、AI 识别、福利活动、本地反代、数据根、保管箱。
/// 判据是「这个分支用不用得到 AppDef / 实例 meta」——用到的一律留在 run_action，
/// 用不到的放这边，在守卫之前就走完。别图省事往 run_action 里加新动作：那边第一道
/// 门就要求"选中某个 Agent"，台账和设置页里的动作不带 app，一点就弹「未选择 Agent」
/// （这个坑踩过三次：存储页、AI 拉模型、API 台账）。
fn app_free(act: &str, payload: &Value, st: &AppState) -> Option<Value> {
    let v = match act {
        // ===== 数据根与空间治理 =====
        // sizes=false 只报位置和各盘余量（毫秒级）；sizes=true 交给后台线程数，先回 counting
        "storage_report" => {
            if payload["sizes"].as_bool().unwrap_or(false) {
                crate::storage::sizes_report(payload["refresh"].as_bool().unwrap_or(false))
            } else {
                json!({ "ok": true, "report": crate::storage::report(false), "junk": [] })
            }
        }

        "storage_migrate" => {
            let target = payload["target"].as_str().unwrap_or("").trim().to_string();
            if payload["confirm"].as_bool() != Some(true) {
                return Some(err(
                    "搬家会改写各账号档案里存的绝对路径、并把换号入口重新挂一遍，确认后才执行",
                ));
            }
            crate::storage::migrate(&target)
        }

        "storage_rollback" => crate::storage::rollback(),

        // 把已有账号各自揣着的可再生大文件收进共享层（不填 app 就所有支持的产品都过一遍）
        "storage_dedup" => {
            let one = payload["app"].as_str().unwrap_or("").trim().to_string();
            let notes = if one.is_empty() {
                space::dedup_all()
            } else {
                space::dedup_app(&one)
            };
            if notes.is_empty() {
                ok("没有可共用的大目录，磁盘占用没变")
            } else {
                ok(notes.join("\n"))
            }
        }

        "junk_clean" => {
            let ids: Vec<String> = payload["ids"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            if ids.is_empty() {
                return Some(err("没说要清理哪几项"));
            }
            let (o, m) = crate::storage::junk_clean(&ids);
            if o {
                ok(m)
            } else {
                err(m)
            }
        }

        // ===== 凭据保护（DPAPI + 可选主密码）=====
        "vault_state" => secrets::status_json(),

        "vault_set_password" => {
            let p1 = payload["pwd"].as_str().unwrap_or("");
            let p2 = payload["again"].as_str().unwrap_or("");
            let hint = payload["hint"].as_str().unwrap_or("");
            let (o, m) = secrets::set_password(p1, p2, hint);
            if o {
                ok(m)
            } else {
                err(m)
            }
        }

        "vault_unlock" => {
            let (o, m) = secrets::unlock(payload["pwd"].as_str().unwrap_or(""));
            if o {
                let n = sweep_plaintext();
                ok(format!(
                    "{}{}",
                    m,
                    if n > 0 {
                        format!("顺手把 {} 处残留明文也收进了保管箱。", n)
                    } else {
                        String::new()
                    }
                ))
            } else {
                err(m)
            }
        }

        "vault_lock" => ok(secrets::lock()),

        "vault_clear_password" => {
            if payload["confirm"].as_bool() != Some(true) {
                return Some(err("移除主密码后，凭据只剩 Windows 账户这一层保护，确认后才做"));
            }
            let (o, m) = secrets::remove_password();
            if o {
                ok(m)
            } else {
                err(m)
            }
        }

        // 设置 → AI 识别：拉当前 base_url + key 支持哪些模型，填进「模型名」
        "ai_list_models" => {
            let base = payload["base"].as_str().unwrap_or("").trim().to_string();
            let key = payload["key"].as_str().unwrap_or("").trim().to_string();
            let base = if base.is_empty() {
                crate::ai::DEFAULT_BASE.to_string()
            } else {
                base
            };
            let (good, msg, ids) = keys::probe_models(&base, &key);
            json!({ "ok": good, "msg": msg, "models": ids })
        }

        "rescan" => {
            let n = do_rescan();
            ok(format!("已重新扫描本机，收录 {} 个 AI 程序", n))
        }

        "kill_pids" => {
            let pids: Vec<u32> = payload["pids"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_u64().map(|x| x as u32))
                        .collect()
                })
                .unwrap_or_default();
            procs::kill_pids(&pids);
            ok(format!("已结束 {} 个进程", pids.len()))
        }

        "delete_backup" => {
            let (o, m) = space::delete_backup(payload["file"].as_str().unwrap_or(""));
            if o { ok(m) } else { err(m) }
        }

        "open_data_dir" => {
            let (o, m) = space::open_dir(&reg::base_dir().to_string_lossy());
            if o { ok(m) } else { err(m) }
        }

        "proxy_start" => {
            st.proxy.set_config(
                payload["upstream"].as_str(),
                payload["strategy"].as_str(),
            );
            let (upstream, strategy) = st.proxy.config();
            // 只覆盖 upstream / strategy，pools 与 dist 必须原地保留（否则一开反代池子就没了）
            let mut cfg = reg::load_config();
            if !cfg["proxy"].is_object() {
                cfg["proxy"] = json!({});
            }
            cfg["proxy"]["upstream"] = json!(upstream);
            cfg["proxy"]["strategy"] = json!(strategy);
            reg::save_config(&cfg);
            let port = crate::settings::get()["proxy"]["port"]
                .as_u64()
                .unwrap_or(8788) as u16;
            let (o, m) = st.proxy.start(port);
            if o {
                ok(m)
            } else {
                err(m)
            }
        }

        "proxy_stop" => {
            let (o, m) = st.proxy.stop();
            if o {
                ok(m)
            } else {
                err(m)
            }
        }

        // ── 反代：池子（成员的归属） ──
        "proxy_pool_upsert" => {
            let (o, m) = crate::gateway::upsert_pool(&payload["item"]);
            if o { ok(m) } else { err(m) }
        }

        "proxy_pool_delete" => {
            let (o, m) = crate::gateway::remove_pool(payload["id"].as_str().unwrap_or(""));
            if o { ok(m) } else { err(m) }
        }

        // ── 反代：对外分发的 API Key ──
        "proxy_dist_create" => {
            let (o, m) = crate::gateway::upsert_dist(&payload["item"]);
            if o { ok(m) } else { err(m) }
        }

        "proxy_dist_update" => {
            let (o, m) = crate::gateway::upsert_dist(&payload["item"]);
            if o { ok(m) } else { err(m) }
        }

        "proxy_dist_delete" => {
            let (o, m) = crate::gateway::remove_dist(payload["id"].as_str().unwrap_or(""));
            if o { ok(m) } else { err(m) }
        }

        "proxy_dist_toggle" => {
            let (o, m) = crate::gateway::toggle_dist(payload["id"].as_str().unwrap_or(""));
            if o { ok(m) } else { err(m) }
        }

        // 点了「显示」才把明文取出来；列表里永远是打码的
        "proxy_dist_reveal" => {
            match crate::gateway::reveal_dist(payload["id"].as_str().unwrap_or("")) {
                Some(k) => ok(k),
                None => err("没找到这条分发记录"),
            }
        }

        // 可勾选的成员清单：按 Agent 分组列账号 + API 台账里的 Key
        "proxy_members" => ok(json!({ "members": crate::gateway::member_options() }).to_string()),

        "key_upsert" => {
            let (o, m, sid) = keys::upsert_site(&payload["item"]);
            if o {
                // 带上最终站点 id：同名站会被并进来，前端只有拿到它才能确定往哪个站里加 key
                json!({ "ok": true, "msg": m, "site": sid })
            } else {
                err(m)
            }
        }

        "key_upsert_key" => {
            let site = payload["site"].as_str().unwrap_or("").to_string();
            let (o, m, kid) = keys::upsert_key(&site, &payload["item"]);
            if o {
                json!({ "ok": true, "msg": m, "id": kid })
            } else {
                err(m)
            }
        }

        "key_delete" => {
            let (o, m) = keys::remove_site(payload["site"].as_str().unwrap_or(""));
            if o { ok(m) } else { err(m) }
        }

        "key_delete_key" => {
            let (o, m) = keys::remove_key(
                payload["site"].as_str().unwrap_or(""),
                payload["id"].as_str().unwrap_or(""),
            );
            if o { ok(m) } else { err(m) }
        }

        // 查重：同一个 key 原文在台账里出现 ≥2 条的组，交给界面勾选后走 key_delete_batch
        "key_dedup_scan" => {
            let (locked, groups) = keys::dedup_scan();
            json!({ "ok": true, "locked": locked, "groups": groups })
        }

        // 批量删除：ids = [{site, id}]，一次读盘写盘
        "key_delete_batch" => {
            let pairs: Vec<(String, String)> = payload["ids"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|x| {
                            Some((
                                x["site"].as_str()?.to_string(),
                                x["id"].as_str()?.to_string(),
                            ))
                        })
                        .collect()
                })
                .unwrap_or_default();
            if pairs.is_empty() {
                return Some(err("没勾任何要删的 key"));
            }
            let (n, miss) = keys::remove_keys_batch(&pairs);
            if n == 0 {
                err("一条都没删掉：勾选的 key 已经不在了（列表可能过期，刷新一下再试）")
            } else {
                let extra = if miss.is_empty() {
                    String::new()
                } else {
                    format!("；另有 {} 条勾选时已不存在", miss.len())
                };
                ok(format!("已删除 {} 条 key{}", n, extra))
            }
        }

        "key_probe_models" => {
            // 「添加账号」的 API 方式里点「从上游获取」：还没进台账，直接用表单里的 base+key 探
            let base = payload["base"].as_str().unwrap_or("");
            let key = payload["key"].as_str().unwrap_or("");
            let (good, msg, ids) = keys::probe_models(base, key);
            json!({ "ok": good, "msg": msg, "models": ids })
        }

        "key_test" => {
            let site = payload["site"].as_str().unwrap_or("");
            let id = payload["id"].as_str().unwrap_or("");
            let (sobj, item) = match find_pair(site, id) {
                Some(p) => p,
                None => return Some(err("记录不存在")),
            };
            let base = keys::base_for(&sobj, &item);
            if base.is_empty() {
                return Some(err("这个站点还没有接入点——点「改站点」把 Base URL 填上（一个站可以有 OpenAI / Anthropic / DashScope 好几种口）"));
            }
            let (good, msg, n) = keys::probe(&base, item["key"].as_str().unwrap_or(""));
            let mut updated = item.clone();
            updated["last_status"] = json!(if good { "ok" } else { "fail" });
            updated["last_msg"] = json!(msg.clone());
            updated["models"] = json!(n);
            updated["last_check"] = json!(chrono::Local::now().format("%m-%d %H:%M").to_string());
            keys::upsert_key(site, &updated);
            if good { ok(msg) } else { err(msg) }
        }

        "key_models" => {
            // 拉取该接入点实际可调用的模型清单，供用户登记/校对
            let (sobj, item) = match find_pair(payload["site"].as_str().unwrap_or(""), payload["id"].as_str().unwrap_or("")) {
                Some(p) => p,
                None => return Some(err("记录不存在")),
            };
            let base = keys::base_for(&sobj, &item).trim_end_matches('/').to_string();
            if base.is_empty() {
                return Some(err("这个站点还没有接入点——先点「改站点」填 Base URL"));
            }
            let url = if base.ends_with("/models") {
                base.clone()
            } else if base.ends_with("/v1") {
                format!("{}/models", base)
            } else {
                format!("{}/v1/models", base)
            };
            let mut req = ureq::get(&url).timeout(std::time::Duration::from_secs(20));
            let key = item["key"].as_str().unwrap_or("").trim().to_string();
            if !key.is_empty() {
                req = req.set("Authorization", &format!("Bearer {}", key));
            }
            let body: Value = match req.call() {
                Ok(r) => match r.into_json() {
                    Ok(v) => v,
                    Err(e) => return Some(err(format!("响应不是合法 JSON：{}", e))),
                },
                Err(ureq::Error::Status(c, _)) => return Some(err(format!("HTTP {}", c))),
                Err(e) => return Some(err(format!("抓取失败：{}", e))),
            };
            // OpenAI 风格 data[].id / Anthropic 风格 data[].id，都兼容
            let models: Vec<String> = body["data"]
                .as_array()
                .map(|a| a.iter().filter_map(|m| m["id"].as_str().map(String::from)).collect())
                .or_else(|| body["models"].as_array().map(|a| a.iter().filter_map(|m| m["id"].as_str().map(String::from)).collect()))
                .unwrap_or_default();
            if models.is_empty() {
                return Some(err("未解析到模型清单"));
            }
            ok(json!({ "models": models }).to_string())
        }

        // 站点级拉模型：一个站的清单是共用的，不必每条 key 各拉一遍
        "site_models" => {
            let site_id = payload["site"].as_str().unwrap_or("");
            let base = payload["base"].as_str().unwrap_or("").trim().to_string();
            let sobj = match keys::sites().into_iter().find(|p| p["id"].as_str() == Some(site_id)) {
                Some(p) => p,
                None => return Some(err("站点不存在")),
            };
            if base.is_empty() {
                return Some(err("没给接入点"));
            }
            // 探活用站点下第一条有原文的 key（同一站的 key 通用），绝不让明文绕到界面
            let probe_key = match payload["key"].as_str().filter(|s| !s.trim().is_empty()) {
                Some(s) => s.trim().to_string(),
                None => sobj["keys"]
                    .as_array()
                    .and_then(|ks| ks.iter().find_map(|k| k["key"].as_str()))
                    .unwrap_or("")
                    .to_string(),
            };
            let (good, msg, ids) = keys::probe_models(&base, &probe_key);
            if !good {
                return Some(err(msg));
            }
            let mut merged: Vec<String> = sobj["models_list"]
                .as_str()
                .unwrap_or("")
                .split([',', '，', '\n'])
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(String::from)
                .collect();
            let mut added = 0;
            for id in &ids {
                if !merged.iter().any(|x| x == id) {
                    merged.push(id.clone());
                    added += 1;
                }
            }
            let mut upd = sobj.clone();
            upd["models_list"] = json!(merged.join(", "));
            let _ = keys::upsert_site(&upd);
            json!({
                "ok": true,
                "msg": format!("拉到 {} 个模型，站点清单里新增 {} 个", ids.len(), added),
                "models": ids,
                "models_list": merged.join(", "),
            })
        }

        "key_fetch_quota" => {
            let site = payload["site"].as_str().unwrap_or("");
            let item = match find_key(site, payload["id"].as_str().unwrap_or("")) {
                Some(k) => k,
                None => return Some(err("记录不存在")),
            };
            let (left, total, e) = keys::fetch_key_quota(&item);
            if let Some(e) = e {
                return Some(err(e));
            }
            let mut updated = item.clone();
            updated["left"] = json!(left);
            if total.is_some() {
                updated["total"] = json!(total);
            }
            updated["fetched_at"] = json!(std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs_f64())
                .unwrap_or(0.0));
            keys::upsert_key(site, &updated);
            match (left, total) {
                (Some(l), Some(t)) => ok(format!("已更新余额：{} / 总额 {}", gfmt(l), gfmt(t))),
                (Some(l), None) => ok(format!("已更新余额：{}", gfmt(l))),
                _ => ok("已更新"),
            }
        }

        // 贴图 / 文本 → 多模态模型 → 结构化结果，给「额度截图识别」「福利活动识别」用。
        // 图片走 dataURL 传进来，Key 只在 Rust 侧拼请求头，不下发到界面。
        "ai_ask" => {
            let prompt = payload["prompt"].as_str().unwrap_or("").to_string();
            let img = payload["image"].as_str().unwrap_or("").trim().to_string();
            let content = if img.is_empty() {
                json!(prompt)
            } else {
                json!([
                    { "type": "text", "text": prompt },
                    { "type": "image_url", "image_url": { "url": img } },
                ])
            };
            match crate::ai::ask(content) {
                Ok(text) => json!({
                    "ok": true,
                    "text": text,
                    // 模型爱加 ```json 围栏和解释话，这里先把 JSON 抠出来再给界面
                    "json": crate::ai::first_json(&text),
                    "msg": "识别完成",
                }),
                Err(e) => err(e),
            }
        }

        // 识别期间前端每秒来问一次「现在到哪一步了」，把等待讲成人话；
        // 空闲时 ok:true + 空 text（调用收尾的一瞬会撞上，前端按没进度处理）
        "ai_progress" => {
            let (stage, chars) = crate::ai::progress();
            let text = match stage {
                1 => "连接模型中…".to_string(),
                2 => "已连上，等模型开始输出…".to_string(),
                3 => format!("模型输出中，已收 {} 字", gfmt(chars as f64)),
                _ => String::new(),
            };
            json!({ "ok": true, "text": text })
        }

        // 各家 Agent 的限时/每日福利活动（官方没接口，手动记一条）
        "perk_save" => {
            let p = &payload["perk"];
            let title = p["title"].as_str().unwrap_or("").trim().to_string();
            if title.is_empty() {
                return Some(err("福利内容不能为空"));
            }
            let mut list = reg::load_perks();
            let id = match p["id"].as_str().filter(|s| !s.is_empty()) {
                Some(s) => s.to_string(),
                None => format!(
                    "p{}",
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs())
                        .unwrap_or(0)
                ),
            };
            let item = json!({
                "id": id,
                "app": p["app"].as_str().unwrap_or(""),
                "title": title,
                "detail": p["detail"].as_str().unwrap_or("").trim(),
                "ends_on": p["ends_on"].as_str().unwrap_or("").trim(),
                "daily": p["daily"].as_bool().unwrap_or(false),
                "url": p["url"].as_str().unwrap_or("").trim(),
            });
            match list.iter().position(|x| x["id"].as_str() == Some(id.as_str())) {
                Some(i) => list[i] = item,
                None => list.push(item),
            }
            reg::save_perks(&list);
            ok("已记下这条福利")
        }

        "perk_delete" => {
            let id = payload["id"].as_str().unwrap_or("");
            let mut list = reg::load_perks();
            let before = list.len();
            list.retain(|x| x["id"].as_str() != Some(id));
            if list.len() == before {
                return Some(err("没找到这条记录"));
            }
            reg::save_perks(&list);
            ok("已删除")
        }

        // 拖动排序：前端把整份新次序一次交回来。次序表必须和当前可见列表一一对应
        // （all_apps 已经滤掉隐藏的），对不上就不写——免得拖出个重复/陌生 id 把位次搅乱
        "app_reorder" => {
            let want: Vec<String> = payload["ids"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            let have: Vec<String> = reg::all_apps().iter().map(|a| a.id.clone()).collect();
            if want.len() != have.len() || want.iter().any(|w| !have.iter().any(|h| h == w)) {
                return Some(err("顺序表和当前程序列表对不上（中间有新装/卸载/隐藏的？）——重新扫描后再拖"));
            }
            reg::set_app_order(&want);
            ok("顺序已保存")
        }

        "reset_caps" => {
            state::reset_caps();
            ok("已重置多开能力判定，下次点启动会重新实测")
        }

        // ===== 把台账里的 key 注入某个 Agent 的自定义模型配置 =====
        // 归属在台账这边：动作针对的是"某条 key"，不是"选中的那个 Agent"，
        // 所以必须留在 app_free 一侧，别又被「未选择 Agent」挡住
        "inject_targets" => {
            match find_key(payload["site"].as_str().unwrap_or(""), payload["id"].as_str().unwrap_or("")) {
                Some(k) => ok(
                    json!({ "targets": inject::list_for(&k), "vault_locked": !inject::vault_ready() }).to_string(),
                ),
                None => err("台账里没找到这条 key"),
            }
        }

        "inject_apply" => {
            let apps: Vec<String> = payload["apps"]
                .as_array()
                .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
                .unwrap_or_default();
            if apps.is_empty() {
                return Some(err("没说要接到哪几个程序"));
            }
            let rows = inject::apply(
                payload["site"].as_str().unwrap_or(""),
                payload["id"].as_str().unwrap_or(""),
                &apps,
            );
            let done = rows.iter().filter(|r| r["ok"] == json!(true)).count();
            let msg = rows
                .iter()
                .map(|r| {
                    format!(
                        "{}：{}",
                        r["label"].as_str().or(r["app"].as_str()).unwrap_or("?"),
                        r["msg"].as_str().unwrap_or("")
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            return Some(json!({ "ok": done > 0, "msg": msg, "rows": rows }));
        }

        "inject_revert" => inject::revert(
            payload["site"].as_str().unwrap_or(""),
            payload["id"].as_str().unwrap_or(""),
            payload["app"].as_str().unwrap_or(""),
        ),

        // ===== 本机当前登录 / 官网逐资源包额度 =====
        // 这两条都不属于某个账号：前者是"这台机器现在登的是谁"，后者是"这个产品的官网数"，
        // 所以放在 app_free 这边，不该被「未选择 Agent」挡住。
        "machine_login" => {
            let aid = payload["app"].as_str().unwrap_or("").to_string();
            if payload["refresh"].as_bool().unwrap_or(false) {
                crate::machine::refresh_now(&aid)
            } else {
                crate::machine::current(&aid)["value"].clone()
            }
        }

        "web_login" => match crate::webquota::open_login(payload["app"].as_str().unwrap_or("")) {
            Ok(m) => ok(m),
            Err(e) => err(e),
        },

        // 「她在官网窗口里登录上了没有」——只翻 cookie 罐、不发请求，给前端那条轮询用。
        // 没有这一条，登录完还得再点一次「读官网逐包额度」才出明细，
        // 而她的预期是"我登录了，积分/用量/时间就该都在"。
        "web_session" => json!({
            "ok": true,
            "signed_in": crate::webquota::has_session(payload["app"].as_str().unwrap_or("")),
        }),

        "web_fetch" => {
            let aid = payload["app"].as_str().unwrap_or("").to_string();
            let data = crate::webquota::fetch(&aid);
            if data["ok"].as_bool() != Some(true) {
                return Some(data);
            }
            let nm = payload["name"].as_str().unwrap_or("").trim().to_string();
            if nm.is_empty() {
                return Some(json!({ "ok": true, "data": data }));
            }
            let meta = match reg::find_instance(&aid, &nm) {
                Some(m) => m,
                None => return Some(err(format!("账号「{nm}」不存在"))),
            };
            let bound = meta["quota_user_id"].as_str().unwrap_or("").trim().to_string();
            let uid = data["user_id"].as_str().unwrap_or("").to_string();
            let force = payload["force"].as_bool().unwrap_or(false);
            if !bound.is_empty() && bound != uid && !force {
                let found = data["account"].as_str().unwrap_or("").to_string();
                return Some(json!({
                    "ok": true, "data": data, "mismatch": true,
                    "card_account": meta["quota_account"].as_str().unwrap_or(""),
                    "msg": format!(
                        "官网读到的是「{}」，这张卡绑的是「{}」。要么把这张卡改绑到当前登录的号，\
                         要么给「{}」单独建一张卡。",
                        found,
                        meta["quota_account"].as_str().unwrap_or(bound.as_str()),
                        found
                    ),
                }));
            }
            let fresh = crate::webquota::apply(&meta, &data);
            reg::save_instance(&aid, &fresh);
            let packs = data["portions"].as_array().map(|a| a.len()).unwrap_or(0);
            let plan_exp = data["plan_expire"].as_str().unwrap_or("").to_string();
            let msg = format!(
                "官网读数已写进「{}」：剩 {} / 共 {}，逐笔 {} 包（套餐到期 {}）",
                nm,
                gfmt(data["left"].as_f64().unwrap_or(0.0)),
                gfmt(data["total"].as_f64().unwrap_or(0.0)),
                packs,
                if plan_exp.is_empty() { "未给出" } else { &plan_exp }
            );
            json!({ "ok": true, "data": data, "saved": true, "msg": msg })
        }

        // ===== Windows 外壳集成（桌面/开始菜单快捷方式，见 shelllink.rs）=====
        // 设置页不带选中 Agent，这三条必须走 app_free 分流，否则第一道门就报「未选择 Agent」
        "shell_shortcut_status" => crate::shelllink::status(),

        "shell_shortcut_create" => {
            crate::shelllink::create(payload["kind"].as_str().unwrap_or(""))
        }

        "shell_shortcut_remove" => {
            crate::shelllink::remove(payload["confirm"].as_bool().unwrap_or(false))
        }

        _ => return None,
    };
    Some(v)
}

/// 一份档案"是谁、还在不在用"——用来代替原来那串路径 + MB。
/// 用户面对 `C:\Users\LENOVO\AppData\Roaming\com.qodercn.app.stable 1153 MB` 是选不出来的，
/// 她要的是"这是我天天在用的那份 / 这是那个早不登的旧号 / 这是空目录"。
/// 只扫顶层目录的修改时间，不做整树遍历（一家 13 万个文件会卡住界面）。
fn profile_facts(app_id: &str, dir: &Path, name: Option<&str>) -> Value {
    let mut newest: u64 = 0;
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            if let Ok(md) = e.metadata() {
                let t = md
                    .modified()
                    .ok()
                    .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                if t > newest {
                    newest = t;
                }
            }
        }
    }
    let last_used = if newest == 0 {
        String::new()
    } else {
        chrono::DateTime::from_timestamp(newest as i64, 0)
            .map(|d| {
                d.with_timezone(&chrono::Local)
                    .format("%Y-%m-%d %H:%M")
                    .to_string()
            })
            .unwrap_or_default()
    };
    let files = crate::identity::inspect(&dir.to_string_lossy());
    // 这份档案现在登的是谁：只有"程序当前默认档案指向它"时才读得到（其它目录是别人的号，
    // 客户端不会替我们留着登录态），读不到就留空，界面会说"没登录过"。
    let is_default = reg::profile_dir(app_id)
        .map(|p| {
            p.to_string_lossy().replace('\\', "/").to_lowercase()
                == dir.to_string_lossy().replace('\\', "/").to_lowercase()
        })
        .unwrap_or(false);
    let mut who = String::new();
    if is_default {
        if let Some(store) = crate::local_quota::store_for(app_id) {
            if let Ok(q) = crate::local_quota::fetch(store, Some(dir.to_string_lossy().as_ref())) {
                who = q.account;
            }
        }
    }
    let claimer = name.unwrap_or("").to_string();
    json!({
        "last_used": last_used,
        "files": files.files,
        "identity": files.state,
        "who": who,
        "is_default": is_default,
        "claimed_by": claimer,
    })
}

/// 「我本地一直在用的那份档案」——程序当前的默认档案，且里面真有东西。
/// 是 junction 说明已被换号接管，不算"原始档案"。
fn seed_profile(app_id: &str) -> Option<PathBuf> {    crate::registry::profile_dir(app_id)
        .filter(|p| p.is_dir() && !crate::registry::is_reparse_point(p) && space::dir_has_data(p))
}

/// 退而求其次：已有账号里最近活跃的那个（用它来"继承上一个号的对话与记忆"）
fn seed_prev(app_id: &str) -> Option<PathBuf> {
    crate::registry::load_instances(app_id)
        .iter()
        .filter_map(|m| m["data_dir"].as_str().map(PathBuf::from))
        .find(|p| p.is_dir() && space::dir_has_data(p))
}

/// 新建账号时这份数据从哪儿来。抽出来给 new_instance 和 seed_sources 共用，
/// 保证向导上的推荐和真正建出来的东西一致。
///
/// auto 的规则：默认档案里那份"我一直在用的" > 已有账号里最近活跃的 > 空档。
/// 仅可换号的产品默认 `adopt`（搬过来，零拷贝，反正默认档案迟早要被 junction 取代）；
/// 可多开 / CLI 的产品默认 `copy`——**绝不搬**，用户还要用原来那份。
fn resolve_seed(
    app_id: &str,
    kind: &str,
    want: &str,
    from_arg: Option<PathBuf>,
) -> (String, Option<PathBuf>) {
    let profile = seed_profile(app_id);
    let prev = seed_prev(app_id);
    match want {
        "fresh" => ("fresh".into(), None),
        "adopt" => ("adopt".into(), profile),
        "copy" => ("copy".into(), from_arg.or(profile).or(prev)),
        _ => match profile {
            Some(p) if kind != "cli" && !crate::state::isolatable(app_id) => {
                ("adopt".into(), Some(p))
            }
            Some(p) => ("copy".into(), Some(p)),
            None => match prev {
                Some(p) => ("copy".into(), Some(p)),
                None => ("fresh".into(), None),
            },
        },
    }
}

/// 把还散在文档里的明文凭据一律收进保管箱：站点 Key、AI Key、账号 token。
/// 启动时跑一次；用户解锁保管箱之后再跑一次（锁定期间保存的东西会先留在原样）。
pub fn sweep_plaintext() -> usize {
    keys::sweep() + crate::settings::sweep() + reg::sweep_tokens()
}

fn dirs_doc_home() -> String {
    let h = reg::home();
    let docs = h.join("Documents");
    if docs.exists() {
        docs.to_string_lossy().into_owned()
    } else {
        h.to_string_lossy().into_owned()
    }
}

/// 像 Python %g 那样输出：整数量不带小数点
/// 没给数就写"没给"，不许塌成 0——"剩 0 积分"和"读不到额度"是两件不同的事
fn gfmt_opt(v: Option<f64>) -> String {
    match v {
        Some(x) => gfmt(x),
        None => "未给出".to_string(),
    }
}

fn gfmt(v: f64) -> String {
    if v == v.trunc() && v.abs() < 1e15 {
        format!("{}", v as i64)
    } else {
        format!("{:.2}", v)
    }
}

/// 接管账号的数据目录＝默认档案目录本身；其它来源才在 instances/ 下新建。
/// 这条判据钉死「接管不许搬家」——早期 adopt 把用户天天用的档案 rename 进
/// instances/，档案因此离开原位，还原默认还没有退路。
fn instance_data_dir(origin: &str, src: Option<&PathBuf>, instances_dir: &Path, name: &str) -> PathBuf {
    match (origin, src) {
        ("adopt", Some(s)) => s.clone(),
        _ => instances_dir.join(name),
    }
}

/// adopt 只在「真的有一个源目录可接管」时才确认：源为空时压根没东西可认领，
/// 不必多问一句（那种情况 seed_instance 会直接报错，不会偷偷建成空档）。
fn adopt_needs_confirm(origin: &str, src: Option<&PathBuf>, confirmed: Option<bool>) -> bool {
    origin == "adopt" && src.is_some() && confirmed != Some(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 接管＝原地登记：账号目录必须就是默认档案目录本身，一个字节都不搬。
    #[test]
    fn adopt_never_relocates_the_profile() {
        let inst = PathBuf::from(r"C:\Users\x\.agent-cockpit\instances\qoder-cn");
        let native = PathBuf::from(r"C:\Users\x\AppData\Roaming\com.qodercn.app.stable");
        let src = Some(native.clone());
        assert_eq!(
            instance_data_dir("adopt", src.as_ref(), &inst, "账号2"),
            native,
            "接管的账号不许被挪进 instances/"
        );
        // 复制/空档才是"在 instances/ 下新建一个目录"
        assert_eq!(
            instance_data_dir("copy", src.as_ref(), &inst, "账号3"),
            inst.join("账号3")
        );
        assert_eq!(
            instance_data_dir("fresh", None, &inst, "账号4"),
            inst.join("账号4")
        );
    }

    /// 台账 / AI / 福利 / 反代 / 存储 / 保管箱这些动作跟产品无关，不能被「未选择 Agent」挡住。
    /// 这里只挑空入参下不会写盘、不会联网的动作验，边跑边确认它们落在 app_free 那一侧。
    #[test]
    fn app_free_actions_do_not_need_a_selected_app() {
        let st = AppState {
            proxy: crate::proxy::Proxy::shared(),
        };
        for act in [
            "storage_report", "vault_state", "storage_rollback", "ai_list_models",
            "key_upsert", "key_delete", "key_test", "key_models", "key_fetch_quota",
            "key_probe_models", "key_dedup_scan", "perk_save", "proxy_members",
            "delete_backup", "kill_pids",
            // 快捷方式三连：空入参下 create/remove 都在起 PowerShell 之前就报错回绝，
            // 只验它们被 app_free 接住；status 会真起子进程，不放这儿
            "shell_shortcut_create", "shell_shortcut_remove",
        ] {
            let p = json!({ "action": act });
            assert!(app_free(act, &p, &st).is_some(), "{} 应该被 app_free 接住", act);
        }
        let p = json!({ "action": "junk_clean", "ids": [] });
        assert_eq!(app_free("junk_clean", &p, &st).unwrap()["ok"], json!(false));
        let p = json!({ "action": "key_delete_batch", "ids": [] });
        assert_eq!(app_free("key_delete_batch", &p, &st).unwrap()["ok"], json!(false));
        assert!(app_free("launch", &json!({}), &st).is_none(), "产品动作要交回主分发");
    }

    #[test]
    fn adopt_is_gated_until_confirmed() {
        let src = Some(PathBuf::from("C:\\Users\\x\\AppData\\Roaming\\WorkBuddy"));
        assert!(
            adopt_needs_confirm("adopt", src.as_ref(), None),
            "没带确认标记时，搬目录的动作必须被拦下来"
        );
        assert!(adopt_needs_confirm("adopt", src.as_ref(), Some(false)));
        assert!(
            !adopt_needs_confirm("adopt", src.as_ref(), Some(true)),
            "用户明确确认过才放行"
        );
        // 没有源目录时 adopt 会退化成空档：不该多问一次
        assert!(!adopt_needs_confirm("adopt", None, None));
        // copy / fresh 不动原档案，一律不需要确认
        let any = Some(PathBuf::from("C:\\Users\\x\\.qoder-cn"));
        assert!(!adopt_needs_confirm("copy", any.as_ref(), None));
        assert!(!adopt_needs_confirm("fresh", None, None));
    }
}
