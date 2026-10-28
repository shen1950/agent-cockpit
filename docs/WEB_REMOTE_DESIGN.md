# AgentCockpit 跨平台与 Web 远控设计方案

> 2026-10-26 · zcode-web-plan · 对应三问：①桌面跨 Win/Mac/Linux ②移动端 App ③浏览器/扫码 Web 远控。
> 结论先行：**③ Web 远控是当前性价比最高、且被现有架构天然支持的路线，作为主推；②移动端用 PWA/Capacitor 壳包 Web 版实现，不做独立原生 App；①桌面跨平台可行但要重写 5 个 Windows 深依赖模块，作为独立轨道分期推进。**

---

## 0. 现状盘点（设计的地基）

| 事实 | 对三个问题的影响 |
|---|---|
| 前后端桥只有 **2 个 Tauri 命令**：`get_state()` + `action(payload)`，后端 commands.rs 分发约 **130 个 action** | 任何新传输层（HTTP/WS）只需桥接这 2 个入口，不用动 130 个 action 的实现 |
| 前端 `ui/` 是**无框架原生 HTML/CSS/JS**（app.js 约 9200 行），`invoke` 本来就是可替换的变量 | 浏览器里跑同一套 UI 只差一个 HTTP 桥；mock.js 已验证"浏览器打开 index.html 就能跑" |
| `tiny_http` 已是 Cargo 依赖（8788 反代在用） | 内嵌 Web 服务器**零新增依赖** |
| Windows 深依赖：20/30 个 Rust 模块有 `cfg(windows)`——DPAPI 保管箱（secrets.rs）、NTFS junction 换号（swap.rs）、WebView2 cookie 抓官网额度（webquota.rs）、.lnk 快捷方式（shelllink.rs）、进程/安装位置发现（procs.rs / discover.rs / registry.rs 2215 行登记表） | 跨平台 = 这 5 类模块抽平台层逐平台重写；不是"编译开关打开就行" |
| 产物单 exe、关窗驻留托盘、`bundle.targets = ["nsis"]` | Tauri 2 本身跨平台（macOS WKWebView / Linux WebKitGTK），壳不是障碍，障碍在上面的平台模块 |
| 隐私承诺：无账号体系、不连作者服务器、Key 全程 DPAPI 加密 | Web 远控的安全模型必须保持"不出本机、不传明文 Key、显式开启" |

---

## 1. 问题①：桌面能不能跨 Win / Mac / Linux？

**结论：能，但按"功能子集"分期能跨，全功能对齐是大工程。**

### 1.1 依赖分层

| 层 | 模块 | 现状 | 跨平台路径 | 难度 |
|---|---|---|---|---|
| 壳/前端 | tauri.conf、ui/ | WebView2 | Tauri 2 原生支持三平台，前端不动 | ★ |
| 纯逻辑 | keys.rs 台账、quota.rs 适配器、ai.rs 识别、gateway/proxy 反代、storage、settings | 路径 + HTTP，无 Windows API | 直接编译通过或小改 | ★ |
| 凭据 | secrets.rs（DPAPI） | Win 专属 | 抽 `SecretsStore` trait：Windows 用 DPAPI、macOS 用 Keychain（`security-framework` 或 keyring crate）、Linux 用 Secret Service / 主密码+argon2 文件兜底 | ★★ |
| 换号 | swap.rs（NTFS junction） | Win 专属 | macOS/Linux 用 symlink（`std::os::unix::fs::symlink`）；Electron 系 `--user-data-dir` 多开本来就跨平台 | ★★ |
| 官网额度 | webquota.rs（开 WebView2 窗口取 cookie） | Win 专属 | Tauri 在 macOS/Linux 的内嵌 WebView 各有 cookie jar（WKWebView / WebKitGTK），要按平台各写一遍取会话逻辑 | ★★★ |
| 发现/注册表 | registry.rs、discover.rs、procs.rs、shelllink.rs、icons.rs、inventory.rs | Win 路径、.lnk、注册表 | 每平台补一份"安装位置登记表"（macOS `/Applications/*.app`、Linux `~/.local/share/applications/*.desktop`）；.lnk → .desktop 解析 | ★★★（体力活为主） |
| 本地直读额度 | local_quota.rs | Windows 客户端数据路径 | Qoder/WorkBuddy 在 mac/Linux 的数据路径逐家补（大多在 `~/` 下，SQLite 直读逻辑可复用） | ★★ |
| 多开/注入类 | inject.rs、cliauth.rs、各 *proxy.rs | 部分 Win API | CLI 系（CODEX_HOME/CLAUDE_CONFIG_DIR）跨平台天然成立；逐个核对 | ★★ |

