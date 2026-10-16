# Agent 驾驶舱 · AgentCockpit

**国内 AI Agent 多开 / 换号 / 额度过期提醒 / API Key 台账 / 本地反代，一台机器全管起来。**

Tauri 2 桌面应用（Rust 后端 + WebView2 前端），产物是单个原生 exe（约 9 MB）——
无 Python、无 Electron、无内置浏览器，关窗驻留系统托盘继续盯额度。

> 详细使用手册（含每一步操作说明）见 [docs/USER_MANUAL.md](docs/USER_MANUAL.md)。

**直接下载**：[Releases](https://github.com/shen1950/agent-cockpit/releases) 里有免安装便携版和 NSIS 安装包，下载即用，不必自己编译。

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
| 每日签到 | 产品 → 账号两级：**每个账号实例各看各的签到状态**，不用先切号才看得到另一个；已领完 ≠ 不用领；没立刻到账的积分自动盯着，到账了在提示区说一声 |
| API 台账 | 站点 → 多 Key，各带用途标签、额度、过期日、测活；**查重 / 批量删 / 一键接入**到 Qoder 等程序 |
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

## 项目结构

```
├── ui/                    # 前端：无框架，原生 HTML/CSS/JS
│   ├── app.js             #   渲染、粘贴解析器、AI 识别交互
│   ├── index.html / styles.css / mock.js
├── src-tauri/src/         # Rust 后端（Tauri 2）
│   ├── keys.rs            #   站点→Key 台账、保管箱、测活、查重/批量删
│   ├── ai.rs              #   多模态识别（流式 + 进度 + 并发互斥）
│   ├── swap.rs / discover.rs / procs.rs   # 多开换号 / Agent 发现 / 进程
│   ├── quota.rs / local_quota.rs / webquota.rs  # 三条额度通道
│   ├── proxy.rs / gateway.rs   # 本地反代：池子 + 分发 Key
│   ├── secrets.rs         #   DPAPI + 主密码 保管箱
│   └── storage.rs / space.rs   # 数据搬家 / 磁盘治理
├── tests/paste-test.js    # 解析器离线用例（node tests/paste-test.js）
├── docs/USER_MANUAL.md    # 完整使用手册
└── legacy-python/         # 第一代 Python 原型（存档）
```

## 隐私与安全

- 所有 API Key、Agent Token 落盘前收进加密保管箱（Windows DPAPI，可再加一层主密码）；
  文档与界面里只剩打码串，原文只在发请求时由 Rust 侧拼进请求头。
- 没有遥测、没有账号体系、不连任何作者服务器；额度请求只发往你自己填的官方/中转站地址。
- AI 截图识别会把截图发给你自己配置的模型网关，介意就只用本机规则解析（离线可用）。

## 免责声明

本项目只操作**你自己机器上、你自己的**程序档案与账号数据，用于个人管理多账号与额度。
请遵守各产品的用户协议；多开与换号功能在部分产品上可能被视为违规使用，后果自负。
积分过期时间与余额以官方页面为准，本程序的识别结果仅供参考。

## License

[MIT](LICENSE)
