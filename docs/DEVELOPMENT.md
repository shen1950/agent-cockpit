# 开发指南

## 环境准备

| 依赖 | 说明 |
|---|---|
| Rust（MSVC 工具链） | 后端编译，edition 2021。安装时选 `x86_64-pc-windows-msvc` |
| VS Build Tools | 需要「使用 C++ 的桌面开发」工作负载 |
| Node.js | 前端无构建步骤，仅用于 `npm test` |
| WebView2 Runtime | Windows 11 自带；Windows 10 需单独安装 |

```bash
npm install
npx tauri dev        # 开发模式（带热重载与 DevTools）
npx tauri build      # 产出 exe + NSIS 安装包
```

## 铁律：编译与测试只走 agsync

项目根目录有 `.agent-sync/`，多对话并行开发时编译与部署是共享资源。**禁止直接 `cargo build` / `cargo test` / `npx tauri build`。**

```bash
export AGSYNC_OWNER=<你这个对话的固定名>      # 定下就别改
agsync board                                 # 先看谁在改什么
agsync task new --title "..." --files <要改的文件>
agsync conflicts --task-id <任务号>           # 与别人的文件重叠就换文件或等
# ...改代码...
agsync task ready                            # 标就绪，等批次编译
agsync build                                 # 编译 + 测试（含隐私门）
agsync deploy                                # 部署
```

`agsync` 不在 PATH 时用 `python ~/.agents/skills/agent-sync/bin/agsync.py`。

不登记的改动不会被编进批次，等于白干。编译失败时**先读自己任务文件里的 `error:` 段**：报错指向你的文件就修你的；与你无关就别动别人的文件，你的任务会自动退回 `ready`。

### 部署前两条硬检查

1. **实例竞态**：有多个驾驶舱实例在跑就中止，先关掉多开的。
2. **8788 自托管陷阱**：驾驶舱窗口和 8788 反代是同一个进程。若你当前会话的模型正挂在 8788 上，部署换 exe 就是杀掉自己的模型。确认后再部署。

## 测试

```bash
npm test                                     # 粘贴解析器离线用例（不依赖 DOM / Tauri）
cd src-tauri && cargo test                   # Rust 单测 + 反代端到端
cd src-tauri && cargo test -- --include-ignored   # 真机用例（真扫本机 / 真连反代）
```

反代模块带端到端单测：起服务 → 空池 502 → 记日志 → 停止释放端口。

前端用例在 `tests/`（`node tests/paste-test.js` 可直接跑）。另有 Python 侧用例（隐私门、用量统计等）。

## 本地调试技巧

### 浏览器里改前端

`ui/index.html` 直接用浏览器打开即可——没有 `window.__TAURI_INTERNALS__` 时会自动 import `ui/mock.js` 挂假数据，不碰真机：

```bash
python -m http.server 8899 --directory ui     # 后台起服务
# 浏览器打开 http://127.0.0.1:8899/index.html
```

> 本机 `http_proxy` 会劫持 `127.0.0.1` 的请求，浏览器和 curl 记得加 `--noproxy '*'`。

`ui/app.js` 顶层的 `const S` 与 `function` 可在 CDP 的 `Runtime.evaluate` 里直接读写，方便临时注入延迟、放大 mock 数据压测列表。

### 无头 Chrome 真渲染

```bash
chrome --headless --remote-debugging-port=9222
# 用 node 走 CDP 协议（node 22 内置 WebSocket）
```

### 真机 WebView2 开 CDP

```bash
set WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS=--remote-debugging-port=9222
```

程序装了 single-instance 插件，得先关掉已在跑的实例。

### Rust 工具链不在默认位置时

部分自动化会话的 Git Bash 环境是残缺的（PATH 为空、`LOCALAPPDATA` 未设置、powershell 不在 PATH），跑 `agsync build` / `deploy` 前先把环境补齐：

```bash
# 下面三处换成你自己机器上的实际位置
export PATH="/c/Windows/System32/WindowsPowerShell/v1.0:<你的 cargo bin>:/usr/bin:/bin:$PATH"
export CARGO_HOME='<你的 CARGO_HOME>'
export RUSTUP_HOME='<你的 RUSTUP_HOME>'
export LOCALAPPDATA="$USERPROFILE/AppData/Local"
```

否则 secrets / authpack 单测会假红、`pre_deploy.ps1` 起不来。

## 发布流程

发布必须过隐私门，顺序不能乱：

```bash
python tools/privacy_check.py                     # 源码扫描：只报文件/行号/类别，不打印原文
python tests/privacy-check.test.py
python tools/private_build.py build --release     # 编译路径映射为中性目录
python tools/privacy_check.py --binary <exe>      # 二进制扫描（UTF-8 / UTF-16）
python tools/export_public_source.py <仓库外的目标.zip>
```

| 工具 | 作用 |
|---|---|
| `privacy_check.py` | 扫邮箱、家目录路径、密钥形态字面量、私钥；规则可配 |
| `private_build.py` | 把编译路径映射成中性目录，减少二进制里的开发者路径 |
| `export_public_source.py` | 生成**无 `.git` 历史**、无本机配置/日志/旧 exe/备份/账号的源码快照 |
| `license_report.py` | 从 Cargo.lock + registry 缓存生成第三方许可报告 |

导出脚本要求输出路径在仓库外，且不能覆盖已有文件——避免把公开发布包误留在工作区。

## 发布前的额外人工检查

任何扫描器都无法保证不存在全部个人信息。正式公开前还要人工过一遍：

- 截图、视频、README 里的演示图
- 提交作者信息（姓名 / 邮箱）
- Release 附件
- Issue 与日志里贴过的内容

旧提交里的隐私残留改当前文件删不掉。需要清除历史就得重写历史并强推，这是独立的高风险操作，必须在明确目标后单独处理。**对外分发请用上面的无历史快照或新建干净仓库，不能把原仓库连带 `.git` 打包上传。**

若旧版本曾包含有效凭据，需要在对应服务撤销或轮换——清除源码不能撤回已经下载的副本。

## 多实例与端口

| 端口 | 用途 |
|---|---|
| `8788` | 本地反代（OpenAI 兼容，只绑 127.0.0.1） |
| `8765` | 千问办公本地桥（按需自愈拉起，空闲自退） |

反代与驾驶舱窗口同进程，杀了窗口 8788 就没了，反之亦然。
