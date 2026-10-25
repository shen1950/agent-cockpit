# AgentCockpit 项目协作规则（所有 Agent 对话自动遵守）

本项目的编译/测试/部署是多个对话共享的资源，一切经由 agsync 协议协调，状态在 `.agent-sync/`。本文件在每次会话开始时自动加载，不需要任何人提醒。

## 铁律

1. 会话开始定身份：`export AGSYNC_OWNER=<本对话固定名>`（如 qcn-win3），全程不许换。
2. 动工改代码前：`agsync task new --title "..." --files <要改的文件>`，再 `agsync conflicts --task-id <任务号>`。有重叠就换文件或等对方 done。
3. 改完：`agsync task ready`。没标 ready 的改动不会被编进去。
4. 编译/测试只能走 `agsync build`，禁止直接 cargo build / cargo test。它会自动拿锁、宽限等待、把全部就绪任务一次编完。
5. 部署只能走 `agsync deploy`。部署前逐条确认：没有多个驾驶舱实例在跑；你当前会话的模型不挂在 8788 上（换 exe 会杀掉 8788）。
6. 编译失败先读自己任务的 error 归因；报错与你的文件无关就别动别人的文件。

## 参考

- 协议全文与红旗表：`~/.agents/skills/agent-sync/SKILL.md`
- agsync 不在 PATH 时：`python ~/.agents/skills/agent-sync/bin/agsync.py`
- 项目长期交接文档：`HANDOFF.md`
