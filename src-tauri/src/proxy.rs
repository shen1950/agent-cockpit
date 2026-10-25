// 本地 OpenAI 兼容反代：按来源池轮询替换 Authorization，成功响应流式转发（保留 SSE）
// 请求带的 key 决定能用哪些上游组：命中下发的 key 就只在那几个上游组里挑上游；
// 不认识的 key 退回"全部启用的上游组"（兼容老的用法）。
use crate::gateway::{self, Upstream};
use crate::registry::{all_apps, load_instances};
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::io::Read;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use tiny_http::{Header, Response, Server, StatusCode};

#[derive(Default)]
pub struct Proxy {
    running: AtomicBool,
    idx: AtomicUsize,
    port: AtomicU16,
    upstream: Mutex<String>,
    strategy: Mutex<String>,
    logs: Mutex<VecDeque<Value>>,
    server: Mutex<Option<Arc<Server>>>,
}

/// 兼容保留：老式（无上游组配置）时的成员清单 = 所有登记过 token 的账号 + 勾了「进反代」的 Key
pub fn legacy_pool(global_upstream: &str) -> Vec<Upstream> {
    let mut out = Vec::new();
    for app in all_apps() {
        for m in load_instances(&app.id) {
            let tok = m["token"].as_str().unwrap_or("").trim().to_string();
            if tok.is_empty() {
                continue;
            }
            let own = m["proxy_base_url"].as_str().unwrap_or("").trim().to_string();
            out.push(Upstream {
                label: format!("{}/{}", app.name, m["name"].as_str().unwrap_or("")),
                token: tok,
                base_url: if own.is_empty() { global_upstream.to_string() } else { own },
                pool: "（未分池）".into(),
            });
        }
    }
    for (label, tok) in crate::keys::proxy_tokens() {
        out.push(Upstream {
            label,
            token: tok,
            base_url: global_upstream.to_string(),
            pool: "（未分池）".into(),
        });
    }
    out
}

/// 一次请求的路由结果
pub struct Route {
    pub ups: Vec<Upstream>,
    pub dist_id: String,    // 命中的下发 key id（空 = 未识别，不计数）
    pub via: String,        // 展示用：key 名（授权上游组）
    pub disabled: bool,     // 命中的 key 被停用
}

