//! 内置 agenthub（agent-memory-suite, MIT）的命令行入口。
//!
//! 发布版优先跑 `usage-runtime/` 里解压出来的 Python + agenthub.py，
//! 这样签到/额度不再依赖用户另装 agent-memory-suite。
//! 内置运行时损坏或被安全软件拦住时，退回 PATH 上的 `agenthub`（开发者机器仍可用自己的版本）。
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

pub fn command(args: &[&str]) -> Command {
    if let Ok((python, script)) = crate::usage::bundled_agenthub() {
        let mut c = Command::new(python);
        c.args(["-I", "-X", "utf8"]).arg(script).args(args);
        c.env("PYTHONIOENCODING", "utf-8");
        c.env("PYTHONUTF8", "1");
        apply_astudio_env(&mut c);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            c.creation_flags(CREATE_NO_WINDOW);
        }
        return c;
    }
    // 退回外部安装：agenthub 是 .cmd 包装，CreateProcess 直接执行不了，必须经 cmd.exe。
    let mut c = Command::new("cmd.exe");
    c.args(["/d", "/c", "agenthub"]).args(args);
    apply_astudio_env(&mut c);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(CREATE_NO_WINDOW);
    }
    c
}
