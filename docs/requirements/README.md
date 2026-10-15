# JchTools 需求目录索引

本目录是自有项目 JchTools 的唯一产品需求权威位置；开发、测试、执行权限及协作纪律由根 [AGENTS.md](../../AGENTS.md) 维护。按当前任务的功能边界读取对应文档，并补读适用的全局约束、依赖及交叉引用；全项目初始化按功能域逐步覆盖，不要求普通任务预先通读全部文档。

| 文档 | 唯一维护范围 |
|---|---|
| [CONTRACT.md](CONTRACT.md) | 全局优先级、定位与范围（H / P）、共用安全（S）、规则面板（R）、界面（U）、7-Zip 与发布（E），以及共用名称、扫描、规则范围和边界期望附录 B～E |
| [RECURSIVE-EXTRACT.md](RECURSIVE-EXTRACT.md) | 递归解压（X）：独立启动、包及分卷、成功源包清理、失败隔离、防护与取消 |
| [DIRECTORY-ORGANIZER.md](DIRECTORY-ORGANIZER.md) | 目录整理（C）：分析与计划、去重、一级归类、命名冲突、清理、幂等，以及普通文件大类映射附录 A |
| [MD-TOOLS.md](MD-TOOLS.md) | MD 整理（M）：Markdown 合并和无损硬限制拆分；不负责其他格式转换 |
| [GIT-TOOLS.md](GIT-TOOLS.md) | Git 工具（G）：逐文件提交推送、staged 保护、合并、重试、停止及可观察状态 |
| [ALL2MARKDOWN.md](ALL2MARKDOWN.md) | 转 Markdown（T）：随包文档与图片转换、音频转写可选资产（FFmpeg 与音频模型）按需下载下的既有媒体转录、输入保护、输出及功能验收矩阵；共享资产交付与选版引用 XB 分区 |
| [SNAP2TEXT.md](SNAP2TEXT.md) | 截图 OCR（O）：随包截图能力、独立 OCR 模型与确定性布局规格、截图交互、结果窗、复制、隐私、jchocr 图片 OCR 命令及功能验收矩阵；共享资产交付引用 XB 分区 |
| [XBERG-INFERENCE.md](XBERG-INFERENCE.md) | 共享推理（XB）：唯一 Xberg 与共享目录、场景及模型隔离、来源与选版（XB-10）、随包运行资产与音频转写可选资产（FFmpeg 与音频模型）下载边界（XB-27）、统一设置、持久配置及截图/Xberg 后台生命周期 |
| [ACP-MODEL-SERVICE.md](ACP-MODEL-SERVICE.md) | 模型服务（AH）：本机 HTTP、官方 ACP 客户端、外部 Agent、独立配置与后台、发现及真实 Agent 验收矩阵 |
| [CODEAGENT.md](CODEAGENT.md) | CodeAgent（CA）：CurrentUser DPAPI 解密、本地注册表 Token 自动维护、持久间隔与独立后台状态 |

各功能域同时遵守 CONTRACT.md 中适用的全局条目；优先级仍按该文档，不因拆分而改变。共享 Xberg 不等于工具互相自动调用，ACP 模型服务不是截图/Xberg 的后台。跨域关系仅引用相应权威条目，不复制另一域的需求。

本索引登记职责与源码入口定位，不维护需求副本、实现进度或通过记录。需求定义与验收条件留在对应文档；实现、失败、未验证范围及执行证据留在开发/验证说明中。2026-10-10 按独立功能域从 CONTRACT.md 迁出 X / C / M / G / AH，既有需求编号、行为及验收矩阵不变；该文档拆分没有新增、删除或重编号产品需求。

## 源码与工程入口定位

本节只给出源码定位，不修改上表各域的行为条目或验收条件，不代表实现或验证通过。P-14 / P-15 的 Rust 独立模块、缓存复用与打包逐步骤计时要求仍唯一见 CONTRACT.md；执行授权、命令和证据口径仍见根 AGENTS.md。

- 根 `src/main.rs` 为主 EXE 入口，GUI 装配与工具注册表在 `crates/gui/src/`，根 `ui/app.slint` 由独立 `crates/ui/build.rs` 生成界面代码。
- 公共日志、性能打点、应用 SQLite 配置在 `crates/foundation/src/`；文件配置、数据库 schema、共享任务与扫描在 `crates/file-core/src/`。递归解压、目录整理、MD 整理、Git 工具分别在 `crates/recursive-extract/`、`crates/directory-organizer/`、`crates/md-tools/`、`crates/git-tools/`。
- 转 Markdown、共享推理、资产管理分别在 `crates/markdown/`、`crates/xberg/`、`crates/assets/`；ACP 模型服务与 CodeAgent 分别在 `crates/acp-api/`、`crates/codeagent/`。截图服务仍在 `optional/snap-ocr-worker/`，通过真实 foundation / xberg 依赖共享实现；`optional/snap-ocr-core/` 的规格参照物职责不变。
- jchocr 为 `crates/jchocr/` 下独立 package，源码及集成测试随该 package；根 `tests/` 继续承接主 EXE 的跨域与 GUI 集成测试。根及全部 14 个生产 crate 是默认成员，两个 optional 包仍用额外显式验收入口。
- Windows 打包入口仍为 `scripts/package-windows.ps1`；引擎嵌入生成在 `crates/file-core/build.rs`，截图清单嵌入生成在 `crates/assets/build.rs`，根 `build.rs` 仅承接 Windows RC / manifest。打包逐步骤墙钟记录保存在独占 `.tmp/package-windows-<标识>/timings.json`，与发布包及测试门共享预算记录分离；缓存是否命中以真实 Cargo fresh/dirty 证据判定，不以路径迁移推定提速。
