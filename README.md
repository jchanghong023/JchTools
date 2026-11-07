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
| [系统压力记录](openspec/specs/system-pressure-logging/spec.md) | 手动启停，每 2 秒记录 CPU、内存和每块物理磁盘的压力；关闭 GUI 后继续运行，单文件保留最近 20 分钟。 |

这些工具独立使用，不会自动串联。截图和文档/图片转换所需的基础运行时随发布包提供；Git 工具需要系统 PATH 中的 Git for Windows。

## 使用与安全

安装后启动 JchTools；便携版解压后运行 `JchTools.exe`。命令行 OCR 用 `jchocr --help` 查看输入与输出：安装版将命令放到当前用户已有 PATH 的 WindowsApps 目录，便携版运行同目录的 `jchocr.exe`。

界面采用统一的深浅色工作台：左侧搜索与工具导航，底部固定「设置」「关于」，外观仍在「关于」切换。转 Markdown 页集中显示输入/输出目录及转换选项，任务操作固定在页底；运行日志可「放大」阅读并「还原」。本次界面调整不改变工具功能、默认参数或执行流程。

安装升级会先自动关闭目标安装目录中的旧主窗口、截图与推理进程，以及本次将更新的 WindowsApps OCR CLI；正在进行的任务会被中断。CodeAgent、定时任务和系统压力记录后台通过新包的内部维护入口正常停止，确认全部目标进程退出后才替换文件；退出失败则停止安装，不允许忽略后混用新旧文件。不会仅凭进程名关闭其他目录中的便携副本，不删除用户配置、凭据或压力记录。此流程仅用于安装升级，不改变日常关窗后后台继续运行的行为。

**截图并复制图片**

在「截图 OCR」页设置「截图并复制图片」快捷键和「最大图片大小」，点击「保存图片复制设置」。初始复制快捷键为 `Ctrl+Alt+C`，上限为 **1000 KB**；`1 KB = 1000 字节`，保存后立即用于后续截图，重启后保留。若快捷键被占用，按页面提示换一个组合；原 OCR 默认快捷键 `Ctrl+Alt+O` 不变。

后台确认复制快捷键实际注册成功后才显示可用；快捷键冲突与 OCR 组件错误不会互相遮蔽。保存等待后台最终确认时保留草稿并暂停编辑，失败后保留输入供重试，不把接收请求当作保存成功。

日常使用：**按复制快捷键 → 冻结画面上框选并松开左键 → 到浏览器按 Ctrl+V**。框选后自动后台压缩并复制，无需保存文件或再点击按钮；Esc / 右键取消框选，图片处理进度窗支持取消。只有 PNG 达标且剪贴板写入成功才提示「已复制」。

剪贴板同时提供最终压缩后的 `PNG` 与同一图像的标准 `CF_DIB` 位图，兼容浏览器及只接受普通位图的桌面软件；不是文件路径或文件复制。标准位图内存可能大于 PNG，上限与成功提示仍只按最终 PNG 编码字节计算。

取消在图片发布前生效；PNG 已交给系统剪贴板后不撤回图片，也不再误报「已取消」。

图片处理以速度优先：先尝试原尺寸快速 RGB PNG，超限后仅补一次原色索引 PNG，避免不超过 256 种原色的文字图无谓缩小。仍超限时按实际 PNG 字节数与大小上限估算新尺寸，预留一定体积余量，直接从原图进行 Triangle 等比缩放，不再按 95%、90% 等逐档搜索，也不再并行计算三档减色。最多 3 次预算估算，必要时继续对半缩小直至 1×1，每边最少 1 像素；总计最多 36 档尺寸、37 次编码，仅在最终 1×1 尺寸使用高等级编码以兼顾极小有效 PNG。允许文字变糊，不保证最高颜色精度、最大分辨率或最小文件体积；候选达标即返回，不继续搜索更清晰的结果。上限连最小尺寸有效 PNG 都无法容纳时明确报错，不输出超限图片；捕获、编码或剪贴板失败仍明确反馈。捕获、处理和 PNG 剪贴板发布均在内存完成，不生成临时截图文件、不上传、不记录截图历史，图片复制不依赖 OCR 模型就绪。

上限严格约束本工具生成的 PNG 编码字节数，不是图片分辨率或剪贴板总内存。浏览器和目标网页可能重新编码，**不保证最终上传图片始终低于设置上限**；生成 PNG、粘贴事件收到的图片和实际上传图片的大小须分别验证。

**先使用副本或测试目录。**解压完整成功后会永久删除源压缩包及实际分卷；目录整理也可能永久删除文件，不经回收站，不提供备份或回滚。文件冲突不覆盖已有文件；Git 项目树受到保护，目录整理可按规则整体归类。处理期间外部程序修改文件不在保证范围内。详细边界见[文件任务安全](openwiki/concepts/file-task-safety.md)及[全局规格](openspec/specs/application-contract/spec.md)。

截图和转换在本地处理；媒体转录需用户主动下载可选资产，Git 网络操作由用户发起。**音频可选资产及其发布信息的 HTTPS 下载从首次请求起跳过服务器证书校验，不能保证服务器身份**；文件大小与摘要校验仍保留。此策略不适用于 Git 或 jchocr，具体网络边界见 [P-03/P-09](openspec/specs/application-contract/spec.md)。

定时任务需求允许用户命令联网；调度服务自身不因此联网或上传日志。

**系统压力记录**

在「系统压力记录」页点击「开始记录」，优先输出到 `D:\JchTools-performance.csv`；D 盘不存在时使用系统临时目录下的同名文件，页面显示实际路径。仅保留最近 20 分钟的数据，不分卷；关闭窗口继续记录，重新打开后可停止，停止后保留文件。不会开机自启，不自动提权或因写入失败切换目录。

