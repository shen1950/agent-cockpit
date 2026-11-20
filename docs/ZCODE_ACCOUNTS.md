# ZCode 账号与共享数据

BigModel 与 Z.ai 按供应商和真实用户 ID 分别登记。身份键为 `zcode:bigmodel:<id>` 或 `zcode:zai:<id>`，避免将无 ID 的 Z.ai 登录识别成通用“ZCode”账号。

对话、项目和配置继续使用本机原有 `.zcode` 普通目录。只备份并切换选中账号的 OAuth、活动供应商和对应 JWT；保留另一供应商登录缓存、bot 及项目 API 配置。凭据快照由本机 DPAPI 保管箱保存，不进入卡片元数据。切换和退出前必须确认重启 ZCode；只处理 ZCode 进程，核验失败恢复原登录。

移除卡片只删除登记，保留登录备份和共享数据。添加账号可选择已保存登录重新录入，无需删除本机目录或重复覆盖其他账号。现有两张卡从原有完整快照迁移，迁移前备份登记、保管箱、配置和当前凭据；迁移不切换当前窗口。

官方客户端的 `ZCODE_DATA_BASE_DIR` 同时改变会话和登录目录，不能只靠此参数实现安全共享数据的多账号并行。本版使用单客户端切换，不宣称支持共享数据的安全多开，不创建 Junction，不修改用户 HOME，不搬移默认目录。

## 凭据加密格式（会随官方客户端变化）

`v2/credentials.json` 的值有两种信封，都走 AES-256-GCM（iv 12B + tag 16B + ct）：

| 信封 | 形态 | 密钥 |
|---|---|---|
| `enc:v1:` | `enc:v1:<iv>.<tag>.<ct>` | `SHA256(ZCODE_CREDENTIAL_SECRET ?? "zcode-credential-fallback:{platform}:{homedir}:{username}")`，纯本地派生 |
| `enc:v2:os:` | `enc:v2:os:<24位指纹>:<iv>.<tag>.<ct>` | 32 字节随机密钥，存**系统凭据库**（Windows 条目名 `"{sha256(realpath(credentials.json))}.ZCode Credential Encryption"`，内容是该密钥的 base64url 文本） |
| `enc:v2:env:` | `enc:v2:env:<24位指纹>:<iv>.<tag>.<ct>` | `SHA256(ZCODE_CREDENTIAL_SECRET 或 credentials.json 同级的 credential-secret 文件)` |

指纹 = `SHA256(key).hexdigest()[:24]`，先验指纹再用密钥，绝不瞎猜。**2026-11-12 版客户端起默认写 `enc:v2:os:`**（同日 `v2/credential-source` 文件内容变为 `os`）。

⚠️ `enc:v2` 的密钥**不跟凭据文件走**：把密文快照搬进临时目录或实例目录（`materialize` / `zcode-read-*`）后，系统凭据库里那条记录仍挂在 `~/.zcode` 的原路径名下，所以取密钥必须允许「按指纹枚举兜底」，否则额度链路会 401。

⚠️ 认不出的信封必须**报错**，不许原样放行。原样放行的后果是 `active_provider` 变成一串密文，`identity()` 于是认为没登录，界面报「读不到（未知）」——把「官方换了格式」伪装成「你还没登录」，这是最难查的一类故障（2026-11-15 实际踩到）。

改动点在两处且必须同步：读取器 `src-tauri/scripts/zcode-auth.py`（登录态）与 agenthub 的 `_zcode_decrypt`（额度）。回归测试 `tests/zcode-credential-v2.test.py`（全合成数据，不读本机真实登录），已挂进 `tests/check-usage.ps1`。

通过 `agsync build` 运行 `tests/check-chatgpt.ps1`。`AGENTCOCKPIT_ZCODE_SMOKE=1` 额外执行现有账号迁移和真实余额检查，只使用临时 OAuth 快照发送余额 GET 请求，不续期、不切换、不退出当前账号；核对当前凭据与 ZCode 进程保持不变。