/// 按请求带来的 key 决定这次能用哪些上游。
/// 没配上游组就退回老行为（所有登记 token 的账号 + 勾选进反代的 Key 一起轮询）。
pub fn pick_upstreams(incoming: &str, global_upstream: &str) -> Route {
    let empty_route = |ups, dist_id: &str, via: String, disabled| Route {
        ups,
        dist_id: dist_id.to_string(),
        via,
        disabled,
    };
    let pools = gateway::pools();
    if pools.is_empty() {
        return empty_route(legacy_pool(global_upstream), "", String::new(), false);
    }
    if let Some(d) = gateway::find_dist(incoming) {
        let name = d["name"].as_str().unwrap_or("未命名").to_string();
        let did = d["id"].as_str().unwrap_or("").to_string();
        if !d["enabled"].as_bool().unwrap_or(true) {
            return empty_route(Vec::new(), &did, name, true);
        }
        let ids: Vec<String> = d["pools"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .iter()
            .filter_map(|v| v.as_str().map(|s| s.to_string()))
            .collect();
        let ups = gateway::upstreams_for(&ids, global_upstream);
        let pool_names: Vec<String> = if ids.is_empty() {
            vec!["全部上游组".into()]
        } else {
            pools
                .iter()
                .filter(|p| ids.iter().any(|i| i == p["id"].as_str().unwrap_or("")))
                .map(|p| p["name"].as_str().unwrap_or("").to_string())
                .collect()
        };
        let via = format!("{}（{}）", name, pool_names.join("、"));
        return empty_route(ups, &did, via, false);
    }
    // 不认识的 key：所有启用的上游组（便于本机自用，也不破坏老配置）
    empty_route(gateway::upstreams_for(&[], global_upstream), "", String::new(), false)
}

/// 展示用：当前反代可路由到的上游清单（标签）。
/// 有上游组配置时按上游组汇总；没配上游组时退回老式成员清单，界面显示口径不变。
pub fn pool_labels(global_upstream: &str) -> Vec<String> {
    let ups = if gateway::pools().is_empty() {
        legacy_pool(global_upstream)
    } else {
        gateway::upstreams_for(&[], global_upstream)
    };
    let mut out: Vec<String> = Vec::new();
    for u in ups {
        // 有上游组时标签带上池名，一眼能看出这个账号挂在哪个池
        let label = if u.pool == "（未分池）" {
            u.label
        } else {
            format!("{} · {}", u.pool, u.label)
        };
        if !out.iter().any(|x| x == &label) {
            out.push(label);
        }
    }
    out
}

fn header(name: &str, value: &str) -> Header {
    Header::from_bytes(name.as_bytes(), value.as_bytes())
        .unwrap_or_else(|_| Header::from_bytes(&b"X-Cockpit"[..], &b"bad-header"[..]).unwrap())
}

impl Proxy {
    pub fn shared() -> Arc<Proxy> {
        Arc::new(Proxy::default())
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::Relaxed)
    }

    /// 实际监听端口；未运行时为 0
    pub fn port(&self) -> u16 {
        self.port.load(Ordering::Relaxed)
    }

    pub fn config(&self) -> (String, String) {
        (
            self.upstream.lock().unwrap().clone(),
            self.strategy.lock().unwrap().clone(),
        )
    }

    pub fn set_config(&self, upstream: Option<&str>, strategy: Option<&str>) {
        if let Some(u) = upstream {
            if !u.trim().is_empty() {
                *self.upstream.lock().unwrap() = u.trim().to_string();
            }
        }
        if let Some(s) = strategy {
            if !s.is_empty() {
                *self.strategy.lock().unwrap() = s.to_string();
            }
        }
    }

    pub fn recent_logs(&self, n: usize) -> Vec<Value> {
        self.logs.lock().unwrap().iter().rev().take(n).cloned().collect()
    }

    fn push_log(
        &self,
        method: &str,
        path: &str,
        model: Option<String>,
        status: u16,
        account: &str,
        pool: &str,
        via: &str,
    ) {
        if let Some(m) = &model {
            if m != "-" {
                model_stats_bump(m, status);
            }
        }
        let mut g = self.logs.lock().unwrap();
        g.push_back(json!({
            "ts": chrono::Local::now().format("%H:%M:%S").to_string(),
            "method": method,
            "path": if path.chars().count() > 60 { path.chars().take(60).collect::<String>() } else { path.to_string() },
            "model": model.unwrap_or_else(|| "-".into()),
            "status": status,
            "account": account,
            "pool": pool,
            "via": via,          // 用了哪个下发的 key；空 = 未识别的 key
        }));
        while g.len() > 100 {
            g.pop_front();
        }
    }

    pub fn start(self: &Arc<Self>, port: u16) -> (bool, String) {
        if self.is_running() {
            return (false, "反代已在运行".into());
        }
        let server = match Server::http(("127.0.0.1", port)) {
            Ok(s) => Arc::new(s),
            Err(e) => return (false, format!("端口 {} 占用：{}", port, e)),
        };
        *self.server.lock().unwrap() = Some(server.clone());
        self.port.store(port, Ordering::Relaxed);
        self.running.store(true, Ordering::Relaxed);

        let inner = self.clone();
        thread::spawn(move || {
            while inner.is_running() {
                let req = match server.recv_timeout(std::time::Duration::from_millis(400)) {
                    Ok(Some(r)) => r,
                    Ok(None) => continue,
                    Err(_) => break,
                };
                let worker = inner.clone();
                thread::spawn(move || worker.serve(req));
            }
            // 线程退出时释放最后一个 Server 引用，监听套接字随之关闭
        });
        (true, format!("反代已启动 127.0.0.1:{}", port))
    }

    pub fn stop(&self) -> (bool, String) {
        if !self.is_running() {
            return (false, "反代未运行".into());
        }
        self.running.store(false, Ordering::Relaxed);
        if let Some(s) = self.server.lock().unwrap().take() {
            s.unblock();
        }
        (true, "反代已停止".into())
    }

    fn serve(&self, req: tiny_http::Request) {
        let method = req.method().as_str().to_string();
        let path = req.url().to_string();

        let mut fwd: Vec<(String, String)> = Vec::new();
        // 客户端带来的 key：决定这次能用哪些上游组（之后会被换成上游自己的凭据）
        let mut incoming_key = String::new();
        for h in req.headers() {
            let name = h.field.as_str().as_str().to_lowercase();
            let val = h.value.as_str().to_string();
            if name == "authorization" {
                incoming_key = val
                    .strip_prefix("Bearer ")
                    .or_else(|| val.strip_prefix("bearer "))
                    .unwrap_or(&val)
                    .trim()
                    .to_string();
                continue;
            }
            if name == "x-api-key" {
                if incoming_key.is_empty() {
                    incoming_key = val.trim().to_string();
                }
                continue;
            }
            if matches!(
                name.as_str(),
                "host" | "content-length" | "connection" | "accept-encoding"
            ) {
                continue;
            }
            fwd.push((name, val));
        }

        if method == "OPTIONS" {
            let _ = req.respond(
                Response::new_empty(StatusCode(204))
                    .with_header(cors_header())
                    .with_header(header(
                        "Access-Control-Allow-Headers",
                        "authorization,content-type,x-api-key,openai-organization",
                    ))
                    .with_header(header(
                        "Access-Control-Allow-Methods",
                        "POST, GET, PUT, DELETE, OPTIONS",
                    ))
                    .with_header(header("Access-Control-Max-Age", "86400")),
            );
            return;
        }

        let mut body = Vec::new();
        let mut req = req;
        let _ = req.as_reader().read_to_end(&mut body);
        let model = serde_json::from_slice::<Value>(&body)
            .ok()
            .and_then(|v| v["model"].as_str().map(|s| s.to_string()));

        let (global_upstream, strategy) = self.config();
        let route = pick_upstreams(&incoming_key, &global_upstream);
        let mut ups = route.ups.clone();
        let via = route.via.clone();
        if route.disabled {
            self.push_log(&method, &path, model, 403, "-", "-", &via);
            let _ = req.respond(text_response(
                403,
                &json!({ "error": format!("这个接入密钥「{}」已停用", via) }).to_string(),
            ));
            return;
        }
        // deny-by-default：没带 key / key 不认识一律 401。
        // 老行为（回落到「全部启用的上游组」）只在设置里打开 lan_loose 时保留 ——
        // 那个默认值意味着浏览器里任何网页都能借她的 token 池打上游。
        if route.dist_id.is_empty() && !crate::wbproxy::loose_mode() {
            self.push_log(&method, &path, model, 401, "-", "-", "");
            let _ = req.respond(text_response(
                401,
                &json!({ "error": {
                    "message": "这个 key 不认识。用「本地反代」页里下发的那把 key；\
                                本机自用不想带 key，就去设置里打开「本机宽松模式」。",
                    "type": "invalid_api_key"
                }})
                .to_string(),
            ));
            return;
        }
        // 模型清单本地应答：别的 Agent 接入时基本都先拉 /v1/models，
        // 转发给 WorkBuddy/Trae 的上游必然 404，用户只会看到「填了地址连不上」。
        if method == "GET" && (path == "/v1/models" || path == "/models") {
            let data = models_for(&ups);
            self.push_log(&method, &path, None, 200, "-", "-", &via);
            let _ = req.respond(text_response(
                200,
                &json!({ "object": "list", "data": data }).to_string(),
            ));
            return;
        }
        // 模型带 Agent 前缀（如 trae/glm-5.3-flash）：只在属于这一家的上游里挑，
        // 指向永远确定。前缀没授权/没账号 → 直接拒绝，别漂到别的 Agent 去。
        if let Some((owner, bare)) = model.as_deref().and_then(split_namespaced) {
            let filtered: Vec<Upstream> = ups
                .iter()
                .filter(|u| owner_of_base(&u.base_url) == Some(owner))
                .cloned()
                .collect();
            if filtered.is_empty() {
                self.push_log(&method, &path, model.clone(), 404, "-", "-", &via);
                let _ = req.respond(text_response(
                    404,
                    &json!({ "error": {
                        "message": format!(
                            "模型前缀 {owner}/ 没有可用上游：这把接入密钥没授权 {owner} 的上游组，或这家没有可用账号"
                        ),
                        "type": "model_not_found"
                    }})
                    .to_string(),
                ));
                return;
            }
            ups = filtered;
            // 报文里的模型名剥成裸名（上游只认裸名）；`model` 保持带前缀的形式，
            // 让日志和逐模型实测按「Agent/模型」分开记。
            if let Ok(v) = serde_json::from_slice::<Value>(&body) {
                if v["model"].as_str().is_some() {
                    let mut v2 = v.clone();
                    v2["model"] = json!(bare);
                    body = serde_json::to_vec(&v2).unwrap_or(body);
                }
            }
        }
        if ups.is_empty() {
            let hint = if via.is_empty() {
                "没挑到上游：到「反代编制」里建一个上游组，把要用的账号/Key 勾进去；\
                 或者直接用上游组生成的「接入密钥」调用。".to_string()
            } else {
                format!("接入密钥「{}」对应的上游组里没有可用成员（账号没登记 Token，或成员被停用）", via)
            };
            self.push_log(&method, &path, model, 502, "无可用上游", "-", &via);
            let _ = req.respond(text_response(502, &json!({ "error": hint }).to_string()));
            return;
        }
        let i = if strategy == "random" {
            (std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos() as usize)
                .unwrap_or(0))
                % ups.len()
        } else {
            self.idx.fetch_add(1, Ordering::Relaxed) % ups.len()
        };
        if crate::wbproxy::is_wb_base(&ups[i].base_url) {
            self.serve_wb(req, &method, &path, &body, &fwd, &ups, i, &route, model.clone());
            return;
        }
        let up = ups[i].clone();
        if crate::traeproxy::is_trae_base(&up.base_url) {
            self.serve_trae(req, &method, &path, &body, &up, &via, &route.dist_id, model.clone());
            return;
        }
        let url = crate::qproxy::upstream_url(&up.base_url, &path);
        let t0 = std::time::Instant::now();
        let qoder = crate::qproxy::is_qoder_base(&up.base_url);

        // 模型名归一：客户端（WorkBuddy / Cherry Studio / 各家 Agent）发的是自己的模型名，
        // Qoder 不认时仍然回 200，正文却是一帧 SSE `event: error`——客户端读不到字节就直接
        // 把这一轮判成"用户已取消"。所以对 Qoder 上游先把名字收到 auto，日志里记下改了什么。
        let mut model_log = model.clone();
        if qoder && !body.is_empty() {
            if let Ok(v) = serde_json::from_slice::<Value>(&body) {
                if let Some(m) = v["model"].as_str() {
                    let (nm, changed) = crate::qproxy::normalize_model(m);
                    if changed {
                        let mut v2 = v.clone();
                        v2["model"] = json!(nm);
                        body = serde_json::to_vec(&v2).unwrap_or(body);
                        model_log = Some(format!("{}→{}", m, nm));
                    }
                }
            }
        }

        let mut call = ureq::request(&method, &url).timeout(std::time::Duration::from_secs(300));
        for (k, v) in &fwd {
            call = call.set(k, v);
        }
        call = call
            .set("Authorization", &format!("Bearer {}", up.token))
            .set("User-Agent", "agent-cockpit/2.0");
        let res = if body.is_empty() {
            call.call()
        } else {
            call.send(&body[..])
        };

        match res {
            Ok(resp) => {
                let status = resp.status() as u16;
                let ctype = resp
                    .header("Content-Type")
                    .unwrap_or("application/json")
                    .to_string();
                let mut rd = resp.into_reader();
                // Qoder 上游的"模型不支持/参数不认"是 200 + 流量内错误帧，不是 4xx。
                // 先看一眼开头几百字节：认出来就把话说清楚，别让客户端只显示"已取消"。
                if qoder {
                    let mut head = Vec::new();
                    let _ = rd.by_ref().take(600).read_to_end(&mut head);
                    if let Some(msg) = crate::qproxy::sse_error_message(&head) {
                        self.push_log(&method, &path, model_log.clone(), 502, &up.label, &up.pool, &via);
                        crate::qproxy::note_by_base(&up.base_url, 502, t0.elapsed().as_millis() as u64, &up.label);
                        let hint = json!({
                            "error": {
                                "message": format!("Qoder 上游拒了这条请求：{}", msg),
                                "type": "upstream_error",
                                "hint": "模型名已经按 auto 归一（可用：auto、gpt-5）；若改过还报，多为额度/限流，稍后重试",
                            }
                        });
                        let _ = req.respond(text_response(502, &hint.to_string()));
                        return;
                    }
                    self.push_log(&method, &path, model_log, status, &up.label, &up.pool, &via);
                    crate::qproxy::note_by_base(&up.base_url, status, t0.elapsed().as_millis() as u64, &up.label);
                    if !route.dist_id.is_empty() {
                        gateway::bump_dist(&route.dist_id, &up.label);
                    }
                    let response = Response::new(
                        StatusCode(status),
                        vec![header("Content-Type", &ctype), cors_header()],
                        std::io::Cursor::new(head).chain(rd),
                        None,
                        None,
                    );
                    let _ = req.respond(response);
                    return;
                }
                self.push_log(&method, &path, model_log, status, &up.label, &up.pool, &via);
                // Qoder 系的上游按区域记账（成功/失败、延迟、最近错误），界面才有"状态可观测"
                crate::qproxy::note_by_base(&up.base_url, status, t0.elapsed().as_millis() as u64, &up.label);
                if !route.dist_id.is_empty() {
                    gateway::bump_dist(&route.dist_id, &up.label);
                }
                let response = Response::new(
                    StatusCode(status),
                    vec![
                        header("Content-Type", &ctype),
                        cors_header(),
                    ],
                    rd,
                    None,
                    None,
                );
                let _ = req.respond(response);
            }
            Err(ureq::Error::Status(code, resp)) => {
                let ctype = resp
                    .header("Content-Type")
                    .unwrap_or("application/json")
                    .to_string();
                let mut buf = Vec::new();
                let _ = resp.into_reader().read_to_end(&mut buf);
                self.push_log(&method, &path, model, code as u16, &up.label, &up.pool, &via);
                crate::qproxy::note_by_base(&up.base_url, code as u16, t0.elapsed().as_millis() as u64, &up.label);
                let _ = req.respond(
                    Response::new(
                        StatusCode(code as u16),
                        vec![header("Content-Type", &ctype), cors_header()],
                        std::io::Cursor::new(buf),
                        None,
                        None,
                    ),
                );
            }
            Err(e) => {
                self.push_log(&method, &path, model, 502, &up.label, &up.pool, &via);
                crate::qproxy::note_by_base(&up.base_url, 502, t0.elapsed().as_millis() as u64, &up.label);
                let _ = req.respond(text_response(
                    502,
                    &json!({ "error": e.to_string() }).to_string(),
                ));
            }
        }
    }

    /// Trae 上游：OpenAI 进、SOLO 出、再把私有 SSE 翻回 OpenAI SSE。协议见 traeproxy.rs。
    ///
    /// 两条与其余上游不同、必须写在这里的行为：
    ///   * 请求要带 `X-User-Real-IP` = 本机出口 IP；4011 时重算一次再试，**只重试一次**。
    ///   * 流是**边读边翻**的，所以 usage/provider 要等 `respond` 返回（流写完）后才拿得到。
    fn serve_trae(
        &self,
        mut req: tiny_http::Request,
        method: &str,
        path: &str,
        body: &[u8],
        up: &crate::gateway::Upstream,
        via: &str,
        dist_id: &str,
        model: Option<String>,
    ) {
        // 模型清单由本地目录直接给，不打上游
        if method == "GET" && path.trim_end_matches('/').ends_with("models") {
            let list = crate::registry::profile_dir("traework-cn")
                .and_then(|d| crate::traeproxy::models_cached(&d))
                .map(|(ch, _)| crate::traeproxy::model_list(&ch))
                .unwrap_or_default();
            let _ = req.respond(text_response(
                200,
                &json!({ "object": "list", "data": list }).to_string(),
            ));
            return;
        }

        let Ok(v) = serde_json::from_slice::<Value>(body) else {
            let _ = req.respond(text_response(
                400,
                &json!({ "error": { "message": "请求体不是 JSON" } }).to_string(),
            ));
            return;
        };
        let want = v["model"].as_str().unwrap_or("").trim().to_string();
        if want.is_empty() {
            let _ = req.respond(text_response(
                400,
                &json!({ "error": { "message": "缺 model" } }).to_string(),
            ));
            return;
        }
        let channels = crate::registry::profile_dir("traework-cn")
            .and_then(|d| crate::traeproxy::models_cached(&d))
            .map(|(c, _)| c)
            .filter(|c| c.as_object().map(|o| !o.is_empty()).unwrap_or(false));
        let Some(channels) = channels else {
            let _ = req.respond(text_response(
                503,
                &json!({ "error": { "message": "Trae 模型目录还没加载：先点一次「刷新 Trae」" } })
                    .to_string(),
            ));
            return;
        };
        let Some(pick) = crate::traeproxy::channel_for(&channels, &want) else {
            let _ = req.respond(text_response(
                400,
                &json!({ "error": { "message": format!("Trae 目录里没有模型 {want}") } }).to_string(),
            ));
            return;
        };
        let (channel, model_name) = match pick.split_once('|') {
            Some((c, m)) => (c.to_string(), m.to_string()),
            None => (pick.clone(), want.clone()),
        };
        let solo = crate::traeproxy::translate_request(&v, &channel, &model_name);

        for attempt in 0..2 {
            let ip = if attempt == 0 {
                crate::traeproxy::egress_ip()
            } else {
                crate::traeproxy::invalidate_ip();
                crate::traeproxy::egress_ip_fresh()
            };
            let Some(ip) = ip else {
                let _ = req.respond(text_response(
                    502,
                    &json!({ "error": { "message": "取不到本机出口 IP" } }).to_string(),
                ));
                return;
            };
            let t0 = std::time::Instant::now();
            let res = ureq::post(&format!(
                "{}{}",
                crate::traeproxy::AGENT_HOST,
                crate::traeproxy::CHAT_PATH
            ))
            .timeout(std::time::Duration::from_secs(300))
            .set("Authorization", &format!("Cloud-IDE-JWT {}", up.token))
            .set("Content-Type", "application/json")
            .set("Accept", "text/event-stream")
            .set("X-App-Id", crate::traeproxy::APP_ID)
            .set("X-User-Real-IP", &ip)
            .send(solo.to_string().as_bytes());

            match res {
                Ok(resp) => {
                    let status = resp.status() as u16;
                    // 非流式调用方要一条完整 JSON；流式才逐块翻
                    if !v["stream"].as_bool().unwrap_or(false) {
                        let mut tr = crate::traeproxy::SoloTranslator::new(resp.into_reader(), &model_name);
                        let out = crate::traeproxy::collect_completion(&mut tr, &model_name);
                        let s = tr.shared_state().lock().map(|g| g.clone()).unwrap_or_default();
                        if let Some((code, msg)) = &s.error {
                            let k = crate::traeproxy::classify(*code, msg);
                            crate::traeproxy::bump(&format!("err:{}", k.as_str()));
                            let st = crate::traeproxy::http_status(k);
                            self.push_log(method, path, model.clone(), st, &up.label, &up.pool, via);
                            let _ = req.respond(text_response(
                                st,
                                &json!({ "error": { "message": msg, "code": code, "type": k.as_str() } })
                                    .to_string(),
                            ));
                            return;
                        }
                        crate::traeproxy::bump("err:ok");
                        self.push_log(method, path, model.clone(), status, &up.label, &up.pool, via);
                        if !dist_id.is_empty() {
                            gateway::bump_dist(dist_id, &up.label);
                        }
                        crate::traeproxy::note(json!({
                            "model": model_name, "channel": channel, "provider": s.provider,
                            "usage": s.usage, "kind": "ok", "ip": ip, "stream": false,
                            "ms": t0.elapsed().as_millis() as u64, "via": via, "account": up.label,
                        }));
                        let _ = req.respond(text_response(200, &out.to_string()));
                        return;
                    }
                    let tr =
                        crate::traeproxy::SoloTranslator::new(resp.into_reader(), &model_name);
                    let shared = tr.shared_state();
                    let _ = req.respond(Response::new(
                        StatusCode(status),
                        vec![
                            header("Content-Type", "text/event-stream; charset=utf-8"),
                            cors_header(),
                        ],
                        tr,
                        None,
                        None,
                    ));
                    // 流写完之后才有 usage / provider / error
                    let s = shared.lock().map(|g| g.clone()).unwrap_or_default();
                    let kind = match &s.error {
                        Some((code, msg)) => crate::traeproxy::classify(*code, msg).as_str(),
                        None => "ok",
                    };
                    crate::traeproxy::bump(&format!("err:{kind}"));
                    self.push_log(method, path, model.clone(), status, &up.label, &up.pool, via);
                    if !dist_id.is_empty() {
                        gateway::bump_dist(dist_id, &up.label);
                    }
                    crate::traeproxy::note(json!({
                        "model": model_name, "channel": channel, "provider": s.provider,
                        "usage": s.usage, "kind": kind, "ip": ip,
                        "ms": t0.elapsed().as_millis() as u64,
                        "via": via, "account": up.label,
                    }));
                    return;
                }
                Err(ureq::Error::Status(code, resp)) => {
                    let mut txt = String::new();
                    let _ = resp.into_reader().take(2000).read_to_string(&mut txt);
                    let k = crate::traeproxy::classify(code as i64, &txt);
                    if k.retry_with_new_ip() && attempt == 0 {
                        continue; // 4011 = 身份没过，重算出口 IP 再试一次
                    }
                    crate::traeproxy::bump(&format!("err:{}", k.as_str()));
                    self.push_log(method, path, model.clone(), code as u16, &up.label, &up.pool, via);
                    let _ = req.respond(text_response(
                        code as u16,
                        &json!({
                            "error": {
                                "message": txt.chars().take(300).collect::<String>(),
                                "type": k.as_str()
                            }
                        })
                        .to_string(),
                    ));
                    return;
                }
                Err(e) => {
                    self.push_log(method, path, model.clone(), 502, &up.label, &up.pool, via);
                    let _ = req.respond(text_response(
                        502,
                        &json!({ "error": { "message": e.to_string() } }).to_string(),
                    ));
                    return;
                }
            }
        }
    }

    /// WorkBuddy 上游：OpenAI 进、OpenAI 出。上游本身就是 OpenAI 形状，
    /// 我们只改头族 + 少量 body 字段（system 首条 / prompt_cache_key / 模型别名）。
    ///
    /// 与 qproxy 路径不同的三件事：
    ///   * 要 prepare（改 body），所以不能复用那条「只换 Authorization」的路径；
    ///   * 失败要按三维拒判决定换不换号 —— 请求形状错换号无用，账号级错换号是唯一出路；
    ///   * 流式要边透传边把尾巴留下来，respond 返回后才拿得到 usage/credit。
    fn serve_wb(
        &self,
        mut req: tiny_http::Request,
        method: &str,
        path: &str,
        body: &[u8],
        fwd: &[(String, String)],
        ups: &[crate::gateway::Upstream],
        start: usize,
        route: &Route,
        model: Option<String>,
    ) {
        if ups.is_empty() {
            let _ = req.respond(text_response(
                502,
                &json!({ "error": { "message": "上游组里没有 WorkBuddy 成员" } }).to_string(),
            ));
            return;
        }
        let budget = ups.len().min(crate::wbproxy::MAX_RETRY_ACCOUNTS).max(1);
        let mut last: Option<(u16, String, String)> = None;

        for k in 0..budget {
            let up = &ups[(start + k) % ups.len()];
            let Some(region) = crate::wbproxy::region_from_base(&up.base_url) else {
                continue;
            };
            let app = crate::wbproxy::app_for_region(region);
            let name = up
                .label
                .rsplit_once('/')
                .map(|(_, n)| n.to_string())
                .unwrap_or_else(|| up.label.clone());
            let acct = crate::wbproxy::acct_key(app, &name);

            let prepared = match crate::wbproxy::prepare(app, &route.dist_id, fwd, body) {
                Ok(p) => p,
                Err(e) => {
                    // 请求级错误换号无用，直接回；账号级错误也该由上层决定，
                    // 但 prepare 阶段的账号级错误（没令牌 / 护栏 / 冷却）换个号确实可能好
                    if e.is_account_level() {
                        last = Some((503, e.message(), name.clone()));
                        continue;
                    }
                    let status = 400;
                    self.push_log(method, path, model.clone(), status, &up.label, &up.pool, &route.via);
                    crate::wbproxy::append_log(json!({
                        "ts": crate::registry::now_secs(), "app": app, "region": region,
                        "account": name, "model_asked": model, "status": status,
                        "outcome": "prep_error", "error": e.message(),
                        "via": route.via, "dist": route.dist_id,
                    }));
                    let _ = req.respond(text_response(
                        status,
                        &json!({ "error": { "message": e.message(), "type": "prep" } }).to_string(),
                    ));
                    return;
                }
            };

            if crate::wbproxy::is_cooling(&acct, &prepared.model_used) {
                last = Some((
                    429,
                    format!("{} 的 {} 正在冷却", name, prepared.model_used),
                    name.clone(),
                ));
                continue;
            }
            if let Err(e) = crate::wbproxy::guardrail_check(app, &name) {
                last = Some((429, e.message(), name.clone()));
                continue;
            }

            let url = format!("{}/v2/chat/completions", crate::wbproxy::model_base(region));
            let t0 = std::time::Instant::now();
            let mut call = ureq::post(&url).timeout(std::time::Duration::from_secs(300));
            for (hk, hv) in &prepared.headers {
                call = call.set(hk, hv);
            }
            let res = call.send(&prepared.body[..]);

            match res {
                Ok(resp) => {
                    let status = resp.status() as u16;
                    let ctype = resp
                        .header("Content-Type")
                        .unwrap_or("text/event-stream")
                        .to_string();

                    if !prepared.stream {
                        // 调用方要非流式：上游仍按流式要，本地聚合。
                        let mut rd = resp.into_reader();
                        match crate::wbproxy::collect_completion(&mut rd, &prepared.model_used) {
                            Ok((v, usage)) => {
                                let ms = t0.elapsed().as_millis() as u64;
                                self.push_log(method, path, model.clone(), 200, &up.label, &up.pool, &route.via);
                                crate::wbproxy::note(
                                    app, &name, &prepared.model_used,
                                    &crate::wbproxy::Outcome::Ok, 200, ms, usage.as_ref(),
                                );
                                crate::wbproxy::append_log(json!({
                                    "ts": crate::registry::now_secs(), "app": app, "region": region,
                                    "account": name, "model_asked": model,
                                    "model_used": prepared.model_used, "status": 200,
                                    "outcome": "ok", "stream": false, "ms": ms,
                                    "usage": usage, "via": route.via, "dist": route.dist_id,
                                    "note": prepared.note,
                                }));
                                if !route.dist_id.is_empty() {
                                    gateway::bump_dist(&route.dist_id, &up.label);
                                }
                                let _ = req.respond(text_response(200, &v.to_string()));
                            }
                            Err(e) => {
                                self.push_log(method, path, model.clone(), 502, &up.label, &up.pool, &route.via);
                                let _ = req.respond(text_response(
                                    502,
                                    &json!({ "error": { "message": format!("聚合失败：{e}") } })
                                        .to_string(),
                                ));
                            }
                        }
                        return;
                    }

                    // 流式：边透传边留尾巴，respond 返回（流写完）后再解析 usage
                    let tail = Arc::new(Mutex::new(Vec::<u8>::new()));
                    let tap = SseTap { inner: resp.into_reader(), tail: tail.clone() };
                    let _ = req.respond(Response::new(
                        StatusCode(status),
                        vec![header("Content-Type", &ctype), cors_header()],
                        tap,
                        None,
                        None,
                    ));
                    let ms = t0.elapsed().as_millis() as u64;
                    let tail_s = {
                        let g = tail.lock().unwrap();
                        String::from_utf8_lossy(&g).to_string()
                    };
                    let usage = crate::wbproxy::parse_usage(&tail_s);
                    let finish = crate::wbproxy::parse_finish(&tail_s);
                    self.push_log(method, path, model.clone(), status, &up.label, &up.pool, &route.via);
                    crate::wbproxy::note(
                        app, &name, &prepared.model_used,
                        &crate::wbproxy::Outcome::Ok, status, ms, usage.as_ref(),
                    );
                    crate::wbproxy::append_log(json!({
                        "ts": crate::registry::now_secs(), "app": app, "region": region,
                        "account": name, "model_asked": model,
                        "model_used": prepared.model_used, "status": status,
                        "outcome": "ok", "stream": true, "finish_reason": finish,
                        "ms": ms, "usage": usage, "via": route.via,
                        "dist": route.dist_id, "note": prepared.note,
                    }));
                    if !route.dist_id.is_empty() {
                        gateway::bump_dist(&route.dist_id, &up.label);
                    }
                    return;
                }
                Err(ureq::Error::Status(code, resp)) => {
                    let mut rd = resp.into_reader();
                    let mut buf = vec![0u8; 4000];
                    let n = std::io::Read::read(&mut rd, &mut buf).unwrap_or(0);
                    let txt = String::from_utf8_lossy(&buf[..n]).to_string();
                    let outcome = crate::wbproxy::classify(code as u16, &txt, &[]);
                    let ms = t0.elapsed().as_millis() as u64;
                    crate::wbproxy::note(
                        app, &name, &prepared.model_used, &outcome, code as u16, ms, None,
                    );
                    self.push_log(method, path, model.clone(), code as u16, &up.label, &up.pool, &route.via);
                    crate::wbproxy::append_log(json!({
                        "ts": crate::registry::now_secs(), "app": app, "region": region,
                        "account": name, "model_asked": model,
                        "model_used": prepared.model_used, "status": code,
                        "outcome": outcome.name(), "ms": ms,
                        "biz_msg": txt.chars().take(200).collect::<String>(),
                        "via": route.via, "dist": route.dist_id,
                    }));
                    last = Some((code as u16, outcome.message(), name.clone()));
                    if outcome.should_rotate() && k + 1 < budget {
                        continue;
                    }
                    // 不换号：把上游原话回给调用方（请求形状错、容量排队都走这条）
                    let _ = req.respond(text_response(code as u16, &txt));
                    return;
                }
                Err(e) => {
                    last = Some((502, e.to_string(), name.clone()));
                    if k + 1 < budget {
                        continue;
                    }
                    self.push_log(method, path, model.clone(), 502, &up.label, &up.pool, &route.via);
                    let _ = req.respond(text_response(
                        502,
                        &json!({ "error": { "message": e.to_string() } }).to_string(),
                    ));
                    return;
                }
            }
        }

        let (st, msg, who) = last.unwrap_or((
            502,
            "没有可用的 WorkBuddy 账号：所有成员都在冷却或没登录".to_string(),
            "-".to_string(),
        ));
        self.push_log(method, path, model, st, &who, "-", &route.via);
        let _ = req.respond(text_response(
            st,
            &json!({ "error": { "message": msg, "type": "no_usable_account" } }).to_string(),
        ));
    }
}

