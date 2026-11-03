# JchTools

JchTools 是 Windows 本地工具箱，使用 Rust、Slint、SQLite 和 7-Zip。下载可直接使用的安装包、便携 ZIP 和校验和：[Releases](https://github.com/jchanghong023/JchTools/releases)。

## 功能

| 工具 | 用途 |
|---|---|
| [递归解压](openwiki/workflows/recursive-extraction.md) | 逐包解压并处理嵌套包；完整成功后删除源包及实际分卷，无法解开的包尝试隔离，取消按安全边界停止。 |
| [目录整理](openwiki/workflows/directory-organization.md) | 先只读分析和确认计划，再按规则去重、归类及清理。 |
| [MD 整理](openwiki/workflows/markdown-organization.md) | 合并或按大小拆分 Markdown，保留原文件。 |
| [转 Markdown](openwiki/workflows/markdown-conversion.md) | 转换文档、图片及媒体；文档/图片使用随包基础组件，媒体转录需先主动下载音频可选资产。 |
| [Git 工具](openwiki/workflows/git-file-publishing.md) | 调用系统 Git，逐文件提交和推送；保留原有 staged 内容，冲突时停止。 |
| [截图 OCR](openwiki/workflows/screenshot-and-cli-ocr.md) | 使用快捷键或托盘截图识别；独立 `jchocr.exe` 提供命令行图片 OCR。 |
| [CodeAgent](openwiki/workflows/codeagent-maintenance.md) | 在本地维护当前用户 Token；GUI 关闭后独立后台继续运行。 |
| [定时任务（已批准需求，待实现）](openspec/specs/scheduled-tasks/spec.md) | 计划支持关窗后后台按分钟或小时周期执行用户命令，并查看、复制或导出执行日志。 |

这些工具独立使用，不会自动串联。截图和文档/图片转换所需的基础运行时随发布包提供；Git 工具需要系统 PATH 中的 Git for Windows。

## 使用与安全

安装后启动 JchTools；便携版解压后运行 `JchTools.exe`。命令行 OCR 用 `jchocr --help` 查看输入与输出：安装版将命令放到当前用户已有 PATH 的 WindowsApps 目录，便携版运行同目录的 `jchocr.exe`。

**先使用副本或测试目录。**解压完整成功后会永久删除源压缩包及实际分卷；目录整理也可能永久删除文件，不经回收站，不提供备份或回滚。文件冲突不覆盖已有文件；Git 项目树受到保护，目录整理可按规则整体归类。处理期间外部程序修改文件不在保证范围内。详细边界见[文件任务安全](openwiki/concepts/file-task-safety.md)及[全局规格](openspec/specs/application-contract/spec.md)。

截图和转换在本地处理；媒体转录需用户主动下载可选资产，Git 网络操作由用户发起。**音频可选资产及其发布信息的 HTTPS 下载从首次请求起跳过服务器证书校验，不能保证服务器身份**；文件大小与摘要校验仍保留。此策略不适用于 Git 或 jchocr，具体网络边界见 [P-03/P-09](openspec/specs/application-contract/spec.md)。

定时任务需求允许用户命令联网；调度服务自身不因此联网或上传日志。

## 从源码构建

开发环境需要 Rust 1.98.1 或更新版本的 `x86_64-pc-windows-msvc` 工具链、Visual Studio C++ Build Tools 和 Windows SDK。Windows PowerShell 中：

```powershell
cargo run --bin JchTools
cargo build
```

构建机可运行 `powershell -NoProfile -File .\scripts\fetch-7zip.ps1` 获取并校验官方 7-Zip 引擎；构建时引擎会压缩内嵌，运行时释放并校验。缺少引擎仍可构建，但运行期无可用引擎时解压会报错。打包入口为 `scripts/package-windows.ps1`；资产准备、测试及安装包要求见[构建与交付](openwiki/operations/build-and-delivery.md)，执行权限见 [AGENTS.md](AGENTS.md#34-三级测试门与执行权限)。

## 验证

本地三级门分别是 `fastcheck`（静态检查及编译）、`fulltest`（离线影响范围功能验证）和 `slowtest`（功能及本地发布打包验证）。后两者每次均须当次明确授权；真实桌面、真实模型及发布另需授权。CI 只编译、打包或发布，不运行功能测试或质量门。

影响范围通过不等于完整验收，未执行或缺环境的项目不是通过。测试命令、完整验收矩阵和授权规则见 [AGENTS.md](AGENTS.md#33-验收方法怎么测试怎么验收)及 [OpenWiki 验证指南](openwiki/testing/verification.md)；非抢占用户桌面的无头交互方式见 [e2e.md](e2e.md)。

## 需求与工程入口

[`openspec/specs/`](openspec/specs/) 是唯一已批准需求基线；提案位于 `openspec/changes/`，不因存在代码或测试结果自动批准。按功能域阅读并同时核对全局约束：

- [应用全局与共用约束](openspec/specs/application-contract/spec.md)
- [递归解压](openspec/specs/recursive-extraction/spec.md) · [目录整理](openspec/specs/directory-organization/spec.md)
- [Markdown 整理](openspec/specs/markdown-organization/spec.md) · [转 Markdown](openspec/specs/markdown-conversion/spec.md)
- [Git 逐文件提交推送](openspec/specs/git-file-publishing/spec.md)
- [截图 OCR 与 jchocr](openspec/specs/screenshot-ocr/spec.md) · [共享 Xberg](openspec/specs/shared-inference/spec.md)
- [CodeAgent Token 维护](openspec/specs/codeagent-token-maintenance/spec.md)
- [定时任务](openspec/specs/scheduled-tasks/spec.md)（已批准需求，尚未验收）

架构、工作流及开发入口见 [OpenWiki 导航](openwiki/quickstart.md)；协作和验证纪律见 [AGENTS.md](AGENTS.md)，诊断日志见 [LOGGING.md](LOGGING.md)，第三方许可见 [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md)。Wiki 是辅助导航；需求、源码和实际验证结果分别以规格、源码和当次证据为准。
