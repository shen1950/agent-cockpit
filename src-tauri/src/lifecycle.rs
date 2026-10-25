//! 生命周期日志：把「程序是怎么没的」留在磁盘上。
//!
//! 起因是 2026-10-20 那次「点了开启反代应用就消失了」：查下来是撞上了换 exe 的部署，
//! 但当时只能拿 `bin/` 里回滚件的时间戳反推，现场什么都没留下——系统日志里零记录，
//! 因为这不是崩溃，是被外部结束；而 Rust 的 panic 默认只往 stderr 印，release 产物
//! 是 GUI 程序（`windows_subsystem = "windows"`），stderr 根本没有去处。
//!
//! 现在启动 / 退出 / panic 各写一行。**被强杀时不会写出 exit 行**，所以
//! 「只有 start 没有配对的 exit」本身就是证据：那是外部 Stop-Process、任务管理器
//! 或者系统关机干的，不是程序自己走的退出流程。

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use chrono::Local;

const NAME: &str = "lifecycle.log";
/// 超过这个体积就在下次启动时裁一次，只留最近 KEEP_LINES 行
const MAX_BYTES: u64 = 512 * 1024;
const KEEP_LINES: usize = 400;

/// 谁要求的退出。托盘「退出」「重启应用」、窗口关闭都各自记一句，
/// 让 exit 那行说得出原因，而不是只留一个"它结束了"。
static WHY: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());

pub fn path() -> PathBuf {
    crate::registry::base_dir().join(NAME)
}

/// 记下退出的发起人。调用点在托盘菜单和窗口事件里。
pub fn why(s: &str) {
    if let Ok(mut g) = WHY.lock() {
        *g = s.to_string();
    }
}

fn take_why() -> String {
    match WHY.lock() {
        Ok(mut g) if !g.is_empty() => std::mem::take(&mut *g),
        Ok(_) => "未记录（被外部结束 / 崩溃 / 没走退出流程）".to_string(),
        // 别的线程 panic 时锁会被搞坏：宁可少说原因，也不许因为记日志再 panic 一次
        Err(_) => "未记录（原因锁不可用）".to_string(),
    }
}

fn write_line(kind: &str, detail: &str) {
    let line = format!(
        "{}  pid={:<7} {:<6} {}\n",
        Local::now().format("%Y-%m-%d %H:%M:%S"),
        std::process::id(),
        kind,
        detail
    );
    let p = path();
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&p) {
        let _ = f.write_all(line.as_bytes());
    }
}

/// 必须在 `main` 最开头调用，argv 里的 `--rehome` 这类命令行分支也要留痕
pub fn start() {
    let argv: Vec<String> = std::env::args().collect();
    let exe = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    trim_if_huge();
    write_line("start", &format!("exe={} argv={:?}", exe, argv));
}

pub fn exit() {
    write_line("exit", &format!("原因={}", take_why()));
}

/// 值得单独留一笔的中间事件（换包、自动恢复反代失败之类）
pub fn note(s: &str) {
    write_line("note", s);
}

/// panic 从"只印到没人看得见的 stderr"变成"文件里有一行带源位置的记录"。
/// 装得越早越好，所以放在 main 的第一批语句里。
pub fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        let loc = info
            .location()
            .map(|l| format!("{}:{}", l.file(), l.line()))
            .unwrap_or_else(|| "位置未知".into());
        write_line("panic", &format!("{} @ {}", info, loc));
    }));
}

pub fn tail(n: usize) -> Vec<String> {
    tail_from(&path(), n)
}

/// 只读文件尾部：为了最后几十行把整个文件读进内存不值当，
/// 而且这文件可能正好在被别的线程追加。
fn tail_from(p: &Path, n: usize) -> Vec<String> {
    let f = match std::fs::File::open(p) {
        Ok(f) => f,
        Err(_) => return Vec::new(),
    };
    let size = match f.metadata() {
        Ok(m) => m.len(),
        Err(_) => return Vec::new(),
    };
    // 每行最长约 200 字节，按 256 估一个窗口，再兜个 8KB 下限
    let window = ((n as u64 + 1) * 256).max(8 * 1024);
    let from = size.saturating_sub(window);
    let mut buf = Vec::new();
    // 只读尾部一段，不需要 BufReader：后面是一次 read_to_end，不是逐行读
    let mut r = f;
    if r.seek(SeekFrom::Start(from)).is_err() {
        return Vec::new();
    }
    if r.read_to_end(&mut buf).is_err() {
        return Vec::new();
    }
    let text = String::from_utf8_lossy(&buf).to_string();
    let mut lines: Vec<String> = text.lines().map(|s| s.to_string()).collect();
    // 窗口切在行中间：第一行是半条，丢掉
    if from > 0 && !lines.is_empty() {
        lines.remove(0);
    }
    if lines.len() > n {
        lines = lines.split_off(lines.len() - n);
    }
    lines
}

fn trim_if_huge() {
    let p = path();
    let size = match std::fs::metadata(&p) {
        Ok(m) => m.len(),
        Err(_) => return,
    };
    if size <= MAX_BYTES {
        return;
    }
    let body = tail_from(&p, KEEP_LINES).join("\n");
    let _ = std::fs::write(&p, format!("{}\n", body));
}

#[cfg(test)]
mod tests {
    use super::tail_from;

    /// 临时目录里造一份日志：不能碰用户真文件
    fn tmp(name: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("cockpit-lifecycle-test-{}-{}", std::process::id(), name));
        let _ = std::fs::create_dir_all(&p);
        p.join("lifecycle.log")
    }

    /// 要最后 n 行，且顺序不变
    #[test]
    fn tail_keeps_last_lines_in_order() {
        let p = tmp("order");
        let body: String = (1..=50).map(|i| format!("line {}\n", i)).collect();
        std::fs::write(&p, body).unwrap();
        let got = tail_from(&p, 5);
        assert_eq!(got, vec!["line 46", "line 47", "line 48", "line 49", "line 50"]);
        let _ = std::fs::remove_file(&p);
    }

    /// 窗口起点落在行中间时，那条半行不许混进结果
    #[test]
    fn tail_drops_the_truncated_first_line() {
        let p = tmp("half");
        // 前面塞足够长的行，保证读窗口是从中间开始的（from > 0）
        let body: String = (1..=400)
            .map(|i| format!("line {:04} {}\n", i, "x".repeat(160)))
            .collect();
        std::fs::write(&p, body).unwrap();
        let got = tail_from(&p, 3);
        assert_eq!(got.len(), 3, "只该拿到三条完整行");
        assert!(got.iter().all(|l| l.starts_with("line ")), "拿到半行了：{:?}", got);
        assert!(got.last().unwrap().starts_with("line 0400"));
        let _ = std::fs::remove_file(&p);
    }

    /// 文件还没生成过（首次运行）时不能炸
    #[test]
    fn tail_on_missing_file_is_empty() {
        let p = tmp("missing").join("nope.log");
        assert!(tail_from(&p, 10).is_empty());
    }
}