/// 透传上游字节，同时把尾巴留一份 —— usage / credit 只在末 chunk 里，
/// 而流式响应要等 respond 返回（流写完）才拿得到。
struct SseTap<R: Read> {
    inner: R,
    tail: Arc<Mutex<Vec<u8>>>,
}

const TAP_TAIL: usize = 64 * 1024;

impl<R: Read> Read for SseTap<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        if n > 0 {
            if let Ok(mut g) = self.tail.lock() {
                g.extend_from_slice(&buf[..n]);
                if g.len() > TAP_TAIL {
                    let drop = g.len() - TAP_TAIL;
                    g.drain(..drop);
                }
            }
        }
        Ok(n)
    }
}

/// 授权上游组里各厂商的可用模型合成一份 OpenAI list。
/// 四家都列：WorkBuddy（/v3/config 白名单 ∩ 免费倍率）、Trae（本地模型目录）、
/// 已知的模型前缀（owner）集合——与 models_for 里各家的 owned_by 一致。
fn known_owners() -> &'static [&'static str] {
    &["workbuddy", "workbuddy-intl", "trae", "qoder", "qoder-cn", "opencode", "astudio"]
}

/// `trae/glm-5.3-flash` → Some(("trae", "glm-5.3-flash"))；不认识的前缀/空段回 None。
/// 带前缀的模型只在属于这一家的上游里挑——同名模型不再互相漂。
fn split_namespaced(model: &str) -> Option<(&str, &str)> {
    let (owner, bare) = model.split_once('/')?;
    let owner = owner.trim();
    let bare = bare.trim();
    if owner.is_empty() || bare.is_empty() || bare.contains('/') {
        return None;
    }
    if known_owners().contains(&owner) {
        Some((owner, bare))
    } else {
        None
    }
}

