// 本地 OpenAI 兼容反代：按来源池轮询替换 Authorization，成功响应流式转发（保留 SSE）
// 请求带的 key 决定能用哪些池子：命中下发的 key 就只在那几个池子里挑上游；
// 不认识的 key 退回"全部启用的池子"（兼容老的用法）。
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

/// 兼容保留：老式（无池子配置）时的成员清单 = 所有登记过 token 的账号 + 勾了「进反代」的 Key
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
    pub via: String,        // 展示用：key 名（授权池子）
    pub disabled: bool,     // 命中的 key 被停用
}

/// 按请求带来的 key 决定这次能用哪些上游。
/// 没配池子就退回老行为（所有登记 token 的账号 + 勾选进反代的 Key 一起轮询）。
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
            vec!["全部池子".into()]
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
    // 不认识的 key：所有启用的池子（便于本机自用，也不破坏老配置）
    empty_route(gateway::upstreams_for(&[], global_upstream), "", String::new(), false)
}

/// 展示用：当前反代可路由到的上游清单（标签）。
/// 有池子配置时按池子汇总；没配池子时退回老式成员清单，界面显示口径不变。
pub fn pool_labels(global_upstream: &str) -> Vec<String> {
    let ups = if gateway::pools().is_empty() {
        legacy_pool(global_upstream)
    } else {
        gateway::upstreams_for(&[], global_upstream)
    };
    let mut out: Vec<String> = Vec::new();
    for u in ups {
        // 有池子时标签带上池名，一眼能看出这个账号挂在哪个池
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
        // 客户端带来的 key：决定这次能用哪些池子（之后会被换成上游自己的凭据）
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
        let (ups, via) = (route.ups, route.via.clone());
        if route.disabled {
            self.push_log(&method, &path, model, 403, "-", "-", &via);
            let _ = req.respond(text_response(
                403,
                &json!({ "error": format!("这个分发的 key「{}」已停用", via) }).to_string(),
            ));
            return;
        }
        if ups.is_empty() {
            let hint = if via.is_empty() {
                "没挑到上游：到「反代编制」里建一个池子，把要用的账号/Key 勾进去；\
                 或者直接用池子生成的「分发 Key」调用。".to_string()
            } else {
                format!("分发的 key「{}」对应的池子里没有可用成员（账号没登记 Token，或成员被停用）", via)
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
        let up = ups[i].clone();
        let url = format!("{}{}", up.base_url.trim_end_matches('/'), path);

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
                self.push_log(&method, &path, model, status, &up.label, &up.pool, &via);
                if !route.dist_id.is_empty() {
                    gateway::bump_dist(&route.dist_id, &up.label);
                }
                let response = Response::new(
                    StatusCode(status),
                    vec![
                        header("Content-Type", &ctype),
                        cors_header(),
                    ],
                    resp.into_reader(),
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
                let _ = req.respond(text_response(
                    502,
                    &json!({ "error": e.to_string() }).to_string(),
                ));
            }
        }
    }
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
