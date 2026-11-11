//! 内置 agenthub（agent-memory-suite, MIT）的命令行入口。
//!
//! 发布版优先跑 `usage-runtime/` 里解压出来的 Python + agenthub.py，
//! 这样签到/额度不再依赖用户另装 agent-memory-suite。
//! 内置运行时未通过校验时将错误返回调用方。
use std::process::Command;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

pub fn bundled_available() -> bool {
    crate::usage::bundled_agenthub().is_ok()
}

/// AStudio 的资料目录跟着安装位置走，不在 ~ 下。驾驶舱已经从 exe 推出
/// `<安装目录上一级>/AStudio Data/userdata`，这里把它的上一级交给 agenthub，
/// 否则内置版按默认安装位置找不到本机装在别处的 AStudio。
fn apply_astudio_env(c: &mut Command) {
    if std::env::var_os("ASTUDIO_HOME").is_some() {
        return;
    }
    let Some(app) = crate::registry::app_def("astudio") else { return };
    let Some(exe) = crate::registry::resolve_exe(&app) else { return };
    let Some(data) = crate::registry::astudio_data_dir(&exe) else { return };
    if let Some(root) = data.parent() {
        c.env("ASTUDIO_HOME", root);
    }
}

pub fn command(args: &[&str]) -> Result<Command, String> {
    let (python, script) = crate::usage::bundled_agenthub()?;
    let mut c = Command::new(python);
    c.args(["-I", "-B", "-X", "utf8"]).arg(script).args(args);
    let base = crate::registry::base_dir();
    c.env("AGENTHUB_HOME", base.join("agenthub-state"));
    c.env("AGENT_COCKPIT_HOME", base);
    if let Some(secret) = crate::wbproxy::at_rest_secret() {
        c.env("AGENTHUB_WORKBUDDY_AT_REST_SECRET", secret);
    }
    apply_astudio_env(&mut c);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(CREATE_NO_WINDOW);
    }
    Ok(c)
}
