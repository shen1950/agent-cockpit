# Trae 反代设计（2026-10-19）

> 依据：`C:\Users\LENOVO\.qoder-cn\projects\D--Claude---------\memory\reference-trae-reverse-proxy.md`
> 本文只写设计决策，协议底账都在那份记忆里。

## 1. 目标

把 Trae CN 的模型与积分变成驾驶舱里的一个上游，满足四件事：

1. **稳定**：真模型可用，不是只能打工具模型；错误不误判导致池子被白锁。
2. **可持续**：不碰 refreshToken，令牌到期前有告警和恢复动作。
3. **可观测**：额度、套餐到期、身份、出口 IP、每笔请求的模型/耗时/积分变化/错误类别都能看到。
4. **一键**：驾驶舱里点一下完成「探测 → 勾账号进池 → 起反代 → 下发 key」，并把 key 分给其他 Agent。

## 2. 非目标

- 不做 Trae 客户端的换号/多开（那是 authpack 的事，且 Trae 那条已明确不做）。
- 不改 Trae 任何产品文件。只读 `storage.json`、`state.vscdb`。
- 不做协议之外的伪造：不编造设备指纹、不伪装 OS。`X-User-Real-IP` 填的是本机真实出口 IP。

## 3. 已核实的事实（实现必须与之对齐）

| 事实 | 后果 |
|---|---|
| 对话要 `X-User-Real-IP` = 本机出口 IP，假值/回环/空 → 4011 | 每次请求前必须拿到当前出口 IP；网络变化要能自愈 |
| 4011 的真实含义是「客户端身份未通过」，不是限流 | 归传输层，重算 IP 后重试一次；绝不进冷却、绝不换号 |
| 4001 = 通道×模型配对不存在 | 归请求内容层，换号无用 |
| 线上目录对 `solo_*` 恒空，对话端点却认 `state.vscdb` 的模型名 | 目录主源用本地缓存，必须标新鲜度 |
| SSE 第一个事件是 `metadata` 不是 `output` | 「已产出内容」的判据不能拿 metadata 当信号 |
| `refreshToken` 一次性，用一次踢掉 IDE | 只读 access token，不续期 |
| `~/.trae-cn/trae-jwt-token` 是另一份令牌，对话端点回 401 | 只认 storage.json 那份 |

## 4. 架构

沿用现有三层，不新增进程：

```
调用方(其他 Agent) --ac-key--> proxy.rs (127.0.0.1) --pool--> gateway.rs
                                   |
                                   +-- base_url == "trae://agent" -->
                                        traeproxy.rs: OpenAI -> SOLO 翻译
                                                     -> POST trae-api-cn.mchost.guru
                                                     -> SOLO SSE -> OpenAI SSE
```

新增一个模块 `traeproxy.rs`，对外接口与 `qproxy.rs` 对齐：

```rust
pub const APPS: [&str; 1] = ["traework-cn"];   // 驾驶舱程序 id
pub fn accounts() -> Vec<Acct>;                 // 各实例档案 + 令牌状态
pub fn token_for(app_id: &str, name: &str) -> Option<String>;
pub fn egress_ip() -> Option<String>;           // 运行时出口 IP，带短缓存
pub fn models_for(app_id: &str, name: &str) -> Vec<Model>;  // 从 state.vscdb 读
pub fn translate_request(openai: &Value, ch: &str) -> Value; // 纯函数
pub fn parse_sse_line(event: &str, data: &str, st: &mut SseState) -> Vec<String>; // 纯函数
pub fn classify(code: i64, msg: &str) -> Kind;  // 纯函数
pub fn probe(app: &str, name: &str, model: &str) -> Value;   // 手动单发
pub fn view() -> Value;
pub fn one_click(proxy: &Arc<Proxy>) -> Value;
```

`gateway.rs` / `proxy.rs` 的改动限定在「识别 `trae://` 这个 base_url 并走翻译分支」，不动机制。

