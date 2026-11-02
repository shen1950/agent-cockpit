# 支持的 Agent

本页数据来自 `src-tauri/src/registry.rs` 的内置登记表与 `src-tauri/src/local_quota.rs` 的直读清单，随版本变动。

## 字段说明

| 字段 | 含义 |
|---|---|
| **隔离方式** | `多开` = 支持独立实例并行（`--user-data-dir` 或家目录环境变量）；`换号` = 只能单开，用 NTFS junction 切换默认档案 |
| **额度直读** | 能把该产品自己存的余额直接解出来。`身份` 表示只能读出「登的是谁」，读不到额度（会如实报未知，不用 0 冒充） |
| **反代上游** | 能作为本地反代的来源。未列出的产品只能通过「API 与额度」里的 Key 进池子 |

## 国内产品

| 产品 | 类型 | 隔离方式 | 额度直读 | 反代上游 |
|---|---|---|---|---|
| Qoder CN | Electron | 换号 | ✅ | ✅ |
| Qoder CN IDE | Electron | 多开 | — | — |
| WorkBuddy | Electron | 换号 | ✅ | ✅ |
| WorkBuddy 国际版 | Electron | 换号 | ✅ | ✅ |
| TRAE CN | Electron | 多开 | ✅ | ✅ |
| TRAE SOLO CN | Electron | 多开 | — | — |
| 千问办公 | Electron | 换号 | ✅ | ✅ |
| MiniMax Code | Electron | 换号 | ✅ | ✅ |
| 妙手（美团 CatPaw） | Electron | 换号 | ✅ | ✅ |
| AStudio | Electron | 换号 | ✅ | ✅ |
| ZCode | Electron | 多开 | ✅ | — |
| DeepSeek Harness | Electron | 换号 | ✅ | — |
| Marvis | plain | 换号 | ✅ | — |
| Xiaomi MiMo | Electron | 换号 | ✅ | — |
| Coze | Electron | 换号 | 身份 | — |
| CodeBuddy | Electron | 多开 | — | — |
| CodeBuddy CN | Electron | 多开 | — | — |

## 国际产品

| 产品 | 类型 | 隔离方式 | 额度直读 | 反代上游 |
|---|---|---|---|---|
| Qoder | Electron | 换号 | ✅ | ✅ |
| Trae | Electron | 多开 | — | — |
| TRAE SOLO | Electron | 多开 | — | — |
| Cursor | Electron | 多开 | — | — |
| Kiro | Electron | 多开 | — | — |
| Windsurf | Electron | 多开 | — | — |
| Antigravity | Electron | 多开 | 身份 | — |
| GitHub Copilot（VS Code） | Electron | 多开 | — | — |
| Zed | Electron | 多开 | — | — |
| Grok Bot | Electron | 多开 | — | — |
| Claude 桌面版 | plain | 换号 | — | — |

## CLI

| 产品 | 隔离方式 | 额度直读 | 反代上游 |
|---|---|---|---|
| Codex CLI | 多开（`CODEX_HOME`） | — | — |
| Claude Code CLI | 多开（`CLAUDE_CONFIG_DIR`） | — | — |
| OpenCode | 换号 | — | ✅（需本地桥） |
| ChatGPT（Codex 桌面版，MSIX） | 多开 | — | — |

## 未登记的产品

绿色版、没有快捷方式的程序，用侧栏下方 **「＋ 添加程序」** 登记一个可执行文件路径即可。登记只记录路径，**不改动那个程序的任何文件**；能不能多开由内核实测判定。

自动扫描到的条目可以「隐藏此程序」，手动添加的可以「移除」，两者都**不碰账号目录**。

## 关于「额度直读」的实现边界

- 直读依赖对各家客户端本地存储的解析，**属于逆向**。上游一改格式就会失效，表现为读不到或解密失败；
- 读得到余额的会换算成逐笔积分结构，并做**账号绑定校验**防止串号（拿 A 账号的凭据去读 B 账号的目录会被拒）；
- 读不到时一律报 `None`，**绝不用 0 冒充余额**——0 会被误读成「额度用完了」，而这两件事完全不同；
- 部分产品（如 Coze、Antigravity）厂商根本没有余额接口，只能认出账号身份，这类不做任何额度推测。

## 关于「反代上游」

反代上游需要该产品有可用的 API Token 或网页会话。各家协议差异很大（OpenAI 兼容 / Anthropic / 私有 SOLO / 网页 WebSocket），部分需要本地桥做协议翻译。详细差别见 [PROXY.md](PROXY.md)。
