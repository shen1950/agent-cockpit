# 第三方声明

## 本项目许可

AgentCockpit 自身以 **MIT License** 发布，见 [LICENSE](LICENSE)，© 2026 shen1950。

本文件列出本项目分发时所包含的第三方组件及其许可信息。**本文件不构成法律意见**；如需就分发合规取得确定性结论，请自行咨询专业法律人士。

## 运行时依赖（不随本项目分发）

| 组件 | 说明 |
|---|---|
| **WebView2 Runtime** | 微软组件，Windows 11 自带，Windows 10 需单独安装。本项目不内嵌、不分发该运行时，其使用受微软的许可条款约束 |
| **Rust 标准库 / MSVC 运行库** | 随 Rust 工具链与 Visual Studio 生成工具提供 |
| **各 AI Agent 客户端** | 本项目只做启动、隔离与管理，不修改、不重分发任何 Agent 的程序文件 |

## Rust 依赖许可分布

后端依赖锁定在 `src-tauri/Cargo.lock`。当前共 **509 个第三方 crate**，许可分布如下（按数量排序，取前 12 组）：

| 许可表达式 | 数量 |
|---|---|
| MIT OR Apache-2.0 | 236 |
| MIT | 115 |
| Apache-2.0 OR MIT | 50 |
| Zlib OR Apache-2.0 OR MIT | 19 |
| Unicode-3.0 | 18 |
| MIT/Apache-2.0 | 13 |
| Unlicense OR MIT | 10 |
| Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT | 5 |
| **MPL-2.0** | **5** |
| Apache-2.0/MIT | 3 |
| BSD-3-Clause | 3 |
| ISC | 3 |

其余 29 个 crate 分散在小规模许可组中，包括 `Apache-2.0`(2)、`Zlib`(2)、`BSD-2-Clause OR Apache-2.0 OR MIT`(2)、`BSD-3-Clause OR Apache-2.0`(2)、`BSD-3-Clause OR MIT OR Apache-2.0`(2)、`CDLA-Permissive-2.0`(2)、`MIT OR Apache-2.0 OR LGPL-2.1-or-later`(2)、`MIT OR Zlib OR Apache-2.0`(2)、`Unlicense/MIT`(2)，以及若干单 crate 组（`0BSD OR MIT OR Apache-2.0`、`BSD-3-Clause AND MIT`、`CC0-1.0 OR MIT-0 OR Apache-2.0` 等）。

**关于 MPL-2.0**：上表中的 5 个 MPL-2.0 crate（`cssparser`、`cssparser-macros`、`dtoa-short`、`option-ext`、`selectors`）来自 HTML 解析依赖链。MPL-2.0 是**文件级**的弱 copyleft：静态链接不会使整个作品受 MPL 约束，但对 MPL 文件本身的修改需按 MPL-2.0 提供源码。本项目未修改这些 crate。

### 重新生成这份报告

许可信息从本地 cargo registry 缓存中的 crate 元数据读取，因此反映的是**实际参与编译**的版本。依赖变动后请重新生成：

```bash
python tools/license_report.py                     # 打印全部分组
python tools/license_report.py --top 12            # 只看最大的 12 组
python tools/license_report.py --out LICENSES.md   # 写入文件
```

脚本只输出包名、版本与 SPDX 表达式，不含任何本机路径。若某个 crate 未在缓存中解包，会被归到 `UNKNOWN` 组——此时先跑一次 `cargo fetch` 再重新生成。

## 主要直接依赖

| Crate | 许可 | 用途 |
|---|---|---|
| `tauri` / `tauri-build` | Apache-2.0 OR MIT | 桌面应用框架 |
| `tauri-plugin-autostart` | Apache-2.0 OR MIT | 开机自启 |
| `tauri-plugin-notification` | Apache-2.0 OR MIT | 系统通知 |
| `tauri-plugin-single-instance` | Apache-2.0 OR MIT | 单实例 |
| `serde` / `serde_json` | MIT OR Apache-2.0 | 序列化 |
| `toml` | MIT OR Apache-2.0 | 配置读写 |
| `sysinfo` | MIT | 进程与系统信息 |
| `ureq` | MIT OR Apache-2.0 | HTTP 上游请求 |
| `tiny_http` | MIT OR Apache-2.0 | 本地反代服务端 |
| `zip` | MIT | 备份打包与恢复 |
| `image` | MIT OR Apache-2.0 | 图标处理 |
| `qrcode` | MIT OR Apache-2.0 | 远程配对二维码 |
| `webview2-com` | MIT | WebView2 cookie 读取 |
| `windows` | MIT OR Apache-2.0 | Win32 API |

## 参考与致谢

- **[farion1231/cc-switch](https://github.com/farion1231/cc-switch)**（MIT）：多平台 CLI 配置切换工具。本项目的「地板 / 独有 / 用户键三分法」「契约摘要」「pending 前滚」等思路受其启发，并沿用其硬规则：官方卡永不参与故障转移、第三方 Key 不写入 `auth.json`、暂存读不出就拒绝操作。
- **[jlcodes99/cockpit-tools](https://github.com/jlcodes99/cockpit-tools)**：内置 Agent 登记表的平台清单参照其整理。

代码未直接复制上述项目；若权利人对署名方式有意见，请通过 [Issue](https://github.com/shen1950/agent-cockpit/issues) 联系，会立即调整。

## 商标

Qoder、WorkBuddy、TRAE、Cursor、Kiro、Windsurf、Antigravity、GitHub Copilot、Zed、Grok、Claude、ChatGPT、Codex、OpenAI、MiniMax、AStudio、千问、DeepSeek、Marvis、MiMo、CodeBuddy、妙手 等名称与商标归各自所有者所有。

本项目**仅在本机范围内管理这些产品的账号与额度数据**，与上述任何厂商均无隶属、授权、赞助或合作关系。界面中出现的产品名称仅用于标识被管理的程序，不构成商标使用的授权主张，也不代表相关厂商对本项目的认可。
