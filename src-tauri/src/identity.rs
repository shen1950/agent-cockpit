// 账号身份识别：从实例数据目录判断"从没启动过 / 启动了但没登录 / 有登录痕迹"
use std::fs;
use std::path::Path;

/// 可能承载登录态的常见落点（Electron / VS Code 系）
const LOGIN_MARKERS: &[&str] = &[
    "Network/Cookies",
    "Cookies",
    "Local Storage/leveldb",
    "User/globalStorage/state.vscdb",
    "User/globalStorage/storage.json",
    "Session Storage",
    "auth.json",
    "credentials.json",
    ".credentials.json",
];

#[derive(Clone, serde::Serialize)]
pub struct Identity {
    pub state: &'static str, // none | init | logged
    pub label: String,
    pub files: usize,
    pub product: String, // 目录里记录的产品标识（防串号用）
}

fn count_files(dir: &Path, cap: usize) -> usize {
    let mut n = 0usize;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(p) = stack.pop() {
        let rd = match fs::read_dir(&p) {
            Ok(r) => r,
            Err(_) => continue,
        };
        for e in rd.filter_map(|x| x.ok()) {
            if n > cap {
                return n;
            }
            match e.file_type() {
                Ok(t) if t.is_dir() => stack.push(e.path()),
                Ok(t) if t.is_file() => n += 1,
                _ => {}
            }
        }
    }
    n
}

/// 状态报文（每 4 秒一轮）里每个账号都要数一遍档案目录的文件。
/// 本机 20 个档案目录实测合计约 3.0 秒磁盘 I/O——单是
/// `AppData\Roaming\com.qodercn.app.stable`（6807 个文件）就 0.42s、
/// `.workbuddy-ai`（5247 个）0.29s。每 4 秒重走一遍等于让界面一直泡在磁盘上：
/// 驾驶舱空载就吃掉三分之一颗核，整机跟着发钝（2026-11-05 她说的"反应还非常慢"）。
///
/// 文件数一分钟内基本不变，缓存 30 秒。真正要即时判断的地方（换号前判"空目录"）
/// 走 `inspect()` 本体，不经过缓存。
pub fn inspect_cached(data_dir: &str, app_id: &str) -> Identity {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    use std::time::{Duration, Instant};
    static CACHE: OnceLock<Mutex<HashMap<String, (Instant, Identity)>>> = OnceLock::new();
    const TTL: Duration = Duration::from_secs(30);
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Ok(g) = cache.lock() {
        if let Some((at, v)) = g.get(data_dir) {
            if at.elapsed() < TTL {
                return v.clone();
            }
        }
    }
    let fresh = inspect_for(app_id, data_dir);
    if let Ok(mut g) = cache.lock() {
        // 顺手清掉过期项：账号被删掉后这些键不该一直留着
        g.retain(|_, (at, _)| at.elapsed() < TTL);
        g.insert(data_dir.to_string(), (Instant::now(), fresh.clone()));
    }
    fresh
}

/// 登录态**不在档案目录里**的产品要单独说一句。
///
/// WorkBuddy 的凭据是 `%LOCALAPPDATA%\CodeBuddyExtension\Data\Public\auth\
/// workbuddy-desktop(-ai).info`，档案目录里本来就不会有登录痕迹；通用的 inspect 去数
/// 文件，只会得出「已写入 4001 个文件，未见登录痕迹」——一个明明登着的号被说成没登录，
/// 详情页还跟着显示"还没检测到登录：换号切到空目录会让程序显示未登录甚至黑屏"。
/// 2026-11-05 她截图圈的就是这句"读不明白账号实例"。
///
/// 只改话术，不改判定：真正"登的是谁"由 machine / authpack 那条路读，别在这里造第二份真值。
pub fn inspect_for(app_id: &str, data_dir: &str) -> Identity {
    let mut id = inspect(data_dir);
    if matches!(app_id, "workbuddy" | "workbuddy-intl") && id.state == "init" {
        id.state = "elsewhere";
        id.label = format!(
            "档案已建立（{} 个文件）· 登录态不在这个目录里，现在登的是谁看上方「本机登录」",
            id.files
        );
    }
    id
}

pub fn inspect(data_dir: &str) -> Identity {
    let d = Path::new(data_dir);
    if !d.exists() {
        return Identity {
            state: "none",
            label: "目录尚未创建".into(),
            files: 0,
            product: String::new(),
        };
    }
    let files = count_files(d, 4000);
    if files == 0 {
        return Identity {
            state: "none",
            label: "从未启动过 · 点「启动并登录」后在窗口里登录".into(),
            files: 0,
            product: String::new(),
        };
    }

    let mut product = String::new();
    let st = d.join(".qoder-app-status.json");
    if let Ok(s) = fs::read_to_string(&st) {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&s) {
            product = v["product"].as_str().unwrap_or("").to_string();
        }
    }

    let hits: Vec<&str> = LOGIN_MARKERS
        .iter()
        .filter(|m| d.join(m).exists())
        .cloned()
        .collect();
    let (state, label) = if hits.is_empty() {
        (
            "init",
            format!("已写入 {} 个文件，未见登录痕迹", files),
        )
    } else {
        (
            "logged",
            format!("有登录数据（{} 项标记 · 共 {} 文件）", hits.len(), files),
        )
    };
    Identity {
        state,
        label,
        files,
        product,
    }
}
