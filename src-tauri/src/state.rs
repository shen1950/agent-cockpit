// 仪表盘状态聚合
use crate::identity;
use crate::procs::{
    alive, instance_pids, procs_rows, program_pids, snapshot, windowed_count, windowed_pids,
};
use crate::proxy::Proxy;
use crate::quota::days_left;
use crate::registry::{
    account_candidates, all_apps, all_apps_raw, bound_instance_name, discovered, exe_name,
    is_in_place, load_config, load_instances, profile_dir, rehome_slot, resolve_exe, save_config,
    short_of as reg_short_of, swaps_of, PROXY_PORT,
};
use serde_json::{json, Value};

/// 快照超过这么久就不要再拿它催"临期"：后台自动取数默认半小时一轮，
/// 六小时读不到就是真读不到（产品没开、令牌掉了），这时候报的到期日多半属于已经花光的笔。
const STALE_ALERT_SEC: f64 = 6.0 * 3600.0;

fn fmt_date(ts: f64) -> String {
    chrono::DateTime::from_timestamp(ts as i64, 0)
        .map(|d| d.with_timezone(&chrono::Local).format("%Y-%m-%d").to_string())
        .unwrap_or_default()
}

fn fmt_min(ts: f64) -> String {
    if ts <= 0.0 {
        return String::new();
    }
    chrono::DateTime::from_timestamp(ts as i64, 0)
        .map(|d| d.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_default()
}

/// 取"更早到期"的那一条：天数相同时先到先得，不做二次排序。
fn sooner(cur: &mut Option<i64>, exp: &mut String, d: i64, e: &str) {
    if cur.map(|p| d < p).unwrap_or(true) {
        *cur = Some(d);
        *exp = e.to_string();
    }
}

/// 排序用的到期时刻。临期列表只按"几天后"排是不够的：同一天里
/// 千问的每日积分 00:00 就刷新、WorkBuddy 的裂变包要到 22:18 才作废，
/// 两者都是"1 天后"，按 app 顺序排会把先作废的压在后面。
/// 这里把同一套写法（"YYYY-MM-DD HH:MM"）的字典序当时间序用；
/// 只给到日的补 23:59（与 quota::expiry 同一口径：那一天整天还能用）。
fn exp_sort_key(s: &str) -> String {
    let s = s.trim();
    let head_ok = s.len() >= 10
        && s.is_char_boundary(10)
        && s[..10].bytes().all(|b| b.is_ascii_digit() || b == b'-');
    if !head_ok {
        return "9999-12-31 23:59".to_string(); // "未给出"/"无限期" 排最后，不许冒充最近到期
    }
    if s.len() >= 16 && s.is_char_boundary(16) && s.as_bytes().get(10) == Some(&b' ') {
        s[..16].to_string()
    } else {
        format!("{} 23:59", &s[..10])
    }
}

/// 该 Agent 能否真正多开：先看静态判定，再看实测结论（启动后校验失败会写进 app_caps）
fn isolate_ok(cfg: &Value, app: &crate::registry::AppDef) -> bool {
    // CLI（CODEX_HOME 环境变量）与 Chromium PWA（--user-data-dir）都是目录隔离，
    // 实测都真能并存，不需要靠 app_caps 记录"试过没有"
    if app.kind == "cli" || app.kind == "pwa" {
        return true;
    }
    if app.kind == "plain" {
        return false;
    }
    match cfg.get("app_caps").and_then(|c| c.get(&app.id)).and_then(|v| v.as_bool()) {
        Some(b) => b,
        None => app.isolatable,
    }
}

/// 给 launch 用的便捷判定：已知不能多开的产品就别去试了。
/// 实测 Qoder 0.4.x 会忽略 `--user-data-dir`（它自己 setPath('userData')），
/// 硬试只会等 8 秒再报"转交给了已开窗口"，不如当场说清该走换号。
pub fn isolatable(app_id: &str) -> bool {
    match all_apps().into_iter().find(|a| a.id == app_id) {
        Some(a) => isolate_ok(&load_config(), &a),
        None => false,
    }
}

pub fn mark_not_isolatable(app_id: &str) {
    let mut cfg = load_config();
    cfg["app_caps"][app_id] = json!(false);
    save_config(&cfg);
}

pub fn reset_caps() {
    let mut cfg = load_config();
    if let Some(obj) = cfg.get_mut("app_caps").and_then(|v| v.as_object_mut()) {
        obj.clear();
    }
    save_config(&cfg);
}

pub fn build_state(proxy: &Proxy) -> Value {
    crate::gateway::migrate_legacy_names();
    let snap = snapshot();
    let vis = windowed_pids();
    let cfg = load_config();
    let swaps = swaps_of(&cfg);
    let st = crate::settings::get();
    let warn_days = crate::settings::warn_days(&st);
    let danger_days = crate::settings::danger_days(&st);
    let (upstream, strategy) = proxy.config();

    let mut apps_out: Vec<Value> = Vec::new();
    let mut instances: Vec<Value> = Vec::new();
    let mut alerts: Vec<Value> = Vec::new();
    let mut n_running = 0usize;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let mut app_list = all_apps();
    crate::registry::sort_apps(&mut app_list);
    for app in app_list {
        let installed = resolve_exe(&app).is_some();
        // 自动发现且 exe 已消失 = 已卸载；首次收录 7 天内且**没被点开看过** = 新装
        // （点开一次就置 seen，标记消失——否则这个「新」会挂满一星期很吵）
        let removed = app.discovered && !installed;
        let is_new =
            app.discovered && !app.seen && app.first_seen > 0 && now.saturating_sub(app.first_seen) < 604800;
        let can_isolate = isolate_ok(&cfg, &app);
        // 「当前账号」以磁盘上的 junction 为准：换号记录会过时。Qoder 和 ZCode 都因为
        // 只信记录被带偏过——界面显示一个账号，实际入口指着已删除的另一个，程序直接起不来
        let swapped = match profile_dir(&app.id) {
            Some(_) => bound_instance_name(&app.id).unwrap_or_default(),
            None => swaps
                .get(&app.id)
                .and_then(|v| v["instance"].as_str())
                .unwrap_or("")
                .to_string(),
        };
        // 程序级：这个 exe 到底有没有在跑。不能多开的产品只能按这个粒度判定运行状态；
        // PWA 也按 exe 名看——它的子进程全叫 ChatGPT.exe，账号级要靠命令行里的目录区分
        let prog_pids = if app.kind == "electron" || app.kind == "plain" || app.kind == "pwa" {
            program_pids(&exe_name(&app).to_lowercase(), &snap)
        } else {
            Vec::new()
        };
        let insts = load_instances(&app.id);
        let inst_names: Vec<&str> = insts.iter().filter_map(|m| m["name"].as_str()).collect();
        // 「这台机器现在登的是谁」——产品级的一份真值，和卡片上绑的账号是两件事。
        // 不摆出来，就出现"人早换成另一个号了，卡片还停在旧号"这种看着像 bug 的错。
        let machine = crate::machine::current(&app.id);
        // 这个产品是不是几个账号共用默认位置那一份档案。共用的那种，切换账号＝在客户端里
        // 退出登录再登录：界面不许摆"换号要先关程序"，程序级进程也要按现在登着谁归属。
        let shared = crate::registry::shares_one_profile(&app.id);
        let m_ok = machine["value"]["ok"].as_bool().unwrap_or(false);
        let m_uid = machine["value"]["user_id"].as_str().unwrap_or("").to_string();
        // 认得出当前登录的那个号时，程序级进程才按它归属；一个都对不上（登的是没建过卡的
        // 第三个号）就退回换号记录，否则整批进程没人认领，卡片上一个「结束」都点不到。
        let login_known = m_ok
            && !m_uid.is_empty()
            && insts.iter().any(|m| m["quota_user_id"].as_str() == Some(m_uid.as_str()));
        apps_out.push(json!({
            "id": app.id, "name": app.name, "short": reg_short_of(&app), "kind": app.kind,
            "installed": installed, "swapped": swapped,
            "machine": machine,
            "shared_profile": shared,
            // 这个产品的登录态格式登记了没有：登记了的才能一键换号（见 authpack.rs）
            "auth_switch": crate::registry::auth_switch_ok(&app.id),
            "web_quota": crate::webquota::supports(&app.id),
            // 换号记录指向一个已经不存在的账号：档案目录还挂着 junction，得能一键还原
            "swap_orphan": !swapped.is_empty() && !inst_names.contains(&swapped.as_str()),
            "discovered": app.discovered, "isolatable": can_isolate,
            "custom": app.id.starts_with("custom-"),
            "removed": removed, "new": is_new,
            "swap_ok": app.kind != "plain" && profile_dir(&app.id).is_some(),
            "local_quota": crate::local_quota::store_for(&app.id).is_some(),
            "icon": crate::icons::get(&app.id),
            "exe": resolve_exe(&app).map(|p| p.to_string_lossy().into_owned()).unwrap_or_default(),
        }));

        for m in insts {
            let cands = account_candidates(&app.id, &m, &swaps);
            let swapped_here = swapped == m["name"].as_str().unwrap_or("");
            // 客户端里现在登的就是这张卡的号（按 userId 认，读不到当前登录时一律为假）
            let logged_in =
                login_known && m["quota_user_id"].as_str().unwrap_or("") == m_uid.as_str();
            // 这个账号名下的进程：先按实例目录精确匹配；匹配不到而它又是当前换号账号，
            // 才按程序级算（换号产品的进程命令行里只有默认档案路径）
            let owned: Vec<u32> = match app.kind.as_str() {
                // 共用一份档案的产品：每个账号的 data_dir 都是同一个目录，按目录匹配会把
                // 整棵进程树算到每张卡头上（两张卡同时显示"运行中"）。谁在跑只有登录态知道，
                // 所以按"现在登着的那个号"归属；读不到登录态才退回换号记录。
                "electron" if shared => {
                    let claims = if login_known { logged_in } else { swapped_here };
                    if claims {
                        prog_pids.clone()
                    } else {
                        Vec::new()
                    }
                }
                "electron" => {
                    let p = instance_pids(&app, &cands, &snap);
                    if !p.is_empty() {
                        p
                    } else if swapped_here {
                        prog_pids.clone()
                    } else {
                        Vec::new()
                    }
                }
                "plain" => prog_pids.clone(),
                _ => Vec::new(),
            };
            // 「运行中」= 有可见窗口。只有进程没有窗口是后台驻留，不算用户开着
            let (running, procs) = if app.kind == "cli" {
                let pid = m["pid"].as_u64().unwrap_or(0) as u32;
                let on = pid != 0 && alive(&snap, pid);
                (on, if on { 1 } else { 0 })
            } else {
                (windowed_count(&owned, &vis) > 0, owned.len())
            };
            let resident = !running && procs > 0;
            if running {
                n_running += 1;
            }
            // 多笔积分：逐笔统计剩余/总额并各自算到期日；没有多笔就回退到单笔字段
            let raw_credits = m.get("credits").and_then(|c| c.as_array()).cloned().unwrap_or_default();
            let has_portions = !raw_credits.is_empty();
            // 这份数是从服务端实时读的，还是"客户端上次联网时的缓存"？
            // 旧缓存不能当实时数用，也不能拿它去报"临期"——否则会催一堆早已作废的到期提醒。
            // 两条判据：① 这一轮实时读失败了（live_error 非空）；② 这份快照本身就太旧。
            // 原来还额外要求 quota_source != "server"，可是一张卡上一次读成功就是从服务端读的，
            // source 会一直写着 server，之后连着几天读不到也永远判不出旧（TRAE 实测踩过：
            // 程序没开时弹窗还顶着"1 天后过期"，而那笔积分几天前就用光了）。
            let live_error = m["quota_live_error"].as_str().unwrap_or("").trim().to_string();
            // "这份数有多新"只认**最近一次成功取数**的时刻，即汇总数的 quota_fetched_at
            // （本机接口/服务端每半小时一刷）。quota_detail_at 是逐笔明细的抓取时刻，
            // 两者天然不同步——国内版 WorkBuddy 的明细走官网窗口、汇总数走本机接口。
            // 拿明细时刻当读数时刻，会把"汇总 23:30 刚到、明细停在 12:01"误判成整份作废：
            // 临期提醒整份消失（14 笔而不是 16 笔），卡片还顶着"读不到服务端"的假警告
            // ——实测 23:30 服务端 source=server、liveError 为空，只是明细 11 小时没刷新。
            // 明细旧不旧是另一件事，由 quota_detail_stale 单独标注，不掺进这里的判据。
            let snap_at = m["quota_fetched_at"]
                .as_f64()
                .or_else(|| m["quota_detail_at"].as_f64())
                .unwrap_or(0.0);
            // 只认到账号、压根没有额度数字的店（MiMo / Antigravity / Coze）：
            // agenthub 那句 live_error 是"厂商没有余额接口"的**解释**，不是"服务端没读成"的
            // **警告**——把它喂给 stale 那套话术，界面会对着一个根本没有数字的画面说
            // "下面是旧缓存"。所以 account_only 的卡片一律不算 stale。
            let account_only = m["quota_local"].as_bool().unwrap_or(false)
                && !m["quota_account"].as_str().unwrap_or("").trim().is_empty()
                && m["credits_left"].is_null()
                && m["credits_total"].is_null()
                && !has_portions
                && m["quota_used_local"].is_null()
                && m["quota_groups"]
                    .as_array()
                    .map(|a| a.is_empty())
                    .unwrap_or(true);
            let quota_stale = !account_only
                && (!live_error.is_empty()
                    || snap_at <= 0.0
                    || (now as f64) - snap_at > STALE_ALERT_SEC);
            let mut portions: Vec<Value> = Vec::new();
            let mut sum_left = 0.0f64;
            let mut has_left = false;
            let mut sum_total = 0.0f64;
            let mut has_total = false;
            // 这里只汇总数值，不算到期——到期判定统一放到下面 groups 之后那一处
            for c in &raw_credits {
                let l = c["left"].as_f64();
                let t = c["total"].as_f64();
                let e = c["expires_on"].as_str().unwrap_or("").trim().to_string();
                let d = days_left(&e);
                if let Some(x) = l {
                    sum_left += x;
                    has_left = true;
                }
                if let Some(x) = t {
                    sum_total += x;
                    has_total = true;
                }
                portions.push(json!({
                    "label": c["label"].as_str().unwrap_or("").to_string(),
                    "left": l,
                    "total": t,
                    "used": c["used"].as_f64(),
                    "category": c["category"].as_str().unwrap_or("").to_string(),
                    "unit": c["unit"].as_str().unwrap_or("credits").to_string(),
                    "expires_on": e,
                    "days_left": d,
                    "note": c["note"].as_str().unwrap_or("").to_string(),
                }));
            }
            // 大组（照 WorkBuddy 官方用量弹窗那三档）+ 组内逐笔明细。
            // 过期与否在这里现算，不用 agenthub 抓取那一刻留下的计数——
            // 一笔"今天 15:31 作废"的积分，15:32 打开界面就该已经在「已过期」里。
            let mut groups_out: Vec<Value> = Vec::new();
            for g in m.get("quota_groups").and_then(|v| v.as_array()).cloned().unwrap_or_default() {
                let mut items: Vec<Value> = Vec::new();
                let (mut live_left, mut live_n, mut dead_n, mut usable_n) =
                    (0.0f64, 0usize, 0usize, 0usize);
                let mut nearest = String::new();
                // 「最早那一刻会作废多少」——和这一档的总剩余是两件事。
                // 只写总量会被读成"这个时间点要作废 4,397 积分"，而真正到点作废的
                // 可能只有那 62.36 积分的一笔。同一时刻到期的几笔要合起来算。
                let mut nearest_left: Option<f64> = None;
                let mut nearest_unknown = false;
                for it in g["items"].as_array().cloned().unwrap_or_default() {
                    let e = it["expires_on"].as_str().unwrap_or("").trim().to_string();
                    let (dead, d) = crate::quota::expiry(&e);
                    let l = it["left"].as_f64();
                    if dead {
                        dead_n += 1;
                    } else {
                        live_n += 1;
                        live_left += l.unwrap_or(0.0);
                        // 「最近到期」只算**还没用完**的那几笔。WorkBuddy 实测踩过：
                        // 平台奖励积分 62 笔里最早那笔今天作废、但那笔早用光了，
                        // 卡上顶着红色「今天到期」，真正要盯的 3,694 积分其实还能用 20 天。
                        // left 没给（厂商不做逐笔剩余，如 AStudio 的获得明细）时按"还没用完"算。
                        let usable = l.is_none() || l.unwrap_or(0.0) > 0.0;
                        // 同一套写法（"YYYY-MM-DD HH:MM"）字典序就是时间序，取最早那笔
                        if usable && d.is_some() {
                            if nearest.is_empty() || e < nearest {
                                nearest = e.clone();
                                nearest_left = l;
                                nearest_unknown = l.is_none();
                            } else if e == nearest {
                                match (nearest_left, l) {
                                    (Some(x), Some(y)) => nearest_left = Some(x + y),
                                    // 有一笔没给逐笔剩余，这一档的到期金额就不能报确数
                                    (_, None) => nearest_unknown = true,
                                    (None, Some(_)) => {}
                                }
                            }
                        }
                        if usable {
                            usable_n += 1;
                        }
                    }
                    items.push(json!({
                        "label": it["label"].as_str().unwrap_or("").to_string(),
                        "pool": it["pool"].as_str().unwrap_or("").to_string(),
                        "detail": it["detail"].as_str().unwrap_or("").to_string(),
                        "left": l,
                        "total": it["total"].as_f64(),
                        "used": it["used"].as_f64(),
                        "unit": it["unit"].as_str().unwrap_or("credits").to_string(),
                        "expires_on": e,
                        "days_left": d,
                        "expired": dead,
                    }));
                }
                groups_out.push(json!({
                    // 档名与单位一律照厂商自己的（Trae 的「每日签到」、AStudio 的
                    // 「星火专属积分」、ZCode 的 token），界面不许再套固定三档
                    "name": g["name"].as_str().unwrap_or("其他").to_string(),
                    "unit": g["unit"].as_str().unwrap_or("credits").to_string(),
                    "scope": g["scope"].as_str().unwrap_or("").to_string(),
                    "left": g["left"].as_f64(),
                    "total": g["total"].as_f64(),
                    "used": g["used"].as_f64(),
                    "live_count": live_n,
                    "expired_count": dead_n,
                    "usable_count": usable_n,
                    "live_left": live_left,
                    "expires": nearest.clone(),
                    "expires_left": nearest_left,
                    "expires_left_unknown": nearest_unknown,
                    "days_left": days_left(&nearest),
                    "items": items,
                }));
            }
            // 明细与汇总数可能来自两次不同的读数：本机那条接口只有汇总数、逐笔到期只有官网有。
            // 「临期」和「最早到期」都长在明细那层，所以明细旧不旧必须先于它们算出来：
            // 明细停在两天前时还拿它逐笔报"今天 07:29 作废"，刷新只会改汇总数，
            // 这几句话一个字都不会动（WorkBuddy 实测）。
            let detail_stale = m["quota_detail_stale"].as_bool().unwrap_or(false);
            let top_left = m["credits_left"].as_f64();
            let mut earliest_dl: Option<i64> = None;
            let mut earliest_exp = String::new();
            if detail_stale {
                // 明细是旧读数：逐笔的"还剩多少 / 哪天作废"都不能当今天报。
                // 只按这一轮刚读到的汇总数报一条，到期日取汇总读数自己带的那个，没给就不报。
                let e = m["expires_on"].as_str().unwrap_or("").trim().to_string();
                let (dead, d) = crate::quota::expiry(&e);
                if !dead {
                    if let (Some(dd), Some(l)) = (d, top_left) {
                        earliest_dl = Some(dd);
                        earliest_exp = e.clone();
                        if !quota_stale && dd <= warn_days && l > 0.0 {
                            let at = fmt_min(m["quota_detail_at"].as_f64().unwrap_or(0.0));
                            alerts.push(json!({
                                "app": app.name, "app_id": app.id,
                                "name": m["name"], "days": dd, "left": l,
                                "unit": groups_out
                                    .first()
                                    .and_then(|g| g["unit"].as_str())
                                    .unwrap_or("credits"),
                                "expires": e.clone(),
                                "exp_left": Value::Null,
                                "exp_left_unknown": false,
                                "detail_stale": true,
                                "detail_at": m["quota_detail_at"],
                                "label": if at.is_empty() {
                                    "汇总读数（逐笔明细比汇总旧）".to_string()
                                } else {
                                    format!("汇总读数 · 逐笔明细停在 {}", at)
                                },
                                "level": if dd <= danger_days { "danger" } else { "warn" },
                            }));
                        }
                    }
                }
            } else if !groups_out.is_empty() {
                for g in &groups_out {
                    let e = g["expires"].as_str().unwrap_or("").trim().to_string();
                    // 这一档没有"还没用完又还没作废"的笔 → expires 是空，不出到期也不出提醒
                    let Some(d) = days_left(&e) else { continue };
                    sooner(&mut earliest_dl, &mut earliest_exp, d, &e);
                    if quota_stale || d > warn_days {
                        continue;
                    }
                    let left = match g["left"].as_f64() {
                        Some(v) => Some(v),
                        None => {
                            let ll = g["live_left"].as_f64().unwrap_or(0.0);
                            if ll > 0.0 { Some(ll) } else { None }
                        }
                    };
                    if left.map(|v| v <= 0.0).unwrap_or(false) {
                        continue;
                    }
                    alerts.push(json!({
                        "app": app.name, "app_id": app.id,
                        "name": m["name"], "days": d, "left": left,
                        "unit": g["unit"],
                        "expires": e.clone(),
                        "exp_left": g["expires_left"].as_f64(),
                        "exp_left_unknown": g["expires_left_unknown"].as_bool().unwrap_or(false),
                        "label": format!(
                            "{} {} 笔在效期 · 最早 {}",
                            g["name"].as_str().unwrap_or("积分"),
                            g["usable_count"].as_u64().unwrap_or(0),
                            e
                        ),
                        "level": if d <= danger_days { "danger" } else { "warn" },
                    }));
                }
            } else {
                // 只有厂商汇总桶的产品：临期按"到期日"合并成一条——一天一笔地弹
                // 会把真正要看的号埋掉，合并后仍说清"几包、共剩多少"。
                let mut soon: std::collections::BTreeMap<String, (i64, Option<f64>, usize)> =
                    Default::default();
                for p in &portions {
                    let e = p["expires_on"].as_str().unwrap_or("").trim().to_string();
                    let (dead, d) = crate::quota::expiry(&e);
                    let Some(d) = d else { continue };
                    // 桶级只到日，expiry 按当天 23:59 判作废：跨过去那一档就别再提醒了
                    if dead {
                        continue;
                    }
                    let l = p["left"].as_f64();
                    if !l.map(|v| v > 0.0).unwrap_or(true) {
                        continue;
                    }
                    sooner(&mut earliest_dl, &mut earliest_exp, d, &e);
                    if quota_stale || d > warn_days {
                        continue;
                    }
                    let en = soon.entry(e).or_insert((d, None, 0));
                    en.0 = en.0.min(d);
                    if let Some(x) = l {
                        en.1 = Some(en.1.unwrap_or(0.0) + x);
                    }
                    en.2 += 1;
                }
                for (exp, (dd, left, n)) in soon {
                    alerts.push(json!({
                        "app": app.name, "app_id": app.id,
                        "name": m["name"], "days": dd, "left": left,
                        // 这一档没有逐笔明细，按"到期日"合并出来的那几包本身就是当天会作废的量
                        "expires": exp.clone(),
                        "exp_left": left,
                        "exp_left_unknown": left.is_none(),
                        "label": format!("{} 笔到期 {}", n, exp),
                        "level": if dd <= danger_days { "danger" } else { "warn" },
                    }));
                }
            }
            // 明细与汇总数可能来自两次不同的读数：本机那条接口只有汇总数、逐笔到期只有官网有。
            // 这种时候大字跟最新的汇总数走，明细单独标"截至几点"——拿旧明细逐笔求和冒充
            // 现在的余额，就是把几小时前的数当今天报出去。
            let use_top = detail_stale && top_left.is_some();
            let (left, total, expires_on, dl) = if use_top {
                (top_left, m["credits_total"].as_f64(), earliest_exp, earliest_dl)
            } else if has_portions {
                (
                    if has_left { Some(sum_left) } else { None },
                    if has_total { Some(sum_total) } else { None },
                    earliest_exp,
                    earliest_dl,
                )
            } else if !groups_out.is_empty() {
                // 只给了分组、没给逐桶明细的产品：合计按组加，
                // 到期日直接沿用上面那一处（不再第三次算一遍，否则又会出现两个答案）
                let mut gl = 0.0f64;
                let mut gt = 0.0f64;
                for g in &groups_out {
                    gl += g["left"].as_f64().unwrap_or(0.0);
                    gt += g["total"].as_f64().unwrap_or(0.0);
                }
                (Some(gl), Some(gt), earliest_exp, earliest_dl)
            } else {
                let e = m["expires_on"].as_str().unwrap_or("");
                let (dead, d) = crate::quota::expiry(e);
                let l = m["credits_left"].as_f64();
                if !dead {
                    if let Some(dd) = d {
                        if !quota_stale && dd <= warn_days && l.map(|v| v > 0.0).unwrap_or(true) {
                            alerts.push(json!({
                                "app": app.name, "app_id": app.id,
                                "name": m["name"], "days": dd, "left": l,
                                "label": "",
                                "level": if dd <= danger_days { "danger" } else { "warn" },
                            }));
                        }
                    }
                }
                (l, m["credits_total"].as_f64(), e.to_string(), d)
            };
            instances.push(json!({
                "app": app.id, "app_name": app.name, "kind": app.kind,
                "name": m["name"], "workdir": m["workdir"].as_str().unwrap_or(""),
                "created": fmt_date(m["created"].as_f64().unwrap_or(0.0)),
                // 这个账号的数据是怎么来的：adopt=接管了本机原有档案 / copy=从别处复制 / fresh=空档
                "origin": m["origin"].as_str().unwrap_or(""),
                "seeded_from": m["seeded_from"].as_str().unwrap_or(""),
                // 原地账号：数据就住在程序默认档案位置，没搬家。界面据此不再摆「还原默认」
                "in_place": is_in_place(
                    &app.id,
                    std::path::Path::new(m["data_dir"].as_str().unwrap_or("")),
                ),
                // 已让位：这份档案现在登的是别的号，这张卡只留历史记录（额度还能看）
                "released": m["released"].as_bool().unwrap_or(false),
                "released_to": m["released_to"].as_str().unwrap_or(""),
                // 早期 adopt 搬进 instances/ 的那些：非空就表示可以一键搬回这个路径
                "rehome_to": rehome_slot(&app.id, &m)
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                "running": running, "procs": procs,
                // 进程还在但一个窗口都没有：不算开着，但会挡住换号/改名，需要给个说法
                "resident": resident,
                // 程序本身在不在跑：给"未运行但窗口其实开着（登的是别的号）"这种状态留个说法
                "app_running": !prog_pids.is_empty(),
                "swapped": swapped_here,
                // 客户端里登的就是这张卡：共用一份档案的产品用它当"当前账号"，
                // 磁盘上的换号记录不会随客户端登录变化，拿它当当前账号必然认错
                "logged_in": logged_in,
                // 这个号存过登录态没有：存过才能一键切回来，没存过界面只给"去登一次"
                "auth_pack": crate::authpack::stored(&app.id, m["name"].as_str().unwrap_or("")),
                "identity": identity::inspect(m["data_dir"].as_str().unwrap_or("")),
                "usage": m["usage"],
                "usage_fetched_at": m["usage_fetched_at"],
                // 今天签到状态：只读后台缓存，绝不在这里发网络请求（见 checkin::for_app）
                "checkin": crate::checkin::for_app(&app.id),
                "quota": {
                    "total": total, "left": left,
                    "expires_on": expires_on, "days_left": dl,
                    "credits": if has_portions { json!(portions) } else { json!([]) },
                    "groups": json!(groups_out),
                    // 官方用量弹窗顶部的套餐名（体验版 / 专业版），没有就空着不编
                    "plan_name": m["quota_plan_name"].as_str().unwrap_or(""),
                    // token 在保管箱里：锁定期间补不出原文，但「有没有配过」要照常显示
                    "has_token": !m["token"].as_str().unwrap_or("").trim().is_empty()
                        || m["token_held"].as_bool().unwrap_or(false),
                    "quota_url": !m["quota_url"].as_str().unwrap_or("").trim().is_empty(),
                    "adapter": m["quota_adapter"].as_str().unwrap_or(""),
                    "fetched_at": m["quota_fetched_at"],
                    // 本机直读绑定的账号：让用户一眼看出这个数是谁的
                    "local_account": m["quota_account"].as_str().unwrap_or(""),
                    // 绑的是哪个 userId：向导第 2 步靠它判断"刚登进来的号是不是已经有卡了"
                    "user_id": m["quota_user_id"].as_str().unwrap_or(""),
                    "local_used": m["quota_used_local"].as_f64().unwrap_or(0.0),
                    // 数据来源：server = 服务端实时；local-cache = 客户端旧缓存（stale=true 时不可信）
                    "source": m["quota_source"].as_str().unwrap_or(""),
                    "stale": quota_stale,
                    // 只有登录账号、没有余额接口的店（MiMo/Antigravity/Coze）：
                    // live_error 是解释不是警告，话术层据此换一套说法（见 quotaMiss/staleNote）
                    "account_only": account_only,
                    "live_error": live_error,
                    // 逐笔明细自己那份来源与时间：官网逐包读回来的明细会留着不动，而汇总数
                    // 每半小时被本机读数刷一次，界面必须能说出"明细是几点从哪读的"。
                    "detail_source": m["quota_detail_source"].as_str().unwrap_or(""),
                    "detail_at": m["quota_detail_at"],
                    "detail_stale": detail_stale,
                    // 官网会话掉了：明细再也读不到新的，界面要提示重新登录一次
                    "web_error": m["quota_web_error"].as_str().unwrap_or(""),
                    // 登录态实际读的档案文件：Qoder 0.4.x 换壳后，这是唯一能看出"读错档案"的线索
                    "auth_from": m["quota_auth_from"].as_str().unwrap_or(""),
                    // 这张卡的数属于谁 ≠ 本机现在登的是谁：卡片要标灰并给出"改绑/新建/换号"三个动作，
                    // 而不是像以前那样把新读数挡回去、让界面永远停在旧号
                    "bound_mismatch": crate::machine::mismatch(
                        m["quota_user_id"].as_str().unwrap_or(""),
                        &machine,
                    ),
                    // 按天消耗序列（波形图/热力图）：`{unit,days[],total,peak...}` 或 `{error}`；
                    // Null = 这个产品压根没有逐笔时刻可聚，界面就不许出现图占位
                    "trend": m["quota_trend"],
                    // CLI 类产品（Codex）的登录态：只读 auth.json（不跑子进程——
                    // build_state 每 4 秒一次，跑 `codex login status` 太频）。
                    // 官方那句话（"Logged in using …"）由前端按需调 codex_status 拿。
                    "cli_auth": cli_auth_info(app.id.as_str(), &m),
                },
            }));
        }
    }

    // 先按"几天后"排，同一天再按具体时刻排——同是 1 天后到期，0 点刷新的那笔要先看到
    alerts.sort_by(|a, b| {
        let ka = (
            a["days"].as_i64().unwrap_or(i64::MAX),
            exp_sort_key(a["expires"].as_str().unwrap_or("")),
        );
        let kb = (
            b["days"].as_i64().unwrap_or(i64::MAX),
            exp_sort_key(b["expires"].as_str().unwrap_or("")),
        );
        ka.cmp(&kb)
    });
    let procs = procs_rows(&snap);

    // 各家 Agent 的限时 / 每日福利（"Qoder 每天免费用 Qwen3.8-Max"这类）。
    // 官方没有接口，全靠手动登记一条；过了 ends_on 就归到"已过期"不再提示。
    let perk_name = |id: &str| {
        apps_out
            .iter()
            .find(|a| a["id"].as_str() == Some(id))
            .and_then(|a| a["name"].as_str())
            .unwrap_or("")
            .to_string()
    };
    let perks: Vec<Value> = crate::registry::load_perks()
        .into_iter()
        .map(|p| {
            let aid = p["app"].as_str().unwrap_or("").to_string();
            let e = p["ends_on"].as_str().unwrap_or("");
            let d = days_left(e);
            json!({
                "id": p["id"], "app": aid, "app_name": perk_name(&aid),
                "title": p["title"], "detail": p["detail"], "ends_on": e,
                "daily": p["daily"].as_bool().unwrap_or(false),
                "url": p["url"].as_str().unwrap_or("").to_string(),
                "days_left": d,
                "live": d.map(|x| x >= 0).unwrap_or(true),
            })
        })
        .collect();
    let perks_live = perks
        .iter()
        .filter(|p| p["live"].as_bool().unwrap_or(false))
        .count();

    // 被屏蔽的程序：只在设置里露出，可一键恢复
    let hidden_set = crate::registry::hidden();
    let hidden_out: Vec<Value> = all_apps_raw()
        .into_iter()
        .filter(|a| hidden_set.iter().any(|h| h == &a.id))
        .map(|a| json!({ "id": a.id, "name": a.name, "discovered": a.discovered,
                         "exe": a.exes.first().cloned().unwrap_or_default() }))
        .collect();

    let n_removed = apps_out
        .iter()
        .filter(|a| a["removed"].as_bool().unwrap_or(false))
        .count();

    json!({
        "apps": apps_out,
        "selfcheck": crate::swap::last_report(),
        "hidden_apps": hidden_out,
        // 「显示未安装的程序」开关（存 config.json，不是 settings.json）
        "show_uninstalled": crate::registry::show_uninstalled(),
        "instances": instances,
        "procs": procs,
        "alerts": alerts,
        "perks": perks,
        // 每日签到整份缓存（后台循环负责刷新，这里一律不发网络请求）
        "checkin": crate::checkin::snapshot(),
        // 「一键刷新全部额度」的后台任务进度（没在跑时是 Null）
        "alljob": crate::commands::alljob(),
        "stats": {
            "instances": instances.len(),
            // 在跑的程序数：自动发现的程序没有实例，按实例数算会永远显示 0。
            // 只算有可见窗口的——关掉 Electron 窗口后进程常常还驻留一会儿，那不算"开着"
            "running": procs.iter()
                .filter(|r| r["windows"].as_u64().unwrap_or(1) > 0)
                .count(),
            "running_instances": n_running,
            "agents": apps_out.len(), "auto": discovered().len(),
            "removed": n_removed,
            "perks_live": perks_live,
        },
        "proxy": {
            "running": proxy.is_running(),
            "port": if proxy.is_running() { proxy.port() } else { PROXY_PORT },
            "base_url": format!(
                "http://127.0.0.1:{}/v1",
                if proxy.is_running() { proxy.port() } else { PROXY_PORT }
            ),
            "upstream": upstream, "strategy": strategy,
            "pool": pool_labels(),
            // 模型反代：上游组（谁进哪个组）+ 接入密钥（谁能用哪些组）
            "pools": crate::gateway::pools_view(&upstream),
            "dist": crate::gateway::list_dist_masked(),
            "members": crate::gateway::member_options(),
            // Qoder / Qoder CN 模型反代：账号状态、能不能进组、一键反代的结果
            "qproxy": crate::qproxy::view(),
            // Trae CN：这里**只读缓存**（4 秒轮询不起 python）；刷新走 trae_refresh 动作
            "traeproxy": crate::traeproxy::view(),
            // WorkBuddy 国内版 / 国际版：账号状态、全量模型清单、护栏、最近日志
            "wbproxy": crate::wbproxy::view(),
            // AStudio：模型令牌读 astron-session.json 的 modelBearerToken，
            // 直连讯飞云端 model-gateway（OpenAI 兼容，不需要协议翻译）
            "astudio": crate::asproxy::view(),
            "logs": proxy.recent_logs(30),
        },
        "now": chrono::Local::now().format("%H:%M:%S").to_string(),
        "settings": st,
        "app_version": env!("CARGO_PKG_VERSION"),
        // 界面要能说清"数据到底在哪"，别让文案里写死 ~/.agent-cockpit
        "data_root": crate::registry::base_dir().to_string_lossy(),
        "vault": crate::secrets::status_json(),
        "keys": crate::keys::list_masked(),
        "backups": crate::space::list_backups(),
        "scan": {
            "auto": discovered().len(),
            "at": crate::commands::scan_stamp(),
        },
    })
}

/// `exp_sort_key()` 同一段后面。CLI 产品（Codex）的登录态摘要：
/// 只解 auth.json，不跑子进程——4 秒一轮的 build_state 经不起每条都 spawn。
/// 返回 `{"mode","key","account","has_auth"}`，非 CLI 产品或没有文件时给 Null。
fn cli_auth_info(app_id: &str, m: &Value) -> Value {
    if app_id != "codex" {
        return Value::Null;
    }
    let dir = m["data_dir"].as_str().unwrap_or("").trim();
    if dir.is_empty() {
        return Value::Null;
    }
    let f = std::path::Path::new(dir).join("auth.json");
    let txt = match std::fs::read_to_string(&f) {
        Ok(t) => t,
        Err(_) => return json!({ "has_auth": false }),
    };
    let v: Value = match serde_json::from_str(&txt) {
        Ok(v) => v,
        Err(_) => return json!({ "has_auth": false }),
    };
    let mode = v["auth_mode"].as_str().unwrap_or("").to_string();
    let key = v["OPENAI_API_KEY"]
        .as_str()
        .map(|k| {
            let n = k.chars().count();
            if n <= 12 {
                "***".to_string()
            } else {
                let head: String = k.chars().take(7).collect();
                let tail: String = k.chars().skip(n - 4).collect();
                format!("{head}***{tail}")
            }
        })
        .unwrap_or_default();
    json!({ "has_auth": true, "mode": mode, "key": key })
}

fn pool_labels() -> Vec<String> {
    let cfg = crate::registry::load_config();
    let upstream = cfg["proxy"]["upstream"]
        .as_str()
        .unwrap_or("https://api.openai.com")
        .to_string();
    crate::proxy::pool_labels(&upstream)
}

#[cfg(test)]
mod tests {
    use super::exp_sort_key;

    /// 同一天里"0 点刷新"的那笔必须排在 22:18 那笔前面：千问每日积分 00:00 就作废，
    /// WorkBuddy 的裂变包要到 22:18——两者都是"1 天后到期"，只按天数排会把先作废的压后。
    #[test]
    fn exp_sort_key_orders_same_day_by_time() {
        assert!(exp_sort_key("2026-10-19 00:00") < exp_sort_key("2026-10-19 22:18"));
    }

    /// 只给到日的按"那天整天还能用"处理（23:59），与 quota::expiry 同一口径
    #[test]
    fn exp_sort_key_pads_date_only() {
        assert_eq!(exp_sort_key("2026-10-19"), "2026-10-19 23:59");
        assert!(exp_sort_key("2026-10-19 22:18") < exp_sort_key("2026-10-19"));
    }

    /// 没给到期日的排最后，不许冒充"最近到期"
    #[test]
    fn exp_sort_key_puts_unknown_last() {
        assert!(exp_sort_key("2026-12-31") < exp_sort_key("未给出"));
        assert!(exp_sort_key("2026-12-31") < exp_sort_key(""));
    }
}
