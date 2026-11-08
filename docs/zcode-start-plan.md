# ZCode Start Plan 接入

驾驶舱管理已保存的 ZCode 登录、账号路由、上游组和接入密钥，固定版本的本地组件负责 ZCode Start Plan 上游协议。不会更改客户端当前账号，不会使用 Coding Plan API Key 代替 Start Plan JWT。

在 ZCode 账号页登录并保存登录后，进入该产品的模型反代面板：安装本地组件 → 选择账号和模型 → 接入所选账号 → 测试所选模型。首次安装需要 Node.js 24+、npm 和网络；组件下载后只在本机运行。全局模型反代向导也支持 ZCode。

客户端地址为驾驶舱反代地址，模型使用 `zcode/<套餐模型 ID>`，API Key 使用驾驶舱生成的接入密钥。支持 OpenAI Chat、Anthropic Messages 和 Responses；Anthropic 请求在 ZCode 专属路由上原生转发，Responses 经驾驶舱已有的协议转换入口处理。模型目录取自 `.zcode/v2/config.json` 的对应服务商 Start Plan 项；该账号自己的额度记录也能提供候选模型，并标注“待实测”。未激活的服务商目录可能为空，候选模型不代表套餐仍有效。上游的实际额度、到期和验证结果需要通过真实请求确认。

组件首次安装到驾驶舱实际数据根的 `tools/zcode-start-plan`，按需启动，停用反代或退出驾驶舱时退出。内部端口随机分配且只绑定 `127.0.0.1`，使用进程内随机密钥；客户端的接入密钥和 OAuth 刷新令牌不会交给组件。Start Plan JWT 只在请求期间从所选账号的快照解密，通过本机认证连接传递，不写入组件配置、命令行或日志。保管箱锁定或身份不符时拒绝转发。不会自动领取活动套餐，也不会启用 Coding Plan 闲时通道。

依赖来源：[TriDefender/zcode-api](https://github.com/TriDefender/zcode-api/tree/37155a5f75f3f2d7bde1b83054c8d6eb2e23a935)，固定提交 `37155a5f75f3f2d7bde1b83054c8d6eb2e23a935`。源码归档 SHA-256 为 `3b2954265e56d22668fe88f19f872467e45522788889570bda1020de73a1aa4b`，下载后校验，再以 `--ignore-scripts` 安装依赖。该组件独立保留源代码和 LICENSE，采用 **CC BY-NC-SA 4.0**，其非商业、署名、相同方式共享条件适用于该组件；驾驶舱适配器没有复制其源码。使用或分发该组件需遵守它的许可。上游更新应重新审查、更新提交与校验值并执行集成测试，运行时不会自动换到最新版本。
