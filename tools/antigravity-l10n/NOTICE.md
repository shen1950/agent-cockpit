# NOTICE — tools/antigravity-l10n

本目录内的 `localization_engine.js` 与 `dicts/` 来自第三方开源项目，随驾驶舱分发，用于
「Antigravity → 界面汉化」面板的本地汉化检测与一键安装。

- 上游项目：<https://github.com/qqxpee/antigravity2-cn>
- 取用日期：2026-11-05（对应上游 main 分支，声明匹配 Antigravity v2.27.0+）
- 取用文件：`localization_engine.js`、`dicts/*.json`（简体），未做任何修改
- 未取用：`showimg/`（展示截图）、`dicts_tw/`（繁体词典）、`scripts/`（上游开发工具）
- 许可：上游仓库**未附带 LICENSE 文件**，版权归上游作者所有。本目录仅随驾驶舱本地
  使用，不构成再分发授权；如需对外分发驾驶舱安装包，请先与上游作者确认许可。
- 运行时依赖：Node.js（引擎内部还会经 npx 拉取 asar 打包工具，首次需要联网）
- 引擎行为：安装时会强制关闭正在运行的 Antigravity，修改其 `resources/app.asar`
  （先备份为 app.asar.bak）；Antigravity 官方更新会覆盖汉化，届时在本面板重新安装即可。