## 5. 组件

### 5.1 凭据 `traeproxy::cred`

- 读 `<profile>/User/globalStorage/storage.json`，取 `iCubeAuthInfo://icube.cloudide`，byteCrypto 解出 `token/expiredAt/userId/host/account`。
- 按文件 mtime 缓存；mtime 变就重读。
- 返回结构带 `expires_at`、`days_left`、`source`（哪个档案）。
- **不读也不写 `refreshToken`**（解析出来也直接丢弃，不进任何序列化输出）。

### 5.2 出口 IP `traeproxy::egress_ip`

- 优先 UDP-connect 到 `8.8.8.8:80` 读 `getsockname()[0]`（不发包），失败回落 `gethostbyname(gethostname())`。
- 缓存 60 秒；每次请求前取一次缓存值。
- 4011 时强制失效缓存、重算、重试一次。
- 值进 `view()`，界面显示「当前出口 IP」。

### 5.3 模型目录 `traeproxy::models`

- 主源：`<profile>/User/globalStorage/state.vscdb` 的 ItemTable，键 `<uid>:AI.agent.model.model_list_map`。
- 解析出每通道的 `config_name/display_name/context_window_size.max/max_tokens/max_mode/reasoning_effort_config`。
- 带 `source_mtime`，界面显示「目录截至 …」。**不许显示成实时**。
- 备源（仅用于发现新模型，不覆盖本地目录）：线上 `batch_get_detail_param`，但它对 `solo_*` 恒空，只有工具模型，所以只用于 `fast_apply` 这类。
- 模型→通道的映射：优先按目录里出现的通道；同名模型跨通道时按固定优先级 `solo_agent > solo_work_lite > solo_agent_remote > ...`，且**允许调用方用 `channel/model` 语法显式指定**，避免猜错。

### 5.4 协议翻译 `translate_request` / `parse_sse_line`

请求方向：

| OpenAI | SOLO |
|---|---|
| `messages[].content` 字符串 | `[{type:"text",text:...}]` |
| `tools[].function.parameters` 对象 | JSON **字符串**（必须在此步序列化） |
| `assistant.tool_calls[].function` | `function_call` |
| `tool_choice` 对象 | 字符串（`auto`/`required`/函数名）；`none` 同时删 tools |
| `model` | `model` + `config_name` 双写 |
| — | `function`（通道）、`stream:true`、`app_version_code` |

响应方向（SOLO → OpenAI）：

| SOLO 事件 | OpenAI |
|---|---|
| `metadata` | 忽略（**不当作已产出内容**） |
| `timing_cost` | 记入该笔日志的 `provider_model_name` 与耗时分段 |
| `output.response` | `delta.content` |
| `output.reasoning_content` | `delta.reasoning_content` |
| `output.tool_calls` | `delta.tool_calls`（`function_call`→`function`，删 `namespace`/`partial_arguments`） |
| `token_usage` | `usage` |
| `done.finish_reason` | `finish_reason` + `[DONE]` |
| `error.code` | 映射后的错误，附 `Kind` |

首字节前允许换号重发；**一旦收到 `output` 就绝不重放**（`metadata`/`timing_cost` 不算已产出）。

### 5.5 错误三维分类 `classify`

纯函数，输入 `(code, message)`，输出 `Kind`：

| Kind | 触发 | 动作 |
|---|---|---|
| `Identity` | 4011 / 401 / 1001 | 失效 IP 缓存→重算→**重试一次**；仍失败则报「客户端身份未通过」，不进冷却、不换号 |
| `BadRequest` | 4001 / 400 | 请求内容错；不重试、不换号、不冷却 |
| `Quota` | 4008 | 账号级长冷却 |
| `Plan` | 1005 | 账号级冷却 |
| `ModelDead` | 404 | 该模型短冷却 |
| `Server` | 5xx | 换号一次 |
| `None` | 其他 | 报错 |

