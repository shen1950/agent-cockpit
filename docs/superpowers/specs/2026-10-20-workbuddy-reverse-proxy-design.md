# WorkBuddy 国内版 / 国际版免费模型反代 · 设计文档

日期：2026-10-20 · 状态：已批准方案，待写实现计划
适用范围：`AgentCockpit`（Tauri 2 + Rust，公开 MIT 仓库 github.com/shen1950/agent-cockpit）

> **本文档不含 `atRestSecretKey` 的明文值。** 该值只存在于本机 `~/.agenthub/agenthub.py`
> 与驾驶舱的 DPAPI 保管箱里，绝不进源码、不进 config.json、不进日志、不进任何提交物。

---

## 1. 目标与非目标

### 1.1 目标

把本机已登录的 WorkBuddy 国内版（`workbuddy`）与国际版（`workbuddy-intl`）账号，变成
一个本机 OpenAI 兼容端点 `http://127.0.0.1:<port>/v1`，在驾驶舱界面上一键开启，
产出一把可分发的 Key 交给其他 Agent 使用。四条硬要求（用户原话）：

| 要求 | 本设计的对应 |
|---|---|
| 稳定 | 三维拒判 + 账号×模型冷却落盘 + 单请求换号上限 + 令牌过期只重读文件不碰 refreshToken |
| 可持续 | 只用 0 倍率模型 + `prompt_cache_key`（实测省 17 倍）+ 账号级护栏机制默认启用（四条里「保留积分下限」给保守值，另三条默认 0＝不限，见 §7.4） |
| 各方面状态可观测 | 逐请求 JSONL 落盘（含厂商第一方 `credit`）+ 冷却表 + 界面观测面板 |
| 集百家之长 | 每条机制都标了出处项目，见 §12 |

### 1.2 非目标（本期明确不做）

- `inject.rs` 扩到六家明文可写产品 —— 用户选择「先跑通现有三家（Qoder CN / Qoder / WorkBuddy）+ 其余给复制按钮手动粘」
- Trae 适配器 —— 协议已单独打通（见 `reference-trae-reverse-proxy`），另立一期
- Qoder 国内版 —— WASM 机器签名，`qproxy.rs` 已判定 `signature_required`，无解
- `/v2/chat/prediction` 预检积分 —— 多一次往返，收益不抵成本（YAGNI）
- 多账号池化运营 —— 用户两个区域各一个号，护栏是防「一夜烧穿」和防风控，不是防多号
- 把 `qproxy.rs` 重构成 trait —— 该文件另一会话今天仍在改，冲突风险高，见 §11

---

## 2. 已核实的事实底账

标注：**〔测〕** = 2026-10-20 本机真发请求看到响应；**〔源〕** = 读过安装包/源码原文；**〔推〕** = 推断。
探针脚本与全部原始输出留在 `D:\Claude\人工智能产品测试\_wb_proxy_probe\`。

### 2.1 端点〔测〕

| 区域 | 推理端点 | 实测 | 网关 |
|---|---|---|---|
| 国内版 | `POST https://www.workbuddy.cn/v2/chat/completions` | **200**，2567ms（全头族）/ 2186ms（最小头族） | APISIX/3.9.1 |
| 国际版 | `POST https://www.workbuddy.ai/v2/chat/completions` | **200**，1687ms（全头族） | APISIX/3.12.0 |

日志佐证：`~/.workbuddy/logs/2026-10-19/*.log` 里国内端点出现 1878 次，
`~/.workbuddy-ai/logs/` 里国际端点出现 7804 次。

国内版故障转移三 origin〔源，asar `CN_SAAS_CORE_API_*`〕：
primary `www.workbuddy.cn` / alternate `copilot.tencent.com` / backup `wb.tencentbuddy.com`。
**本期只用 primary**；故障转移是后续增量，不进第一版（避免把「换 origin」和「换账号」两套重试搅在一起）。

**关键结论：WorkBuddy 国内版没有 Qoder 国内版那种签名墙。** 两个 asar 里 `x-sign` 命中 0 次；
asar 内唯一的 HMAC-SHA256 是 `VlmContextEnvelope`，用进程内随机 32 字节 secret 签本地 sessionId，
不出网、无法从进程外伪造。所以两个区域走**同一条代码路径**，只差 origin 与归属常量。

### 2.2 凭据〔测〕

登录态文件（`%LOCALAPPDATA%\CodeBuddyExtension\Data\Public\auth\`）：

| 区域 | 文件 | uid | nickname | `auth.domain` | `auth.expiresAt` |
|---|---|---|---|---|---|
| 国内版 | `workbuddy-desktop.info` | `06708772-c41c-4ddc-9b94-02cf9cce9948` | DuckDeity | `www.codebuddy.cn` | 1794843281209（≈2026-11-16） |
| 国际版 | `workbuddy-desktop-ai.info` | `403fabba-2284-4b31-9d5b-2c099cb436cf` | shen1950 | `www.workbuddy.ai` | 1822899910698（≈2027-10） |

文件是 JSON，顶层四键 `account` / `auth` / `accounts` / `allAccounts`。
`auth.accessToken`、`auth.refreshToken`、`account.nickname`、`account.phoneNumber`
是 `$wbEncrypted` 信封；`account.uid`、`auth.domain`、`auth.expiresAt` 等是明文。
**全文扫描确认：不存在任何明文 bearer token / cookie / session token。**

解密链路（与 `~/.agenthub/agenthub.py` 的 `_wb_read_login` 逐字一致，该链路今天仍在生产使用）：

```
K   = SHA256( atRestSecretKey 的 utf8 字节 )            # 32 字节
env = base64_decode( field["envelope"] ) → JSON { suite, keyId, nonce, authTag, ciphertext }
AAD = "WB-AAD\0" + 0x01
    + u32be(len("WBEV1")) + "WBEV1"        # framing tag = field
    + u32be(len("sym-v1")) + "sym-v1"      # scheme
    + u32be(suite)
    + u32be(len(keyId)) + keyId
    + 0x02                                  # framing type = field
    + 0x00 0x00
明文 = AES-256-GCM-Open( K, base64_decode(nonce),
                         base64_decode(ciphertext) || base64_decode(authTag), AAD )