/// 上游基址 → 归属的 Agent id（模型前缀路由用）。key 类上游回 None。
fn owner_of_base(base: &str) -> Option<&'static str> {
    if let Some(r) = crate::wbproxy::region_from_base(base) {
        return Some(crate::wbproxy::app_for_region(r));
    }
    if crate::traeproxy::is_trae_base(base) {
        return Some("trae");
    }
    if crate::asproxy::is_astudio_base(base) {
        return Some("astudio");
    }
    if crate::qproxy::is_qoder_base(base) {
        let b = base.trim_end_matches('/');
        return Some(if b.starts_with(crate::qproxy::model_base("cn")) {
            "qoder-cn"
        } else {
            "qoder"
        });
    }
    if base.starts_with("http://127.0.0.1:8211") {
        return Some("opencode");
    }
    // Qoder CN 的本地桥（官方运行时 + OpenAI 兼容外壳）
    if base.starts_with(&format!("http://127.0.0.1:{}", crate::qproxy::CN_PORT)) {
        return Some("qoder-cn");
    }
    None
}

// ───────────────────────── 逐模型实测计数 ─────────────────────────

static MODEL_STATS_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn model_stats_path() -> std::path::PathBuf {
    crate::registry::base_dir().join("proxymodel.json")
}

/// 反代每处理一个请求就记一次（重启不丢）：「反代全景」里模型质量的本地证据。
/// 键是请求里带的模型名，带前缀的按前缀记（同一家内部的裸名也合并）。
fn model_stats_bump(model: &str, status: u16) {
    let _g = MODEL_STATS_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let path = model_stats_path();
    let mut v: Value = std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .filter(|x| x.is_object())
        .unwrap_or_else(|| json!({}));
    let e = &v["models"][model];
    let calls = e["calls"].as_u64().unwrap_or(0) + 1;
    let ok = e["ok"].as_u64().unwrap_or(0) + u64::from((200..300).contains(&status));
    let fail = calls - ok;
    v["models"][model] = json!({
        "calls": calls, "ok": ok, "fail": fail,
        "last_status": status, "at": crate::registry::now_secs(),
    });
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, serde_json::to_string(&v).unwrap_or_default()).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