**硬约束**：任何 `BadRequest` 都不得写入任何冷却或换号账本。这是为了防止重演「内容级错误当账号故障，整池休息 5 天」。

### 5.6 观测

每笔请求一行：时间、调用方 key 尾号、模型、通道、上游 `provider_model_name`、prompt/completion token、积分变化（请求前后各读一次余额，异步且去抖，不能每笔都打）、首字节耗时、总耗时、`Kind`、是否重试过、出口 IP。

`view()` 汇总：账号、身份（Free/Pro）、权限、积分各组余额、最近到期笔、目录新鲜度、出口 IP、令牌到期倒计时、4011 计数、每模型成功/失败比。

### 5.7 池子与下发 key

完全复用 `gateway.rs`：

- Trae 账号作为池成员，`base_url = "trae://agent"`，`token` 字段存该实例的 access token（仅内存，落盘由 secrets 加密）。
- `ac-` 下发 key 限定可用池子，与现有 `proxy.rs` 的鉴权一致。
- 不认识的 key 一律 401（沿用现有行为；若现有实现是「退回全部池子」，本次**不动它**，另案处理，避免扩大改动面）。

### 5.8 一键 `one_click`

1. 枚举 Trae 实例档案 → 逐个解令牌，报状态。
2. 读模型目录，报新鲜度。
3. 取出口 IP。
4. 用户选一个模型做**一次**探测（默认 `glm-5.3-flash`，可取消）。成功才继续。
5. 起反代（若未起）。
6. 建/复用池子，把账号加进去。
7. 生成 `ac-` key，写进台账，返回可复制的 `base_url` + key。

界面上每一步单独显示结果，失败停在原地并给出原因，不吞。

## 6. 关键不变量

1. 只读产品文件。任何 `fs::write` 不得落在 Trae 目录。
2. 不序列化 `refreshToken`；日志、`view()`、key 台账里都不许出现。
3. `X-User-Real-IP` 只能是本机真实出口 IP，不许配置成任意值。
4. 目录新鲜度必须显示；超过 7 天标黄。
5. `BadRequest` 类错误不触发任何冷却/换号。
6. 令牌到期 ≤ 3 天时驾驶舱告警，并给「打开 Trae 客户端」按钮（复用现有 `launch_app`）。

## 7. 测试

Rust 单测（纯函数，全部离线）：

- `translate_request`：字符串 content、工具参数对象→字符串、`tool_calls`↔`function_call`、`tool_choice` 四种形态、`none` 连带删 tools、`model`/`config_name` 双写。
- `parse_sse_line`：六种事件各一条；`metadata` 不得置「已产出」；`timing_cost` 不产出 chunk；`error` 映射到 `Kind`。
- `classify`：七个分支；**断言 `BadRequest` 返回的 `Kind` 不满足「需要冷却」**。
- `models`：用一份固定的 `model_list_map` 样本解析出通道与模型数。
- `egress_ip`：只测「取不到时返回 None」的分支，不 mock 网络。

集成探针（手动、单发）：

- `probe()` 一次，断言 HTTP 200 且首个非 metadata 事件能解析出 `delta`。
- 4011 时断言重算 IP 后只重试一次。

## 8. 风险与回滚

| 风险 | 缓解 |
|---|---|
| 服务端改校验方式（如 IP 头被废弃） | 4011 计数进观测面；`probe` 一键可复测；协议常量集中在一个模块，便于改 |
| 令牌到期后反代静默失效 | 到期倒计时 + ≤3 天告警 + 「打开 Trae」按钮 |
| 目录缓存过期导致列出已下架模型 | 显示新鲜度；调用失败标 `ModelDead`，不冷却账号 |
| 账号被风控/封禁 | 这是使用非官方客户端的固有风险，界面明示；只服务本机、不转卖 |
| 改动波及现有 Qoder/其他池子 | Trae 分支只在 `base_url == "trae://agent"` 时进入；其余路径零改动 |

