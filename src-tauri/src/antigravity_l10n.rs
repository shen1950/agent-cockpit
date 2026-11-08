//! Antigravity 界面汉化管理。引擎与词典来自第三方开源项目 qqxpee/antigravity2-cn，
//! 随驾驶舱分发在 tools/antigravity-l10n/（来源与许可见同目录 NOTICE.md）。
//!
//! 职责只有两件事：检测 Antigravity 的 app.asar 是否已注入汉化；一键运行引擎完成
//! 安装/更新。引擎自带「关闭 Antigravity → 备份 → npx 解包 → 注入 → 重打包」全流程，
//! 这里只负责定位文件、起进程、守超时、验结果，不复制它的逻辑。

use serde_json::{json, Value};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

/// 汉化引擎注入 menu.js 时留下的特征串：asar 里搜得到 = 已注入。
const MARKER: &[u8] = b"MENU TRANSLATION START";
const ENGINE_JS: &str = "localization_engine.js";
/// 引擎要跑 npx 解包/重打包，首次还要下载 asar 工具包，给足 5 分钟。
const INSTALL_TIMEOUT: Duration = Duration::from_secs(300);

// ---------------------------------------------------------------------------
// 定位
// ---------------------------------------------------------------------------

/// Antigravity 的 resources/app.asar，解析链与 antigravity.rs 的 detect_ide_version 同源。
fn asar_path() -> Option<PathBuf> {
    let app = crate::registry::app_def(crate::antigravity::APP)?;
    let exe = crate::registry::resolve_exe(&app)?;
    let asar = exe.parent()?.join("resources").join("app.asar");
    asar.is_file().then_some(asar)
}

/// 汉化引擎目录：从 exe 往上找 tools/antigravity-l10n，找不到再试当前工作目录。
/// 覆盖两种布局：开发态 bin/ 下运行（项目根有 tools/）和安装后的独立目录。
fn engine_dir() -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        let mut dir = exe.parent().map(Path::to_path_buf);
        for _ in 0..4 {
            match dir.take() {
                Some(d) => {
                    candidates.push(d.join("tools").join("antigravity-l10n"));
                    dir = d.parent().map(Path::to_path_buf);
                }
                None => break,
            }
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        candidates.push(cwd.join("tools").join("antigravity-l10n"));
    }
    candidates.into_iter().find(|d| d.join(ENGINE_JS).is_file())
}

// ---------------------------------------------------------------------------
// 状态检测
// ---------------------------------------------------------------------------