### 1.2 推荐策略：功能分级发布，不做"一步全对齐"

- **Tier 1（先跨平台）**：额度监控（官网适配器 + AI 截图识别）、API Key 台账、快速登记、每日签到、本地反代、首页看板。这些正是"远程 Web 版"要复用的同一批能力。
- **Tier 2**：Agent 发现 + Electron 系多开 + CLI 系换号（per-OS 路径表）。
- **Tier 3**：junction 换号、官网 WebView 登录抓 cookie、系统托盘细节（每平台行为差异大）。

### 1.3 工作量粗估

5 个平台模块抽 trait + 三平台实现 + registry 登记表补齐 + 三平台 CI（GitHub Actions macos/ubuntu runner 现成）≈ **4~6 人周**。可先出 macOS 版（Linux 桌面用户少），Tier 1 子集约 **2 人周**。

### 1.4 与 Web 远控的协同

本方案的 M1 会把"动作分发 + 状态快照"整理成与 Tauri IPC 无关的纯接口（见 §3.3），这一步**同时是跨平台的铺路**：三平台壳（Tauri desktop / HTTP remote）共用同一 Rust service 层。

---

## 2. 问题②：要不要做移动端 App（安卓 / 苹果）？

**结论：不做独立原生 App；移动端需求由 Web 版 + PWA 覆盖，重交互壳需求再用 Capacitor 打包。**

理由：

1. **手机上没有驾驶舱的操作对象**。多开、junction、本地数据库直读、进程管理全都针对"本机文件/进程"，手机端做不了也没有意义。移动端真实场景是：**看额度/临期提醒、触发刷新/签到、看反代请求日志、扫一眼 Key 台账**——全是"读 + 轻控制"，正是 Web 版的子集。
2. **原生 App 双端成本高**（Flutter/RN 或双原生，外加推送、签名、上架、审核），换来的离线能力和系统能力本项目几乎用不上；且"自己连自己电脑"的 App 还要处理内网穿透，体验不如浏览器 + 组网。
3. **PWA 路线**：Web 版加 `manifest.json` + Service Worker 后，安卓/iOS 都能"添加到主屏幕"全屏运行，一套代码双端。iOS 对 PWA 支持已够用（推送需 iOS 16.4+ 且用户先安装）。
4. **确需 App 形态时**：用 Capacitor 把同一套 Web 前端打包成 apk/ipa，加原生推送（APNs/FCM）。这是 M3 之后的选择题，不是现在的必答题。

推送通知（托盘盯到临期 → 手机收到）在 M2 走 **Bark / 飞书机器人 / 自定义 webhook**，不依赖 App 形态。

---

## 3. 问题③：浏览器 / 扫码进入的 Web 远控（主推，详细设计）

### 3.1 总体架构

```
┌───────────────────────────── 驾驶舱进程（同一个 exe）────────────────────────────┐
│                                                                                  │
│  Tauri 桌面窗口                Rust service 层（现有 commands.rs 分发逻辑）        │
│  ┌──────────┐  invoke           ┌───────────────────────────────────────────┐    │
│  │ ui/ 桌面UI │ ──────────────▶ │  get_state() / action(payload)  130 actions │    │
│  └──────────┘                   └───────────────▲───────────────────────────┘    │
│                                                 │ 同一入口，白名单过滤             │
│  本地反代 127.0.0.1:8788（现有）                  │                                    │
│                                                 │                                    │
│  ★ 新增 remote.rs ── 0.0.0.0:8790（默认关，显式开）                                  │
│  ├─ GET  /                 → ui/mobile.html（移动端精简页）                         │
│  ├─ GET  /api/pair         → 一次性配对码校验 → 发设备 token                        │
│  ├─ GET  /api/state        → get_state() 快照（脱敏）                              │
│  ├─ POST /api/action       → action()（只放行白名单）                               │
│  ├─ GET  /api/events       → SSE：刷新进度/签到进度/额度变化推送                     │
│  └─ 静态资源 /assets/*     → ui/ 目录                                             │
└──────────────────────────────────────────────────────────────────────────────────┘
        ▲                                   ▲
   局域网：手机扫驾驶舱里的二维码          外网：Tailscale/ZeroTier 虚拟网卡
   （QR = http://192.168.x.x:8790/#T=配对码） （v1 不做公网直暴，见 §3.5）
```