```

`atRestSecretKey` 是硬编码在 `WorkBuddy.exe` 里的常量（TEA-18 加密，RVA 0xA0F5798），
**对所有安装通用**，且国内版与国际版**共用同一把**。

两个账号的 accessToken 解出来都是 `eyJhb…` 开头的 JWT 形态（长度 1327 / 1398）。

### 2.3 请求头〔测〕

实测通过的**全头族（28 个）**，逐条复刻 asar 的 `buildVlmGatewayHeaders` + `applyGatewayHeaders` + `applyTraceHeaders`：

```
Authorization: Bearer <accessToken>
Accept: application/json
Content-Type: application/json
Accept-Encoding: identity
x-requested-with: XMLHttpRequest
x-codebuddy-request: 1
X-Conversation-ID: <uuid>
X-Conversation-Request-ID: <uuid>
X-Conversation-Message-ID: <uuid>
X-Request-ID: <uuid>
acp-connection-id: <uuid>
X-Agent-Intent: craft
X-Agent-Purpose: conversation
X-IDE-Type: WorkBuddy
X-IDE-Name: WorkBuddy
X-IDE-Version: <客户端版本>        # CN 5.8.3 / intl 5.8.4，本机安装实测
X-Private-Data: true
X-Product: SaaS
X-User-Id: <account.uid>
X-Domain: <auth.domain>            # 取登录态里的值，不是端点 host
User-Agent: WorkBuddy/<ver> WorkBuddy/<ver>
traceparent: 00-<32hex>-<16hex>-01
b3: <32hex>-<16hex>-1-<16hex>
X-B3-TraceId / X-B3-SpanId / X-B3-ParentSpanId / X-B3-Sampled: 1 / X-Trace-ID
```

**最小头族（11 个）实测结果分区域不同**：

| 区域 | 11 头最小族 | 28 头全族 |
|---|---|---|
| 国内版 | **200 通过** | 200 通过 |
| 国际版 | **400 `11128 "request illegal"`**，`displayTips` 明说「这次调用没通过渠道校验，可能不是官方客户端发出的」 | 200 通过 |

⇒ **设计决定：一律发全头族，不做最小化。** 理由不是为了通过，而是为了让流量与官方客户端
不可区分（这正是「可持续」的要求）；把头发少只会让流量更容易被识别。国际版的渠道校验
证明这条边界真实存在，不是理论担忧。

关于 `X-Domain`：OFM 的实现注释主张「必须与端点 host 一致」，而官方客户端 asar 取的是
`s.auth.domain`。本机国内版登录态里 `auth.domain = www.codebuddy.cn`，端点却是
`www.workbuddy.cn` —— **两者本来就不同域，而客户端每天 1878 次调用都成功**。
⇒ 采信客户端：`X-Domain` 取登录态的 `auth.domain` 原值，不做「纠正」。

关于 `X-Product`：OFM 主张填归属名（`WorkBuddy`）而非 `SaaS`，理由是「历史实现发成 SaaS
导致后台归因不到产品」。但客户端 asar 发的就是 `SaaS`，且本机实测 200。
⇒ 采信客户端填 `SaaS`。归因显示是她的 WorkBuddy 后台观感问题，不是连通性问题；
若她日后发现后台「使用端」列显示为 `-`，改这一个常量即可，不影响其他逻辑。

### 2.4 请求体〔测〕

实测通过的形状：

```json
{
  "model": "hy3",
  "messages": [
    {"role": "system", "content": "You are a helpful assistant."},
    {"role": "user",   "content": "Reply with exactly: OK"}
  ],
  "stream": true,
  "max_tokens": 16,
  "prompt_cache_key": "<uuid>"
}
```

即**标准 OpenAI Chat Completions 请求体**，无需协议翻译。

OFM〔源，`buddy-adapter.ts:1354-1410`〕另证两个字段有价值：
- `prompt_cache_key`：不带则 `prompt_cache_hit_tokens` 恒为 0。OFM 实测同一段 8k token 前缀，
  不带 = 0.34 积分，带 = 0.02 积分，**差约 17 倍**。
- `reasoning_effort`：是思考的**真开关**；单独发 `thinking:{type:"enabled"}` 无效（实测 reasoning_content 恒 0）。
  只在模型确实支持该档位时才发，否则服务端 400。

### 2.5 响应〔测〕

`Content-Type: text/event-stream`，标准 OpenAI SSE：

- 开头有 `: heartbeat` 注释行（SSE comment，必须容忍并透传）
- 只有 `data:` 行，**没有 `event:` 行**（与 Trae 的自定义 SSE 不同，这里是标准形状）
- chunk：`{"id","model","object":"chat.completion.chunk","created","choices":[{"index":0,
  "delta":{"role"?, "content", "reasoning_content", "function_call", "refusal", "tool_calls",
  "extra_fields"}, "logprobs":null, "finish_reason":""}], "usage":null}`
- 末 chunk 带 `usage`，含 `prompt_tokens` / `completion_tokens` /
  `prompt_tokens_details.{reasoning_tokens, cached_tokens, …}` /
  `prompt_cache_hit_tokens` / `prompt_cache_miss_tokens` / `cache_read_input_tokens` /
  `cache_creation_input_tokens` / `prompt_cache_write_tokens` / `completion_thinking_tokens` /
  **`credit`**
- 终止符 `data: [DONE]`

**`usage.credit` 是厂商第一方的逐请求成本**（本次实测全部为 `0`，因为用 0 倍率模型）。
⇒ 成本归因不需要轮询 billing 接口、不需要解析本地 sqlite、不需要自己算单价。
这是本设计可观测层的地基。

注意 `completion_tokens` **包含** `reasoning_tokens`。OFM 记录过一个真实事故：
思考陷入病态重复会把 128000 额度烧光而正文零产出，用户只看到「已达到输出 token 上限」。

### 2.6 0 倍率模型〔测，本机 `/v3/config` 缓存〕

缓存位置 `~/.workbuddy{,-ai}/cache/acc-product-config-v3.json`（国内版 411 KB / mtime 2026-10-19 21:33；
国际版 378 KB / mtime 2026-10-18）。国内版 73 个模型、国际版 30 个。

| 区域 | `credits == "x0.00"` 的模型 |
|---|---|
| 国内版 | `hy3`、`hy3-b`、`hy3-c`、`step-5-preview` |
| 国际版 | `deepseek-v4.1-flash`、`hy4-preview-f`、`hy3`、`step-5-preview` |

⚠️ **这份清单会过期。** 缓存里还有按月编号的促销徽章（`hy4-night-discount-daytime-badge-202609`、
`step-5-discount-daytime-badge-202610`），说明限免是运营行为。
⇒ 免费模型清单**必须运行时从缓存/接口读，不许写死在源码里**，且界面要标出数据新鲜度
（缓存 mtime），照 Trae 模型目录那条先例办。

### 2.7 错误码〔测〕+〔源〕

| code | `msg` | 维度 | 处置 | 证据 |
|---|---|---|---|---|
| `11128` | `first message is not system prompt` | **请求形状** | 补 system 首条后重发一次；**不换号、不冷却** | 〔测〕intl 全头族去掉 system → 400 |
| `11128` | `request illegal` | **请求形状（我们的头没配齐）** | 当自己的 bug 大声报出来；**不换号、不冷却** | 〔测〕intl 11 头最小族 → 400，`displayTips` 明说渠道校验 |
| `11140` | `request illegal` | **账号级** | 换号 + 该账号冷却；服务端不给解除时长，**不编造** | 〔源〕OFM `buddy-adapter.ts:406-444`，同一体在 7 账号上「2 通 4 拦 1 限流」 |
| `6020` / `6021` / `6022` | — | **模型容量** | WAITING / FULL / USER_LIMIT；不换号，可轮询 `/v2/chat/queue/status` | 〔源〕asar `Es={WAITING:6020,FULL:6021,USER_LIMIT:6022}` |
| HTTP `401` | — | **账号级（令牌过期）** | 重读 `.info` 文件（客户端自己会刷新）；仍 401 才标终态 | 〔源〕asar `VlmUpstream.forward` 就是 401→refresh→重发一次 |
| HTTP `429` | — | **账号级（限流）** | 账号×模型冷却，照 `Retry-After` 但下限 10s | 〔源〕CLIProxyAPI 分档 |
| HTTP `200` 但 SSE 内含 error | — | 视 code 而定 | **健康判据必须解析到事件层**，状态码永远 200 会骗人 | 〔源〕Trae 实测教训 |

**这张表最重要的一条：`11128` 和 `11140` 的 `msg` 都可能是 `"request illegal"`，
但维度相反 —— 11128 换号无用，11140 换号是唯一出路。所以判据必须是 `(code, msg)` 配对，
不能只看 code，也不能只看 msg。**

magpie 的 LESSONS.md 记过这个坑的真实代价：把 WorkBuddy 的 "unapproved channel" 当账号故障，
**全部 WorkBuddy AI 账号休息了 5 天**。OFM 也在注释里明文警告过同一件事。
两个独立项目分别踩到 ⇒ 这不是理论风险。

### 2.8 其他已发现端点〔源〕

| 用途 | 方法/路径 | 本期用不用 |
|---|---|---|
| 免费探活（不花积分） | `POST {origin}/billing/meter/get-user-resource-summary`，body `{}`，需 `Bearer` + `X-User-Id` + `User-Agent` | **用**（probe） |
| 模型目录 | `GET {origin}/v3/config` → `data.data.models[]` / `agents[]` | **用**（免费模型清单） |
| 容量排队 | `POST {origin}/v2/chat/queue/status` / `cancel`，body `{model}` | 不用（先记录，6020-6022 出现时再补） |
| 预检积分 | `POST {origin}/v2/chat/prediction` → `data.predicted_total_credit.{min,max}` | 不用（§1.2） |
| 令牌刷新 | `POST {origin}/v2/plugin/auth/token/refresh` | **绝不调用**（见 §5.3） |

asar 自己的网络操作策略表〔源〕对 `chat.stream` 的定性值得直接采纳：
`replayClass: 'forbidden'`、`maxEndpointAttempts: 2`、`commitPoint: 'first-valid-token'`。
即厂商自己也认为「聊天流不可重放，首字节之后就是提交点」。这与 CLIProxyAPI 的
「bootstrap 缓冲，首字节前才允许整轮换号重发」是同一件事的两次独立发现。

---

## 3. 架构

### 3.1 选定的方案与理由

三个候选：

| | A：`wbproxy.rs` 平行复制 qproxy | B：重构成 `Vendor` trait | **C：`wbproxy.rs` + 薄 `vendor` 分派层（选定）** |
|---|---|---|---|
| 改动面 | 只加新文件，但 proxy.rs 里要并列调两套钩子 | 重写 qproxy.rs 内部 | 加 2 个新文件；proxy.rs / gateway.rs 各改约 5 行调用点 |
| 请求体改写落在哪 | 无处可落（proxy.rs 现在完全不改 body） | trait 的一等方法 | `vendor::prepare_request()` 单一钩子 |
| 与并发会话的冲突 | 低 | **高**（qproxy.rs 今天 22:54 还在被改） | 低（不碰 qproxy 内部） |
| 加 Trae 的代价 | 再并列一套 | 一个 impl | 一个模块 + 一个 match 分支 |

选 C。核心理由：**请求体改写不是 WorkBuddy 的特殊需求，是这一层本来就缺的能力。**
Trae 已经独立证明了这点 —— 它的 body 要求 `content` 必须是数组、
`tools[].function.parameters` 必须是 JSON 字符串、`app_version_code` 必须 int64，
与 OpenAI 都不同。所以把 `prepare_request` 加在共享层是补真实缺口，
不是「为迁就某一家的缺陷去改共享服务层」。

### 3.2 模块划分

```
src-tauri/src/
  wbproxy.rs     新增 · WorkBuddy 厂商适配器（凭据 / 账号 / 探测 / 分类 / 请求准备 / 记账 / 视图 / 一键）
  vendor.rs      新增 · 薄分派层：按 app_id 把调用转给 qproxy 或 wbproxy
  proxy.rs       改动 · ① 调 vendor 而非直调 qproxy ② 加 prepare_request 钩子
                       ③ fail-closed 鉴权 ④ 冷却参与选路 ⑤ 单请求换号上限 ⑥ JSONL 落盘
  gateway.rs     改动 · resolve_member / member_options 改走 vendor
  commands.rs    改动 · 新增 wproxy_* 动作（全部在 app_free 侧）
  state.rs       改动 · proxy 段新增 wproxy 视图键
  secrets.rs     不动 · 直接用 put/get/flush
  qproxy.rs      不动 · 一行都不改（并发会话正在编辑它）
