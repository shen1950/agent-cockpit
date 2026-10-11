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
