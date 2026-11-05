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
| [截图 OCR](openwiki/workflows/screenshot-and-cli-ocr.md) | 快捷键或托盘截图识别；另有按 PNG 大小上限压缩并复制图片的快捷键；独立 `jchocr.exe` 提供命令行图片 OCR。 |
| [CodeAgent](openwiki/workflows/codeagent-maintenance.md) | 在本地维护当前用户 Token；GUI 关闭后独立后台继续运行。 |
| [定时任务](openspec/specs/scheduled-tasks/spec.md) | 关闭 GUI 后由独立后台按分钟或小时周期执行用户命令，并查看、复制或导出执行日志。 |

这些工具独立使用，不会自动串联。截图和文档/图片转换所需的基础运行时随发布包提供；Git 工具需要系统 PATH 中的 Git for Windows。

## 使用与安全

安装后启动 JchTools；便携版解压后运行 `JchTools.exe`。命令行 OCR 用 `jchocr --help` 查看输入与输出：安装版将命令放到当前用户已有 PATH 的 WindowsApps 目录，便携版运行同目录的 `jchocr.exe`。

**截图并复制图片**

在「截图 OCR」页设置「截图并复制图片」快捷键和「最大图片大小」，点击「保存图片复制设置」。初始复制快捷键为 `Ctrl+Alt+C`，上限为 **1000 KB**；`1 KB = 1000 字节`，保存后立即用于后续截图，重启后保留。若快捷键被占用，按页面提示换一个组合；原 OCR 默认快捷键 `Ctrl+Alt+O` 不变。

后台确认复制快捷键实际注册成功后才显示可用；快捷键冲突与 OCR 组件错误不会互相遮蔽。保存等待后台最终确认时保留草稿并暂停编辑，失败后保留输入供重试，不把接收请求当作保存成功。

日常使用：**按复制快捷键 → 冻结画面上框选并松开左键 → 到浏览器按 Ctrl+V**。框选后自动后台压缩并复制，无需保存文件或再点击按钮；Esc / 右键取消框选，图片处理进度窗支持取消。只有 PNG 达标且剪贴板写入成功才提示「已复制」。

取消在图片发布前生效；PNG 已交给系统剪贴板后不撤回图片，也不再误报「已取消」。

处理优先使用原尺寸、原颜色的高等级无损 PNG；超限后依次有限减色，再尝试 90%～50% 等比缩图，各尺寸重新优先完整颜色。最多编码 24 次，每边不低于原边长与 64 像素中的较小值；达到底线仍超限时提示提高上限或缩小选区，不输出超限图片。捕获、处理和 PNG 剪贴板发布均在内存完成，不生成临时截图文件、不上传、不记录截图历史，图片复制不依赖 OCR 模型就绪。

上限严格约束本工具生成的 PNG 编码字节数，不是图片分辨率或剪贴板总内存。浏览器和目标网页可能重新编码，**不保证最终上传图片始终低于设置上限**；生成 PNG、粘贴事件收到的图片和实际上传图片的大小须分别验证。

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

截图后台位于 `optional/snap-ocr-worker/`，Win32 接口统一使用 `windows-sys` 官方绑定；屏幕捕获仍采用 GDI `BitBlt + CAPTUREBLT`，不手工维护系统函数声明或结构体布局。

## 验证

本地三级门分别是 `fastcheck`（静态检查及编译）、`fulltest`（离线影响范围功能验证）和 `slowtest`（功能及本地发布打包验证）。后两者每次均须当次明确授权；真实桌面、真实模型及发布另需授权。CI 只编译、打包或发布，不运行功能测试或质量门。

影响范围通过不等于完整验收，未执行或缺环境的项目不是通过。测试命令、完整验收矩阵和授权规则见 [AGENTS.md](AGENTS.md#33-验收方法怎么测试怎么验收)及 [OpenWiki 验证指南](openwiki/testing/verification.md)；非抢占用户桌面的无头交互方式见 [e2e.md](e2e.md)。

截图后台的全目标静态检查使用 `cargo clippy --offline --manifest-path optional/snap-ocr-worker/Cargo.toml --all-targets -- -D warnings`；加上 `--features test-hooks` 可检查隔离测试特性下的代码。两种命令均不运行测试，不能代替功能验收。

## 需求与工程入口

[`openspec/specs/`](openspec/specs/) 是唯一已批准需求基线；提案位于 `openspec/changes/`，不因存在代码或测试结果自动批准。按功能域阅读并同时核对全局约束：

- [应用全局与共用约束](openspec/specs/application-contract/spec.md)
- [递归解压](openspec/specs/recursive-extraction/spec.md) · [目录整理](openspec/specs/directory-organization/spec.md)
- [Markdown 整理](openspec/specs/markdown-organization/spec.md) · [转 Markdown](openspec/specs/markdown-conversion/spec.md)
- [Git 逐文件提交推送](openspec/specs/git-file-publishing/spec.md)
- [截图 OCR 与 jchocr](openspec/specs/screenshot-ocr/spec.md) · [共享 Xberg](openspec/specs/shared-inference/spec.md)
- [CodeAgent Token 维护](openspec/specs/codeagent-token-maintenance/spec.md)
- [定时任务](openspec/specs/scheduled-tasks/spec.md)

架构、工作流及开发入口见 [OpenWiki 导航](openwiki/quickstart.md)；协作和验证纪律见 [AGENTS.md](AGENTS.md)，诊断日志见 [LOGGING.md](LOGGING.md)，第三方许可见 [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md)。Wiki 是辅助导航；需求、源码和实际验证结果分别以规格、源码和当次证据为准。