ui/
  index.html     改动 · #qproxy 旁并列一个 #wproxy
  app.js         改动 · renderWproxy + 一键 + 观测面板
  styles.css     改动 · 复用现有 .cards/.chip/.hintline，不新造类
```

### 3.3 `vendor.rs` 分派契约

分派层只认 `app_id`，不做任何厂商逻辑。签名：

```rust
/// 这个 app_id 是否属于某个已接通的厂商反代家族
pub fn family_of(app_id: &str) -> Option<&'static str>;      // "qoder" | "workbuddy"

pub fn region_of(app_id: &str) -> Option<&'static str>;      // "cn" | "global"
pub fn model_base(region: &str) -> &'static str;             // 推理 origin
pub fn token_for(app_id: &str, name: &str) -> Option<String>;
pub fn upstream_url(base: &str, path: &str) -> String;       // 客户端路径 → 厂商路径

/// 新增能力：发出前改写请求。返回 (headers_to_set, body_bytes)
/// 厂商不需要改写时原样返回，proxy.rs 不区分「没改」和「改成一样」
pub fn prepare_request(
    family: &str, region: &str, account: &VendorAccount,
    method: &str, path: &str, headers: &[(String, String)], body: &[u8],
) -> Result<Prepared, PrepError>;

pub fn note_outcome(family: &str, account_key: &str, model: &str,
                    status: u16, body_peek: &str, ms: u128, ok: bool);
pub fn member_usable(app_id: &str, name: &str) -> (bool, String);   // 供 gateway 报可用性
pub fn stored_state(app_id: &str, name: &str) -> Value;
```

`PrepError` 必须是结构化的、带维度的，不能是字符串 —— proxy.rs 要据此决定
「直接回 4xx 给调用方」还是「换一个账号重试」：

```rust
pub enum PrepError {
    NoCredential(String),        // 账号级：没有可用令牌
    ModelUnknown { asked: String, available: Vec<String> },  // 请求级：别名表认不出
    Guardrail(String),           // 账号级：护栏挡下（今日额度用完等）
    Cooldown { until: u64 },     // 账号级：冷却中
    MalformedBody(String),       // 请求级：body 不是合法 JSON / 缺 messages
}
```

`ModelUnknown` 与 `MalformedBody` 是**请求级**：换号无用，必须原样把原因回给调用方。
这条区分是本设计最容易写错的地方，实现时要有单测钉住。

`Cooldown` 变体的定位要说清：**正常情况下选路阶段（§3.4 第 2 步）就已经跳过冷却中的成员，
根本走不到 prepare。** 它只是一道竞态兜底 —— 选路与准备之间另一个线程把该 (账号,模型)
打进冷却时会命中。所以它的语义是「当作账号级失败，换下一个成员」，
不是「回 4xx 给调用方」。两者混淆会导致一个请求直接把错误抛给用户而不是换号。

### 3.4 数据流

```
其他 Agent
  │  POST http://127.0.0.1:8788/v1/chat/completions
  │  Authorization: Bearer ac-xxxxxxxx        ← 驾驶舱下发的 Key
  ▼
