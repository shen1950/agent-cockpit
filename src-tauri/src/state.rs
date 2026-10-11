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

fn fmt_date(ts: f64) -> String {
    chrono::DateTime::from_timestamp(ts as i64, 0)
        .map(|d| d.with_timezone(&chrono::Local).format("%Y-%m-%d").to_string())
        .unwrap_or_default()
}

/// 该 Agent 能否真正多开：先看静态判定，再看实测结论（启动后校验失败会写进 app_caps）
fn isolate_ok(cfg: &Value, app: &crate::registry::AppDef) -> bool {
    if app.kind == "cli" {
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
        // 程序级：这个 exe 到底有没有在跑。不能多开的产品只能按这个粒度判定运行状态
        let prog_pids = if app.kind == "electron" || app.kind == "plain" {
            program_pids(&exe_name(&app).to_lowercase(), &snap)
        } else {
            Vec::new()
        };
        let insts = load_instances(&app.id);
        let inst_names: Vec<&str> = insts.iter().filter_map(|m| m["name"].as_str()).collect();
        // 「这台机器现在登的是谁」——产品级的一份真值，和卡片上绑的账号是两件事。
        // 不摆出来，就出现"人早换成另一个号了，卡片还停在旧号"这种看着像 bug 的错。
        let machine = crate::machine::current(&app.id);
        apps_out.push(json!({
            "id": app.id, "name": app.name, "short": reg_short_of(&app), "kind": app.kind,
            "installed": installed, "swapped": swapped,
            "machine": machine,
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
            // 这个账号名下的进程：先按实例目录精确匹配；匹配不到而它又是当前换号账号，
            // 才按程序级算（换号产品的进程命令行里只有默认档案路径）
            let owned: Vec<u32> = match app.kind.as_str() {
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
            // 旧缓存（典型：Qoder 登录令牌过期后只剩 8 月的快照）不能当实时数用，
            // 也不能拿它去报"临期"——否则会弹出一堆早已作废的到期提醒。
            let live_error = m["quota_live_error"].as_str().unwrap_or("").trim().to_string();
            let quota_stale = !live_error.is_empty()
                && m["quota_source"].as_str().unwrap_or("") != "server";
            let mut portions: Vec<Value> = Vec::new();
            // 临期提醒按"到期日"合并成一条：官网逐资源包能列出二十几笔，
            // 一天一笔地弹会把真正要看的号埋掉；合并后仍说清"几包、共剩多少"。
            let mut soon: std::collections::BTreeMap<String, (i64, f64, usize)> = Default::default();
            let mut sum_left = 0.0f64;
            let mut has_left = false;
            let mut sum_total = 0.0f64;
            let mut has_total = false;
            let mut earliest_dl: Option<i64> = None;
            let mut earliest_exp = String::new();
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
                // 卡片大字的「N 天后过期」同理会跳过已用光的那笔（见下面 groups 那段）
                if let Some(x) = d {
                    let usable = l.is_none() || l.unwrap_or(0.0) > 0.0;
                    if usable && earliest_dl.map(|p| x < p).unwrap_or(true) {
                        earliest_dl = Some(x);
                        earliest_exp = e.clone();
                    }
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
                // 旧缓存除外：那是过期的数，提醒只会误导
                if let Some(dd) = d {
                    if !quota_stale && dd <= warn_days && l.map(|v| v > 0.0).unwrap_or(true) {
                        let e = soon.entry(e.clone()).or_insert((dd, 0.0, 0));
                        e.0 = e.0.min(dd);
                        e.1 += l.unwrap_or(0.0);
                        e.2 += 1;
                    }
                }
            }
            for (exp, (dd, left, n)) in &soon {
                alerts.push(json!({
                    "app": app.name, "app_id": app.id,
                    "name": m["name"], "days": dd, "left": left,
                    "label": format!("{} 笔到期 {}", n, exp),
                    "level": if *dd <= danger_days { "danger" } else { "warn" },
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
                        if usable && d.is_some() && (nearest.is_empty() || e < nearest) {
                            nearest = e.clone();
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
                    "days_left": days_left(&nearest),
                    "items": items,
                }));
            }
            // 明细与汇总数可能来自两次不同的读数：本机那条接口只有汇总数、逐笔到期只有官网有。
            // 这种时候大字跟最新的汇总数走，明细单独标"截至几点"——拿旧明细逐笔求和冒充
            // 现在的余额，就是把几小时前的数当今天报出去。
            let detail_stale = m["quota_detail_stale"].as_bool().unwrap_or(false);
            let top_left = m["credits_left"].as_f64();
            let use_top = detail_stale && has_portions && top_left.is_some();
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
                // 只给了分组、没给逐桶明细的产品：合计按组加
                let mut gl = 0.0f64;
                let mut gt = 0.0f64;
                let mut gd: Option<i64> = None;
                let mut ge = String::new();
                for g in &groups_out {
                    gl += g["left"].as_f64().unwrap_or(0.0);
                    gt += g["total"].as_f64().unwrap_or(0.0);
                    let e = g["expires"].as_str().unwrap_or("");
                    if let Some(d) = days_left(e) {
                        if gd.map(|p| d < p).unwrap_or(true) {
                            gd = Some(d);
                            ge = e.to_string();
                        }
                    }
                }
                (Some(gl), Some(gt), ge, gd)
            } else {
                let e = m["expires_on"].as_str().unwrap_or("");
                let d = days_left(e);
                let l = m["credits_left"].as_f64();
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
                "identity": identity::inspect(m["data_dir"].as_str().unwrap_or("")),
                "usage": m["usage"],
                "usage_fetched_at": m["usage_fetched_at"],
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
                },
            }));
        }
    }

    alerts.sort_by(|a, b| a["days"].as_i64().unwrap_or(0).cmp(&b["days"].as_i64().unwrap_or(0)));
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
        "instances": instances,
        "procs": procs,
        "alerts": alerts,
        "perks": perks,
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
            "upstream": upstream, "strategy": strategy,
            "pool": pool_labels(),
            // 反代编制：池子（谁进哪个池）+ 分发出去的 Key（谁能用哪些池）
            "pools": crate::gateway::pools_view(&upstream),
            "dist": crate::gateway::list_dist_masked(),
            "members": crate::gateway::member_options(),
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

fn pool_labels() -> Vec<String> {
    let cfg = crate::registry::load_config();
    let upstream = cfg["proxy"]["upstream"]
        .as_str()
        .unwrap_or("https://api.openai.com")
        .to_string();
    crate::proxy::pool_labels(&upstream)
}
