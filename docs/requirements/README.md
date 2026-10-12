# JchTools 需求目录索引

本目录是自有项目 JchTools 的唯一产品需求权威位置；开发、测试、执行权限及协作纪律由根 [AGENTS.md](../../AGENTS.md) 维护。按当前任务的功能边界读取对应文档，并补读适用的全局约束、依赖及交叉引用；全项目初始化按功能域逐步覆盖，不要求普通任务预先通读全部文档。

| 文档 | 唯一维护范围 |
|---|---|
| [CONTRACT.md](CONTRACT.md) | 全局优先级、定位与范围（H / P）、共用安全（S）、规则面板（R）、界面（U）、7-Zip 与发布（E），以及共用名称、扫描、规则范围和边界期望附录 B～E |
| [RECURSIVE-EXTRACT.md](RECURSIVE-EXTRACT.md) | 递归解压（X）：独立启动、包及分卷、成功源包清理、失败隔离、防护与取消 |
| [DIRECTORY-ORGANIZER.md](DIRECTORY-ORGANIZER.md) | 目录整理（C）：分析与计划、去重、一级归类、命名冲突、清理、幂等，以及普通文件大类映射附录 A |
| [MD-TOOLS.md](MD-TOOLS.md) | MD 整理（M）：Markdown 合并和无损硬限制拆分；不负责其他格式转换 |
| [GIT-TOOLS.md](GIT-TOOLS.md) | Git 工具（G）：逐文件提交推送、staged 保护、合并、重试、停止及可观察状态 |
| [ALL2MARKDOWN.md](ALL2MARKDOWN.md) | 转 Markdown（T）：文档与图片转换、媒体转录、输入保护、输出及功能验收矩阵 |
| [SNAP2TEXT.md](SNAP2TEXT.md) | 截图 OCR（O）：截图交互、模型与确定性布局规格、结果窗、复制、隐私及功能验收矩阵 |
| [XBERG-INFERENCE.md](XBERG-INFERENCE.md) | 共享推理（XB）：唯一 Xberg、场景隔离、获取与版本、统一设置、持久配置及截图/Xberg 后台生命周期 |
| [ACP-MODEL-SERVICE.md](ACP-MODEL-SERVICE.md) | 模型服务（AH）：本机 HTTP、官方 ACP 客户端、外部 Agent、独立配置与后台、发现及真实 Agent 验收矩阵 |

各功能域同时遵守 CONTRACT.md 中适用的全局条目；优先级仍按该文档，不因拆分而改变。共享 Xberg 不等于工具互相自动调用，ACP 模型服务不是截图/Xberg 的后台。跨域关系仅引用相应权威条目，不复制另一域的需求。

本索引只登记职责，不维护需求副本、实现进度或通过记录。需求定义与验收条件留在对应文档；实现、失败、未验证范围及执行证据留在开发/验证说明中。2026-10-10 按独立功能域从 CONTRACT.md 迁出 X / C / M / G / AH，既有需求编号、行为及验收矩阵不变；本次没有新增、删除或重编号产品需求。