proxy.rs
  1 鉴权：dist_keys 里查 → 命中则取授权的池子；未命中 → 401（fail closed）
  2 选路：池内成员按 round-robin，跳过冷却中的 (账号×模型)，最多换 max_retry_accounts 个
  3 vendor::token_for(app,name) 取令牌；没有 → 换下一个成员
  4 vendor::prepare_request(...) 改写 headers + body
       · 注入 system 首条（幂等）
       · 注入 prompt_cache_key（稳定派生）
       · 模型别名 → 真实模型 id
       · 补齐 28 个头
       · 剥掉调用方传来的 Authorization / 归属头（不许穿透）
  5 转发到 model_base(region) + /v2/chat/completions
  6 流式：首字节前失败 → 允许换号重发；首字节后 → 零拷贝透传，绝不重试
  7 vendor::note_outcome(...) 记账 + 冷却
  8 JSONL 落一行
  ▼
调用方拿到标准 OpenAI SSE
```

### 3.5 必须本地应答、不许转发上游的路径

`proxy.rs` 现在**没有路由表**，`serve()` 把一切 method/path 直接透传给选中的上游
（`proxy.rs:236`）。这对「上游本来就是 OpenAI 兼容」的 Qoder 国际版没问题，
但对要分发给别的 Agent 的场景是个真缺口：Codex / ZCode / 大多数 OpenAI 客户端
在接入一个新 base_url 时会**先拉 `GET /v1/models`**，转发到
`www.workbuddy.cn/v1/models` 必然 404，她看到的就是「填了地址却连不上」，
而真实原因完全不可见。

⇒ proxy.rs 加一张**极小**的本地路由表，只两条：

| 路径 | 处置 |
|---|---|
| `GET /v1/models`（含 `/models`） | **本地应答**：把授权池子里各厂商的可用模型合成一份 OpenAI `{"object":"list","data":[{"id","object":"model","owned_by"}]}`。`owned_by` 用 `workbuddy-cn` / `workbuddy-intl` / `qoder-global` 这种能认出来源的值。数据来自 §2.6 的运行时清单，**不写死**。 |
| 其余一切 | 照旧透传（保持 `proxy.rs` 现有行为，不扩大改动面） |

`/v1/models` 同样要过 §8 的鉴权：无 key / 未知 key → 401，不泄露模型清单。
（magpie 自陈的安全缺口正是「无 key 拿到 `/v1/models` 200」，10-10 复审在 main 分支复现过 ——
别重犯。）

---

## 4. 凭据获取与保管

### 4.1 `atRestSecretKey` 的存放（用户已定：进保管箱）

- **不进源码。** 仓库是公开 MIT，写进去一 push 就公开了一个「对所有安装通用」的厂商密钥，不可撤回。
- 存 `secrets::put("wbproxy:at_rest_secret", <value>)` + `secrets::flush()`，即 DPAPI + 可选主密码。
  与既有 `apikeys.json` / `authpack:*` 同一套保管箱，防护等级不降。
- **导入动作 `wproxy_import_secret`**（`app_free` 侧）：
  1. 读 `~/.agenthub/agenthub.py`，正则取 `WORKBUDDY_AT_REST_SECRET_KEY\s*=\s*"([^"]+)"`
  2. 校验：base64 可解、解出 32 字节（`Sik9…` 那串是 44 字符 base64 = 32 字节）
  3. `secrets::put` + `flush`；flush 失败必须报错（只在内存里存着，程序一重启就没了 —— authpack 已有同款教训）
  4. 保管箱锁着 → 明确报「先解锁保管箱再导入」，不静默失败
  5. agenthub 不在 / 正则没命中 → 报「读不到，可以手动粘贴」，并给 `wproxy_set_secret`（payload 带 `value`）
- **`wproxy_secret_status`**：只回 `{present: bool, fingerprint: "sha256前8位", imported_at}`。
  指纹用来让她认出「是哪一把」，绝不回明文（照 `cliauth.rs::mask` 与 `keys.rs` 的既有做法）。
- 读路径唯一出口 `wbproxy::at_rest_key() -> Option<[u8;32]>`：`secrets::get` → base64 decode →
  SHA256 → 32 字节。**取不到就返回 None，上层如实报「还没导入密钥」，绝不回落到任何硬编码值。**

### 4.2 手写 SHA-256

Cargo 依赖树是钉死的（`Cargo.toml:22-27` 有 `indexmap` 那条构建修复注释），
`qproxy.rs:761-765` 明说「不引外部加密 crate 是刻意的」，且已手写 AES-256-GCM 并钉了 NIST 向量。
⇒ **SHA-256 同样手写**，约 60 行，放 `wbproxy.rs`（若 qproxy 日后也要，再提到公共模块，本期不动它）。
必须钉三条 NIST/FIPS 180-4 向量：空串、`"abc"`、448-bit 那条。

### 4.3 解密

复用 `qproxy::gcm_open(key, nonce, ct_and_tag, aad)`（已有、已钉 NIST GCM 用例 13/14 + 篡改必失败）。
新增 `wbproxy::aad_field(suite, key_id) -> Vec<u8>` 按 §2.2 拼帧，用 golden-bytes 单测钉死
（断言 AAD 的十六进制串，不靠「跑通了」来证明）。

`wbEncrypted_field(key, field) -> Result<String, String>`：
- `field` 是字符串 → 原样返回（客户端未加密时的情形，OFM/cockpit-tools 都遇到过）
- `$wbEncrypted != 1` → 报 `UNSUPPORTED`
- `suite != 1` → 报 `UNSUPPORTED`（cockpit-tools 同款白名单纪律）
- 出现未知加密字段 → 不猜，报错

### 4.4 账号枚举与 mismatch 守卫

照 `qproxy::accounts()` 的形状：遍历 `registry::load_instances("workbuddy" | "workbuddy-intl")`，
每条实例取 `name` + `data_dir`，登录态路径**不用 data_dir**，而是
`registry::auth_pack_files(app_id)` 里登记的那条绝对路径（`{LOCALAPPDATA}` 占位符由
`registry::auth_pack_path()` 解析）。

理由：WorkBuddy 的登录态**整机一份**在 `CodeBuddyExtension` 下，不在档案目录里；
`local_quota::falls_back_to_default()` 对这两个产品恒 true 就是这个事实。
所以「按账号查」只能靠调用方显式给路径（agenthub `_wb_read_login(auth_file=…)` 的注释写得很清楚：
**给了就只认它，绝不回落默认路径 —— 回落就是串号**）。本期两个产品各一张卡、
各一份登录态，不做多号；`mismatch` 守卫照 qproxy：登录态 uid ≠ 卡片 `quota_user_id` 就不给令牌。

---

## 5. 令牌生命周期

### 5.1 缓存

内存缓存 300 秒（照 qproxy 的 `CREDS`）。缓存键 = `app_id`（因为登录态整机一份）。
缓存必须能被**主动失效**：`invalidate(app_id)`，供 401 重读与 authpack 换号后调用。

### 5.2 到期

`auth.expiresAt` 是毫秒时间戳。国内版实测 ≈2026-11-16（约 27 天后），国际版 ≈2027-10。
界面显示剩余天数（复用 `quota::days_left`）。**到期不是错误状态**，因为客户端会自己续。

### 5.3 绝不碰 refreshToken（硬规则）

