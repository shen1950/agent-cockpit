# Agent 驾驶舱 · AgentCockpit

**国内 AI Agent 多开 / 换号 / 额度过期提醒 / API Key 台账 / 本地反代，一台机器全管起来。**

![平台](https://img.shields.io/badge/platform-Windows%2010%2F11-blue) ![框架](https://img.shields.io/badge/framework-Tauri%202-orange) ![语言](https://img.shields.io/badge/rust-2021-red) ![许可](https://img.shields.io/badge/license-MIT-green)

Tauri 2 桌面应用（Rust 后端 + WebView2 前端），提供单文件便携版。用量采集运行时随程序内置，无需单独安装 Python；首次使用时会在驾驶舱数据目录解压运行时。程序不依赖 Electron，关窗后会驻留系统托盘继续监控额度。

**直接下载**：[Releases](https://github.com/shen1950/agent-cockpit/releases) 里有免安装便携版和 NSIS 安装包，下载即用，不必自己编译。

---

## ⚠️ 先读这段：法律与风险提示

**本软件按「原样」（AS IS）提供，不提供任何形式的担保。作者不对因使用或无法使用本软件所导致的任何后果承担责任——包括但不限于账号封禁或限制、额度或订阅损失、数据丢失、商业利润损失、合规风险与法律纠纷。是否使用、如何使用，由使用者自行判断并承担全部风险。**

请务必了解以下三点：

1. **合规由你自己负责。** 各 AI 产品的用户协议可能禁止多账号、账号共享、自动化访问或对客户端接口的逆向调用。本软件只提供管理能力，不授予你绕过任何服务条款的权利。使用前请自行阅读并遵守你所使用产品的条款，以及你所在司法辖区的法律。
2. **本地反代风险最高。** 反代会把你自己的账号凭据与 API Key 暴露给本机客户端与上游服务，并可能产生集中化的请求日志；一旦端口或分发 Key 泄露，第三方即可盗用你的额度。上游随时可能变更接口，导致请求失败、额度异常消耗或被判定为异常流量。**启用反代、生成分发 Key、把凭据交给任何客户端之前，请先通读 [docs/PROXY.md](docs/PROXY.md)。**
3. **数据仅供参考，不作决策依据。** 额度余额、过期时间、用量统计来自本机缓存或你自行配置的接口，可能存在延迟或识别误差。一切以官方页面为准。

完整条款见 [docs/LEGAL.md](docs/LEGAL.md)。商标与产品名称归各自所有者所有，本项目与任何被管理的 AI 产品厂商均无隶属、授权或合作关系。

> 详细使用手册（含每一步操作说明）见 [docs/USER_MANUAL.md](docs/USER_MANUAL.md)。

---

## 为什么做这个

国内的 AI 编程工具（Qoder、WorkBuddy、TRAE、CodeBuddy……）一个比一个能白嫖，
白嫖的代价是：一个工具三四个账号、每家积分规则不一样、过期时间藏在活动页里、
中转站的 Key 散落在各种配置文件中。这个驾驶舱把散装体验收进一张桌面：

- **多开 / 换号**：Electron 系用 `--user-data-dir` 隔离实例，CLI 系用 `CODEX_HOME` /
  `CLAUDE_CONFIG_DIR` 隔离账号；只能单开的程序用 NTFS junction 把默认入口指向当前账号，
  零额外空间、对话记录跟着账号走。
- **额度过期提醒**：赠送积分的到期时间各家都没有接口——能本机直读的（解客户端自己存的积分）
  一键直读；只能看官网的，截图贴进去让多模态模型读成逐笔额度，人工核对后落盘。
  30 天内临期的聚成顶栏徽章，7 天内启动时弹窗提醒。
- **API Key 台账**：站点 → Key 两级管理。一段乱排版的中转站信息（或截图）直接贴进
  「快速登记」，本机规则解析失败时交给多模态模型兜底，站点、接入点、每条 key 与用途名当场建好。
- **本地反代**：把多个账号 / Key 归进池子，生成 `ac-…` 分发 Key 给客户端用，
  请求日志能看到每一笔走了哪个上游。

## 功能一览

| 模块 | 能干什么 |
|---|---|
| 首页 | 落地页不是"第一个 Agent"：今天要处理什么一眼看完（临期积分 / 今天还能领 / Agent 概览 / 最近领过），两个主操作 **一键刷新全部额度**、**一键签到** |
| Agent 管理 | 自动扫描本机装了哪些 AI Agent，新增/卸载有标记；六步向导建账号（登录 → 数据来源 → 额度） |
| 多开/换号 | 可多开的建独立目录同屏跑；仅可换号的 junction 切换，随时切回 |
| 额度监控 | 本机直读（Qoder / WorkBuddy 系）+ 截图 AI 识别 + 适配器抓取（OpenAI / new-api / one-api 格式） |
| 每日签到 | 产品 → 账号两级：**每个账号实例各看各的签到状态**，不用先切号才看得到另一个；每行一颗 ↻ 只重查这一家；「登录并领取」一条龙（拉起客户端 → 等你登进去 → 自动领掉）；已领完 ≠ 不用领；没立刻到账的积分自动盯着；**领取日志**保留一年、可按今天/7 天/30 天/全部筛并导出 CSV；打卡对话走限免模型（step-5-preview → hy3），不花积分 |
| API 台账 | 站点 → 多 Key，各带用途标签、额度、过期日、测活；**站点内批量管理**（勾选 / 全选 / 改用途 / 改过期日 / 测活 / 复制 / 移到其他站点 / 删除）；**查重一键全选** / 一键接入到 Qoder 等程序 |
| 快速登记 | 粘贴任意排版文本或截图 → 本机规则优先解析（离线、零 token）→ 多模态模型兜底 → 核对后落盘 |
| AI 识别 | 文本/截图两条通道分开配模型；识别全程有进度反馈，同时只跑一次，重复请求直接挡掉 |
| 本地反代 | 池子（多上游轮询）+ 分发 Key，`127.0.0.1:8788/v1` OpenAI 兼容 |
| 凭据安全 | 所有 Key / Token 进 DPAPI + 可选主密码的加密保管箱，界面只显示打码串 |
| 数据治理 | 数据根搬家 / 回滚、可再生大文件去重、本机 Agent 盘点 |

## 支持的 Agent

内置登记表覆盖下面这些（参照 [jlcodes99/cockpit-tools](https://github.com/jlcodes99/cockpit-tools)
的平台清单补齐）。登记了官方默认安装位置，换台机器装上就自动认出来；
**这台机器上没装、也没有账号卡的不占侧栏格子**，要看全量去 设置 → 高级 → 「显示未安装的程序」。

| 类别 | Agent |
|---|---|
| 国内 | Qoder CN、Qoder CN IDE、WorkBuddy（国内版 / 国际版）、Trae CN、TRAE SOLO CN、千问办公、MiniMax Code、AStudio、ZCode、DeepSeek Harness、Marvis、Xiaomi MiMo、CodeBuddy / CodeBuddy CN |
| 国际 | Qoder、Trae、TRAE SOLO、Cursor、Kiro、Windsurf、Antigravity、GitHub Copilot（VS Code）、Zed、Grok Bot |
| CLI | Codex CLI、Claude Code CLI、OpenCode |

各家上游的协议差异、令牌位置与已知限制见 [docs/SUPPORTED_AGENTS.md](docs/SUPPORTED_AGENTS.md)。

> 认不出来的是绿色版 / 没建快捷方式的：侧栏下方「＋ 添加程序」登记一个可执行文件路径即可，
> 不改动那个程序的任何文件；能不能多开由内核实测判定。

## 快速开始

```bash
git clone https://github.com/shen1950/agent-cockpit.git
cd agent-cockpit
npm install
npx tauri dev        # 开发模式
npx tauri build      # 产出 exe + NSIS 安装包
```

构建依赖：**Rust（MSVC 工具链）** + **VS Build Tools（C++ 工作负载）** + WebView2 Runtime（Win11 自带）。
纯前端调试可以直接用浏览器打开 `ui/index.html`（自动挂 mock 数据，不碰真机）。

本机开发时编译与测试请走 `agsync build`，不要直接 `cargo build`——见 [docs/DEVELOPMENT.md](docs/DEVELOPMENT.md)。

## 自测

```bash
npm test                                  # 粘贴解析器离线用例（不依赖 DOM / Tauri）
cd src-tauri && cargo test                # Rust 单测 + 反代端到端
cd src-tauri && cargo test -- --include-ignored   # 真机用例（真扫本机 / 真连反代）
```

## AI 识别怎么配

到 **设置 → AI 识别** 填一个 OpenAI 兼容接口的 Key（默认阿里云百炼，任意兼容网关都行）：

- **文本模型**：解析粘贴的 key 清单用，小快模型即可（默认 `qwen-flash`）；
- **截图模型**：读积分页截图用，必须是视觉模型（默认 `qwen-vl-flash`）。

识别请求全程流式 + 思考模型自动关思考；页右下角有常驻进度（连接中 / 模型输出中 · 已收 N 字），
同一时间只放行一次识别，连点不会再跑出几份重复结果；同一条 key 重复登记会在落盘时自动合并。

**截图会发送给你自己配置的模型网关。** 介意就只用「快速登记」的本机规则解析通道（离线、零 token）。

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

- 所有 API Key、Agent Token 落盘前收进加密保管箱（Windows DPAPI，可再加一层主密码）；
  文档与界面里只剩打码串，原文只在发请求时由 Rust 侧拼进请求头。
- 没有遥测、没有账号体系、不连任何作者服务器；额度请求只发往你自己填的官方/中转站地址。
- 反代与仪表盘只绑定 `127.0.0.1`，不对外网开放。
- AI 截图识别会把截图发给你自己配置的模型网关，介意就只用本机规则解析（离线可用）。

详细见 [docs/PRIVACY.md](docs/PRIVACY.md)。

## License

[MIT](LICENSE) · © 2026 shen1950

第三方依赖与参考项目的许可声明见 [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md)。