fn find_sub(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn localized(asar: &Path) -> bool {
    fs::read(asar)
        .map(|bytes| find_sub(&bytes, MARKER).is_some())
        .unwrap_or(false)
}

/// 在 asar 头部找主 package.json 的 `"version": "x.y.z"`（取法同 antigravity.rs）。
fn first_version_in(bytes: &[u8]) -> Option<String> {
    let needle = b"\"version\"";
    let mut i = 0;
    while i + needle.len() <= bytes.len() {
        if &bytes[i..i + needle.len()] == needle {
            let mut j = i + needle.len();
            while j < bytes.len() && matches!(bytes[j], b' ' | b':' | b'\t' | b'\n' | b'\r') {
                j += 1;
            }
            if j < bytes.len() && bytes[j] == b'"' {
                let start = j + 1;
                let mut end = start;
                while end < bytes.len() && bytes[end] != b'"' {
                    end += 1;
                }
                let v = String::from_utf8_lossy(&bytes[start..end]).to_string();
                let dotted = v.split('.').count() >= 2;
                if dotted && v.chars().next().is_some_and(|c| c.is_ascii_digit()) {
                    return Some(v);
                }
            }
        }
        i += 1;
    }
    None
}

fn asar_version(asar: &Path) -> Option<String> {
    let bytes = fs::read(asar).ok()?;
    let head = &bytes[..bytes.len().min(512 * 1024)];
    first_version_in(head)
}

fn node_available() -> bool {
    Command::new("node")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn l10n_status() -> Value {
    let asar = asar_path();
    let engine = engine_dir();
    json!({
        "ok": true,
        "found": asar.is_some(),
        "asar": asar.as_ref().map(|p| p.display().to_string()).unwrap_or_default(),
        "version": asar.as_ref().and_then(|p| asar_version(p)).unwrap_or_default(),
        "localized": asar.as_ref().map(|p| localized(p)).unwrap_or(false),
        "engine_ready": engine.is_some(),
        "engine_dir": engine.as_ref().map(|p| p.display().to_string()).unwrap_or_default(),
        "node_ready": node_available(),
    })
}

// ---------------------------------------------------------------------------
// 安装 / 更新
// ---------------------------------------------------------------------------

#[cfg(windows)]
fn hide_window(cmd: &mut Command) {
    use std::os::windows::process::CommandExt;
    // CREATE_NO_WINDOW：不闪黑框
    cmd.creation_flags(0x0800_0000);
}

#[cfg(not(windows))]
fn hide_window(_cmd: &mut Command) {}

fn tail(text: &str, keep: usize) -> String {
    let text = text.trim_end();
    if text.chars().count() <= keep {
        return text.to_string();
    }
    let skipped = text.chars().count() - keep;
    let cut: String = text.chars().skip(skipped).collect();
    format!("…{cut}")
}

fn l10n_install() -> Value {
    let Some(engine) = engine_dir() else {
        return json!({"ok": false, "msg": "未找到汉化引擎（tools/antigravity-l10n/localization_engine.js）"});
    };
    if !node_available() {
        return json!({"ok": false, "msg": "本机没有可用的 node，请先安装 Node.js 再装汉化"});
    }
    let Some(asar) = asar_path() else {
        return json!({"ok": false, "msg": "未找到 Antigravity 安装目录（resources/app.asar）"});
    };

    let mut cmd = Command::new("node");
    cmd.arg(engine.join(ENGINE_JS))
        .arg("--brand-title")
        .arg("english")
        .current_dir(&engine)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    // 引擎内部会调 npx 拉 asar 工具包，这两个环境变量让它免交互确认
    cmd.env("CI", "1").env("NPM_CONFIG_YES", "true");
    hide_window(&mut cmd);

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return json!({"ok": false, "msg": format!("启动汉化引擎失败: {e}")}),
    };
    // 管道必须一直有人读，否则子进程写满缓冲区会卡死；各起一个线程排干。
    let mut stdout_pipe = child.stdout.take().expect("stdout piped");
    let mut stderr_pipe = child.stderr.take().expect("stderr piped");
    let out_handle = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout_pipe.read_to_end(&mut buf);
        buf
    });
    let err_handle = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stderr_pipe.read_to_end(&mut buf);
        buf
    });

    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break Some(st),
            Ok(None) => {
                if started.elapsed() > INSTALL_TIMEOUT {
                    let _ = child.kill();
                    let _ = child.wait();
                    break None;
                }
                std::thread::sleep(Duration::from_millis(500));
            }
            Err(_) => break None,
        }
    };

    let out = out_handle.join().unwrap_or_default();
    let err = err_handle.join().unwrap_or_default();
    let mut log = String::from_utf8_lossy(&out).to_string();
    log.push_str(&String::from_utf8_lossy(&err));
    let log = tail(&log, 4000);

    match status {
        None => json!({"ok": false, "msg": "汉化引擎超时（5 分钟）被终止", "output": log}),
        Some(st) if !st.success() => json!({"ok": false, "msg": "汉化引擎运行失败，详见输出", "output": log}),
        Some(_) => {
            let done = localized(&asar);
            let msg = if done {
                "汉化已安装/更新，重新打开 Antigravity 生效"
            } else {
                "引擎跑完了，但没在 app.asar 里找到注入标记，可能不兼容当前版本"
            };
            json!({"ok": done, "localized": done, "msg": msg, "output": log})
        }
    }
}

// ---------------------------------------------------------------------------
// Tauri 命令
// ---------------------------------------------------------------------------

/// 前端入口：payload {action: "status" | "install"}。业务失败走 {ok:false,msg}，
/// 不返回 Err（与 commands.rs 同一套约定）。install 会强杀 Antigravity.exe，
/// 确认对话框由前端负责。
#[tauri::command]
pub async fn antigravity_l10n(payload: Value) -> Result<Value, String> {
    let act = payload["action"].as_str().unwrap_or("status");
    let result = match act {
        "install" => l10n_install(),
        _ => l10n_status(),
    };
    Ok(result)
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marker_search_finds_injection() {
        let mut fake = b"console.log('official');".to_vec();
        fake.extend_from_slice(b"\n/* --- MENU TRANSLATION START --- */\nconst translations={};");
        assert!(find_sub(&fake, MARKER).is_some());
        assert!(find_sub(b"clean official asar", MARKER).is_none());
    }

    #[test]
    fn version_scan_reads_package_json_field() {
        let head = br#"{"name":"antigravity","productName":"Antigravity","version":"2.29.0","main":"dist/main.js"}"#;
        assert_eq!(first_version_in(head).as_deref(), Some("2.29.0"));
        assert_eq!(first_version_in(b"{}"), None);
    }

    #[test]
    fn tail_keeps_last_lines() {
        assert_eq!(tail("abcdef", 10), "abcdef");
        assert_eq!(tail("abcdef", 3), "…def");
    }
}