refreshToken 是一次性的，用一次就把她的客户端踢下线。这条在三个独立来源上收敛：
magpie `logins.go:3-10`（凭据只能有一个持有者）、channel-pack 的 Qoder 实现
（「refresh token 一用就废，所以 move 不 copy」）、Trae 反代实测同款结论。
`qproxy.rs` 已经这么做了，WorkBuddy 照办。

**401 的正确处置**：重读 `.info` 文件（客户端自己会刷新写回 —— 国内版那份 mtime 是
今天 23:34，证明它在活跃续期）→ `invalidate` 缓存 → 重发一次。
仍 401 才标记 `token_expired` 终态，界面如实说：
「令牌过期了，开一次 WorkBuddy 客户端让它自己续上，再回来点刷新」。
**不代替客户端刷新，也不假装能刷新。**

---

## 6. 请求准备（`prepare_request`）

按顺序执行，每步都要幂等：

1. **解析 body**：非合法 JSON → `MalformedBody`。取出 `model`、`messages`、`stream`。
2. **模型别名解析**：
   - 先查该区域的 0 倍率清单（§2.6，运行时从 `/v3/config` 缓存读，标新鲜度）
   - 命中 → 原样用
   - 未命中 → 查别名表；别名表认不出 → `ModelUnknown{asked, available}`，
     **把可用清单一起回给调用方**，不许原样透传（透传必然 400，且错误信息对不上）
   - 别名表初始内容（可编辑，存 config.json 的 `wproxy.aliases`）：
     常见请求名 → 该区域的某个 0 倍率模型。第一版给 `gpt-4o`/`gpt-4o-mini`/`claude-3-5-sonnet`/
     `claude-sonnet-4`/`deepseek-chat`/`qwen-max` 之类落到 `hy3`（两区都免费）。
     **别名表是配置不是代码**，因为她随时可能改主意，也可能上游改免费清单。
3. **注入 system 首条**（幂等）：`messages[0].role != "system"` 时 unshift 一条最小 system。
   已有 system → 一个字节都不动。默认内容取 `wproxy.system_fallback`（可编辑，
   默认 `"You are a helpful assistant."`）。
   ⚠️ 这条对国际版是硬要求（§2.7），对国内版是保险 —— 判据不依赖「另一个区域碰巧不校验」，
   照 OFM `account-probe.ts` 的注释理由办。
4. **注入 `prompt_cache_key`**（幂等：调用方已给就不覆盖）：
   稳定派生 = `sha256("<account_uid>|<dist_key_id>|<caller_conversation_hint>")` 取前 32 hex。
   `caller_conversation_hint` 取调用方 body 里的 `user` 字段（OpenAI 惯例）或
   `X-Conversation-ID` 头；都没有就用 `dist_key_id`。
   **绝不用随机 UUID** —— 随机的话前缀缓存永不命中，等于白扔 17 倍成本。
5. **`stream` 处理**：调用方要 `stream:true` → 原样。要 `stream:false` →
   **上游仍发 `stream:true`，本地聚合成一个非流式 `chat.completion` 响应**。
   理由：§2.4 实测只验过 `stream:true`；`stream:false` 上游是否支持属未知（§13）。
   本地聚合是已知可行的做法，且不依赖未验证的上游行为。
   聚合时保留末 chunk 的 `usage`（含 `credit`），`choices[0].message.{content,reasoning_content,tool_calls}`
   由 delta 拼接。
6. **剥掉穿透头**：调用方传来的 `Authorization` / `x-api-key` / 任何 `X-IDE-*` / `X-Domain` /
   `X-User-Id` / `X-Product*` / `traceparent` / `b3` / `X-B3-*` / `User-Agent` 一律丢弃，
   由我们重新生成。**不许让调用方伪造归属头。**
7. **生成 28 个头**（§2.3）。会话标识按 wb2api-hub 的 `derive_id` 思路**稳定派生**而非随机：
   `X-Conversation-ID = sha256("<uid>|conv|<dist_key_id>")[:36]` 格式化成一个 UUID 形状，
   `acp-connection-id` 同理用 `"conn"` 盐，`X-Conversation-Request-ID` / `X-Request-ID` /
   `X-Conversation-Message-ID` **每次请求随机**（它们本来就是逐请求的）。
   稳定的好处：同一账号的流量在服务端看起来是一条连续会话，而不是每秒新建会话的机器人；
   不同账号之间不可关联（盐里含 uid）。
8. **`max_tokens` 兜底**：调用方没给就不发（保持网关默认），给了就透传。
   ⚠️ OFM 记录过：不发该字段时网关默认上限实测仅 32000，大文件写入会被截成
   `finish_reason:'length'`。界面要把这条写进帮助文案，让她知道「被截断可能是没设 max_tokens」。
9. **其余字段一律原样透传**（`tools` / `tool_choice` / `temperature` / `top_p` / `stop` /
   `response_format` / `reasoning_effort` …）。**不做白名单过滤** —— 上游认不认由上游回答，
   我们过滤只会把「上游支持但我们不知道」的字段悄悄吃掉，那比透传更糟。
   ⚠️ 但 `tools` 本期属**未验证**（§13 第 2 条）：透传它，同时在界面如实标
   「工具调用未验证」，不假装支持。

---

## 7. 号池治理

### 7.1 三维拒判

见 §2.7 表。实现要点：`classify(status, body_peek) -> Outcome`，`Outcome` 是枚举而非字符串：

```rust
pub enum Outcome {
    Ok,
    RequestShape { fix: ShapeFix },   // 11128 两种 → 不换号不冷却
    AccountBlocked,                   // 11140 → 换号 + 账号冷却（不编造时长）
    AccountUnauthorized,              // 401 → 重读文件后重发一次
    AccountRateLimited { retry_after: Option<u64> },  // 429 → 账号×模型冷却
    ModelCapacity { code: u16 },      // 6020/6021/6022 → 不换号
    ModelUnavailable,                 // 404 / 模型不支持 → 长冷却该 (账号,模型)
    UpstreamGateway,                  // 502/503/504 → 短冷却
    Unknown { status: u16 },          // 对不上号的一律 unknown，不当 empty、不当账号故障
}
```

**`Unknown` 必须是一个独立分支。** magpie 的 `readLogins` 把解析错误当「没有账号」，
两天后在 `agent/stash.go` 重演；OFM 用 `coversToday` 而不是「领到过」记账 —— 都是同一条纪律：
**读/解析失败算 unknown，不许当 empty，更不许当账号故障。**

### 7.2 冷却表

- 键 = `<app_id>/<账号名> × <真实模型 id>`（CLIProxyAPI 与 OFM 都是这个键；只用账号会误伤兄弟模型，只用模型会误伤账号）
- **落盘** `<数据根>/wproxy.json` 的 `cooldowns` 段（CLIProxyAPI 的 `.cds` 落盘、重启不丢；
  magpie 只在内存、重启即丢 —— 这一维照抄前者）
- **写入只许变长不许变短**（OFM 的硬规则）
- 分档：`AccountBlocked` 无服务端时长 → 用固定档（初值 30 分钟，可配）；
  `429` 有 `Retry-After` 照抄但下限 10s、无则 `1s × 2^level` 封顶 30 分钟，
  同窗口并发失败只推进一次；`ModelUnavailable` 12 小时；`UpstreamGateway` 60s
