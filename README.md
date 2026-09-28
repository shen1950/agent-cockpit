# Agent 驾驶舱

国内 AI Agent 多开 / 赠送额度过期提醒 / 本地反代 一体机。单文件、零依赖（Python 3.10+ 标准库），启动后自动打开浏览器仪表盘 `http://127.0.0.1:8787`。

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
- **额度台账与过期提醒**：每个实例可登记总额度、剩余、过期日、API Token
  - 剩余/总额以进度条展示；过期倒计时徽章（≤7 天红、≤30 天黄）
  - 顶部横幅汇总所有临期赠送额度，标题栏同步显示待办数
  - 可选 `quota_url` + `quota_path`：自动从返回 JSON 的接口抓取剩余值
- **本地反代**（不常用时保持停止）：`127.0.0.1:8788` 起一个 OpenAI 兼容转发
  - token 池 = 所有登记了 API Token 的实例，轮询替换 `Authorization` 头
  - 请求日志（时间/方法/路径/模型/状态/所用账号）在面板实时展示
- **进程监控**：每 4 秒刷新运行中的 Agent 进程，可一键结束
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

- 视觉模板：石墨底 `#0C0E14` + 琥珀金 `#C8956C` + 衬线标题（Noto Serif SC 回退 Georgia），8pt 间距网格、12px 圆角、无渐变
- 单文件 `agent_cockpit.py`：stdlib `http.server` 提供 JSON API + 内嵌前端，无任何第三方依赖

## 隐私

本仓库为私有仓库。实例数据、Token、备份均只存本机 `~/.agent-cockpit/`，不随代码上传。
