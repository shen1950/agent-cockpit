# Agent 驾驶舱 · AgentCockpit

**国内 AI Agent 多开 / 换号 / 额度过期提醒 / API Key 台账 / 本地反代，一台机器全管起来。**

![平台](https://img.shields.io/badge/platform-Windows%2010%2F11-blue) ![框架](https://img.shields.io/badge/framework-Tauri%202-orange) ![语言](https://img.shields.io/badge/rust-2021-red) ![许可](https://img.shields.io/badge/license-MIT-green)

Tauri 2 桌面应用（Rust 后端 + WebView2 前端），提供 Windows x64 安装包和便携 ZIP。使用便携版时请完整解压，保留主程序同目录的 `codex-shared-resume.exe`。用量采集、会话索引与签到/额度后端随程序内置，无需单独安装 Python、AgentFind 或 agent-memory-suite；首次使用时会在驾驶舱数据目录解压运行时。程序不依赖 Electron，关窗后会驻留系统托盘继续监控额度。

WorkBuddy 5.6 起把登录态改成客户端运行时密钥加密。驾驶舱只读检查本机 WorkBuddy `keyblob` 与正在运行的主进程，候选密钥通过 keyblob 的 GCM 校验后才写入 Windows DPAPI 保管箱；客户端升级或轮换密钥后会自动刷新。源码和发布包不保存厂商密钥。

**直接下载**：[Releases](https://github.com/shen1950/agent-cockpit/releases) 里有免安装便携版和 NSIS 安装包，下载即用，不必自己编译。

---

## 使用前

本项目自有代码依照 MIT 许可证按「原样」（AS IS）提供；担保排除和责任限制在适用法律允许的范围内适用，不排除依法不可排除的责任。第三方组件另按各自许可提供。

请务必了解以下三点：

1. **合规由你自己负责。** 各 AI 产品的用户协议可能禁止多账号、账号共享、自动化访问或对客户端接口的逆向调用。本软件只提供管理能力，不授予你绕过任何服务条款的权利。使用前请自行阅读并遵守你所使用产品的条款，以及你所在司法辖区的法律。
2. **接入密钥需要保密。** 下游客户端使用网关密钥，上游凭据用于对应服务的鉴权；正常转发不要求把上游原始密钥交给客户端。图片代读、备用路由和模型识别会向相应模型服务发送内容。接入、日志和费用说明见 [docs/PROXY.md](docs/PROXY.md)。
3. **数据可能延迟或缺失。** 余额、过期时间和费用估算来自本机记录或接口数据，以官方页面与账单为准。账号操作前请保留备份。

完整条款见 [docs/LEGAL.md](docs/LEGAL.md)。商标与产品名称归各自所有者所有，本项目与任何被管理的 AI 产品厂商均无隶属、授权或合作关系。

> 详细使用手册（含每一步操作说明）见 [docs/USER_MANUAL.md](docs/USER_MANUAL.md)。

---

## 为什么做这个

同时使用 Qoder、WorkBuddy、TRAE、CodeBuddy 等工具时，账号档案、积分规则、到期时间和 API Key 往往分散在不同地方。这个驾驶舱把常用管理操作集中到一张桌面：

- **多开 / 换号**：Electron 系用 `--user-data-dir` 隔离实例，CLI 系用 `CODEX_HOME` /
  `CLAUDE_CONFIG_DIR` 隔离账号；只能单开的程序用 NTFS junction 把默认入口指向当前账号，
  零额外空间、对话记录跟着账号走。
- **额度过期提醒**：赠送积分的到期时间各家都没有接口——能本机直读的（解客户端自己存的积分）
  一键直读；只能看官网的，截图贴进去让多模态模型读成逐笔额度，人工核对后落盘。
  30 天内临期的聚成顶栏徽章，7 天内启动时弹窗提醒。
- **API Key 台账**：站点 → Key 两级管理。一段乱排版的中转站信息（或截图）直接贴进
  「快速登记」，本机规则解析失败时交给多模态模型兜底，站点、接入点、每条 key 与用途名当场建好。
- **本地网关**：把账号 / Key 按供应商平铺管理，生成 `ac-…` 接入密钥给客户端用，
  请求日志能看到每一笔走了哪个供应商和备用路由。

## 功能一览

| 模块 | 能干什么 |
|---|---|
| 首页 | 落地页不是"第一个 Agent"：今天要处理什么一眼看完（临期积分 / 今天还能领 / Agent 概览 / 最近领过），两个主操作 **一键刷新全部额度**、**一键签到** |
| Agent 管理 | 自动扫描本机装了哪些 AI Agent，新增/卸载有标记；六步向导建账号（登录 → 数据来源 → 额度） |
| 多开/换号 | 可多开的建独立目录同屏跑；仅可换号的 junction 切换，随时切回 |
| 额度监控 | 本机直读（Qoder / WorkBuddy 系）+ 截图 AI 识别 + 适配器抓取（OpenAI / new-api / one-api 格式） |
| 每日签到 | 产品 → 账号两级：**每个账号实例各看各的签到状态**，不用先切号才看得到另一个；每行一颗 ↻ 只重查这一家；「登录并领取」一条龙（拉起客户端 → 等你登进去 → 自动领掉）；已领完 ≠ 不用领；没立刻到账的积分自动盯着；**领取日志**保留一年、可按今天/7 天/30 天/全部筛并导出 CSV；打卡对话按当前可用模型规则发起，是否扣费以服务商为准。签到/额度后端（agenthub + cryptography）随程序内置，开箱即用；内置组件未通过校验时会显示错误，可用原发布包修复 |
| API 台账 | 站点 → 多 Key，各带用途标签、额度、过期日、测活；**站点内批量管理**（勾选 / 全选 / 改用途 / 改过期日 / 测活 / 复制 / 移到其他站点 / 删除）；**查重一键全选** / 一键接入到 Qoder 等程序 |
| 快速登记 | 粘贴任意排版文本或截图 → 本机规则优先解析（离线、零 token）→ 多模态模型兜底 → 核对后落盘 |
| AI 识别 | 文本/截图两条通道分开配模型；识别全程有进度反馈，同时只跑一次，重复请求直接挡掉 |
| 本地网关 | 供应商列表 + 接入密钥；可选择目标 Agent 接入，配置图片能力、上下文/输出/压缩阈值、供应商排队与备用模型，查看实际请求路由与费用估算 |
| 会话看板 | 内置 AgentFind 索引本机各 Agent 的会话记录，按关键词 / 项目 / 日期 / 产物文件名反查「何时 · 哪个产品 · 哪个项目 · 哪场对话」，可一键跳回原应用；无需另行安装服务 |
| 凭据保护 | 保管箱使用 DPAPI + 可选主密码；客户端配置、备份、剪贴板可能另含凭据副本，见隐私说明 |
| 数据治理 | 数据根搬家 / 回滚、可再生大文件去重、本机 Agent 盘点 |

## 支持的 Agent

内置登记表覆盖下面这些（参照 [jlcodes99/cockpit-tools](https://github.com/jlcodes99/cockpit-tools)
的平台清单补齐）。驾驶舱也会从这台机器的安装记录、快捷方式、已知 CLI 和用户登记的安装目录定位程序。
**这台机器上没装、也没有账号卡的不占侧栏格子**，要看全量去 设置 → 高级 → 「显示未安装的程序」。

| 类别 | Agent |
|---|---|
| 国内 | Qoder CN、Qoder CN IDE、WorkBuddy（国内版 / 国际版）、Trae CN、TRAE SOLO CN、千问办公、MiniMax Code、AStudio、ZCode、DeepSeek Harness、Marvis、Xiaomi MiMo、CodeBuddy / CodeBuddy CN |
| 国际 | Qoder、Trae、TRAE SOLO、Cursor、Kiro、Windsurf、Antigravity、GitHub Copilot（VS Code）、Zed、Grok Bot |
| CLI | Codex CLI、Claude Code CLI、OpenCode |

各家上游的协议差异、令牌位置与已知限制见 [docs/SUPPORTED_AGENTS.md](docs/SUPPORTED_AGENTS.md)。

> 自定义目录中的程序没有被扫描到时，可用「＋ 添加程序」选择它的可执行文件。更换电脑或 Windows 账户后，请先在对应客户端登录，再到签到页刷新。Windows DPAPI 保护的保管箱与客户端登录态通常只能由原机器上的原用户解密。

## 快速开始

普通使用下载对应版本的发布附件，运行后添加账号即可。从源码开发、构建与测试见 [开发指南](docs/DEVELOPMENT.md)。工作区存在 `.agent-sync/` 时，必须通过 `agsync build` / `agsync deploy` 协调，不直接启动另一轮构建。

构建依赖：**Rust（MSVC 工具链）** + **VS Build Tools（C++ 工作负载）** + WebView2 Runtime（Win11 自带）。
纯前端调试可以直接用浏览器打开 `ui/index.html`（自动挂 mock 数据，不碰真机）。

本机开发时编译与测试请走 `agsync build`，不要直接 `cargo build`——见 [docs/DEVELOPMENT.md](docs/DEVELOPMENT.md)。

## 接入本地网关

供应商列表支持逐个启用、停用和进入详情；接入密钥只列出已启用的供应商。最后一步「接入本地网关」可选择目标 Agent，每个 Agent 页也有同一入口。支持的客户端可自动写入并恢复配置；高级设置包含原生读图/视觉代读、token 限制、供应商调度和本地计价。详细操作与客户端限制见 [网关说明](docs/PROXY.md)。

路由图按真实请求显示 Agent、网关、上游尝试及改道结果。未发请求时显示等待状态；模型目录刷新不会增加实测次数。

## AI 识别怎么配

到 **设置 → AI 识别** 填一个 OpenAI 兼容接口的 Key（默认阿里云百炼，任意兼容网关都行）：

- **文本模型**：解析粘贴的 key 清单用，小快模型即可（默认 `qwen-flash`）；
- **截图模型**：读积分页截图用，必须是视觉模型（默认 `qwen-vl-flash`）。

识别请求全程流式 + 思考模型自动关思考；页右下角有常驻进度（连接中 / 模型输出中 · 已收 N 字），
同一时间只放行一次识别，连点不会再跑出几份重复结果；同一条 key 重复登记会在落盘时自动合并。

**文本和截图会发送给你配置的模型网关；其中包含密钥时，密钥也会成为模型输入。** 需要离线处理时使用「快速登记」的本机规则解析通道。

## 文档

| 面向 | 文档 | 内容 |
|---|---|---|
| 所有使用者 | [docs/USER_MANUAL.md](docs/USER_MANUAL.md) | 完整使用手册，逐步说明 |
| 所有使用者 | [docs/PROXY.md](docs/PROXY.md) | 本地反代配置与风险（**用反代前必读**） |
| 所有使用者 | [docs/PRIVACY.md](docs/PRIVACY.md) | 数据存在哪、什么会外发 |
| 所有使用者 | [docs/FAQ.md](docs/FAQ.md) | 常见问题与排查 |
| 所有使用者 | [docs/LEGAL.md](docs/LEGAL.md) | 完整免责条款与许可说明 |
| 开发者 | [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) | 模块划分与数据流 |
| 开发者 | [docs/DEVELOPMENT.md](docs/DEVELOPMENT.md) | 环境、构建、测试、发布 |
| 开发者 | [CONTRIBUTING.md](CONTRIBUTING.md) | 提交规范与评审要求 |
| 安全 | [SECURITY.md](SECURITY.md) | 漏洞报告与凭据处理 |
| 维护 | [CHANGELOG.md](CHANGELOG.md) | 版本记录 |

## 项目结构

```
├── ui/                    # 前端：无框架，原生 HTML/CSS/JS
│   ├── app.js             #   渲染、粘贴解析器、AI 识别交互
│   ├── legal.js           #   应用内法律声明与风险确认
│   ├── index.html / styles.css / mock.js
├── src-tauri/src/         # Rust 后端（Tauri 2）
│   ├── keys.rs            #   站点→Key 台账、保管箱、测活、查重/批量删
│   ├── ai.rs              #   多模态识别（流式 + 进度 + 并发互斥）
│   ├── swap.rs / discover.rs / procs.rs   # 多开换号 / Agent 发现 / 进程
│   ├── quota.rs / local_quota.rs / webquota.rs  # 三条额度通道
│   ├── proxy.rs / gateway.rs   # 本地反代：池子 + 分发 Key
│   ├── secrets.rs         #   DPAPI + 主密码 保管箱
│   └── storage.rs / space.rs   # 数据搬家 / 磁盘治理
├── tests/                 # 前端与 Python 侧用例
├── tools/                 # 构建与发布工具（含发布隐私门）
├── docs/                  # 公开文档
└── legacy-python/         # 第一代 Python 原型（存档）
```

## 隐私与安全

- 账号管理数据主要保存在本机，保管箱通过 Windows DPAPI 保护；配置、账号档案、备份和剪贴板中的副本仍需保密。
- 项目没有自建遥测服务。登录、查询额度、签到、模型识别、图片代读、备用路由和公共目录刷新会连接相应服务。
- 模型网关默认绑定 `127.0.0.1`；单独开启的手机远控可通过局域网或所配置的中继访问，具有自己的配对边界。

详细见 [docs/PRIVACY.md](docs/PRIVACY.md)。

## 参考与致谢

感谢 **[Cockpit Tools](https://github.com/jlcodes99/cockpit-tools)** 在 Agent 平台、账号实例管理、登录流程和会话用量方面的参考，以及 **[Magpie](https://github.com/yetone/magpie)** 在本地网关接入、模型能力、调度计价和路由可视化方面的参考。也感谢 CC Switch、Our Free Model、models.dev 与底层依赖的维护者。

参考范围、固定版本、版权与分发边界见 [第三方声明](THIRD_PARTY_NOTICES.md)。其中 Cockpit Tools 采用 CC BY-NC-SA 4.0，Magpie 采用 MIT；署名不等于获得额外授权，也不表示官方合作。

## License

[MIT](LICENSE) · © 2026 shen1950

上述 MIT 仅覆盖本项目有权提供的自有内容。内置 CPython、Rust 依赖和另行安装的 zcode-api 等组件分别遵守各自许可；可选组件并不因本项目采用 MIT 而获得商用授权，见 [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md)。