文件被占用（Windows 错误 32/33）时，后台仍每 2 秒采样，并按原采样周期重试写入，不设重试次数或总时长上限；界面显示异常、实际路径及自动重试说明。文件释放后自动补写最近 20 分钟内存窗口中的有效样本，并恢复正常状态；超出窗口的样本不再保留。占用期间也可手动停止。权限不足、磁盘故障等非共享 I/O 错误仍停止记录并明确报错。

## 从源码构建

开发环境需要 Rust 1.98.1 或更新版本的 `x86_64-pc-windows-msvc` 工具链、Visual Studio C++ Build Tools 和 Windows SDK。Windows PowerShell 中：

```powershell
cargo run --bin JchTools
cargo build
```

构建机可运行 `powershell -NoProfile -File .\scripts\fetch-7zip.ps1` 获取并校验官方 7-Zip 引擎；构建时引擎会压缩内嵌，运行时释放并校验。缺少引擎仍可构建，但运行期无可用引擎时解压会报错。打包入口为 `scripts/package-windows.ps1`；资产准备、测试及安装包要求见[构建与交付](openwiki/operations/build-and-delivery.md)，执行权限见 [AGENTS.md](AGENTS.md#34-三级测试门与执行权限)。

打包默认使用 Release，且不启用专用性能日志。加 `-Debug` 同时选择 Debug 构建并开启 `perf-tracing`：

```powershell
.\scripts\package-windows.ps1 -SkipTests -Debug
```

`-Debug` 复用 PowerShell 公共开关，同时控制主程序、jchocr 和截图后台的调试构建，并启用主程序与截图后台的性能埋点及共用性能日志层，写入状态目录 `perf-logs/<本次进程标识>/`。构建模式和 perf 状态均记录到安装包和 ZIP 内的 `BUILD-INFO.json`。不传 `-Debug`（或传 `-Debug:$false`）保持默认 Release、关闭 perf。Debug 会改变运行性能，不能代表 Release 耗时；该开关不重编译或改变随包 Xberg 引擎自身的性能开关。

Debug 的业务与生成 UI 代码仍采用默认未优化配置，并保留调试信息、断言及 perf 开关；仅对热点依赖使用 `opt-level = 3`：`image` / `png` / `fdeflate` 用于图像编码和缩放，`fontique` / `parley` / `skrifa` / `swash` 用于字体发现、文字排版及字形栅格化，`sha2` 用于组件完整性校验。不更换字体、渲染器或编码参数，不改变 PNG 大小上限及 Release 配置；组件就绪仍逐次完整计算 SHA-256，不用缓存或跳过校验换速度。启用此配置后需重新构建，已有安装包不会自动变化。无头软件绘制的对照结果不代表生产 winit/femtovg 首帧验收。

截图后台位于 `optional/snap-ocr-worker/`，Win32 接口统一使用 `windows-sys` 官方绑定；屏幕捕获仍采用 GDI `BitBlt + CAPTUREBLT`，不手工维护系统函数声明或结构体布局。

### 启动与截图性能打点

源码运行主程序用 `cargo run --bin JchTools --features perf-tracing`；单独构建截图后台用 `cargo build --manifest-path optional/snap-ocr-worker/Cargo.toml --features perf-tracing`。worker 的本地特性同时启用阶段打点和 foundation 日志层，只启用依赖日志层不会编译 worker 阶段打点。

| 链路 | 记录范围 |
|---|---|
| 主程序启动 | main 入口、状态目录与日志初始化、窗口/后端创建、初始状态、页面数据、回调和事件泵装配、显示/居中、首次绘制；设置加载/迁移/应用及后台服务、组件就绪检查分别计时 |
| 截图 OCR | `screenshot_ocr_task`：有效请求、隐藏旧结果、托盘排队、GDI 捕获、冻结框选/裁剪、等待模型/推理排队、PNG/Base64、共享推理请求、结果窗创建及首帧；模型加载/预热与正文复制另有阶段 |
| 截图并复制图片 | `screenshot_copy_task`：共用捕获/框选、后台排队、RGB 转换、原尺寸/索引 PNG、有限缩放与编码、PNG/CF_DIB 内存准备、剪贴板打开重试/发布、托盘成功通知 |

`gui_startup_finished` 的 `elapsed_ms` 从入口起算，`endpoint=first_after_rendering` 表示 Slint 首次绘制回调，不等于物理显示呈现；后端不支持回调时明确标记 `event_loop_ready`。异步初始化独立记录完成结果，不把首帧就绪当作全部后台检查完成，也不把整个程序驻留时间算作启动。

截图根 span 带 `operation_id`、`generation`；`screenshot_task_done` 记录成功/失败/取消、阶段和逻辑终态总耗时。OCR 首帧与复制通知另有 `screenshot_result_presentation_done` / `screenshot_copy_notification_done`，根引用保留到对应展示边界；不支持首帧观察时只标记 `unobserved/show_returned`，不冒报已绘制。排队与框选等待单列为 idle；`time.busy` 是 enter 区间墙钟，不是 CPU 时间，包含区间内的阻塞等待。日志只含耗时、尺寸、字节数、次数和去敏结果分类，不记录图像、OCR 正文或用户路径；未启用特性时新增打点不参与编译。

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
- [系统压力记录](openspec/specs/system-pressure-logging/spec.md)

架构、工作流及开发入口见 [OpenWiki 导航](openwiki/quickstart.md)；协作和验证纪律见 [AGENTS.md](AGENTS.md)，诊断日志见 [LOGGING.md](LOGGING.md)，第三方许可见 [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md)。Wiki 是辅助导航；需求、源码和实际验证结果分别以规格、源码和当次证据为准。