/// 读全部逐模型计数（纯本地文件，不起进程）。
pub fn model_stats() -> Value {
    let _g = MODEL_STATS_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    std::fs::read_to_string(model_stats_path())
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .filter(|x| x.is_object())
        .unwrap_or_else(|| json!({}))
}

/// Qoder（客户端模型缓存，国内版含本地桥）、OpenCode（本地桥 /v1/models）。
/// 宁可少列，不列一个调不通的。
/// 模型清单一律读全量：WorkBuddy 也不再只挑 0 倍率那几条。
/// 每个 id 都带 Agent 前缀（trae/glm-5.3-flash），同名模型不再互相吞——
/// 客户端填带前缀的 id，指向就固定在那一家的上游。
fn models_for(ups: &[Upstream]) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut push = |id: &str, owned_by: &str, extra: Value| {
        if id.is_empty() {
            return;
        }
        let cid = format!("{owned_by}/{id}");
        if !seen.insert(cid.clone()) {
            return;
        }
        let mut v = json!({ "id": cid, "object": "model", "owned_by": owned_by });
        if let (Some(obj), Some(e)) = (v.as_object_mut(), extra.as_object()) {
            for (k, val) in e {
                obj.insert(k.clone(), val.clone());
            }
        }
        out.push(v);
    };
    let mut has_wb = false;
    let mut has_trae = false;
    let mut has_qoder = false;
    let mut has_oc = false;
    let mut has_as = false;
    for up in ups {
        if let Some(region) = crate::wbproxy::region_from_base(&up.base_url) {
            has_wb = true;
            let app = crate::wbproxy::app_for_region(region);
            for m in crate::wbproxy::all_models(app) {
                push(&m.id.clone(), app, json!({
                    "name": m.name,
                    "credits": m.credits,
                    "context_window": m.context,
                    "max_output_tokens": m.max_output,
                }));
            }
        } else if crate::traeproxy::is_trae_base(&up.base_url) {
            has_trae = true;
        } else if crate::qproxy::is_qoder_base(&up.base_url) {
            has_qoder = true;
        } else if crate::asproxy::is_astudio_base(&up.base_url) {
            has_as = true;
        } else if up.base_url.starts_with("http://127.0.0.1:8211") {
            has_oc = true;
        }
    }
    if has_trae {
        if let Some(dir) = crate::registry::profile_dir("traework-cn") {
            // 目录是内存缓存：新进程里没人点过「刷新」就是空的。
            // 这里冷启动自己加载一次（models_for 内部按 mtime 缓存 60 秒），
            // 否则「获取可用模型」在重启后永远列不出 Trae 的模型。
            let channels = crate::traeproxy::models_cached(&dir)
                .or_else(|| crate::traeproxy::models_for(&dir).ok());
            if let Some((ch, _)) = channels {
                for m in crate::traeproxy::model_list(&ch) {
                    push(m["id"].as_str().unwrap_or(""), "trae", json!({
                        "channel": m["channel"].clone(),
                        "context_window": m["context_window"].clone(),
                    }));
                }
            }
        }
    }
    if has_qoder {
        let rows = match crate::qproxy::models_view() {
            Value::Array(a) => a,
            _ => Vec::new(),
        };
        for row in rows {
            if row["models"].as_array().cloned().unwrap_or_default().is_empty() {
                continue;
            }
            let app = row["app"].as_str().unwrap_or("qoder").to_string();
            for m in row["models"].as_array().cloned().unwrap_or_default() {
                push(m["id"].as_str().unwrap_or(""), &app, json!({
                    "name": m["name"].clone(),
                    "free": m["free"].clone(),
                    "context_window": m["context"].clone(),
                }));
            }
        }
    }
    if has_oc {
        let b = crate::opencode::models();
        for m in b["models"].as_array().cloned().unwrap_or_default() {
            push(m["id"].as_str().unwrap_or(""), "opencode", json!({}));
        }
    }
    if has_as {
        for m in crate::asproxy::models_static() {
            push(
                m["id"].as_str().unwrap_or(""),
                "astudio",
                json!({
                    "name": m["name"].clone(),
                    "free": m["free"].clone(),
                    "multiplier": m["multiplier"].clone(),
                }),
            );
        }
    }
    out
}