- **`RequestShape` 一律不进冷却**（这是本设计最容易写错的一条，必须有单测钉住）
- ⚠️ 顺带补一个 qproxy 的真实缺口：它记了 `cooldown_until` 字段但
  **proxy.rs 的选路根本没读它**。本期在 proxy.rs 的选路里统一读冷却（对两个厂商都生效），
  这是补共享层的缺口，不是为 WorkBuddy 特判。

### 7.3 重试预算

- `max_retry_accounts = 3`（Qoder-free 的参数，作者原话：「没有它的话，一个请求能把整池账号
  挨个试一遍、全打上失败标记，一晚上能把账号池废掉一半」）。她只有 2 个号，
  所以实际值是 `min(3, 池内成员数)`，但参数要留着。
- **首字节提交点**：SSE 首个 `data:` 行（跳过 `: heartbeat` 注释行）到达前失败 → 允许换号重发；
  到达后 → 零拷贝透传，**绝不重试、绝不换号**。
  依据：asar 自己的 `commitPoint:'first-valid-token'` + CLIProxyAPI 的 bootstrap 缓冲。
  ⚠️ 注意「首个 `data:` 行」不等于「首个有内容的 chunk」—— 实测首个 data 行的
  `delta.content` 是空串、`reasoning_content` 也是空串，它只是 role 声明帧。
  判据要用「收到第一个 `data:` 行」，与 Trae 那条「第一个事件是 metadata 不是 output，
  别把它当成已产出内容」是同一类坑。
- **池化换号时绝不回放 reasoning**（OFM 明确删掉加密 reasoning 项，理由「换号后必然 400」）。
  换号重发时必须用**原始请求体**重新走一遍 `prepare_request`，不许拿上一次的响应内容当输入。

### 7.4 四条护栏（wb2api-hub）

wb2api-hub 四条**机制全部默认关闭**；我们反过来**机制全部默认启用**，但只有
「保留积分下限」给非零默认值 —— 因为这两个号是她日常在用的真账号，烧穿了没有备份号，
而另外三条在「只用 0 倍率模型」的前提下本来就不会触发（`credit` 恒 0）。
留成可编辑配置而不是死逻辑，是为了她日后放开倍率时不用改代码。

| 护栏 | 机制 | 默认值 | 触发后 |
|---|---|---|---|
| 保留积分下限 | 启用 | **200 积分** | 低于则不接单，回 429 + 说明 |
| 每日积分上限（按账号） | 启用 | 0（＝不限） | 达限只服务 0 倍率模型 |
| 每日 token 上限（按账号） | 启用 | 0（＝不限） | 达限切号；无号可切回 429 |
| 按模型每日 token 上限 | 启用 | 0（＝不限） | 只禁该模型 |

本地 0 点解封，单账号独立计数。余额从既有的额度管线读（`local_quota` / agenthub 的
`get-user-resource-summary`），**不新写抓取器**。

---

## 8. 鉴权（用户已批准改成默认拒绝）

现状（两个独立来源确认仍在源码里）：`proxy.rs:101-102` 不认识的 key 回落「全部启用的池子」，
`proxy.rs:399-401` CORS `Access-Control-Allow-Origin: *`（预检也回 ACAO `*`）。
HANDOFF `:213` 记录这是「本机自用方便」有意为之。

改为：

- **未知 key → 401，deny-by-default**（CLIProxyAPI `sdk/access/manager.go:59-87` 与 OFM 都是这个做法）
- **CORS 收紧**：默认不发 `Access-Control-Allow-Origin`；预检对无 key 请求直接 401
- **留一个开关** `settings.proxy.lan_loose`（默认 `false`）：打开才恢复「未知 key 回落全部池子」，
  且界面上用红字写明「浏览器里任何网页都能借你的 token 池打上游」
- 只绑回环（现状已是）。若日后要开局域网中继，照 OFM：独立 key + 路径白名单 +
  跳数头 508 断环 + 日志只记 path 不记 query，且 `dns.lookup` 全部结果必须回环才绑
- **下发 Key 的生成要换成密码学强度**：现在 `gateway.rs:24-40` 是 xorshift，注释自认
  「不追求密码学强度」。既然 §8 的整个安全模型都压在这把 key 上，就必须换。
  **实现方式：手声明 `#[link(name = "advapi32")] extern "system" { fn SystemFunction036(...) }`
  （即 `RtlGenRandom`）**，与 `procs.rs::windowed_pids()` 手声明 `EnumWindows` 是同一套路，
  **零新增依赖**（Cargo 依赖树钉死，不能加 `rand`/`getrandom`）。
  这条是 §8 的必然推论，不是范围外扩张。

---

## 9. 可观测性

### 9.1 逐请求日志（JSONL，落盘）

`<数据根>/logs/proxy-YYYYMMDD.jsonl`，按天滚动，追加式。
**不引 SQLite** —— Cargo 依赖树钉死，加 `rusqlite` 风险大于收益；JSONL 零依赖、可 grep、可被
她自己的脚本处理（与 `reference-daily-usage-sources` 那条链路同构）。
保留 30 天，启动时清更早的（清理失败不阻塞启动）。

每行字段（以 Cockpit Tools 的 `request_logs` 37 列为终版参照，裁掉我们拿不到的）：

| 组 | 字段 |
|---|---|
| 标识 | `ts`、`req_id`、`dist_key_id`、`pool`、`via`（key 名） |
| 路由 | `app`、`region`、`account`、`model_asked`、`model_used`、`attempt`（第几次换号） |
| 结果 | `status`、`biz_code`、`biz_msg`、`outcome`（§7.1 枚举名）、`ok` |
| 时延 | `ttfb_ms`（首字节）、`total_ms` |
| 用量 | `prompt_tokens`、`completion_tokens`、`reasoning_tokens`、`cached_tokens`、`prompt_cache_hit_tokens`、**`credit`** |
| 凭据版本 | `token_sha256`（前 12 位，标令牌版本而不泄露令牌 —— CLIProxyAPI 的 `AccessTokenSHA256` 做法） |
| 流式 | `stream`、`chunks`、`finish_reason`、`committed`（是否过了首字节提交点） |

⚠️ **绝不记 body、不记 messages、不记令牌明文。** OFM 的纪律是「日志只记 path 不记 query」，
我们比它更严：连 path 之外的请求内容都不记。`model_asked`/`model_used` 是模型名不是内容。

### 9.2 聚合视图

`wbproxy::view()` 输出（照 `qproxy::view()` 的形状，界面复用同一套渲染）：

- `accounts[]`：名称 / 区域 / 状态 / 令牌打码 / 到期天数 / 延迟 / 调用成败计数 / 最近错误 /
  是否在池 / 是否可用 / **今日 credit 合计** / **今日 token 合计**
- `models[]`：该区域当前 0 倍率清单 + 缓存新鲜度（mtime）
- `secret`：`{present, fingerprint, imported_at}`
- `guardrails`：四条当前值与今日计数
- `cooldowns[]`：正在冷却的 (账号, 模型) 与剩余秒数
- `base_url`、`port`、`pool`、`dist`（与 qproxy 同）
- `today`：`{calls, ok, fail, credit, tokens, by_account[], by_key[]}`

### 9.3 界面

`index.html` 在 `#qproxy` 旁并列 `#wproxy`；`app.js` 加 `renderWproxy(w)`，
复用 `.cards` / `.chip` / `.hintline` / `.qgroup` 现有类，**不新造类**
（坑 33 与 10-11 那轮版式重排的纪律）。

