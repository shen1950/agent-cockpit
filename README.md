# Agent 驾驶舱

国内 AI Agent 多开 / 赠送额度过期提醒 / 本地反代 一体机。单文件、唯一依赖 pywebview（Python 3.10+），启动后打开原生桌面窗口（WebView2 壳，不打开浏览器）。

## 快速开始

```bat
双击 启动驾驶舱.bat
```

或命令行：

```bash
pythonw agent_cockpit.py
```

## 功能

- **多开**：每个实例一个独立数据目录，互不干扰，首次启动各自登录账号
  - Electron 系（Qoder / Qoder CN / Qoder CN IDE / WorkBuddy / TraeWork CN / ZCode）：`--user-data-dir` 隔离
  - CLI 系（Codex / Claude Code）：`CODEX_HOME` / `CLAUDE_CONFIG_DIR` 环境变量隔离，新终端窗口运行
- **换号模式（零空间）**：把 Agent 默认档案目录改成指向实例的 NTFS junction，官方入口直接以该账号运行；原档案自动备份为 `*.cockpit-original`，「还原默认」一键恢复；目录被官方后台进程锁定时会点名提示并可确认强切
- **空间治理**：每实例「占用」实时统计 MB；「清缓存」删除 Cache/logs/扩展缓存等（登录态与设置保留）
- **额度台账与过期提醒**：每个实例可登记总额度、剩余、过期日、API Token
  - 剩余/总额以进度条展示；过期倒计时徽章（≤7 天红、≤30 天黄）
  - 顶部横幅汇总所有临期赠送额度，标题栏同步显示待办数
  - 可选 `quota_url` + `quota_path`：自动从返回 JSON 的接口抓取剩余值
  - 内置适配器：`openai` = 官方 `/dashboard/billing/credit_grants`（需 API Token，自动回填总额）
  - 国产 IDE 无公开用量接口（token 存于 DPAPI 加密存储），逆向成本过高未内置，走通用接口或手动登记
- **本地反代**（不常用时保持停止）：`127.0.0.1:8788` 起一个 OpenAI 兼容转发
  - token 池 = 所有登记了 API Token 的实例，轮询替换 `Authorization` 头
  - 请求日志（时间/方法/路径/模型/状态/所用账号）在面板实时展示
- **进程监控**：每 4 秒刷新运行中的 Agent 进程
  - 实例卡片「■ 结束」= 实例级：按命令行 `--user-data-dir=<实例目录>` 精确匹配，只杀该实例窗口
  - 进程表「结束全部」= 程序级：杀该程序所有窗口（含正在使用的），带二次确认
- **账号备份/恢复**：实例数据目录 zip 备份（自动跳过 Cache 等垃圾目录）

## 数据位置

| 内容 | 路径 |
|---|---|
| 实例档案与数据目录 | `~/.agent-cockpit/instances/<agent>/<实例名>/` |
| 账号备份 zip | `~/.agent-cockpit/backups/` |
| 反代配置 | `~/.agent-cockpit/config.json` |

## 额度自动抓取示例

在实例设置里填：

- 额度接口：`https://api.openai.com/dashboard/billing/credit_grants`
- API Token：`sk-...`
- 取值路径：留空自动猜常见字段，或填点路径如 `total_available`

## 设计

- **程序架构**（对标 Cockpit Tools / Tauri 的壳原理）：原生桌面壳 + Web 技术 UI + 本地后端
  - Cockpit Tools = Tauri（WebView2 壳 + Rust 后端）；本项目 = pywebview（WebView2 壳 + Python 后端）
  - 独立桌面窗口（自有图标、最小尺寸约束），关窗即退出，不打开浏览器
  - 后端为 stdlib `http.server` JSON API（`127.0.0.1:8787` 仅本机回环），前端内嵌于单文件
- **视觉模板**：石墨底 `#0C0E14` + 琥珀金 `#C8956C` + 衬线标题（Noto Serif SC 回退 Georgia），8pt 间距网格、12px 圆角、无渐变
- 单文件 `agent_cockpit.py` + 唯一依赖 `pywebview`（见 requirements.txt）

## 安装依赖

```bash
pip install -r requirements.txt
```

## 隐私

本仓库为私有仓库。实例数据、Token、备份均只存本机 `~/.agent-cockpit/`，不随代码上传。