/// 「可用模型」面板的数据源：不是照抄 OpenAI list，而是按厂商分组 + 上下文 + 倍率。
/// 上游组没建时返回空数组——界面如实报"先建上游组"，不假装有模型。
pub fn models_payload() -> Value {
    let global_upstream = crate::registry::load_config()["proxy"]["upstream"]
        .as_str()
        .unwrap_or("https://api.openai.com")
        .to_string();
    let ups = if gateway::pools().is_empty() {
        legacy_pool(&global_upstream)
    } else {
        gateway::upstreams_for(&[], &global_upstream)
    };
    let models = models_for(&ups);
    let stats = model_stats();
    let by_owner: std::collections::BTreeMap<String, Vec<&Value>> =
        models.iter().fold(std::collections::BTreeMap::new(), |mut acc, m| {
            let k = m["owned_by"].as_str().unwrap_or("other").to_string();
            acc.entry(k).or_default().push(m);
            acc
        });
    let groups: Vec<Value> = by_owner
        .into_iter()
        .map(|(owner, ms)| {
            let models: Vec<Value> = ms
                .into_iter()
                .map(|m| {
                    let mut m = m.clone();
                    let key = m["id"].as_str().unwrap_or("").to_string();
                    let bare = key
                        .split_once('/')
                        .map(|(_, b)| b.to_string())
                        .unwrap_or_else(|| key.clone());
                    // 实测计数：先按带前缀的 id 找，再退回裸名（老客户端还在发裸名）
                    if let Some(s) = stats["models"]
                        .get(&key)
                        .or_else(|| stats["models"].get(&bare))
                    {
                        m["calls"] = s["calls"].clone();
                        m["ok"] = s["ok"].clone();
                        m["fail"] = s["fail"].clone();
                        m["last_status"] = s["last_status"].clone();
                    }
                    m
                })
                .collect();
            json!({
                "owner": owner,
                "count": models.len(),
                "models": models,
            })
        })
        .collect();
    json!({ "groups": groups, "count": models.len() })
}