区块顺序照「只读状态 → 可改配置 → 动作」：
1. 密钥状态（present / fingerprint / 「导入」按钮 / 「手动粘贴」兜底）
2. 账号卡片（两个区域各一张，含今日 credit/token）
3. 0 倍率模型清单 + 新鲜度
4. 别名表（可编辑）
5. 护栏四条（可编辑，默认开）
6. 冷却中列表
7. 接入区：`base_url` + 「显示 Key」/「复制」+ 最近 30 条日志
8. 「↻ 探测」与「⚡ 一键开启」

⚠️ 界面文案里**不许出现按钮名去指代后端状态**（10-12 定的规矩：
后端回结构化字段，前端只看字段，因为文案对不上会导致确认框永远不弹）。

---

## 10. 一键流程

`wproxy_oneclick`（`app_free` 侧），幂等，照 `qproxy::one_click` 的六步：

1. 密钥不在保管箱 → 先尝试从 agenthub 导入；仍拿不到 → 返回 `{ok:false, need:"secret"}`，
   界面弹「导入密钥 / 手动粘贴」
2. 枚举两个产品的账号，逐个 probe（免费探针 = `get-user-resource-summary`，不花积分）
3. 探测通过（`state == ok`）的账号以 `member_src_instance` 进池；池名 `WorkBuddy 免费模型`
   （同名池复用，不重复建）
4. `upsert_dist` 生成/复用下发 Key，名 `WorkBuddy 免费（自用）`
5. 反代没在跑就 `proxy.start(port)`
6. 返回 `{ok, msg, base_url, key, key_created, pool, members, skipped[]}`；
   `skipped[]` 逐条说明为什么某个号没进池（令牌过期 / mismatch / 探测失败），
   **不许静默跳过**

「一键」的判据是：她点一次，拿到一个能直接粘进别的 Agent 的 `base_url` + `key`。
中间任何一步失败都要说清是哪一步、为什么、退路是什么。

⚠️ **`inject.rs` 的 WorkBuddy 目标对本期是循环的，不要选它。**
`inject.rs` 写的是 `~/.workbuddy/models.json`（把自定义模型注入 WorkBuddy 客户端），
而本期是把 WorkBuddy 变成上游 —— 把 WorkBuddy 的免费模型注回 WorkBuddy 自己没有意义，
而且国内版与国际版共用那一份 `models.json`（`inject.rs:14`），写错会同时影响两个产品。
本期真正有意义的注入目标是 **Qoder CN / Qoder**（它们能接 OpenAI 兼容自定义 provider）。
其余产品按用户决定走「复制地址 + 复制 key」手动粘。

---

## 11. 并发与部署约束（必读）

⚠️ **这个仓库此刻有别的会话在同时改。** 本次勘察实测：`qproxy.rs` mtime 2026-10-19 22:54、
`proxy.rs` 22:28、`commands.rs` 22:24，`git status` 有 15 个已修改文件 + 2 个未跟踪新文件，
全部未提交。历史上已经踩过三次（坑 133 / 199 / 350）。

纪律：

- **`qproxy.rs` 一行都不改。** 需要它的能力就通过 `vendor.rs` 调用它现有的 `pub fn`。
- 每次 Edit 前重读目标区域；改完 `node --check ui/app.js` + `cargo test` 兜底。
- 构建前先看 `ui/*` 与 `src-tauri/src/*` 的 mtime，确认没把别人半截的东西打进去。
- **本期先只落源码，不替换 `bin/AgentCockpit.exe`**（10-11 定过的规矩）。
  改 `ui/` 不重编看不见（`frontendDist:"../ui"` 是编译期内嵌，坑 198），
  所以她的真机验收必须等一次干净的构建。
- `cargo test` 必须在驾驶舱实例停掉时跑：`proxy::tests::proxy_serves_and_stops` 绑的是
  生产端口 8788，实例在跑时必挂 `os error 10048`（坑 235）。新增端到端测试要用**独立端口**。
- 工具链：`cargo` 不在 PATH，要 `RUSTUP_HOME` / `CARGO_HOME` 指到 `D:\CodeProgram\caches\...`；
  **Bash 里传 Windows 路径必须用单引号**，双引号会让反斜杠被吞、cargo 在当前目录建出垃圾目录（坑 268）。

---

## 12. 机制出处对照（「集百家之长」的账）

| 机制 | 出处 | 许可 | 我们怎么用 |
|---|---|---|---|
| `$wbEncrypted` 解密链路（AAD 拼帧 / suite 白名单） | cockpit-tools〔源〕、dsh-workbuddy-connect〔源〕 | CC BY-NC-SA / 未查 | **只借机制，代码自己写**；实际实现照本机 agenthub（她自己的代码） |
| 全头族 28 个 | WorkBuddy 自己的 asar〔源〕 | — | 逐条复刻 |
| 端点 / 0 倍率清单 | 本机日志 + `/v3/config` 缓存〔测〕 | — | 运行时读，不写死 |
| `prompt_cache_key` 省 17 倍 | workbuddy2api-hub 血缘（OFM vendored，iJetLi 上游） | **纯 MIT** | 采纳，改成稳定派生 |
| `reasoning_effort` 才是思考真开关 | 同上 | MIT | 采纳 |
| 11140 = 账号级、必须换号 | 同上 | MIT | 采纳 |
| 国际版强制 system 首条 → 11128 | 同上 `account-probe.ts:98-112` | MIT | 采纳，且无条件对所有区域注入 |
| `derive_id` 稳定派生会话标识 | workbuddy2api-hub〔读〕 | **纯 MIT** | 采纳思路，自己实现 |
| 四条账号级护栏 | workbuddy2api-hub〔读〕 | 纯 MIT | 采纳，默认改成开 |
| 三维拒判（账号/模型/内容） | OFM + magpie LESSONS 双源收敛 | MIT | 采纳，落成 `Outcome` 枚举 |
| 冷却键 = 账号×模型、落盘、只许变长 | CLIProxyAPI + OFM | **纯 MIT** | 采纳 |
| `max_retry_accounts=3` | Qoder-free〔读〕 | **无 LICENSE，代码不可抄** | 只借参数与理由，代码自己写 |
| 首字节提交点 / bootstrap 缓冲 | CLIProxyAPI + WorkBuddy 自己的 asar | MIT / — | 采纳 |
| 未知 key 即拒（deny-by-default） | CLIProxyAPI + OFM | MIT | 采纳（§8） |
| `AccessTokenSHA256` 记令牌版本不泄露令牌 | CLIProxyAPI | MIT | 采纳 |
| 池化换号绝不回放 reasoning | OFM | MIT | 采纳 |
| 健康判据必须解析到 SSE 事件层 | Trae 反代本机实测 | — | 采纳 |
| 读/解析失败算 unknown 不算 empty | magpie + OFM 双源 | MIT | 采纳 |
| 令牌只读不刷、refreshToken 一次性的 | magpie + channel-pack + Trae 三源 | MIT | 采纳（§5.3） |
| 日志不记 body/query | OFM | MIT | 采纳并加严 |

**代码复制纪律**：cockpit-tools（CC BY-NC-SA）、Qoder-free / workbuddy-account-hub /
agent-cockpit（无 LICENSE 文件）、agent2api（MIT + 禁商用/禁批量账号运营）——
这五家**只借机制与结论，一行代码都不抄**。可抄的是 CLIProxyAPI、magpie、
workbuddy2api-hub、OFM 的 vendored channel-pack（均纯 MIT 且 LICENSE 文件已验）。

