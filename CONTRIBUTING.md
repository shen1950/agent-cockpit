# 贡献指南

感谢你愿意为 AgentCockpit 提改进。本项目是个人维护的桌面工具，**合入节奏偏保守**——它直接操作本机账号档案与凭据，任何改动都可能让使用者的账号或额度出问题。宁可慢一点，不要冒进。

## 开工前

1. **先开 Issue 说清楚要做什么。** 特别是涉及新增 Agent 适配、改反代行为、动数据目录结构这类改动。没讨论过就提上来的大 PR 大概率会被要求返工。
2. **小改动可以直接提。** 修文案、修明显 bug、补文档、补测试，不用先报备。
3. **看一眼 `.agent-sync/`。** 项目里有其他对话正在改代码时，先跑 `agsync board`，别撞车。

## 开发环境

| 依赖 | 说明 |
|---|---|
| Rust（MSVC 工具链） | 后端编译，edition 2021 |
| VS Build Tools | 需要「C++ 生成工具」工作负载 |
| Node.js | 前端无构建步骤，只用于 `npm test` |
| WebView2 Runtime | Win11 自带，Win10 需要单独装 |

```bash
npm install
npx tauri dev
```

前端是**原生 HTML/CSS/JS，没有框架、没有打包器**。`ui/index.html` 直接用浏览器打开也能跑——没有 `window.__TAURI_INTERNALS__` 时会自动挂 `ui/mock.js` 的假数据，方便改界面时不用起后端。

## 铁律：编译与测试只走 agsync

项目根目录有 `.agent-sync/`，多对话并行开发时编译/部署是共享资源：

```bash
export AGSYNC_OWNER=<你这个对话的固定名>     # 定下就别改
agsync task new --title "..." --files <要改的文件>
agsync conflicts --task-id <任务号>          # 查和别人的文件是否重叠
# ...改代码...
agsync task ready                            # 标就绪，等批次编译
agsync build                                 # 编译 + 测试（禁止直接 cargo build / cargo test）
agsync deploy                                # 部署（禁止自己拷 exe）
```

`agsync` 不在 PATH 时用 `python ~/.agents/skills/agent-sync/bin/agsync.py`。不登记的改动不会被编进批次，等于白干。

## 提交规范

- 一条提交只做一件事，信息用中文，说清「改了什么 + 为什么」。
- 涉及界面文案的改动，遵守下面的文案规矩。
- 改了行为就要补测试：
  - 前端解析器逻辑 → `tests/`
  - Rust 逻辑 → 模块内 `#[cfg(test)]` 或 `src-tauri/tests/`
- 不要提交：本机数据、日志、`.bak-*` 回滚副本、exe、截图草稿。

## 界面文案规矩

这个项目对界面文字有明确约束，改界面前请先看：

- **一句只服务当前这一步。** 不写设计动机，不写版本历史，不做功能对比段落。
- **不摆灰色说明段落。** 说明一律收进 `qhelp` 问号图标里（`DESC_INLINE_MAX = 0`），弹窗说明控制在两句话以内。
- **卡片动作区**用 `.acts`，主按钮在 `.acts-main`，删除按钮钉在行尾。

## 滚动护栏（改任何列表 / 面板前必看）

整块 `clear()` 推倒重建会让 `scrollHeight` 瞬间塌掉，`scrollTop` 被夹小、滚动锚定节点被销毁，表现就是「点个按钮整页往上跳」。本项目已备好三个工具（`ui/app.js` 顶部）：

| 工具 | 用途 |
|---|---|
| `restoreScroll(box, top)` | 立刻还位，**下一帧再核一次**（布局要等任务结束才算完） |
| `keepScroll(box, fn)` | 同步重建外壳，`fn` 内**不许有 await** |
| `scrollerIn(view)` | 挑真正在滚的容器，别猜 |

两点最容易踩：

- **滚动容器别猜。** 带 `.setlayout` 的页（密钥 / 反代 / 设置）`.pagebody` 被 `overflow-y: hidden` 锁着，真正在滚的是右栏 `.setbody`。
- **只改一小块就定点替换**（`replaceWith` 单节点 + `data-*` 标记），别整表重建。

`.pagebody` / `.setbody` 已加 `overflow-anchor: none`——浏览器的锚定补偿会和显式恢复打架，是「有时跳有时不跳」的来源。

改完记得 `node --check ui/app.js`（整段替换数组容易留下重复的 `];`）。

## 新增一个 Agent 适配

1. 在 `src-tauri/src/registry.rs` 登记：显示名、`bundle`/进程特征、官方默认安装位置、配置与凭据路径。
2. 判定多开能力：优先看是不是 Electron 系（支持 `--user-data-dir`），CLI 系看家目录环境变量；拿不准就交给内核实测，别硬猜。
3. 凭据路径支持 `|` 分隔的多个候选，`resolve_profile()` 按「① 已是 junction 的 ② 有登录数据的 ③ 第一个」三级优先——少了①会漂回错的候选。
4. **边界：驾驶舱绝不修改任何 Agent 的配置文件。** API Key 只做台账登记，可进反代池；账号档案只做隔离与切换。
5. 补一个 `tests/` 用例，用虚构的令牌与路径，别用真账号。

## 发布隐私门

每次发布前必须过一遍：

```bash
python tools/privacy_check.py                    # 源码扫描：只报告文件/行号/类别，不打印原文
python tests/privacy-check.test.py
python tools/private_build.py build --release
python tools/privacy_check.py --binary <exe>     # 二进制扫描（UTF-8 / UTF-16）
python tools/export_public_source.py <仓库外的目标.zip>
```

已知的个人关键词存在本机被忽略的 `.agent-sync/private-terms.json`，不进公开包。

**任何扫描器都无法保证不存在全部个人信息。** 正式公开前还要人工检查截图、视频、提交作者信息、发布附件与 Issue 里的日志。旧提交里的隐私残留改当前文件删不掉——对外分发请用上面的无历史快照，或新建干净仓库，不能把原仓库连带 `.git` 打包上传。

## 反馈漏洞

不要开公开 Issue 报安全漏洞，见 [SECURITY.md](SECURITY.md)。