回滚：删 `traeproxy.rs` 与 `mod` 声明、撤回 `gateway/proxy/state/commands` 里那几处 `trae://` 分支即可，无数据迁移。

## 9. 分阶段交付

1. **P1 纯函数**：`classify`、`translate_request`、`parse_sse_line` + 单测。不含网络。
2. **P2 只读观测**：`cred` / `models` / `egress_ip` / `view()` 接进 `state.rs`，界面只显示不给按钮。
3. **P3 单发探针**：`probe()` + 界面一颗按钮，验证真模型可取。
4. **P4 接池子**：`gateway/proxy` 的 `trae://` 分支 + 下发 key。
5. **P5 一键**：`one_click` 串起 P2–P4。

每阶段 `cargo test` 必须保持现有基线（100 过、2 条环境性失败）。

---

## 10. 落地记录（2026-10-19 实现时与设计的三处偏差）

1. **凭据解密与模型目录改为经 agenthub，不在 Rust 里写加密。**
   理由：仓库里没有 AES/SHA-512 依赖（`qproxy` 只有手写的 AES-256 **加密**），
   而 agenthub 的 `_trae_decrypt_blob` / `_trae_read_login` 已经实测可用，
   `state.vscdb` 的 SQLite 读取本来也是 agenthub 的职责（`local_quota.rs` 同一分工）。
   新增两个只读入口：`agenthub trae --token` / `agenthub trae --models`，**都不输出 refreshToken**。
   代价：缓存失效时起一次 Python（1–2 秒）；令牌按 storage.json 的 mtime 缓存，目录按 60 秒缓存。

2. **`view()` 拆成「纯缓存」和「显式刷新」两条。**
   界面默认 4 秒一轮轮询，`view()` 一旦起 python 就会每分钟卡一下。
   现在 `view()` 只读缓存，`refresh()` 才是会起 agenthub 的那个（接 `trae_refresh` 动作）。

3. **补了非流式路径。**
   设计时只写了 SSE 翻译，但很多 Agent 会发 `stream:false`。
   `collect_completion()` 把翻译后的 OpenAI SSE 收成一条 `chat.completion`；
   错误类别映射成 HTTP 状态（401/400/402/403/404/502），不再一律 200。

### 实际落地的接口

- Rust：`traeproxy.rs`（凭据/出口 IP/目录/翻译/SSE/分类/探针/一键/view）、
  `proxy.rs::serve_trae`（`trae://agent` 分支 + 非流式合并）、
  `gateway.rs`（成员解析与 base 选择）、`state.rs`（`traeproxy` 进快照）、
  `commands.rs`（`trae_status` / `trae_refresh` / `trae_probe` / `trae_oneclick` / `trae_reveal`）。
- Python：`agenthub.py` 的 `trae` 子命令（`cmd_trae` / `_trae_models_map` / `_trae_profile`）。
- UI：`ui/index.html` 的 `#traeproxy` 容器 + `ui/app.js` 的 `renderTrae` / `traeOneClick` / `traeKey`。

### 已实测

- `live_probe`：`glm-5.3-flash` 200，`provider=glm-5.3-flash`，usage 17/69/86，1675ms。
- `real_oneclick_serves_a_completion`：一键建池发 `ac-` key → 非流式拿到标准 `chat.completion`（content「收到」）→ 流式拿到 OpenAI chunk 且以 `[DONE]` 收尾。
- 全量 `cargo test`：132 过 0 挂（含 12 条 traeproxy 单测）。

### 已知未做

- 请求级积分增量没有逐笔读（每次读余额要打一次服务端），目前只在额度页按需刷新。
- 计数落盘是每次 `bump` 写一次小 JSON；量大时应改成批量 flush。
- Trae 的登录态整机一份，多账号池化要等 cockpit 能真正隔离 Trae 的 userData。
