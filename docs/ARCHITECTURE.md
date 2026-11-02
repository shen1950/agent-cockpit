# 架构说明

## 总体形态

AgentCockpit 是一个 **Tauri 2** 桌面应用：Rust 后端负责所有本机操作，前端是一层原生 WebView2 界面，**没有前端框架、没有打包器**。

```
┌────────────────────────────────────────────┐
│  前端 ui/（原生 HTML/CSS/JS）              │  只负责渲染与交互
│    app.js / settings-ui.js / proxy-guide.js│  所有数据来自 invoke('get_state')
├────────────────────────────────────────────┤  Tauri IPC
│  命令层 commands.rs                        │  get_state / action / get_meta …
├────────────────────────────────────────────┤
│  领域模块（Rust）                          │  进程 / 文件 / 网络 / 加密
│    registry  swap   procs   discover        │
│    quota     usage  keys    secrets         │
│    proxy     gateway ingress                │
└────────────────────────────────────────────┘
```

关键点：**所有本机副作用都发生在 Rust 侧**。前端拿到的永远是已经聚合、脱敏过的状态快照。

## 命令面

前端只通过少数几个 Tauri 命令与后端通信（`src-tauri/src/commands.rs`）：

| 命令 | 作用 |
|---|---|
| `get_state` | 拿整份仪表盘状态（异步；进程快照、档案统计、窗口枚举都在这里做） |
| `action` | 执行一个动作，用 payload 里的 `action` 字段分发 |
| `get_meta` | 取按名字索引的元信息（各面板的下拉选项等） |
| `get_settings` / `set_settings` | 设置的读写 |
| `set_autostart` | 开机自启（写系统插件 + 记进设置，保证两处一致） |
| `get_key_plain` | **按需**取一条 Key 的原文（复制/编辑时用），列表接口一律打码 |
| `scan_inventory` | 一键盘点本机 Agent（三路查安装痕迹 + 与上次快照比对） |
| `pick_folder` | 选目录对话框 |
| `remove_app` | 移除手动添加的条目（自动发现的走「隐藏此程序」） |
| `usage_dashboard` | 用量看板快照（可 `force` 强制重采） |

设置写 `config.json`，和 `action` 抢同一份账本，所以两边一起排队（`cmd_guard`），且都用 `async` 以免卡住窗口。

`get_state` 特意做成异步：每轮要做进程快照（冷启 300ms+）、逐账号数档案文件（实测 250–300ms）、`EnumWindows`。以前是同步命令、全跑在主线程，前端每 4 秒 invoke 一次，滚动/缩放正好撞上就被卡住几百毫秒。

## 模块划分

### 应用装配

| 模块 | 职责 |
|---|---|
| `main.rs` | 插件注册、系统托盘、后台额度循环、`--rehome` 命令行入口 |
| `state.rs` | 仪表盘状态聚合（前端 `get_state` 的那份大 JSON） |
| `settings.rs` | 设置读写 |
| `lifecycle.rs` | 启动 / 关闭 / 托盘行为 |
| `machine.rs` | 本机环境信息 |

### 账号与进程

| 模块 | 职责 |
|---|---|
| `registry.rs` | Agent 注册表（手工适配 + 自动发现合并）、路径、实例与配置的 JSON 存储 |
| `discover.rs` | 扫描开始菜单/桌面快捷方式，识别新装 AI 程序与 Electron 隔离能力 |
| `procs.rs` | 进程快照（`sysinfo`）、启动/结束、实例级命令行匹配 |
| `swap.rs` | 换号：把默认档案目录做成指向实例目录的 NTFS junction（零额外空间） |
| `identity.rs` | 从实例目录读出「当前登的是谁」 |
| `inventory.rs` | 本机 Agent 盘点：三路查安装痕迹 + 与上次快照比对 |
| `shelllink.rs` / `shell_identity.rs` | 快捷方式解析与外壳侧身份 |

### 额度与用量

| 模块 | 职责 |
|---|---|
| `quota.rs` | 过期倒计时 + 额度抓取调度 |
| `local_quota.rs` | 通道一：本机直读客户端自己存的余额（解密交给 agenthub），含账号绑定校验防串号 |
| `webquota.rs` | 通道二：开一个 WebView2 窗口登录官网，从 cookie 罐取会话再抓 |
| `ai.rs` | 通道三：多模态识别（文本 / 截图两条通道分开配模型，流式 + 进度 + 并发互斥） |
| `usage.rs` | 本地会话日志用量聚合（Codex `token_count` 等），只读 |
| `usage_dashboard.rs` | 用量看板快照：热力图 / 柱状 / 环形 / 趋势 |

### 台账与凭据

| 模块 | 职责 |
|---|---|
| `keys.rs` | 站点 → Key 台账、保管箱、测活、查重 / 批量删 |
| `secrets.rs` | 加密保管箱：Windows DPAPI（CurrentUser）+ 可选主密码（PBKDF2-SHA512 派生 entropy） |
| `authpack.rs` | 凭据打包 / 导出 |
| `cliauth.rs` | CLI 系产品的登录态 |
| `inject.rs` | 把 Key 注入客户端（只在用户明确要求时） |

### 反代