要点：

- **不引入第二个进程/后端**。remote.rs 与 8788 反代一样跑在驾驶舱进程内，状态、保管箱、额度缓存全部复用，零同步问题。
- **手机扫的是驾驶舱窗口里显示的二维码**，内容 = `http(s)://<局域网IP>:8790/#T=<一次性配对码>`；token 走 URL fragment，不进服务器日志与 Referer。
- 桌面 UI 与 Web UI **同一份底层数据接口**，桌面端不受任何影响。

### 3.2 安全模型（把"能换号删 Key 的驾驶舱"放到网络上，安全是第一设计约束）

1. **默认关闭**。设置页新增「远程控制」开关，开启时：
   - 生成/展示二维码 + 明确提示"同一 Wi-Fi 内可被访问"；
   - 绑定地址默认 `0.0.0.0`（局域网可用），提供"仅 Tailscale 网卡 IP"选项；
   - 可设"无人访问 N 小时自动关闭"。
2. **一次性配对 + 设备 token**：
   - 配对码 8 位、10 分钟过期、扫码后即作废；设备换回一个 32 字节随机 token（存 DPAPI 保管箱，界面可见"已配对设备"列表，可吊销）。
   - 之后所有 `/api/*` 请求带 `Authorization: Bearer <token>`；token 不落 localStorage 明文（存 localStorage 但值加密于配对时下发的 key……v1 简化：直接 localStorage + 界面可吊销，风险声明写入文档）。
3. **三级权限 + 动作白名单**（130 个 action 不是全放行）：

   | 级别 | 允许 | 示例 action |
   |---|---|---|
   | viewer | 只读状态 | `get_state`、额度/台账/日志查看 |
   | operator | + 安全操作 | `refresh_all`、`checkin_view/refresh/claim`、key `test_alive`、`mark_seen` |
   | admin | 默认**不下发** | 换号 `auth_switch`、删除、数据搬家、反代启停 |

   v1 固定 viewer+operator 合并下发；换号等高危 action 直接不在 web 分发表里出现（白名单硬编码于 remote.rs，不做成配置以防误开）。
4. **脱敏铁律**：API Key/Token 明文**永不**通过 web API 返回（复用现有打码逻辑，web 侧只收 `sk-***abc` + 额度/过期日）；`workdir` 等本机路径按需遮蔽。
5. **传输**：v1 局域网 HTTP + token（同 Wi-Fi 威胁模型可接受，文档明示）；HTTPS 自签放 v1.1（自签证书在手机浏览器要手动信任，体验差，先不做）。外网一律建议 Tailscale（其链路本身加密）。
6. **审计与限速**：remote.rs 记录 `(时间, IP, token 设备名, action, 结果)` 到 logs/，界面可查；每 token 每 10 秒最多 N 次 action，防脚本误刷触发风控。

### 3.3 服务端实现要点（remote.rs，零新依赖）

- 复用 `tiny_http`（8788 反代同款）。SSE 用 chunked 流即可，**不引 WS 库**——状态推送频率低（刷新进度/签到进度），SSE 足够且实现量小一个量级。
- 核心是把现有 Tauri 命令入口重构为**传输无关的 service 函数**：

  ```rust
  // 现状：main.rs 的 tauri::command 直接调 commands.rs
  // 目标：commands.rs 的 (get_state, action) 提为 fn service_state(&State) -> Json / fn service_action(&State, payload) -> Result
  //      tauri command 与 remote.rs 都只做薄薄的参数/鉴权包装
  ```
  这一步同时是跨平台（§1.4）和未来其他传输（CLI、RPC）的地基。
- 白名单 action 在 remote.rs 内静态表：`const WEB_ACTIONS: &[&str] = &["refresh_all", "checkin_view", ...];`
- 静态文件服务仅映射 `ui/` 白名单子目录（mobile.html / styles.css / 图标），**不暴露** ui/ 全目录（桌面 app.js 不适合手机，也减少攻击面）。
- 端口选 **8790**（8788 已被反代占用；配置可改）。

### 3.4 移动端 UI（ui/mobile.html，精简页而不是硬塞桌面页）