---

## 13. 已知未知（实现期要用 1-2 发请求定的）

按用户授权（「允许，用 0 倍率模型」），实现期可以再做少量实测。已用 8 发，剩余预算控制在 10 发内。

1. **`stream:false` 上游是否支持** —— §6 第 5 步选择本地聚合来绕开这个未知；
   若实测发现上游原生支持，可以简化（但本地聚合仍然更稳，因为不依赖未文档化行为）。
2. **`tools` / `tool_calls` 是否可用** —— 本期不主动测；OFM 的实现证明可用
   （它做了 tool_calls 分片按 index 缓存 id 的处理），但我们没有一手证据。
   第一版**如实标注「工具调用未验证」**，不假装支持。
3. **`reasoning_effort` 在 `hy3` / `step-5-preview` 上的合法档位** —— 发错档位服务端 400。
   第一版**不发**该字段（保持默认），界面上给一个可选项让她自己试。
4. **国内版 `X-Domain=www.codebuddy.cn` 与端点 `www.workbuddy.cn` 不同域是否长期安全** ——
   已按客户端原样发且实测 200；若日后出现 11128 `request illegal`，这是第一个要怀疑的点。
5. **0 倍率清单的过期速度** —— 促销徽章按月编号，说明会变。
   运行时读缓存 + 标新鲜度已经处理；若缓存过期太久（>7 天）界面要提示她开一次客户端刷新。

---

## 14. 测试计划

### 14.1 单元测试（离线，必须全过）

| 测试 | 钉住的不变量 |
|---|---|
| `sha256_matches_nist_vectors` | 空串 / `"abc"` / 448-bit 三条 FIPS 180-4 向量 |
| `aad_field_framing_matches_golden_bytes` | AAD 十六进制串逐字节等于 §2.2 的拼帧结果 |
| `wbencrypted_rejects_unknown_suite` | `suite != 1` → `UNSUPPORTED`，不猜 |
| `wbencrypted_passes_plaintext_through` | 字段是字符串 → 原样返回 |
| `regions_map_to_the_hosts_we_measured` | 两个 origin 常量钉死（照 qproxy 同名测试） |
| `classify_separates_the_three_dimensions` | 11128 两种 msg → `RequestShape`；11140 → `AccountBlocked`；6020/6021/6022 → `ModelCapacity`；对不上号 → `Unknown` |
| `request_shape_never_cools_an_account` | **最关键的一条**：`RequestShape` 之后冷却表必须仍为空 |
| `system_injection_is_idempotent` | 已有 system 首条 → body 一个字节都不变 |
| `prompt_cache_key_is_stable_not_random` | 同 (uid, dist_key, hint) 两次调用得到同一个值；不同 dist_key 得到不同值 |
| `model_alias_unknown_is_a_request_error` | `ModelUnknown` 是请求级，不触发换号，且返回体里带可用清单 |
| `caller_attribution_headers_are_stripped` | 调用方传 `X-User-Id` / `X-Domain` / `Authorization` 全部被丢弃并重生成 |
| `cooldown_only_lengthens_and_persists` | 缩短写入被忽略；重启后仍在 |
| `max_retry_accounts_caps_a_single_request` | 一个请求最多把 N 个账号标失败 |
| `committed_stream_never_retries` | 过了首字节提交点后，上游报错也不换号 |
| `unknown_dist_key_is_rejected` | fail-closed：未知 key → 401（**这条反转了现有行为，必须钉住**） |
| `loose_mode_restores_the_old_behaviour` | `lan_loose=true` 时才回落全部池子 |
| `secret_never_falls_back_to_a_hardcoded_value` | 保管箱没有 → `at_rest_key()` 返回 None，上层报「还没导入」 |
| `token_sha256_never_contains_the_token` | 日志行里不含令牌明文任何片段 |
| `models_endpoint_is_served_locally` | `GET /v1/models` 不转发上游，返回本地合成的 OpenAI list 形状 |
| `models_endpoint_requires_a_key` | 无 key / 未知 key → 401（magpie 那个洞不许重犯） |
| `dist_key_is_cryptographically_random` | 连生成 1000 把，去重后仍是 1000 把，且熵源不是时间戳（两次同纳秒调用结果不同） |
| `unknown_fields_pass_through_unfiltered` | body 里塞一个我们不认识的字段，转发出去的 body 里它还在 |

### 14.2 真机测试（`#[ignore]`，照 qproxy 的两条同款）

- `real_wb_blob_decrypts_on_this_machine`：读真 `.info`，断言解出的 accessToken 以 `eyJ` 开头、
  长度 >100，**不断言具体值、不打印**
- `real_wb_oneclick_serves_a_completion`：起独立端口（不用 8788）→ `one_click` →
  用下发 Key 打一次真补全（0 倍率模型）→ 断言响应含 `choices`、`usage.credit == 0`
  → `CfgGuard` 还原 config.json

### 14.3 界面验收（浏览器 mock，坑 121/197 的方法）

`python -m http.server` + `mock.js`，注意 `http.server.test(...)` 在这台机器上绑 IPv6 `::`，
要用 `localhost` 不是 `127.0.0.1`；改 `ui/` 后必须先 `fetch(url+'?cb='+Date.now(), {cache:'reload'})`
再 reload，否则加载旧文件。量：`undefined|null|NaN` 0 处、横向溢出 0、断字 0。

---

## 15. 回滚

| 层 | 回滚方式 |
|---|---|
| 源码 | 本期只加 2 个新文件 + 5 个文件的最小切口；`git checkout -- <file>` 即可，新文件直接删 |
| exe | 本期不替换 `bin/AgentCockpit.exe`；若后续构建，照惯例先备份 `.bak-<日期>-<意图>` |
| config.json | 新增段 `wproxy`（`aliases` / `guardrails` / `secret_fingerprint`），删除该段即回到现状；`save_config` 已是临时文件 + rename 原子覆盖 |
| 保管箱 | `wproxy:at_rest_secret` 一条，`secrets::remove` 即撤销 |
| 下发 Key | 走既有 `proxy_dist_delete` |
| 注入到其他 Agent | 走既有 `inject_revert`（只删自己写过的那几条，备份在 `<数据根>/backups/inject/`） |
| 日志 | `<数据根>/logs/proxy-*.jsonl`，直接删 |

---

## 16. 交付顺序建议

1. `wbproxy.rs` 的密码学层（SHA-256 + AAD + 解密）+ 全部离线单测 —— 不碰任何共享文件，零冲突风险
2. `wbproxy.rs` 的账号/探测/分类/状态落盘
3. `vendor.rs` 分派层 + proxy.rs / gateway.rs 的调用点改写（最小切口）
4. `prepare_request`（§6）+ `/v1/models` 本地应答（§3.5）+ 冷却参与选路 + fail-closed（§8）
5. JSONL 日志（§9.1）+ 聚合视图（§9.2）
6. 护栏（§7.4）
7. `commands.rs` 动作 + `state.rs` 视图键
8. UI（§9.3）
9. 真机 `#[ignore]` 测试 + 浏览器 mock 验收
10. 等她确认树干净后一起构建、换 exe、重启进程

第 1、2 步完全不碰共享文件，可以立刻开始而不担心与并发会话冲突。
