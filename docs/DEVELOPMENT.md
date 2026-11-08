# 开发指南

## 环境准备

| 依赖 | 说明 |
|---|---|
| Rust（MSVC 工具链） | 后端编译，edition 2021。安装时选 `x86_64-pc-windows-msvc` |
| VS Build Tools | 需要「使用 C++ 的桌面开发」工作负载 |
| Node.js | Tauri CLI、前端检查与浏览器测试；原生前端本身无打包步骤 |
| WebView2 Runtime | Windows 11 自带；Windows 10 需单独安装 |

先运行 `npm install` 安装开发依赖。编译和测试使用下面的协调入口；不要先启动一轮独立的 Tauri/Cargo 构建。

## 铁律：编译与测试只走 agsync

项目根目录有 `.agent-sync/`，多对话并行开发时编译与部署是共享资源。**禁止直接 `cargo build` / `cargo test` / `npx tauri build`。**

```bash
export AGSYNC_OWNER=<你这个对话的固定名>      # 定下就别改
agsync board                                 # 先看谁在改什么
agsync task new --title "..." --files <要改的文件>
agsync conflicts --task-id <任务号>           # 与别人的文件重叠就换文件或等
# ...改代码...
agsync task ready                            # 标就绪，等批次编译
agsync build                                 # 执行该工作区配置的编译与测试命令
agsync deploy                                # 部署
```

`agsync` 不在 PATH 时用 `python ~/.agents/skills/agent-sync/bin/agsync.py`。

`agsync` 协调共享工作区，不是自动隔离未完成修改的构建系统；所有参与者都应登记并确认就绪，避免构建读到别人改到一半的文件。编译失败时先读自己任务的错误归因：报错指向自己的文件就修自己的，其他文件由其 owner 处理。

PowerShell 设置身份使用 `$env:AGSYNC_OWNER='你的固定名'`。新克隆目录若没有 `.agent-sync/`，先安装 agent-sync 工具并初始化该工作区；编译命令、测试命令和部署路径需要按本机配置。`agsync build` 是否包含隐私门，取决于工作区实际的 `build_cmd` / `test_cmd`，不能仅凭命令名称认定已经扫描。

### 部署前两条硬检查

1. **实例竞态**：有多个驾驶舱实例在跑就中止，先关掉多开的。
2. **8788 自托管陷阱**：驾驶舱窗口和 8788 反代是同一个进程。若你当前会话的模型正挂在 8788 上，部署换 exe 就是杀掉自己的模型。确认后再部署。

## 测试

默认使用 `agsync build` 中配置的测试入口。需要更换测试命令时使用 `--test-cmd`；下面是可放入该参数的检查内容，不要绕过协调锁直接运行构建或测试：

| 检查内容 | 工作目录为项目根时的命令 |
|---|---|
| Rust 单测 | `python tools/private_build.py test` |
| 粘贴解析 | `npm test` |
| 隐私扫描回归 | `python tests/privacy-check.test.py` |
| 许可版本匹配回归 | `python tests/license-report.test.py` |
| 网关前端 | 选择 `tests/` 中对应的浏览器用例，通过同一批次执行 |

包含 `--include-ignored` 的真机测试可能读本机档案或实际发出请求，应先查看用例的副作用；日常构建不用它替代离线单测。

反代模块带端到端单测：起服务 → 空池 502 → 记日志 → 停止释放端口。

前端与 Python 用例在 `tests/`，由协调批次的测试命令统一执行。

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

发布先完成源码扫描与测试，再用路径脱敏构建检查最终 EXE。以下是各阶段的命令内容；编译和测试命令必须通过 `agsync build --cmd` / `--test-cmd` 执行，部署仍走 `agsync deploy`：

```bash
python tools/privacy_check.py                     # 源码扫描：只报文件/行号/类别，不打印原文
python tests/privacy-check.test.py
python tests/license-report.test.py
python tools/private_build.py build --release     # 编译路径映射为中性目录
python tools/privacy_check.py --binary <exe>      # 二进制扫描（UTF-8 / UTF-16）
python tools/export_public_source.py <仓库外的目标.zip>
```

| 工具 | 作用 |
|---|---|
| `privacy_check.py` | 扫邮箱、家目录路径、密钥形态字面量、私钥；规则可配 |
| `private_build.py` | 把编译路径映射成中性目录，减少二进制里的开发者路径 |
| `export_public_source.py` | 生成**无 `.git` 历史**、无本机配置/日志/旧 exe/备份/账号的源码快照 |
| `license_report.py` | 按 Cargo.lock 的精确版本读取本地缓存，生成许可索引；缺失版本如实标未知 |

导出脚本要求输出路径在仓库外，且不能覆盖已有文件——避免把公开发布包误留在工作区。

发行包应同时附带 [LICENSE](../LICENSE)、[THIRD_PARTY_NOTICES.md](../THIRD_PARTY_NOTICES.md)、`docs/third-party/` 中的原许可和实际打包依赖要求的声明。CPython 归档内的 LICENSE 必须保留；zcode-api 等另行安装的非商业组件若进入组合包，不能把整包简单标为 MIT。许可索引只是索引，不代替许可证原文或必要的源码提供信息。

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
| `8790` | 用户主动开启的手机远控；内置会话索引服务也有自己的默认端口与冲突处理，以当前服务状态为准 |

模型网关与驾驶舱同进程；退出或替换进程会中断该网关连接。普通关窗可只是收进托盘，不等于退出。