桌面 app.js 9200 行、为 1320×860 优化，硬做响应式性价比低。**单独一个 mobile.html**（预计 600~900 行），复用 styles.css 的设计变量（配色/卡片/徽章），四个底部 Tab：

1. **看板**：临期额度徽章列、今日待领签到、Agent 在跑数——首页的移动版。
2. **额度**：按 Agent 分组的额度卡（余额/到期/来源标签），下拉刷新 = `refresh_all`（带进度条）。
3. **签到**：产品→账号两级、单行 ↻ 重查、「登录并领取」状态展示（手机上不拉起客户端，只看结果与触发重试）。
4. **更多**：已配对设备、访问日志、开关远程控制（本机操作时）。

桥接层 10 行：检测非 Tauri 环境 → `invoke = (cmd, args) => fetch('/api/...')`，与 mock.js 同一思路。

### 3.5 外网访问（v1 明确不做公网直暴）

- 推荐 **Tailscale/ZeroTier**：手机装客户端 → 出门在外也能访问家里机器的 8790，链路加密且不暴露公网端口。设置页文案直接引导。
- 明确不支持：端口映射/公网 IP 直连、任何"云中转"。这是隐私承诺（不连任何作者服务器）的自然延伸，也避免 8790 变成公网上的"换号器"。

### 3.6 推送（M2）

额度临期/签到到账事件 → remote.rs 的 webhook 出口：Bark（iOS 一条 URL）、飞书机器人、自定义 POST。与托盘通知同一事件源。

---

## 4. 实施分期与验收

| 里程碑 | 内容 | 验收标准 | 估量 |
|---|---|---|---|
| **M1 Web 远控 MVP** | service 层重构（传输无关化）+ remote.rs（静态/REST/SSE/白名单/配对码）+ mobile.html 四 Tab + 设置页「远程控制」开关与二维码 | 手机扫码 → 看到全部 Agent 额度；点「一键刷新」桌面端同步刷新；断网重连状态自动恢复；未配对访问一律 401 | Rust ~800 行 + 前端 ~800 行，1.5~2 人周 |
| **M2 通知与日志** | webhook 推送（Bark/飞书）、访问审计页、速率限制、已配对设备管理/吊销 | 临期 ≤3 天事件推到手机；吊销 token 立即失效 | ~400 行，3~4 天 |
| **M3 PWA 与打包** | manifest + Service Worker（可安装到主屏）、可选 Capacitor 壳 | 安卓/iOS 主屏图标全屏打开；离线打开显示最近一次快照 | ~300 行，3 天 |
| **跨平台轨道（并行）** | 平台 trait 抽取（SecretsStore/Swapper/WebQuota）→ macOS Tier 1 → Linux | macOS 上 Tier 1 功能可用；Windows 行为零回归 | 4~6 人周，独立推进 |

风险与对策：

- **8790 安全面**：白名单 + 一次性配对 + 默认关闭 + 审计，四层叠加；上线前用另一台设备做未授权访问/重放测试。
- **service 层重构动 main.rs/commands.rs**：这是核心代码，需按 agsync 协议与其他对话错峰；重构分两步（先提函数不改行为，再加 remote 包装），每步 `agsync build` 过单测。
- **SSE 经 Nginx 等反代被缓冲**：文档标注 `X-Accel-Buffering: no`；v1 目标场景（局域网/Tailscale 直连）无此问题。

## 5. 三问最终回答（一页版）

1. **能跨平台吗？** 能——Tauri 2 壳和前端天然跨，但 DPAPI/junction/WebView2/.lnk/安装登记表五类 Windows 深依赖要抽平台层重写。按功能分级（监控+台账+反代先行）约 4~6 人周做到三平台，Tier 1 先出 macOS 约 2 人周。
2. **能做移动端吗？** 能，但**不做独立原生 App**：手机端场景是"看额度 + 轻控制"，Web 版 + PWA（必要时 Capacitor 打包）一套代码覆盖安卓/iOS，原生双端开发纯属浪费。
3. **Web 远控可行吗？** **最可行且立即可做**：前后端本就只有 2 个命令的窄接口、tiny_http 已在依赖里、前端浏览器可跑已被 mock 验证。新增 remote.rs（HTTP+SSE+配对二维码+动作白名单）+ ui/mobile.html，约 1.5~2 人周出 MVP，且重构出的 service 层同时是跨平台的铺路石。**推荐路线：M1 Web 远控 → M2 通知 → M3 PWA，跨平台作为并行轨道。**