fn cors_header() -> Header {
    header("Access-Control-Allow-Origin", "*")
}

fn text_response(code: u16, body: &str) -> Response<std::io::Cursor<Vec<u8>>> {
    Response::from_string(body).with_status_code(StatusCode(code))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 真实起一次反代：验证监听、请求解析、响应回传、日志、停止后端口释放。
    /// token 池是否非空取决于本机数据，两种结果都接受，只校验不变量。
    #[test]
    fn proxy_serves_and_stops() {
        let p = Proxy::shared();
        p.set_config(Some("https://api.openai.com"), Some("round-robin"));
        let (ok, msg) = p.start(crate::registry::PROXY_PORT);
        assert!(ok, "启动失败: {}", msg);

        let res = ureq::post(&format!(
            "http://127.0.0.1:{}/v1/chat/completions",
            crate::registry::PROXY_PORT
        ))
        .set("Content-Type", "application/json")
        .timeout(std::time::Duration::from_secs(20))
        .send_string(r#"{"model":"gpt-4o-mini","messages":[]}"#);

        // token 池是否为空取决于本机数据，因此只断言"拿到了 HTTP 状态码"这一条不变量；
        // 若本机网络栈拦住了未签名测试程序的回环连接，跳过响应断言，仍验证起停。
        match res {
            Ok(r) => {
                let logs = p.recent_logs(10);
                assert!(!logs.is_empty(), "请求已回包但日志未记录");
                assert_eq!(logs[0]["path"], "/v1/chat/completions");
                assert_eq!(logs[0]["status"].as_u64(), Some(r.status() as u64));
            }
            Err(ureq::Error::Status(code, _)) => {
                let logs = p.recent_logs(10);
                assert!(!logs.is_empty(), "请求已回包但日志未记录");
                assert_eq!(logs[0]["status"].as_u64(), Some(code as u64));
            }
            Err(e) => eprintln!("跳过回环断言（本机网络栈拦截该测试进程？）：{}", e),
        }

        let (ok, _) = p.stop();
        assert!(ok);
        std::thread::sleep(std::time::Duration::from_millis(800));
        assert!(
            std::net::TcpStream::connect("127.0.0.1:8788").is_err(),
            "停止后端口仍被占用"
        );
    }
}
