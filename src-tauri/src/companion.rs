use serde_json::{json, Value};
use std::{
    io::Write,
    process::{Command, Stdio},
    time::Duration,
};
use tauri::{
    AppHandle, Emitter, Manager, PhysicalPosition, PhysicalSize, WebviewUrl, WebviewWindow,
    WebviewWindowBuilder,
};

fn label(kind: &str) -> Option<&'static str> {
    match kind {
        "orb" => Some("desktop-orb"),
        "pet" => Some("desktop-pet"),
        "panel" => Some("desktop-panel"),
        _ => None,
    }
}

fn kind_of(label: &str) -> Option<&'static str> {
    match label {
        "desktop-orb" => Some("orb"),
        "desktop-pet" => Some("pet"),
        "desktop-panel" => Some("panel"),
        _ => None,
    }
}

pub fn show(app: &AppHandle, expand: bool) -> Result<(), String> {
    let kind = if expand { "panel" } else { "orb" };
    show_kind(app, kind, expand)?;
    app.emit_to("main", "companion-opened", json!({"kind":kind}))
        .map_err(|e| e.to_string())?;
    Ok(())
}

fn show_kind(app: &AppHandle, kind: &str, expand: bool) -> Result<(), String> {
    let name = label(kind).ok_or("未知桌面助手窗口")?;
    if let Some(w) = app.get_webview_window(name) {
        w.show().map_err(|e| e.to_string())?;
        if expand && kind == "panel" {
            w.emit("companion-expand", ()).map_err(|e| e.to_string())?
        }
        return Ok(());
    }
    let page = format!("companion.html?kind={kind}");
    let (width, height) = if kind == "panel" {
        (392., 530.)
    } else {
        (86., 86.)
    };
    let window = WebviewWindowBuilder::new(app, name, WebviewUrl::App(page.into()))
        .title("Agent 驾驶舱 · 桌面助手")
        .inner_size(width, height)
        .decorations(false)
        .transparent(true)
        .shadow(false)
        .always_on_top(true)
        .skip_taskbar(true)
        .focused(false)
        .visible(false)
        .resizable(false)
        .build()
        .map_err(|e| e.to_string())?;
    // Desktop windows use their own 86×86 / 392×530 CSS layout. A WebView2
    // zoom saved for the main window must not magnify that layout and clip it.
    if let Err(error) = window.set_zoom(1.0) {
        let _ = window.destroy();
        return Err(error.to_string());
    }
    Ok(())
}

fn layout(window: &WebviewWindow, p: &Value) -> Result<Value, String> {
    let kind = kind_of(window.label()).ok_or("未知桌面助手窗口")?;
    window.set_zoom(1.0).map_err(|e| e.to_string())?;
    let saved = p["x"].as_i64().zip(p["y"].as_i64());
    let current = window.outer_position().map_err(|e| e.to_string())?;
    let (ax, ay) = saved.unwrap_or((current.x.into(), current.y.into()));
    let monitors = window.available_monitors().map_err(|e| e.to_string())?;
    let m = monitors
        .iter()
        .find(|m| {
            let r = m.work_area();
            ax >= r.position.x as i64
                && ay >= r.position.y as i64
                && ax < (r.position.x as i64 + r.size.width as i64)
                && ay < (r.position.y as i64 + r.size.height as i64)
        })
        .cloned()
        .or(window.primary_monitor().map_err(|e| e.to_string())?)
        .ok_or("未检测到显示器")?;
    let r = m.work_area();
    let scale = m.scale_factor();
    let px = |v: f64| (v * scale).round() as i32;
    let (left, top) = (r.position.x, r.position.y);
    let (right, bottom) = (left + r.size.width as i32, top + r.size.height as i32);
    let ball = px(86.);
    if right <= left || bottom <= top {
        return Err("显示器可用区域无效".into());
    }
    // A monitor may have been unplugged since the position was saved. Clamp
    // before calculating edge distances so the ball returns to a visible area.
    let panel = kind == "panel";
    let (normal_w, normal_h) = if panel {
        (px(392.), px(530.))
    } else {
        (ball, ball)
    };
    let (x, y) = if saved.is_none() && p["initial"] == true {
        let offset = if kind == "pet" { px(125.) } else { px(24.) };
        (right - normal_w - offset, bottom - normal_h - px(60.))
    } else {
        (
            ax.clamp(i64::from(left), i64::from(right)) as i32,
            ay.clamp(i64::from(top), i64::from(bottom)) as i32,
        )
    };
    let mx = m.position().x;
    let my = m.position().y;
    let mut edges = [
        ("left", (x - left).abs(), left == mx),
        (
            "right",
            (x + normal_w - right).abs(),
            right == mx + m.size().width as i32,
        ),
        ("top", (y - top).abs(), top == my),
        (
            "bottom",
            (y + normal_h - bottom).abs(),
            bottom == my + m.size().height as i32,
        ),
    ];
    edges.sort_by_key(|e| e.1);
    let edge = if panel {
        edges
            .iter()
            .find(|e| e.2 && e.1 <= px(24.))
            .map(|e| e.0)
            .unwrap_or("")
    } else {
        ""
    };
    let collapsed = panel && p["mode"].as_str() == Some("tab") && !edge.is_empty();
    let seam = collapsed && p["hint"] != true;
    let (w, h) = if collapsed && !seam {
        if edge == "left" || edge == "right" {
            (px(32.), ball)
        } else {
            (ball, px(32.))
        }
    } else {
        (normal_w, normal_h)
    };
    let w = w.min(right - left);
    let h = h.min(bottom - top);
    // TieZ-style dock: the collapsed card keeps its full size and slides so
    // only a 3px seam stays inside the work area; the optional hint tab
    // shrinks instead and stays fully on-screen.
    let hide = 3;
    let x = if edge == "right" {
        if seam {
            right - hide
        } else {
            right - w
        }
    } else if edge == "left" {
        if seam {
            left - normal_w + hide
        } else {
            left
        }
    } else {
        x
    };
    let y = if edge == "bottom" {
        if seam {
            bottom - hide
        } else {
            bottom - h
        }
    } else if edge == "top" {
        if seam {
            top - normal_h + hide
        } else {
            top
        }
    } else {
        y
    };
    let x = if seam && (edge == "left" || edge == "right") {
        x
    } else {
        x.clamp(left, right - w)
    };
    let y = if seam && (edge == "top" || edge == "bottom") {
        y
    } else {
        y.clamp(top, bottom - h)
    };
    window
        .set_size(PhysicalSize::new(w as u32, h as u32))
        .map_err(|e| e.to_string())?;
    window
        .set_position(PhysicalPosition::new(x, y))
        .map_err(|e| e.to_string())?;
    window.show().map_err(|e| e.to_string())?;
    Ok(json!({"edge":edge,"x":x,"y":y,"scale":scale}))
}

