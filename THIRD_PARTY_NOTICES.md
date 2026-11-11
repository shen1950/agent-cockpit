# 第三方组件、参考项目与致谢

核对日期：2026-11-05。许可证按所引用的版本核对；上游以后更改许可，不自动改变历史版本的许可。

AgentCockpit 自有代码以 [MIT License](LICENSE) 提供，© 2026 shen1950。第三方代码、运行时、数据和素材仍受各自许可约束。本清单区分**功能参考、改写适配、随包组件和另行安装的组件**；致谢不能代替原许可要求，也不表示上游认可本项目。

## 重点参考项目

| 项目与维护者 | 在 AgentCockpit 中的用途 | 参考版本与许可 | 分发边界 |
|---|---|---|---|
| **[Cockpit Tools](https://github.com/jlcodes99/cockpit-tools)** · jlcodes99 及贡献者 | Agent 平台登记清单、账号与实例隔离、ChatGPT/Codex 登录流程、本地会话用量展示等功能参考 | [`acc057d1fbc7d85d0aae288fdd2bc73581493d1e` 的 README 许可声明](https://github.com/jlcodes99/cockpit-tools/blob/acc057d1fbc7d85d0aae288fdd2bc73581493d1e/README.md#许可证)：**CC BY-NC-SA 4.0** | 本项目的相关实现位于 `registry.rs`、`chatgpt.rs`、用量模块等；该项目是行为和流程参考，不作为本程序的打包依赖。其代码与资源不能仅加致谢就按本项目 MIT 重新许可 |
| **[Magpie](https://github.com/yetone/magpie)** · yetone 及贡献者 | Agent 接入本地网关、模型图片能力、视觉代读、上下文/输出/压缩设置、供应商调度与估算计价、路由连线展示 | [`d783df46cbac9021e26a6bd71ca0b55544552eb2` 的 LICENSE](https://github.com/yetone/magpie/blob/d783df46cbac9021e26a6bd71ca0b55544552eb2/LICENSE)：**MIT**，Copyright (c) 2026 yetone | 参考其交互与协议处理方式，在本项目 Rust 网关和原生前端中适配，使用本项目的接入密钥授权及账号结构；不打包 Magpie 应用。原许可完整保存在 [MAGPIE-LICENSE.txt](docs/third-party/MAGPIE-LICENSE.txt) |
| **[CC Switch](https://github.com/farion1231/cc-switch)** · Jason Young 及贡献者 | CLI 配置切换、官方账号与第三方供应商配置分离、配置恢复与失败处理方式参考 | [LICENSE](https://github.com/farion1231/cc-switch/blob/main/LICENSE)：**MIT**，Copyright (c) 2025 Jason Young | 功能与配置行为参考，不作为本程序的运行时依赖 |
| **[Our Free Model](https://github.com/Ebony-Vinyl/dsh-our-free-model)** · Ebony-Vinyl 及贡献者 | WorkBuddy、TRAE 等上游协议和图片内容传递方式的参考与交叉核对 | [`e490a98dc01c0fda3dfdaa3fa23ad49bf0acb7ec` 的 LICENSE](https://github.com/Ebony-Vinyl/dsh-our-free-model/blob/e490a98dc01c0fda3dfdaa3fa23ad49bf0acb7ec/LICENSE)：**MIT** | 参考项目。其 vendored channel-pack 有独立版权声明；分发该组件时应保留组件自带的 LICENSE/NOTICE，不能只保留仓库顶层署名 |
| **[models.dev](https://github.com/anomalyco/models.dev)** · models.dev 贡献者 | 用户主动刷新时取得公开模型能力与价格目录，供本地能力补全和费用估算 | [LICENSE](https://github.com/anomalyco/models.dev/blob/dev/LICENSE)：**MIT**，Copyright (c) 2025 models.dev | 运行时取得并缓存目录数据；不将其视作服务商账单。许可原文见 [MODELS-DEV-LICENSE.txt](docs/third-party/MODELS-DEV-LICENSE.txt) |

Cockpit Tools 的具体对照入口另见 [ChatGPT 登录适配说明](docs/CHATGPT_LOGIN.md)。Magpie 的参考包括 `internal/agent/` 下的客户端适配与 `internal/gateway/vision.go`；本项目对应 `client_connect.rs`、`model_caps.rs`、`gateway_vision.rs`、`gateway_dispatch.rs`、`gateway_cost.rs` 和网关界面文件。

**Cockpit Tools 不是 MIT 项目。** 其所用 [CC BY-NC-SA 4.0](https://creativecommons.org/licenses/by-nc-sa/4.0/) 包含署名、非商业及相同方式共享条件。借鉴功能不等于复制受保护表达；若后续引入其代码、文案、图标等材料，应逐项核对权限、保留署名并注明修改，不能因为重新编写部分代码就断言无需遵守原许可。商业使用该材料应核对上游商业授权要求。本声明不对整个仓库历史作“从未复制任何代码”的保证。

## 随程序包含的组件

### CPython 嵌入式运行时

便携程序内含官方 CPython Windows x64 嵌入式归档，用于本地用量采集、会话索引与签到/额度后端，首次使用会在数据根解压。归档内的 `LICENSE.txt` 包含 Python Software Foundation 许可及所附第三方库声明，分发和解压时必须保留。版本、来源、文件摘要见 [src-tauri/vendor/README.md](src-tauri/vendor/README.md)。不能把“用户无需安装 Python”表述成“程序不包含第三方运行时”。

### 内置 agenthub 与 Python 加密库

签到与额度直读由 [shen1950/agent-memory-suite](https://github.com/shen1950/agent-memory-suite) 的 `agenthub.py` 提供，**MIT**，Copyright (c) 2026 shen1950；源码随程序保存在 `src-tauri/vendor/agenthub.py` 与 `console.html`，许可原文见 [AGENTHUB-LICENSE.txt](docs/third-party/AGENTHUB-LICENSE.txt)。用户自己安装的 `agenthub` 只作为内置运行时损坏时的回退，不是运行必需。

内置运行时另外包含以下 Python 包，打包在 `src-tauri/vendor/agenthub-libs.zip`，首次使用时解压到数据根的 `usage-runtime/`：

| 组件 | 版本 | 许可 | 用途 |
|---|---|---|---|
| `cryptography` | 49.0.0 | Apache-2.0 OR BSD-3-Clause | AES-GCM / 客户端凭据解密 |
| `cffi` | 2.1.0 | MIT | cryptography 的 C 扩展接口 |
| `pycparser` | 3.0 | BSD-3-Clause | cffi 的 C 声明解析 |

许可原文分别见 [CRYPTOGRAPHY-LICENSE.txt](docs/third-party/CRYPTOGRAPHY-LICENSE.txt)、[CFFI-LICENSE.txt](docs/third-party/CFFI-LICENSE.txt)、[PYCPARSER-LICENSE.txt](docs/third-party/PYCPARSER-LICENSE.txt)。

### AgentFind 会话索引服务

会话看板的本地索引服务源码位于 `src-tauri/scripts/agentfind.py`，编译进可执行文件，首次使用时释放到驾驶舱数据目录，并以同一便携包内的 CPython 运行时启动；无需另行安装 AgentFind 或系统 Python。该文件为本项目自有代码，以 [MIT License](LICENSE) 提供，© 2026 shen1950；同一份源码也在 [shen1950/agent-memory-suite](https://github.com/shen1950/agent-memory-suite) 以 MIT 公开。索引数据默认保存在 `~/.agentfind`，只读扫描本机会话记录，不修改原始文件。

### Rust 与桌面依赖

锁定清单见 `src-tauri/Cargo.lock`，前端开发工具清单见 `package-lock.json`。Cargo.lock 包含可选依赖及其他平台的包，**锁定条目数不等于最终 EXE 实际包含的包数**。原生前端不在发布时打包整个 `node_modules`。

| 主要依赖 | 许可表达式 | 用途 |
|---|---|---|
| `tauri` / `tauri-build` / Tauri 官方插件 | Apache-2.0 OR MIT | 桌面框架、托盘、通知与单实例等 |
| `serde` / `serde_json` | MIT OR Apache-2.0 | 序列化 |
| `toml` | MIT OR Apache-2.0 | 配置处理 |
| `sysinfo` | MIT | 进程与系统信息 |
| `ureq` / `tiny_http` | MIT OR Apache-2.0 | HTTP 客户端与本地服务 |
| `zip` | MIT | 备份打包 |
| `image` / `qrcode` | MIT OR Apache-2.0 | 图像与二维码 |
| `webview2-com` | MIT | WebView2 接口 |
| `windows` | MIT OR Apache-2.0 | Win32 接口 |

可运行 `python tools/license_report.py --out LICENSES.md` 生成**精确锁定版本**的许可索引；工具只读取本地缓存，不联网。不在缓存中的版本标为 `UNKNOWN`，使用 `license-file` 的包标为 `LICENSE-FILE`，都需进一步核对；不会拿其他版本的许可替代。该索引不等同于完整许可文本集合，也不是完整的软件物料清单。

发布者还需保留实际分发依赖的 LICENSE、COPYING、NOTICE 及必要的源码提供信息。部分 HTML 解析依赖采用 MPL-2.0，具体义务应以对应版本原文为准；本项目 MIT 不覆盖这些文件。Rust 标准库、MSVC 与 WebView2 的发布方式同样应以实际构建产物和各自许可核对。

## 单独安装的可选组件

| 组件 | 集成方式与许可边界 |
|---|---|
| **[zcode-api](https://github.com/TriDefender/zcode-api)** | ZCode Start Plan 可选桥接依赖，固定提交 `37155a5f75f3f2d7bde1b83054c8d6eb2e23a935`；[原 LICENSE](https://github.com/TriDefender/zcode-api/blob/37155a5f75f3f2d7bde1b83054c8d6eb2e23a935/LICENSE) 为 **CC BY-NC-SA 4.0**。由安装脚本另行取得并保留其源码与许可，不是 MIT 组件；不要把含该组件的安装目录、缓存或组合包整体标成“MIT、可任意商用”。来源与摘要见 [ZCode 说明](docs/zcode-start-plan.md) |
| **WebView2 Runtime** | 使用系统已安装的 Microsoft WebView2；安装器如包含下载或引导步骤，需同时遵守对应 Microsoft 条款 |
| **被管理的 Agent 客户端** | 由用户独立安装并登录。本项目能够读取档案和修改接入配置，不代表取得重分发其程序、图标或商标的许可 |

## 发布时保留的材料

源码快照及二进制发布应附上本项目 LICENSE、本声明、已引入材料的原版权/许可文本，以及实际分发依赖的必要 NOTICE。可选依赖是否进入安装包必须按最终文件核对，不能只根据“单独进程”判断许可关系。新增移植时记录来源 URL、固定版本、本地对应文件、修改范围及所保留声明。

隐私清理可以替换开发者个人路径、测试身份和秘密数据，**不应删除上游作者的必要版权署名**。公开仓库账号及原许可证中的权利人信息有明确归属用途；如何清理其他发布资料见 [发布隐私检查](docs/PRIVACY_AUDIT.md)。

产品名和商标归各自权利人所有。引用、兼容、署名和致谢均不表示本项目是上述项目的官方版本，或获得其维护者、厂商的赞助与认可。