| 模块 | 职责 |
|---|---|
| `gateway.rs` | 编制层：上游组与分发 Key 的存储、成员解析、路由上游集合 |
| `ingress.rs` | 进站层：Responses / Anthropic / OpenAI 三种进站协议统一翻译成 OpenAI Chat 报文，回环打到自己的 `/v1/chat/completions` |
| `proxy.rs` | 服务端：`tiny_http` 监听 + `ureq` 流式转发，按请求带的 key 决定能走哪些上游组，保留 SSE |
| `qproxy.rs` / `wbproxy.rs` / `traeproxy.rs` / `mmproxy.rs` / `asproxy.rs` / `qwbridge.rs` / `pawbridge.rs` / `opencode.rs` | 各家上游适配器与本地桥 |
| `chatgpt*.rs` / `antigravity.rs` / `zcode.rs` / `codexcatalog.rs` | 特定产品的登录、历史、额度与模型目录 |

回环能安全工作的前提是 **proxy 每个请求一个 worker 线程**，不会把 accept 循环堵死。

### 数据与存储

| 模块 | 职责 |
|---|---|
| `storage.rs` | 数据根搬家 / 回滚 |
| `space.rs` | 占用统计、清缓存、zip 备份恢复、多开去重 |
| `remote.rs` / `remote_tunnel.rs` | Web 远程控制（手机扫码）与跨网络中继 |
| `brand.rs` / `icons.rs` | 品牌与图标资源 |

## 三条额度通道

各家都不公开积分接口，所以按「能不能拿到」分三路，互不替代：

```
通道一 本机直读   ── 客户端自己存了余额 → 解本地加密库 → 直读（最快，无网络）
通道二 网页抓取   ── 只能看官网 → 开 WebView2 登录 → 取 cookie 会话 → 抓
通道三 AI 识别    ── 只剩截图 → 多模态模型读成逐笔额度 → 人工核对后落盘
```

**左/总数读不到时一律报 `None`，绝不用 0 冒充余额。** 这是本项目反复强调的规矩：0 会被当成「额度用完了」，而「读不到」和「用完了」是两件事。同理，缓存数据必须标注来源时间，过期缓存不产生临期提醒。

## 换号的三种形态

| 形态 | 适用 | 做法 | 代价 |
|---|---|---|---|
| **多开** | 支持 `--user-data-dir` 的 Electron 系 | 每个账号一个独立目录，同屏跑 | 各占一份磁盘 |
| **家目录隔离** | CLI 系（Codex / Claude Code） | 启动时注入 `CODEX_HOME` / `CLAUDE_CONFIG_DIR` | 无 |
| **换号（junction）** | 只能单开的程序 | 把默认入口做成指向当前账号目录的 NTFS junction | 零额外空间，随时切回 |

三者是**实测判定**的，不是猜的：先探测进程特征与启动参数，再决定用哪种。判定结果会缓存，设置里可以清掉重测。

## 存储布局

```
~/.agent-cockpit/
├── config.json              设置、反代配置、换号记录
├── instances/<agent>/<实例名>/   每个账号一份完整档案
├── backups/                 账号备份 zip（含凭据，按密钥保管）
└── ...                      运行时、日志、用量采样
```

Windows 下 `~` 即 `%USERPROFILE%`。搬家时整份搬，旧位置留回滚点。

## 设计边界（改代码前必须知道）

1. **驾驶舱绝不修改任何 Agent 的配置文件。** API Key 只做台账登记、可进反代池；各 Agent 的登录态留在它们自己的加密库里，我们只读不写。
2. **只绑 `127.0.0.1`。** 反代、用量看板、Web 远程都不对外网开放。
3. **凭据不出界面。** 列表接口只给打码串，原文只在复制/编辑/发请求时由 Rust 侧取出拼进请求头。
4. **失败要显式报错，不要返回空成功。** 尤其反代：上游返回空、解密失败、令牌过期，都要如实抛出，别让前端显示「成功但没内容」。

## 前端约定

### 滚动护栏

整块 `clear()` 推倒重建会让 `scrollHeight` 瞬间塌掉，`scrollTop` 被夹小、滚动锚定节点被销毁，表现就是「点个按钮整页往上跳」。`ui/app.js` 顶部有三个工具：

| 工具 | 用途 |
|---|---|
| `restoreScroll(box, top)` | 立刻还位，**下一帧再核一次**——布局要等任务结束才算完 |
| `keepScroll(box, fn)` | 同步重建外壳，`fn` 内**不许有 await** |
| `scrollerIn(view)` | 挑真正在滚的容器，别猜 |

**滚动容器别猜**：带 `.setlayout` 的页（密钥 / 反代 / 设置）`.pagebody` 被 `overflow-y: hidden` 锁着，真正在滚的是右栏 `.setbody`。踩过这个坑的 `rerenderProxy` 早先拿 `.pagebody`，等于保持了个 0。

`.pagebody` / `.setbody` 已加 `overflow-anchor: none`：浏览器的锚定补偿会和显式恢复打架，是「有时跳有时不跳」的来源。

只改一小块就**定点替换**（`replaceWith` 单节点 + `data-*` 标记），别整表重建。注意 `el()` 的 `data:` 必须传对象形式，字符串键读不到。

### 文案规矩

- 一句只服务当前这一步；不写设计动机、版本历史。
- 不摆灰色说明段落，说明一律收进 `qhelp` 问号图标（`DESC_INLINE_MAX = 0`），弹窗说明 ≤2 句。
- 卡片动作区 `.acts` = `.acts-main` + 删除钉行尾；`.cards` 用 `align-items: start`。

改完跑 `node --check ui/app.js`——整段替换数组容易留下重复的 `];`。