#[tauri::command]
pub async fn desktop_companion(
    app: AppHandle,
    window: WebviewWindow,
    op: String,
    p: Value,
) -> Result<Value, String> {
    if window.label() != "main" && kind_of(window.label()).is_none() {
        return Err("仅限驾驶舱本地窗口".into());
    }
    if op == "show" {
        let kind = p["kind"]
            .as_str()
            .unwrap_or(if p["expand"] == true { "panel" } else { "orb" });
        show_kind(&app, kind, kind == "panel" || p["expand"] == true)?;
        app.emit_to("main", "companion-opened", json!({"kind":kind}))
            .map_err(|e| e.to_string())?;
        return Ok(Value::Null);
    }
    if op == "close" {
        let kind = if window.label() == "main" {
            p["kind"].as_str().ok_or("请选择桌面助手窗口")?
        } else {
            kind_of(window.label()).ok_or("未知桌面助手窗口")?
        };
        let target = label(kind).ok_or("未知桌面助手窗口")?;
        if let Some(existing) = app.get_webview_window(target) {
            existing.destroy().map_err(|e| e.to_string())?;
        }
        app.emit_to("main", "companion-closed", json!({"kind":kind}))
            .map_err(|e| e.to_string())?;
        return Ok(Value::Null);
    }
    if kind_of(window.label()).is_none() {
        return Err("请从桌面助手操作".into());
    }
    match op.as_str() {
        "layout" => layout(&window, &p),
        "drag" => {
            window.start_dragging().map_err(|e| e.to_string())?;
            Ok(Value::Null)
        }
        "navigate" => {
            crate::show_main(&app);
            app.emit_to("main", "companion-navigate", &p)
                .map_err(|e| e.to_string())?;
            Ok(Value::Null)
        }
        "feed" | "open" => {
            let port = crate::agentfind::agentfind_port().await?;
            tauri::async_runtime::spawn_blocking(move || {
                let base = format!("http://127.0.0.1:{port}");
                // The service listens on loopback. Never send local session data
                // through a proxy inherited from the desktop environment.
                let http = ureq::AgentBuilder::new()
                    .try_proxy_from_env(false)
                    .redirects(0)
                    .timeout(Duration::from_secs(8))
                    .build();
                let res = if op == "feed" {
                    http.get(&format!("{base}/api/desktop-feed")).call()
                } else {
                    let sid = p["sid"]
                        .as_str()
                        .filter(|s| !s.is_empty() && s.len() < 240)
                        .ok_or("会话 ID 无效")?;
                    http.post(&format!("{base}/api/open"))
                        .set("X-Requested-With", "agentfind")
                        .send_json(json!({"sid":sid}))
                }
                .map_err(|e| format!("AgentFind 请求失败：{e}"))?;
                let data: Value = res.into_json().map_err(|_| "AgentFind 返回格式无效")?;
                if let Some(msg) = data["error"].as_str() {
                    return Err(msg.to_owned());
                }
                Ok(data)
            })
            .await
            .map_err(|e| e.to_string())?
        }
        "speak" => {
            let text = p["text"]
                .as_str()
                .unwrap_or("")
                .chars()
                .take(220)
                .collect::<String>();
            tauri::async_runtime::spawn_blocking(move||{
                #[cfg(windows)] use std::os::windows::process::CommandExt;
                let mut cmd=Command::new("powershell.exe");
                #[cfg(windows)] cmd.creation_flags(0x08000000);
                let mut child=cmd.args(["-NoProfile","-NonInteractive","-Command",
                    "$ErrorActionPreference='Stop'; [Console]::InputEncoding=[Text.UTF8Encoding]::new($false); Add-Type -AssemblyName System.Speech; $v=New-Object System.Speech.Synthesis.SpeechSynthesizer; try {$v.Speak([Console]::In.ReadToEnd())}finally{$v.Dispose()}"])
                    .stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null()).spawn().map_err(|e|e.to_string())?;
                child.stdin.take().ok_or("本机朗读未就绪")?.write_all(text.as_bytes()).map_err(|e|e.to_string())?;
                if !child.wait().map_err(|e|e.to_string())?.success(){return Err("Windows 朗读不可用，请检查已安装的语音".into())}Ok(Value::Null)
            }).await.map_err(|e|e.to_string())?
        }
        _ => Err("未知桌面助手操作".into()),
    }
}
