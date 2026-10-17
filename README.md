# JchTools

仅支持 Windows 的 Rust + Slint 本地工具箱。当前通过工具注册表接入「递归解压」「目录整理」「MD 整理」「转 Markdown」「Git 工具」「截图 OCR」「模型服务」「CodeAgent」八个工具。截图与文档/图片转换的基础资产随包提供，处理完全离线；媒体转录先主动下载音频可选资产。模型服务仅监听本机，用户配置的外部 ACP Agent 自行承接模型调用；[CodeAgent](docs/requirements/CODEAGENT.md) 仅维护当前用户本地 Token，不联网。联网边界见需求合同 P-03、T-21、AH 分区与 [截图 OCR 需求](docs/requirements/SNAP2TEXT.md)。

> **验证以对应提交的结果为准**：本地 `fulltest` 覆盖静态与 Python 质量检查、Rust 格式与 Clippy、编译、所有适用的 Windows 本地单元/集成/doc/e2e 测试、真实 7-Zip 用例及本地发布打包；真实桌面 driver 保留独立并如实标注未运行，不以门通过代替其验收。功能验收以 Windows 本地结果为准；CI 仅编译、打包和发布，不重复运行完整测试。安装包、便携 ZIP 与发布记录见 [Releases](https://github.com/jchanghong023/JchTools/releases)。没有引擎时构建不内嵌，运行期解压会给出明确错误；运行 `scripts/fetch-7zip.ps1` 获取官方引擎后，构建会自动把它压缩内嵌进 EXE。发布包由 `scripts/package-windows.ps1` 生成，只附带许可证与上游源码，不含引擎可执行文件。

三级门不触发 CI、远程流水线或发布：`python scripts/test_gate.py slowtest --authorized` 只执行本地适用覆盖，不能用来触发或等待 `check.yml` / `release.yml`。CI 仍独立编译、打包、发布；独立触发流水线须按当次指令执行，默认报告本次 run 链接及未完结 / 未验证状态，只有用户明确要求时等待最终结果，不借同提交历史 run 冒报成功。真实发布仍须单独授权。

测试代码采用宽松静态配置：纯测试、夹具和替身不要求严格格式、Clippy 风格规则或 Python lint/类型/Bandit/Vulture；仍检查全部 Python 语法，编译 Rust 全部测试目标并按原范围执行测试，断言与隔离要求不变。生产库、测试运行器、GUI 验收和部署脚本仍严格检查。角色边界由 `scripts/quality_scope.py`、`pyproject.toml` 与测试专属 Rust 属性维护；本地入口仍为 `python scripts/test_gate.py fulltest --authorized`，不会自动运行独立桌面或真实模型验收。

静态门按分号终止的完整 Slint `enabled` 绑定检查确认框门禁，允许表达式跨行；不因换行误判已存在的 `confirm-kind` 条件，也不借后续控件的条件放行当前控件。转 Markdown 验收使用诊断日志的进程启动标识/分段文件名与本轮新文件内的 `application_started` 结构事件证明隔离启动，不接受向旧文件补写启动行；受控备用日志目录和保留锁是正常状态，未知文件不能冒充日志或掩盖组件下载。部分提取诊断只以对应成员标识之后的内容核对失败阶段，宿主文件名或另一成员的失败不能抵账。DOCX 夹具统一经 `defusedxml` 的解析和序列化入口处理，XML 节点在拥有正文树的局部作用域内推导类型，不为类型导入豁免安全检查。

目录整理遵守现有合同的占用和范围边界：固定容器的占用文件或 Git 项目原位保留；旧 `.jchtools-link-*` 普通项照常扫描，不凭名称回收历史资料。非递归收尾仅清理本轮实际新建的目标目录，不删除未纳入的原有子目录。Windows 盘根不被额外拒绝，系统目录子树仍排除；合法 Unicode 空白由独立名称规则控制。ACP 初始化失败同样保持自有 Agent 的自然退役和停止等待，不自动强杀。解压的 Git 整树保护和未完整落盘保留原包规则不变。

诊断日志共享 API、级别配置、容量与历史归属，以及单文件补日志边界见 [日志约定包](LOGGING.md)。该包的 `ready` 表示基础设施静态机制就绪，不代替完整功能或发布验收。

业务日志与性能数据同样按真实进程启动隔离，不只是测试门：诊断文件含 PID 与唯一启动标识；性能数据写入 `perf-logs/<启动标识>/`，Xberg 每次引擎启动也有独占子目录。同日重启或 PID 复用不追加旧文件；原有诊断容量/留存/去敏策略、性能默认关闭与统计口径保持不变，具体规则唯一见 P-10。

## 7-Zip 引擎（内置）

- 官方完整引擎 `7z.exe` + `7z.dll` 在构建时以 zlib 压缩编进主 EXE，不需要最终用户另装 7-Zip。主包另外提供截图后台 worker 与合规材料；7-Zip 引擎不作为独立可执行文件随包交付，运行时按 E-02 释放并校验。
- 首次需要解压时，引擎被释放到 `%LOCALAPPDATA%\JchTools\data\engine\<版本-哈希>\`，逐文件符合发布清单的 SHA-256 后才使用；已有文件不被覆盖。解析顺序固定为 `<exe 目录>\resources\7zip` → 已释放的内嵌副本 → 从 EXE 释放。缺失或摘要不符的候选不执行；无效文件占用释放位置时使用新的自有位置，不删除占用文件。可替换候选仍须符合发布清单，不能仅放入任意同名程序。
- 引擎释放临时文件采用随机 UUID 路径并独占创建；失败仅清理本次实际创建的文件，不按文件名称扫描或清理历史残留。
- 发布包仍随附 `resources/7zip/` 下的许可证、NOTICE 与上游源码压缩包（LGPL/BSD/unRAR 要求），但**不再随包放引擎可执行文件**——`scripts/package-windows.ps1` 会在打包时校验引擎哈希并拒绝把 `7z.exe`/`7z.dll` 混进发布目录。
- 构建机获取官方引擎：`powershell -NoProfile -File .\scripts\fetch-7zip.ps1`（固定官方 release、校验上游 SHA-256、只接受官方下载域名）。没有引擎时构建仍能成功，只是不内嵌（运行期会明确报错）。
- 代价与边界：首次使用会把引擎写盘再执行，个别杀软可能对「释放并执行可执行文件」敏感；引擎文件本身按官方原样分发，未做任何修改。

## Windows 构建与发布

构建机需要 Rust stable 的 `x86_64-pc-windows-msvc` 工具链、Visual Studio C++ Build Tools 和 Windows SDK；这些是开发依赖，**不是最终用户的安装要求**。

在本仓库根目录运行：

```powershell
powershell -NoProfile -File .\scripts\package-windows.ps1
```

脚本从官方 `ip7z/7zip` 的固定 `26.03` 发布获取完整 x64 MSI 和对应源码，要求 GitHub 发布元数据提供 SHA-256 摘要；缺少摘要、校验失败或资产缺失时停止，不换非官方镜像。MSI 只在构建机生成 administrative image，提取完整 `7z.exe + 7z.dll`，不把安装步骤交给最终用户。

构建装配通过 `scripts/prepare-xberg-runtime.ps1` 解析 Xberg 最新正式发布，仅获取并校验 document、snapshot 两个功能包及清单，合并为交付目录的 `xberg/`，写入锁定版本和摘要的 `jchtools-xberg-release.json`。构建缓存复用须重新校验；离线构建要求已有锁定收据及完整资产，不把离线复用冒称已核验最新发布。构建过程不下载音频附加包，运行和截图不查询最新版本。

当前登记的 Xberg 发布为 `v2026.10.17-060616-local.841f1fe8d8`。截图模型按 [O-07](docs/requirements/SNAP2TEXT.md) 保持 PP-OCRv6 medium FP32 检测、原 TextSnap small FP32 识别与原有序字典的确定字节，文档 tiny 不变；截图采用新版的 0.7 识别分数下限、分层插值、超宽裁剪分段、辅助细线检测、行首标点扩展重试与逐间隙空行推进，具体约束见 O-23～O-28 及附录 B。资产清单、运行时固定版本及格式清单来源同步更新，打包仍逐成员校验大小与 SHA-256，不因升级放宽模型身份检查；本次替换不代表安装包或完整功能验收已通过。

Inno Setup 编译通过 `.tmp/` 下独占的短路径 junction 读取已校验交付树，避免 Hugging Face 模型路径超过 MAX_PATH；编译成功或失败都只移除链接，不改写源资产或其目录布局。

随后在缺少时生成 Cargo.lock，执行 `cargo check`、workspace 测试和真实引擎解压测试；先以 `cargo build --locked --release --target x86_64-pc-windows-msvc --manifest-path optional/snap-ocr-worker/Cargo.toml --bin snap-ocr-worker` 构建**独立的截图 OCR 后台 worker**，把原始 EXE 放到 `dist/optional-components-v0.1.2/snap-ocr-worker.exe`，按实际字节数及 SHA-256 回填清单。在 Cargo 返回的 worker EXE 所在 release 目录中，以最终清单完整 SHA-256 命名 `snap-ocr-assets-<SHA256>.json`；相同字节不重写，变更输出目录不改变这个构建输入。脚本只在主包构建期间通过 `JCHTOOLS_SNAP_OCR_MANIFEST` 让 `crates/assets/build.rs` 内嵌这份稳定清单，结束或异常时恢复调用者环境，不改仓库内的占位清单；optional 暂存目录和发布目录 `resources/snap-ocr-assets.json` 交付相同字节。

主程序与独立 CLI 使用 `cargo build --locked --release --target x86_64-pc-windows-msvc -p jchtools -p jchocr --bins`；保留已有构建缓存、profile、flags、features 与目标配置，不执行 `cargo clean`。随后校验引擎及清单，复制许可证和 7-Zip 源码（发布包不含 7z.exe/dll），生成 `dist/JchTools-Windows-x64-时间戳.zip`；构建机装有 Inno Setup 6 时还会生成 `dist/JchTools-Setup-x64.exe`（当前用户安装、免 UAC、桌面及开始菜单入口、控制面板卸载；缺 ISCC 时安装包阶段如实标为 NOT_RUN，仍生成 ZIP）。**ZIP 和安装包均包含 `jchocr.exe`、`snap-ocr-worker.exe`、资产清单及完整 `xberg/` 基础运行目录，两套 OCR 模型、字典、共享 DLL 与许可随包提供，截图字体内置于 worker；默认不携带 FFmpeg、SenseVoice 或 VAD 音频资产。随包文件缺失时应重新解包或安装同一发布包，不用初始化下载代替修复。** `BUILD-INFO.json` 只记录实际执行范围，不声称手工 UI、多 TB 性能或当前源码实测提速已通过。

打包只使用本次 Cargo JSON `compiler-artifact` 中按 manifest 与 bin 唯一匹配的实际可执行文件：GUI 属于根 `Cargo.toml`，CLI 属于 `crates/jchocr/Cargo.toml`。内嵌引擎状态绑定 `crates/file-core/Cargo.toml` 的真实 `jchtools_file_core` 库 artifact 的 `package_id`，从对应 `build-script-executed` 的 `OUT_DIR` 读取；不把 GUI 的 OUT_DIR 当引擎来源，不按时间戳挑缓存，缺失、歧义或未完整内嵌时拒绝打包。第三方许可收集仅按 `metadata.workspace_members` 的 package ID 排除本工程自有 crate，非 workspace 的 vendor path 补丁仍收集；根 `LICENSE` 继续交付。按 [vendor/SOURCES.json](vendor/SOURCES.json) 的 `license_and_notice_materials` 保留相对路径（含嵌套目录）和实际字节，登记材料缺失时停止。本地源修补及理由以该清单的 `project_source_modifications` 为准；上游摘要标识原发布归档，不代表修补后的本地树与上游逐字节相同。

`-Offline` 要求已备齐并校验 7-Zip 等外部交付资产；Git 不作为打包依赖或随包资产。Rust 依赖源码固定在仓库 `vendor/`，不需要本机 crates.io 源码缓存。`-SkipTests` 在构建记录明确标为 NOT RUN；CI 编译、打包和发布使用此开关，功能验收复用已通过且源码未变的 Windows 本地证据，不能以 CI 成功替代本地测试。脚本不修改系统执行策略，不自动安装编译器，不自动提升权限。
安装包与便携 ZIP 均携带 `BUILD-INFO.json`：安装包内记录 ISCC 编译前的共同测试结果与未验证项，安装包编译阶段自身如实标为未完结；编译完成后，便携 ZIP 中的记录再更新为该阶段的实际结果。文件存在不表示真实桌面或完整发布验收已通过。

脚本通过 `python scripts/make_tmp.py workspace` 创建独占 `.tmp/package-windows-<随机ID>/timings.jsonl`，启动及终局输出记录的绝对路径。日志以无 BOM UTF-8 JSONL 格式单次打开、仅追加写入，并自动 Flush；每一完整行是一份包含全部阶段与 total 的快照，不反复替换同一文件。读取时逐行解析，以最后一条完整 JSON 行取得各 step 最终状态/墙钟秒数；进程或存储异常留下的不完整末行不能当作结果。记录 setup、assets、font-and-engine-validation、lockfile、tests、worker-build、manifest、main-build、staging、licenses、package-validation、installer、zip 和 total；阶段转换追加，正常异常路径追加失败状态，未执行项为 NOT_RUN（不冒记 PASS）。存储故障时先前完整快照保留可读，报告日志写入故障并继续传播原打包异常，不把旧 RUNNING 快照冒称终局成功。该文件仅供开发观测，不进安装包/ZIP；Cargo 阶段是整条命令墙钟，并非纯编译时间，不改变 `GateTimingFile` 或质量门共享预算会计。首次 crate 拆分会产生必要重编；缓存命中、提速与实际耗时须由对应源码的真实 Cargo 和完整打包记录证明。


最终用户解压构建出的 ZIP，运行 `JchTools.exe`；**不用另装 7-Zip，7-Zip 引擎也不依赖 PATH**；Git 工具使用系统已安装且可经 PATH 解析的 Git for Windows。图形驱动异常时可运行同目录的 `launch-software.cmd` 请求软件渲染；软件渲染及中文效果同样需要真实 Windows 验证。

正式发布走 GitHub Actions 的 **Release** 工作流（`.github/workflows/release.yml`）：**仅手动触发**（Actions 页 Run workflow），在 `windows-2022` 上执行同一套 `package-windows.ps1`，以 UTC 时间到分钟的 tag（`yyyyMMdd-HHmm`）创建 Release，并把便携 ZIP、安装包 `JchTools-Setup-x64.exe` 与 SHA256SUMS 校验和挂到该 Release。最终用户直接从仓库 **Releases** 页下载即可。

截图后台、字体、共享 Xberg、两套独立 OCR 模型及文档/截图运行依赖随主安装包和便携 ZIP 交付，安装或解包后自动定位，无需先打开设置或下载基础组件。只有 FFmpeg、SenseVoice INT8、Silero VAD 等音频转写可选资产由用户在「设置」页主动下载，严格匹配包内收据的精确版本，不替换已有引擎、模型或共享 DLL；支持进度、取消、失败后重试。「检查已安装组件」只离线检查，基础文件缺失时指引修复安装或重新解包。不再提供三个功能勾选或手填 Xberg 路径。Git 不随包分发，仅调用 Windows 系统已安装且可经 PATH 解析的 Git for Windows，沿用其现有配置、身份、认证、代理、hooks、签名、合并策略及继承环境。没有可用 Git 时仅 Git 工具提示，不影响其他功能。普通启动、打开功能页、截图或转换不会自动下载或更新；发布物与真实模型的验收以对应提交的验证结果为准。

组件下载及其 GitHub 发布查询从第一次 HTTPS 请求起直接忽略服务端证书的信任链、域名和有效期校验，进度会明确提示；不再先执行严格证书校验或额外重复一轮请求。仍优先使用 Windows 系统手动代理（支持内网服务器域名或 IP），代理传输失败后直连重试一次。TLS 加密、握手签名及文件大小/摘要校验保留，但不再保障服务器身份；不修改 Windows 全局证书、Git 或 jchocr 的网络策略。详细边界见 [P-09](docs/requirements/CONTRACT.md)；跳过证书不能解决 HTTP 403 访问限制或 407 代理认证失败。
升级时保留已有 Xberg 配置和旧路径原件，不删除或搬动用户原有引擎目录；兼容迁移不恢复手工路径设置入口。

## 使用流程

八个工具各自独立，导航切换：

- **递归解压**：选目录 → 调整扫描范围与防护上限 → 点「开始解压」→ 一次确认 → 连续执行到结束。每个包就地解压，只有整包解码、校验及全部成员落盘成功后才永久删除原包及实际分卷；不去重、不删除已有文件或普通解压结果。目标重名时只给新文件的基础名加唯一编号，保留普通及复合扩展名。未能完全解开的包整组移入根目录的「解压失败」并记录原因；空间不足或取消则停止并保留当前源包，不作为坏包隔离。同一次任务不重复处理已经处理的包，新任务允许重新处理仍在范围内的包；固定整树跳过「解压失败」目录。
- **目录整理**：选目录 → 调整规则 → 点「开始分析」（**只读**，不改任何文件）→ 查看真实文件产生的分页计划 → 确认后执行去重、归类与清理。文件动作可逐项勾选/取消，状态立即更新；空目录清理是无独立开关的固定收尾步骤，任务取消后停止后续删除，清理失败不报告整理完成。
- **MD 整理**：只读处理 Markdown。「合并 MD」按文件创建时间从早到晚（平局按相对路径自然排序）合并目录中的 .md，每个文件以「# 文件名.md」为一级标题、内部标题整体下移一级（六级封顶），fenced code block 内容原样保留；「拆分 MD」按 KB/MB 硬限制在 UTF-8 安全边界分片，分片按编号顺序拼接与原文件完全一致。原文件一律不改动；输出冲突先确认、不静默覆盖。底部主操作保留完整按钮文案，长路径或状态仅在自身区域省略，不挤压操作按钮。
- **转 Markdown**：直接在工具页选择输入、输出目录和转换类型；文档/图片转换使用随包基础运行时，不需要先下载，准备期间的提示也不要求下载这些组件；故障原因见任务详情。媒体转录先在独立「设置」页主动下载音频可选资产。各功能共用应用管理的 Xberg 运行时；文档及图片支持格式以该运行时的 `formats` 能力为准，PDF、DOCX、PPTX、XLSX 是必需类型，媒体仍固定 MP4/M4A。转换完全离线，缺少相应资产或必需能力时明确提示。下载与就绪检查的详细约定见 [共享 Xberg 需求](docs/requirements/XBERG-INFERENCE.md)。
- **Git 工具**：逐文件提交并推送——一个文件单独 add → 单独 commit（只含当前文件，保护既有 staged 内容）→ 单独 push，push 成功后才处理下一个；提交信息固定「update: 相对路径」。push 被远端新提交拒绝时自动 fetch + merge（不 rebase），冲突即停并保留现场；可重试失败按 5→10→20→40→80→80… 秒无限退避，可随时停止；以 git 状态为唯一进度来源，重启不重复提交。
- **截图 OCR**：页面仅设置快捷键并显示后台状态；通过快捷键或托盘截图。打开 GUI 后，自动定位随包 Xberg 并异步启动后台服务，无需下载截图模型；GUI 关闭后服务、模型和托盘继续常驻，重新打开连接同一服务。需要完全退出时使用系统托盘「退出后台服务」，正在处理的任务先确认并安全停止。就绪检查和故障恢复统一位于「设置」页；开机启动默认关闭。
- **模型服务**：保存 ACP Agent 启动程序、参数和端口 → 异步启动或连接独立后台 → HTTP 调用 Models / Chat Completions；关闭工具箱不停止服务，配置应用与主动退出使用页面上的独立入口，具体协议及限制见下方「ACP 模型服务」。
- **CodeAgent**：打开页面启动或连接独立后台，默认每 60 秒读取当前用户 `.cac` 凭据并用 CurrentUser DPAPI 解密，仅在 Token 变化时更新当前用户注册表。页面可保存正整数秒检查间隔，显示后台状态、最近检查时间与去敏结果，不显示 Token；读取或解密失败保留已有 Token。关闭 GUI 后后台继续运行，不属于截图/Xberg 托盘或模型服务页的退出范围，具体行为见 [CodeAgent 需求](docs/requirements/CODEAGENT.md)。

导航只显示已注册工具（递归解压、目录整理、MD 整理、转 Markdown、Git 工具、截图 OCR、模型服务、CodeAgent）以及独立设置、关于；工具描述集中注册。「关于」包含 Slint 的 AboutSlint 署名组件。旧工具不提供独立任务记录页或设置页：规则仍只在主界面「处理规则」里会话内调整，不落盘、不导入导出；有效共享运行目录由应用自动发现和持久保存，旧源安全迁移按共享推理需求执行；截图快捷键与开机启动设置、CodeAgent 检查间隔也持久保存。

转 Markdown 的单文件超时只接受正整数秒数；非法值在字段旁提示并禁用开始，不自动钳制。用户取消与处理超时分别呈现，停止只取消本次请求，不终止共享 Xberg 或影响另一场景。Xberg 下载的本地发布记录保留实际归档来源、版本、大小与摘要；下载取消在最终落位前检查，不覆盖已有有效资产。

当前媒体链路的旧范围**仍未恢复**：[JchTools 启动配置](crates/xberg/src/xberg_runtime.rs) 仅启用转录，继承 Xberg 的 **512 MiB 输入文件大小、30 分钟解码音频时长**上限。后者按解码音频样本检查，不是所有视频容器的画面时长上限（无音轨容器不会触发该音频时长检查），也不是默认 6 小时的单文件处理墙钟超时。[`convert_media`](crates/markdown/src/markdown.rs) 已发送媒体路径，但上游 `crates/xberg-cli/src/commands/worker.rs::transcribe_media` 仍先 `std::fs::read` 整个文件，再调用 `transcribe_bytes_cancellable`；该字节 API 的 `stage_input` 又写出临时媒体副本，之后才进入内部按路径解码流水线。因此上层传路径不等于端到端流式读取。安全恢复的前提是上游公开可取消的 path 转录 API、worker 实际改用该 API，并发布可确认此能力的运行时，再由本仓库接入并验证跨越旧限制的真实行为；**不能仅把 `max_bytes` / `max_duration_ms` 设为 `null` 冒充修复**，否则全文件内存路径仍可能耗尽内存并影响截图。此处记录实现缺口，不削弱或替代 [T-19/T-23/T-29 及附录 D](docs/requirements/ALL2MARKDOWN.md) 的原需求与验收要求。

共享代理的内部协议现为 v3：原请求登记并排入引擎队列后返回登记确认，客户端收到确认才发送取消，避免取消先到而丢失。升级时若旧版本后台仍常驻，应先通过旧托盘退出全部后台服务，再重新打开新版本；协议不匹配会明确拒绝，不静默退回旧取消行为，也不启动第二个引擎。

截图服务将用户取消、超时、通信失败与引擎退出分别处理。关闭结果窗口释放正文及其窗口引用；新截图期间仅在内存暂存旧结果，取消、无文字或失败时恢复。热键修改必须等待注册及必要回滚的明确结果，不能将尚未确认的组合显示为已生效。

托盘退出会等待已提交任务与响应安全结束，但不会因尚未提交完整请求的连接留下共享代理进程。通知区使用与主程序一致的渐变 J 品牌图标，图像随后台内置，按系统小图标尺寸加载并保留透明圆角；首次注册或 Explorer 重建时遇到瞬态失败会有限重试，不修改 Windows 的通知区/折叠区显示设置。截图与 Xberg 后台的退出范围按 XB-23；独立模型服务仍按 AH-10 从其页面退出，不混同为截图后台残留。

## jchocr 命令行图片 OCR

安装包和便携 ZIP 均提供 `jchocr.exe`，供程序或 AI Agent 直接调用，不打开 GUI、不增加 MCP。
命令经共享代理调用与截图相同的 `ocr_snapshot` 接口，复用截图模型和布局。
安装或解包后自动发现随包截图运行时，无需先打开 GUI、下载模型或设置 Xberg/ONNX Runtime 环境变量；安装到 WindowsApps 的命令通过安装位置登记定位同一运行目录。
安装包免 UAC，将 `jchocr.exe` 复制到当前用户已有 PATH 的 `%LOCALAPPDATA%\Microsoft\WindowsApps` 目录，不修改用户或系统 PATH；安装完成即可直接运行 `jchocr`，无需重启终端。
便携 ZIP 只随附程序，不自动复制到 PATH 目录。
输入方式、输出格式和错误约定以 `jchocr --help`（或 `jchocr help`）为准；帮助不依赖 Xberg。

## 窗口与外观

- 自绘标题栏：不使用系统标题栏（`no-frame`），顶部 32px 条带可拖动窗口，右上角是最小化 / 最大化-还原 / 关闭三个自绘按钮；双击条带切换最大化。边缘仍可拖拽缩放，最大化、还原、任务栏与快捷键行为都不变。
- 启动居中：按整块屏幕计算对称留白（上下相等、左右相等），再夹进工作区，避免压住任务栏。
- 默认尺寸 1120x720（最小 960x620），偏小不占满屏幕。
- 配色：自绘控件使用 `ui/app.slint` 的 `Design` 统一设计令牌（浅色主色 `#2563EB`、深色主色 `#4C8DFF`），标准控件沿用 Fluent；采用中性页面底色、圆角卡片与克制阴影。主题默认深色，可在「关于」页切换浅色或跟随系统，自绘配色与控件一起切换。
- 密度：默认窗口 1120x720 下“处理规则”可同屏显示 9 行规则（行高 44px，标题 + 单行说明 + 右侧控件）；小窗口通过滚动浏览，悬停说明可查看完整文字，不撑高规则行。窗口顶部仍是【目录栏 + 面板页签】两行紧凑工具条。
- 侧栏导航带小图标（文件夹 / 信息圈），用几何图形绘制，不依赖字体或图片资源；选中项以弱强调底色和左侧色条标识，悬停与按压使用短过渡；侧栏中段留空，预留给后续工具。
- 输入框：侧栏搜索与各工具路径输入共用 `Field`，前置图标上下居中，文字输入区域保持输入框全高。
- 两组切换用两种样式：面板切换（处理规则 / 整理计划 / 进度与日志）是下划线页签，规则分区（解压 / 去重 / 归类 / 清理 / 安全与性能）是实心胶囊，不会混淆。解压不提供冲突策略开关或交互询问；主题在「关于」页。
- 解压基础页用紧凑说明卡呈现完整成功后原包及实际分卷永久删除、已有文件保留、冲突改名与失败隔离规则，并提示高级防护选项的位置，不改变原有处置流程。失败列表为空时，标题与提示紧凑相邻，明确区分尚无任务与当前任务无失败项。
- 进度：执行阶段显示百分比 + 进度条 + “已完成 / 总数 项”计数，状态栏同时显示“执行中：已处理 N / M 项”；分析阶段总量无法提前预知，显示往返扫动的光带与实时计数（已扫描文件数、读取量、平均速度）。空闲时进度条隐藏。
- 日志：最新的消息显示在最上面，界面只保留最近 300 条；面板隐藏时只缓冲，打开后批量刷新，避免重复拼接不可见的日志文本。完整逐文件记录仍在本次任务数据库里，不提供日志或报告导出。
- 提示分两级：复制成功这类消息走蓝色提示条，配置不成立或操作失败走红色错误条，两者都可单独关闭。展开诊断时，短提示按内容收缩，长正文限制阅读区高度并可滚动查看，不以大块空白隔开正文与恢复操作。
- 计划行显示中文状态（待执行 / 已执行 / 已跳过 / 已取消勾选 / 执行失败）；用户主动取消任务库状态为「已取消」，不写成失败。
- 计划翻页按钮只在确实还有上/下一页时可用；切换类型筛选会回到第一页并同步复位。
- 文件对话框有明确默认位置：选择目录时从已输入的目录开始。
- 任务运行中点击关闭窗口会先弹出「停止任务并关闭」确认（不需要勾选永久删除确认项），关闭确认位于最上层；取消任务用蓝色提示条反馈，不当作错误。
- 应用图标：`resources/app-icon.png`（256px，运行时窗口与任务栏图标）与 `resources/app.ico`（16/24/32/48/64/128/256 多尺寸，由 build.rs 用 Windows SDK 的 rc.exe 嵌入 EXE，资源管理器与图钉快捷键都能显示）。图案为靛蓝→青色渐变圆角方块 + 白色 J；重新生成用 `python scripts/make-icon.py`。侧栏徽标的蓝色与字色随主题调整。

## 已实现的功能代码

| 范围 | 行为 |
|---|---|
| 扫描 | 默认递归并包含隐藏/系统属性文件，排除模式默认留空；缩小范围时明确提示；不跟随符号链接、junction、reparse point；直接含 `.git` 文件或目录（不区分大小写）的目录整树保护，所选根或任一祖先直接含 `.git` 时拒绝任务；目录整理仅按 C-14 整体归类范围内的子级 Git 项目，不进入树内处理 |
| 解压 | 仅处理 X-01 白名单：ZIP、7Z、RAR、TAR、GZ、BZ2、XZ、ZST、LZMA、Z、TGZ、TBZ2、TXZ、TZST 及约定的 TAR 复合压缩流；CAB、ISO、WIM 等白名单外容器完全不碰；继续处理范围内新解出的嵌套包 |
| 解压冲突 | 固定保留已有文件，新文件基础名追加唯一编号；不比较内容、不覆盖、不提供策略切换 |
| 原压缩包 | 整包完整成功后永久删除原包及实际分卷；未能完全解开的包整组无覆盖移入「解压失败」；卷集按 X-10 精确命名族识别，缺号或缺入口仍作为一组失败包；展开比例按实际输入卷集总大小计算 |
| 数字分卷 | 从 `.001` 起，编号最少三位，超过 `.999` 后自然增长为 `.1000`、`.1001`……；扫描、成功删源、失败隔离及冲突改名均覆盖完整卷族，非规范补零与备份文件不并入；完整规则见 [X-10](docs/requirements/RECURSIVE-EXTRACT.md) |
| 真正去重 | 名称与大小筛选候选后计算完整 BLAKE3，同一文件在一次整理中最多完整哈希一次；执行时复用分析结果，不重新读取内容 |
| 去重分类 | 同名、`(1)/(2)/Copy/副本`、不同名内容相同三类独立启停；保留修改时间最新、最旧或最短名称 |
| 归类 | 固定「大类」一级结构；大类按扩展名映射（附录 A），未匹配进入「其他」大类，不产生任何第二级分类目录；可单独归类超大文件 |
| 命名与目录 | 按开启的规则清理普通文件副本后缀、Unicode NFC 和连续空白；仅实际目标冲突时逐级加入来源目录及稳定路径摘要；不移动有内容目录来提升层级 |
| 清理 | 强制清理扫描范围内的空目录及空目录链（保留根目录和 Git 保护树）；垃圾、临时/备份、零字节文件分别有开关与删除方式；真实格式检测及可选修正扩展名 |
| 删除 | 全局默认与各类覆盖；保留/永久删除。删除一律为永久删除、不经回收站、不可由本软件恢复（S-02）；取消在安全边界停止后续删除，已完成删除如实计入状态与统计，不回滚 |
| 硬链接保护 | 不新建硬链接；可靠文件标识证明多个路径为同一物理文件时，不作为独立副本执行去重删除 |
| 操作 | 暂停、取消；真实进度与有限 GUI 日志；计划分页、单项取消；主界面会话内调整规则 |

“最新”明确指文件修改时间，不表示内容语义上的最新版本；时间相同用路径长度、路径字典序稳定决胜。同名但内容不同的文件不提供版本取舍或自动淘汰——去重只删除完整哈希一致的内容。

## ACP 模型服务

侧栏「模型服务」将用户配置的 ACP Agent 接到本机 OpenAI 风格 HTTP 接口。HTTP 使用 Axum 0.8.9，ACP 使用官方 Rust SDK 3.2.0；它们编译进 JchTools，不需要另装 Node/Python HTTP 后端。Agent 由用户准备，模型认证与其自身工具由 Agent 承接，需求边界见 [ACP 模型服务需求](docs/requirements/ACP-MODEL-SERVICE.md)。

- 启动程序和参数分栏保存：程序栏填写可执行文件路径，不包引号；参数栏按 Windows CRT 引号规则处理空格与中文，仅空格和制表符分隔参数，换行保留为参数内容，末尾未闭合引号收纳剩余字符。本次 OpenCode 验收启动命令为 `opencode acp`：程序栏填写真实 `opencode.exe` 路径，参数栏只填 `acp`，不添加未经其公开能力支持的模型、推理或 yolo 参数。配置保存在应用状态目录的 `config.sqlite3`，默认端口 8765。
- 打开工具箱后按有效配置启动或连接独立后台；关闭或终止 GUI 不退出服务。修改并保存配置不会打断旧请求；「应用并重启」等待已接收请求完成后生效。退出模型服务使用独立确认，Stop 可抢占正在排空的 Apply；退出请求失败且后台仍存活时，可再次确认退出，禁止自动重启的意图仍保留。资源回收完成后，即使收尾报错也退出后台和控制管道。退出后迟到的 Apply 不会重新拉起服务，同一次 GUI 会话也不自动重启。
- `GET http://127.0.0.1:<端口>/v1/models` 返回 Agent 协商出的真实可选模型 ID；每个模型包含整数 `created: 0`，表示 ACP 未提供创建时间，不伪造实际时间。探测会话在首次消费前收到配置更新时，同步公开目录。将返回的 ID 用于 `POST /v1/chat/completions`，实际选择仍按目标会话当前选项核验；各会话的 Model 配置 ID 独立登记，不假定全连接相同，不将 new 响应和随后通知当成原子事件。没有可选择模型能力时明确报错，不提供假别名。
- `messages` 支持 `system`、`developer`、`user`、`assistant` 文本及 text 内容块；`stream: true` 返回实时 SSE。每个 `POST /v1/chat/completions` 须在顶层提供 `cwd`，其值必须是非空、绝对且已存在的目录；缺失或非法时返回 400。服务将目录规范化后用于业务会话，并向 ACP 传递普通 Windows 盘符/UNC 路径（不泄漏 `\\?\` 前缀）。调用方不执行工具；`tools`、`tool_choice`、媒体和未实现的能力明确拒绝。未映射的非 null `verbosity`、`web_search_options` 等选项返回 400，不静默丢弃调用方设置。
- 默认每次请求新建会话并提交完整历史；续会话将响应的 `X-JchTools-Session-ID` 放回请求头，并提交包含上一轮实际 assistant 输出的完整历史。前缀不一致或同一会话更换 `cwd` 时拒绝请求；会话绑定其规范化后的 `cwd`。同会话串行，不同会话并发。prompt 发布前取消且配置与回调安全收尾成功时保留会话和历史，已排队的后续轮次仍可执行；prompt 发布后取消仍使该会话失效，后续请求须新建会话。最多 4 个执行请求和 16 个等待请求，容量满返回 429。
- HTTP 断连只取消本轮；Agent 异常不重放任务。Windows 连接通过每 100ms 的只读 `SIO_TCP_INFO` v0 内核 TCP 状态查询观察对端关闭（该 API 要求 Windows 10 1703 或更新版本），不消费、复制管线请求字节，不替换 Mio 的事件注册；查询失败不是断连证据，会记录告警，不能据此保证缓冲管线下的关闭观察。服务只监听 `127.0.0.1`，端口占用不会自动换端口。Agent 进程初始工作目录及模型探测会话仍使用应用状态目录的 `acp-workspace`；业务 ACP `newSession` 使用调用方请求中的规范化 `cwd`。
- 同一 Windows 用户、同一登录会话中的 OMP 通过控制管道的只读 `Discover` 自动取得实际就绪地址，再读取真实模型列表；不猜端口、不启动服务、不读启动参数或凭据。OMP 模型选择器显示 `jchtools/<真实 ID>` 和“后端 Agent 执行”，支持文本流式任务、取消与明确错误，不运行前端工具或自动重放任务；发起请求时自动从当前 `SessionManager` 读取并发送任务目录 `cwd`。服务端启动的 ACP Agent 子进程固定带 `OMP_JCHTOOLS_DISCOVERY=0`，防止后端 OMP 再发现本服务；普通 OMP 也可用该进程开关或 `disabledProviders` 禁用发现，不改写用户配置。这是 AH-16 既有产品保护，不因真实验收改用 OpenCode 而移除；OpenCode 验收不证明其实现 OMP 的发现或模型选择器入口，也不要求启动真实 OMP。
- Agent 文件/终端回调按请求归属回传操作与通信失败。终端请求未显式指定 `cwd` 时默认使用所属会话的 `cwd`，显式指定时使用其指定目录；`wait_for_exit` 只保证主命令退出，不等待仍持有输出管道的后代；输出查询返回当前累计内容，释放与服务退出负责自有终端资源收尾。
- 每次实际 `session/prompt` 提交记录去敏摘要：请求 ID、实际 ACP 状态复用情况、本轮新增消息数、各角色数量及原文本 UTF-8 字节数；不记录正文或工作目录。日志要求归属全局合同 P-10。

Rust 局部验证入口：`cargo test --features test-hooks --test acp_settings --test acp_http_contract --test acp_callbacks --test acp_service_process --test acp_discovery --test acp_discovery_env --test acp_prompt_summary`。官方 SDK 合成 Agent、真实 TCP、独立进程与无硬件鼠标窗口用例不代替 [模型服务第 3 节](docs/requirements/ACP-MODEL-SERVICE.md#3-验收方案) 的真实 Agent 与模型验收。唯一真实验收目标为 `acp_opencode`，目标模型、第一阶段范围、完整 AH-A01～AH-A15 矩阵及失败边界均在该节唯一维护；本文不复制验收需求或将历史授权视为本次执行权限。真实模型调用须取得当次明确指令，不包含 fulltest、slowtest 或发布；未执行项仍为 `NOT RUN`，局部验证不等于完整验收。源码与许可来源见 `vendor/SOURCES.json`；全新 Cargo 源缓存可用 `--offline --locked` 验证。


## 多 TB 目录的处理方式与边界

文件清单、候选、动作及日志保存在 SQLite；Hash 分块读取，不把整文件内容装入内存。完整 Hash 每工作线程复用约 1 MiB 读取缓冲，不做预哈希采样；线程数可配。SQLite 缓存设为约 64 MiB，排序和分组可落临时文件；GUI 每页最多 100 项，保留最近 300 条实时日志。**这些是代码设计，不是已测得的峰值内存或吞吐保证**；Slint、7-Zip 解码字典、系统缓存和 DB 索引还会占用额外内存/磁盘。

每次只解一个压缩包，固实包一遍解码；完整输出先写入包所在卷的独立自有临时工作区，整包解码、检查与校验成功后通过不覆盖的同卷 rename 合入，不再复制一份文件数据。该方案仍要求有容纳**当前整包解压结果**的空间，不是零临时空间；合入全部成功后才永久删除源包及实际分卷。这样避免对每个文件重新解码固实包，并让解码或 CRC 失败不直接污染最终文件。空间不足时保留源包并停止，不移入失败目录；默认额外预留 1 GiB，可调。

同卷移动不复制数据。普通文件确需跨卷移动时，以不覆盖方式完整复制并保留创建、修改时间，输出关闭成功后才删除源项；任一步失败保留源项并报告未完成。Git 项目只允许不遍历内部的同卷整树移动，跨卷安全失败。摘要按逻辑大小统计已永久删除项，不声称等于 NTFS 实际释放量（S-01/S-06）。

暂停 Hash 在分块边界生效；已经启动的整包解压会继续排空输出并完成该包后暂停，取消则停止子进程。关闭运行中窗口会先确认并请求取消。任务完成后再次整理相同规则不会向分类输出目录重复套娃；用户手工改动、规则切换仍可能产生新计划。

## 安全与明确限制

所有正常文件操作不自动提权、不清除只读属性；不实现处理期间外部修改的检测或写入锁。目标绝不使用有覆盖语义的 rename，重名只改新文件名。含 `.git` 文件或目录的目录在进入、读取内容、哈希及删改之前整树排除，解压产物同样遵守；不通过删改祖先目录绕过保护。自有工作区清理由本次随机 UUID 和 OWNER 标记限定，不递归删除用户原目录。

不实现自己的备份、事务回滚或自动任务重放；任务日志只供审计。进程被强杀可能留下 `.jchtools-work` 残留和已完成操作，下次不会自动清空旧工作区或恢复执行；已结束/失败任务不能直接重放，需重新分析。当前任务库格式为版本 6，不兼容的旧任务库被明确拒绝，不迁移、恢复或重新标记旧计划；旧任务须重新分析。

加密包整体移入「解压失败」（密码不写入进程命令行，没有密码输入/密码存储）；分卷包按 X-10 精确命名族归组，完整成功后删除实际卷集、失败整组隔离。「解压失败」整树固定排除于两工具的文件处理范围，但其中实际为空的目录仍参与整理的强制空目录清理（C-09/X-07）。格式修正仅显式开启时按有效结构及必要类型标识判型，不凭几个相同头字节猜测；无法可靠确认时不改名，ZIP 容器的 DOCX/EPUB 等不误改成 ZIP。路径里有 Windows 不支持的名称、无法无损表示的名称或危险条目时安全失败；归档任一危险成员使整包失败，不把跳过后的安全子集视为成功。

界面目视检查覆盖 150% DPI 下的 1120×720、960×620 两档窗口与深浅主题；功能自动化结果以对应提交的本地验证记录为准，Actions 与发布包 `BUILD-INFO.json` 记录编译和交付结果。未验证：网络盘、非 150% DPI、无 GPU 显示、RAR 多版本与多卷、大固实包、多个 TB 的真实性能。处理期间被其他程序修改文件的情形不在需求与防御范围内（P-08）；路径检查不等于对恶意并发文件系统攻击的形式化安全保证。首次运行必须使用副本或测试目录；经用户亲自验收后再决定生产使用。

## 手工测试数据集

```powershell
python scripts/make_tmp.py testdata
```

脚本默认把测试集生成到仓库内 `.tmp\testdata\`（自包含，不依赖仓库外目录；也可 `--destination` 另指专门测试目录），会**先清空目标目录**再重建一份可重复的手工测试集（规模以脚本实际输出为准，当前约 60 个文件 / 66 MiB）。

普通流程测试不加 `--git`，需要还原时重新生成测试集。`--git` 会额外建立 Git 基线并附 `恢复.ps1`，此时所选根目录直接含 `.git`，两个工具都必须拒绝处理；该模式只用于验证 Git 根目录拒绝行为，不能用于正常解压或整理验收。测试完成后仅清理本会话自建的 `.tmp\` 子目录；只有确认没有并行会话使用 `.tmp\` 时，才可用 `python scripts/make_tmp.py clean` 清空整个目录。

测试集含 16 组用例：基础文件与中文名、三类重复内容、同名版本冲突、垃圾/临时/零字节、空目录与单层链、扩展名与真实格式不符、11 种压缩格式、解压目标冲突、5 层嵌套包、18 层超深嵌套、截断损坏包、带密码包、1 MiB 分卷包、两个 32 MiB 大文件与 64 MiB 全零包、隐藏与系统属性文件、含 `../` / 绝对路径 / 符号链接的恶意包。目标目录非空时会先列出将删除的文件数量与体积，要求输入 `yes` 确认（非交互环境用 `--force`）；盘符根、用户主目录及其上/下级、系统目录与仓库内非 `.tmp` 位置会被直接拒绝。目录里的 `_测试说明.md` 由脚本生成，逐条列出每个用例的预期行为。

各工具分别测试，不会自动串联：递归解压确认完整成功后的原包/实际分卷永久删除、已有文件保留、冲突另存、嵌套产物落盘及失败隔离；重新生成干净数据集后，再测试目录整理的分析、计划勾选、执行和空目录收尾；MD 整理验证合并排序/标题下移/代码块保护/拆分无损还原与冲突确认；Git 工具用本地 bare 仓库模拟远端验证逐文件提交推送、staged 保护、退避重试、自动合并与冲突停止。转换与截图按各自需求文档及共享 Xberg 验收覆盖目标执行。隐藏/系统属性文件默认包含，缩小范围时应出现提示。

## 核心测试

平台范围按需求合同 P-07 仅支持 Windows：构建、测试、验证与 CI 流水线都只在 Windows 上运行（原 Linux/WSL 验证入口已于 2026-09-19 移除）。

```powershell
cargo test --features test-hooks   # 单元与集成测试（真实引擎用例默认 #[ignore]）
python scripts/test_gate.py fastcheck   # 静态检查 + format + compile，无测试；非编译共享预算 60s
powershell -NoProfile -File .\scripts\acceptance.ps1 -WithEngine   # 单命令验收
```
`acceptance.ps1` 默认不运行真实桌面测试。仅在当次另获真实桌面验收授权时加 `-WithDesktopTests`；该声明也适用于 `-WithGuiSmoke`、`-WithMarkdownAcceptance` 及资产就绪后的热键服务探测，开关或资产环境变量本身不创造执行授权。未执行的桌面项目保持 `NOT_RUN_SEPARATE_USER_INSTRUCTION_REQUIRED` / `NOT RUN`，不能当作通过。

三级门统一由 `scripts/test_gate.py` 编排，完整治理与授权规则见 [AGENTS.md §3.4–3.5](AGENTS.md#34-三级测试门与执行权限)：

| 门 | 适用覆盖 | 非编译共享预算 |
| --- | --- | --- |
| `fastcheck` | 静态检查 + format + compile，不运行任何测试 | 60 秒 |
| `fulltest` | fastcheck 同项 + 所有适用 Windows 本地 UT / integration / doc / e2e（含可选组件、无头 GUI 与真实引擎）+ 本地 release 打包 | 900 秒 |
| `slowtest` | P-07 仅 Windows，与 fulltest 等覆盖完整执行一次；无跨平台/WSL 增量标记 `SKIPPED_NOT_APPLICABLE`，不重复跑 fulltest | 1500 秒 |

所有门均不运行 CI、远程、发布、ComputerUse、全局鼠标或焦点抢占测试。`gui_flow` 全部用例、`acp_service_process` 两项 `real_gui` 用例、`gui_smoke.py` / `markdown_acceptance.py` 的真实桌面 driver，以及注册全局热键的 `check_worker_root.py` 服务探测保留原测试/断言和独立入口，门中报告 `NOT_RUN_SEPARATE_USER_INSTRUCTION_REQUIRED`，不因门外而判 FAIL，也不冒报 PASS；软件渲染不等于焦点隔离，须用户当次另行明确指令才执行。库内 Slint TestingBackend、OCR 隔离 desktop / window station 用例仍在门内。适用测试缺环境/工具时报告覆盖缺口，编译或门通过不等于未运行项验收通过。

fulltest / slowtest 须取得当次明确授权；用户明确授权本次任务所需运行及必要重跑时，仅在本任务范围内有效，不继承历史授权。`--authorized` 只是当次明确指令的入口声明，不创造授权（会话软约束）；不得静默重跑到绿。门授权不包含真实模型调用，也不继承此前真实 ACP 验收的授权；真实模型验收须另行取得当次明确指令。

每级使用一个共享墙钟，setup / static / format / tests / download / packaging / cleanup 全计费；只有受控 Windows Job 内实际观测的 compiler 编译独占区间、且无非编译并行工作才扣除。不能把整条 Cargo/测试/打包命令或缓存预热都扣除；混合打包只在同步等待纯 `cargo build` 且符合观测条件时扣除，其他阶段全收费。所有终止路径均输出 `total` / `compile_excluded` / `budgeted` / `limit` / `status`，`budgeted = total - compile_excluded`，超出非编译预算即失败并回收所属进程树。

Windows 入口由独立监督进程持有工作进程及其全部后代的外层 Job；调度器只向监督进程上报已观测的编译扣时。首错立即终止整个所属 Job；共享预算到期也强制终止，不依赖工作进程继续执行超时检查。阶段清理、日志或最终汇总卡死不能延长上限，失败或超时均返回非零退出码。

Job PID 枚举按内核实际填写的列表长度读取，成员增长时按 `ERROR_MORE_DATA` 扩容，不把 assigned 计数补成未初始化的 PID；阶段存续期间保留已观察到的所属进程对象句柄，避免已退出 PID 在并行用例间复用造成身份误判，结束时释放句柄及 Job。不保证捕获两次采样间完整出生并退出的短命后代。仍以持有句柄及 Job 成员身份确认清理边界；观察和关闭开销计入非编译预算。非编译工作重叠时直接计费，不为检查器反复枚举编译进程。必要的 fastcheck 在相关改动成批完成后执行，输入未变的通过结果复用。

独立命令使用四槽位池，短阶段完成即补位，不因同批慢测试阻塞其他可运行阶段；共享 Cargo 写缓存的不同编译维度仍串行。共享同一互斥资源的阶段只有在前项所属 Job 清理完成后才能启动，等待项不阻挡其他独立阶段补位；清理未确认时停止启动后续项。源码快照使用 UTF-8，记录 HEAD、dirty 摘要及工具版本，不能把中止或不同快照的结果拼成通过。

每次门进程启动或重跑都使用新的 `.tmp/test-gate/<门级>/pid-<PID>-<唯一运行标识>/`，启动输出该路径；同一进程内再次创建门运行也重新分配目录。阶段日志采用排他创建，原生测试结果、失败通知、打包计时状态及本轮打包产物均跟随该目录，嵌套验证不复用外层路径。独立命令日志使用 `.tmp/test-gate/commands/pid-<PID>-<唯一运行标识>/`。旧日志不覆盖、不追加、不因重跑清空；读取日志应使用本轮输出路径，不能把历史目录混作本轮记录。其他预算、失败即停、测试覆盖、缓存和授权规则不变。

三级门统一采用失败即停：任一用例失败、阶段非零退出、编排异常、命令启动失败、绑定环检查失败或必需前置缺失，立即停止后续调度并终止本门已启动的全部进程树，不再执行后续检查、测试或打包，也不等失败阶段清理结束才通知监督进程。保留首个错误及完整日志，以非零退出码结束；仅标记为门外独立授权的项目不触发失败即停。各项仍可并行启动，“立即停止”指发现失败后停止，不将普通 stderr 输出直接认定为失败。

嵌套运行器同样失败即停：Python unittest 使用 `-f`，并在 failure/error、子测试或夹具错误、unexpectedSuccess 以及报告写入错误回调中，向门指定的独占 JSONL 文件立即通知首错，不等 teardown、cleanup 或报告保存返回；expectedFailure 和普通输出不触发通知。静态检查循环在首个失败项后停止。稳定版 Rust libtest 的 `--fail-fast` 仍需 nightly，不切换工具链或构建配置；门通过其原生 `--logfile` 逐项结果文件观察首次失败，一旦发现便终止该测试 runner 及其他并行进程树，不等待整套测试结束或预算超时，不解析可能混入测试输出的 stdout/stderr。`--logfile` 当前仍可用但已标记弃用，未来升级工具链时须重新核验此接口。

正常保留 `target/`、固定 features / flags / toolchain / target / profile 矩阵并复用增量缓存，不执行 `cargo clean` 或预算外预热，不通过缩减覆盖满足预算。release 打包属于 fulltest / slowtest；CI 与发布继续独立。既有检查定义、判据、测试断言与依赖质量配置保持不变。

Windows 编译报 Win32 1455（页面文件太小）时，可在运行门前为当前进程设置 `$env:CARGO_BUILD_JOBS = "2"`，并在整轮中保持该值。它只限制编译并发，不减少目标、特性、profile 或测试覆盖；不据此删除缓存，也不修改系统页面文件设置。

三级门先用 `make_tmp.py workspace` 创建当前门的 `.tmp/` 日志目录；运行期 tempfile 另放仓库外同盘的本门独占临时目录，避免 Git 整树保护拒绝合成数据处理，所属子进程的 `TEMP` / `TMP` 指向该目录，终局回收。不改系统环境，不清空 `target/`。真实媒体测试未显式配置时，门在最新 Xberg 身份核验通过后生成固定公开语音与独立无音轨 MP4，使用校验 SHA-256 的便携 FFmpeg 缓存和 Windows SAPI 文件输出，不播放声音、不安装全局工具；固定期望正文先于转录确定，原测试断言不变。下载、合成和转录均计入非编译预算；显式配置不完整、工具或语音缺失均不能冒充通过。

完整门在最新 Xberg 前置通过后显式运行 `xberg_assets` 的真实目录存在性用例，核对 engine / snapshot / media / document 四个场景。普通 fulltest / slowtest 不探测或自动运行真实 OpenCode 模型验收，统一报告 `real-opencode: NOT_RUN_SEPARATE_USER_INSTRUCTION_REQUIRED`；其测试 artifact 仍编译并按普通非 ignored 入口运行，其他单元、文档、打包、7-Zip、Xberg 与媒体覆盖保持不变。该门外状态既不冒报模型验收 PASS，也不据此判门 FAIL。

仅在用户当次另行明确授权真实模型验收后，独立运行以下入口（不是 fulltest / slowtest 的一部分）：

```powershell
cargo test --features test-hooks --test acp_opencode -- --ignored --exact real_opencode_models_json_and_incremental_sse_single_agent --nocapture --test-threads=1
```

独立 `acp_opencode` fixture 保留 `JCHTOOLS_TEST_OPENCODE_EXE` 可配置的原生 `opencode.exe` 绝对路径；设置该变量或安装 OpenCode 只提供程序路径，不构成授权，不使用 `.cmd` 启动器代替原生程序。唯一指定模型仍为 `opencode/mimo-v2.6-flash-free`，仅单 Agent、单并发 Models / JSON / 增量 SSE；认证、Free 可用性或能力不满足时如实报告，不回退其他或付费模型。历史授权与文档中的命令均不是下一次执行的授权。

真实媒体用例使用本轮带 `test-hooks` 的 `JchTools.exe` 作为 `JCHTOOLS_TEST_BROKER_EXE`，不让测试二进制误充代理。语音与无音轨两个既有用例以 `--ignored --exact` 分别在独立 Job 中执行，互斥串行且逐项回收常驻后台，避免各自的隔离设置目录之间争用会话级唯一 Xberg；媒体测试与 7-Zip、打包阶段仍可独立并行。真实 7-Zip 测试使用捆绑目录时与打包互斥，避免测试运行 `7z.exe` 期间打包下载阶段覆盖同一引擎文件；指定其他独立引擎目录时不施加该互斥。测试正文、结构和无音轨断言保持不变。

完整门在运行进程测试前显式执行 `cargo build --bins --features test-hooks`，构建隔离后台入口及 ACP 夹具，并另行执行 `cargo build --manifest-path optional/snap-ocr-worker/Cargo.toml --bin snap-ocr-worker --features test-hooks`，保证截图库测试使用的独立 worker 代理支持隔离；`cargo test --all-targets --no-run` 的测试 artifact 不能证明这两个实际程序已带隔离钩子。生产构建维度仍单独验证，测试阶段不沿用生产 EXE，正常复用同一 `target/` 缓存。

OMP `server_agent` 开发边界：来源过滤及聊天到 `X-JchTools-Session-ID` 的内存关联由 OMP 的 `jchtools-agent` 适配器维护；JchTools 继续接受文本完整历史，校验已提交前缀并仅将新增消息交给 ACP，不为该前端统一删除 `system`/`developer`。新建/分支聊天或目录改变由 OMP 发起新会话，明确失效时只显示错误、不自动重放；不增加仅新增消息的 HTTP 接口、跨后台重启恢复、MCP 或子代理桥接。

`crates/acp-api/src/acp_api/acp/mod.rs` 在实际 `session/prompt` 提交边界输出 INFO 事件 `acp_prompt_summary`：请求关联 `request_id`、是否显式复用 `reused_session`、本轮实际新增 `message_count`、四种角色数量及原始文本 UTF-8 总字节数 `text_bytes`。首轮统计全部提交消息，续轮只统计扣除已确认历史后的新增消息；不计角色包装文本，不记录正文、完整请求、目录、工具参数或凭据，也不增加导出接口。隔离官方 SDK 夹具验证入口为 `cargo test --features test-hooks --test acp_prompt_summary --test acp_http_contract`；该证据不代替另行授权的真实 OpenCode 多轮验收。

代码中的删除测试全部使用 tempfile 临时夹具，不会删除测试机的个人文件。真实引擎测试有 ignore 标记，只有显式运行时启用。

`gui_smoke.py` 与 `markdown_acceptance.py` 仅对自己启动的验收实例使用 Windows Job Object；GUI 正常或异常退出后都会回收所属后台，避免上一阶段遗留的 Xberg 阻塞下一阶段，不影响用户已有实例。Markdown 验收同时识别未观察到忙态的瞬时失败收尾，保留真实失败统计与无产物结果，不将其误报为驱动超时。
真实 GUI 验收仅在当次另获授权后，通过 `acceptance.ps1 -WithDesktopTests` 编排隔离目录；GUI 冒烟和转换验收还分别指定 `-WithGuiSmoke` 或 `-WithMarkdownAcceptance`。直接调用 `gui_smoke.py` 的非自隔离阶段（S1–S5、S10–S18）或启动真实转换驱动前，必须提供 `.tmp/` 下的绝对隔离 `JCHTOOLS_TEST_STATE_DIR` 和 `JCHTOOLS_SNAP_OCR_TEST_ASSET_ROOT`；S6–S9 自行生成隔离环境。`--allow-isolated-run` 不绕过公共启动路径的隔离要求，缺少隔离条件时拒绝启动，不能使用生产目录代替。
转换驱动观察到「停止任务」后只等待本轮完成，不再次点击「开始转换」，避免将首轮成功产物误计为第二轮的「已有结果跳过」。本地 Git 验证可用 `RUST_TEST_THREADS=2` 限制并发资源占用；用例数、断言与时间阈值不变。
仅在当次另获真实桌面测试授权且完整根包测试范围明确时，可用 `cargo test --all-targets --features test-hooks --no-fail-fast -- --nocapture --test-threads=1` 串行完成全部同维度用例并保留即时失败证据；直接 Cargo 入口不经过默认排除真实桌面测试的验收脚本门禁，不应当作常规无桌面验证命令。用例内部的会话并发、停止竞态及真实进程断言不变。此命令不属于无测试的 fastcheck，单独通过也不等于 fulltest 通过。Windows 退出验证等待原进程句柄 signaled，不能将退出码已变化误当作进程对象已完成退出。
独立断网测试已于 2026-10-05 按用户决定删除，完整门不再要求修改系统网络或提供断网证据；产品的离线运行能力与不上传约束保持不变。

无头 GUI 测试对真实后台结果最多等待 30 秒；该上限只约束测试驱动，不改变产品超时或任务行为。Git 用例仍断言完成状态、文件计数、工作区及远端内容；Windows 进程启动开销不再因原 5 秒等待值被误报成功能失败。三级门统一受上述共享非编译预算约束，不以整个编译命令的墙钟作为可扣除编译时间。

另行获授权的独立 Markdown 验收经 `acceptance.ps1 -WithDesktopTests -WithMarkdownAcceptance` 接入，传参时使用 PowerShell 数组绑定；多个参数以及含空格、单引号的路径保持各自边界，不受 Windows PowerShell 5.1 的 `-File` 多值数组限制。这一驱动说明和开关均不代表三级门会调用真实桌面 driver，也不创造执行授权。

本地验收保留调用方显式设置的 `JCHTOOLS_TEST_XBERG_DIR`，未设置时才使用本机固定测试引擎目录；真实引擎验收须采用当时最新发布版。无效的显式目录由验收报错，不静默换测其他二进制；运行目录须同时包含所选 `xberg.exe`、模型及 DLL。媒体夹具合成另需 PATH 中的 `ffmpeg.exe`；独立桌面验收缺少安装版/便携版产物或独立 Windows 用户会话仍如实报告覆盖缺口，不能以目录改名或复制开发 EXE 代替；不要求独立断网测试。门外真实桌面项目继续按 `NOT_RUN_SEPARATE_USER_INSTRUCTION_REQUIRED` 报告，既不冒报 PASS，也不据此判三级门 FAIL。

### 审查定位与证据映射

以下登记审查涉及的 16 项，区分行为、架构、交付、测量与非目标；权威需求仍只在各自需求文档维护。本次仅修正审查定位与映射，**不声称完整覆盖、整项验收通过或媒体修复完成**。表中的断言/执法点是现有代码入口，不是本次执行结果；源码引用、模拟引擎成功与真实模型执行必须分开记录。

[`requirement_coverage.py`](scripts/requirement_coverage.py) 的候选发现范围修复只解决位置盲区，不能自动识别所有无编号的行为，也不能把引用升级为断言或执行证据；清单仍保持 `UNVERIFIED`，扫描通过或候选数增加均不表示验收通过。尤其交付脚本、工作流、安装脚本和 manifest 中的实现/阻断条件，必须与绑定源码和实际产物的执行结果分别记录。

| 需求 | 验收类别 | 现有真实断言 / 执法点 | 未证明范围 |
| --- | --- | --- | --- |
| [T-30](docs/requirements/ALL2MARKDOWN.md) | 跨功能行为 / 变更范围 | [`gui.rs`](crates/gui/src/gui.rs) 的 `markdown_converter_navigation_is_isolated_before_initialization` 检查导航与状态隔离；[`gui_smoke.py`](scripts/gui_smoke.py) 的 S2/S3/S4/S15/S16 分别检查分析不写入、整理、解压、MD 还原和真实本地 Git 产物。 | 分散用例不证明同实例转换/共享设置前后四个旧工具的完整行为与扫描范围均不变；不顺带重构还需变更审查。 |
| [P-01](docs/requirements/CONTRACT.md) | 产品身份 / 静态约束 | [`static_check.py::product_naming`](scripts/static_check.py) 断言窗口名、状态目录名并拒绝旧名；[`gui_smoke.py`](scripts/gui_smoke.py) 按真实标题 `JchTools` 查窗。 | 未完整断言安装产品名、入口名与工具名的边界；窗口匹配代码不是已运行记录。 |
| [P-05](docs/requirements/CONTRACT.md) | 双形态交付 / 安装行为 | [`package-windows.ps1`](scripts/package-windows.ps1) 组装共同树并校验资产；[`test_package_windows.py`](scripts/test_package_windows.py) 的 `test_xberg_final_tree_detects_audio_leak_and_missing_base` 检查负向资产边界，`test_inno6_compiles_deep_hf_source_without_mutation_or_alias_leak` 检查真实 Inno 编译和隔离安装。 | Inno 回归使用合成载荷并关闭图标/卸载登记，不能证明默认快捷方式、卸载项、免提升或正式双包首次 OCR/转换；分发扫描也非全部材料完整验收。 |
| [P-14](docs/requirements/CONTRACT.md) | 架构 / 增量构建 | [`Cargo.toml`](Cargo.toml) 已拆独立业务 crate；[`crates/ui/build.rs`](crates/ui/build.rs) 独立生成 Slint；[`static_check.py`](scripts/static_check.py) 的 `project_manifests` / `manifests` 检查包和默认成员。 | 静态结构不证明依赖边界不回退或实际 Cargo 缓存命中；缺隔离变更后的 sibling/UI `fresh` 与必要重编实测，不需要重新拆 workspace。 |
| [P-15](docs/requirements/CONTRACT.md) | 打包计时 / 稳定构建输入 | [`test_package_windows.py`](scripts/test_package_windows.py) 的 `test_snap_manifest_unchanged_worker_preserves_bytes_and_last_write_time`、`test_main_build_failure_preserves_completed_times_and_unrun_steps` 检查输入稳定及失败/未运行状态；[`test_fetch_7zip.py`](scripts/test_fetch_7zip.py) 检查相同已验证输入的字节和 mtime。 | 局部阶段回归不证明完整打包所有阶段及重复构建缓存；计时存储失败可能只留下旧 RUNNING 快照，不能承诺必有最终 FAIL 记录。 |
| [U-08](docs/requirements/CONTRACT.md) | DPI / 交付启动行为 | [`windows.manifest`](resources/windows.manifest) 声明 PerMonitorV2，[`build.rs`](build.rs) 在 Windows/MSVC GUI 构建中嵌入；[`launch-software.cmd`](scripts/launch-software.cmd) 无参数转发地启动同一 GUI，打包与安装链路均携带它。 | 软件后端 [`gui_flow.rs`](tests/gui_flow.rs) 用例绕过交付 CMD；未证明最终 EXE manifest、两种交付的 CMD 实际启动及运行期 DPI 行为。 |
| [E-04](docs/requirements/CONTRACT.md) | 发布执法 / 双产物交付 | [`release.yml`](.github/workflows/release.yml) 手动触发、分别要求恰好一份 ZIP/安装 EXE 并生成摘要；[`package-windows.ps1`](scripts/package-windows.ps1) 校验材料并如实写 `NOT RUN` / 未完结状态。 | 工作流配置不等于本次 run 或正式资产证据；真实 Inno 合成载荷回归不证明正式包完整性、启动或模型能力，ZIP 成功也不能代替安装包未运行/失败。 |
| [O-02](docs/requirements/SNAP2TEXT.md) | Rust/Slint 架构 / 无头核心 | [`snap-ocr-worker/Cargo.toml`](optional/snap-ocr-worker/Cargo.toml) 与 [`jchocr::recognize`](crates/jchocr/src/jchocr.rs) 提供当前产品接线；[`test_markdown_acceptance.py`](scripts/test_markdown_acceptance.py) 的 `test_default_distribution_rejects_audio_and_python_anywhere` 检查分发扫描拒绝 Python 负载。 | 当前扫描不是全产品“禁 Python/第二 GUI”架构执法；转换进程检查不等于截图服务全链路无 Python 的实际交付证明。 |
| [O-04](docs/requirements/SNAP2TEXT.md) | 平台 / 目标机真实测量 | [`xberg_runtime_windows.rs`](crates/xberg/src/xberg_runtime_windows.rs) 固定 `XBERG_ORT_EP=cpu`；[`package-windows.ps1`](scripts/package-windows.ps1) 要求 Windows x64 MSVC。 | 环境变量和目标检查不证明 Windows 11/AVX2 原生模型实跑；[历史 OCR 记录](tests/ocr_fixtures/README.md) 使用其他 CPU 和旧链路，不是 i7-13700 首轮对照。 |
| [O-08](docs/requirements/SNAP2TEXT.md) | 共享资产 / 兼容行为 | [`xberg_assets.rs`](tests/xberg_assets.rs) 的 `same_root_optional_media_does_not_block_base_readiness` 检查音频可选边界；[`xberg_runtime.rs`](crates/xberg/src/xberg_runtime.rs) 的 `scene_capabilities_require_compiled_backends` 检查能力握手。 | 合成成员存在性与协议断言不证明真实 DLL 装载、固定模型普通图/4K OCR、模型不兼容时不切源；不得另加违背 XB-09 的运行期摘要准入。 |
| [O-31](docs/requirements/SNAP2TEXT.md) | 唯一来源 / 干净交付运行 | [`crates/jchocr/tests/jchocr.rs`](crates/jchocr/tests/jchocr.rs) 的 `bundled_runtime_is_discovered_without_gui_configuration` 检查随包发现；[`service.rs`](optional/snap-ocr-worker/src/service.rs) 的 `font_readiness_works_without_external_cache` 检查字体无需外部缓存；启动代码移除开发机模型覆盖。 | 占位引擎发现回归不是 OCR 成功；未证明真实 ZIP/安装树在无旧目录/缓存时识别成功或依赖全无开发路径。 |
| [O-32](docs/requirements/SNAP2TEXT.md) | 性能 / 内存 / 并用交互测量 | [`compare.py::self_test`](tests/ocr_fixtures/compare.py) 拒绝非法耗时；[`xberg_shared_process.rs`](tests/xberg_shared_process.rs) 的 `documents_reuse_process_and_snapshot_finishes_during_document` 检查共享协议并用。 | 比较器不生成当前引擎报告、不采集内存；模拟引擎同 PID 不证明模型常驻。缺当前普通图/4K、预加载、常驻/峰值内存与真实并用响应的完整证据。 |
| [XB-04](docs/requirements/XBERG-INFERENCE.md) | 本地 IPC / 隐私行为 | [`xberg_runtime_windows.rs`](crates/xberg/src/xberg_runtime_windows.rs) 的 `consumed_response_logs_safe_results_and_decode_failure_stage` 检查正文 sentinel 不入日志；[`SharedXbergClient::recognize`](optional/snap-ocr-worker/src/shared_xberg.rs) 从内存 PNG 发请求，代理拒绝远程管道客户端。 | 定向日志断言不是全边界禁敏感落盘/上传证明；离线环境不是 OS 网络沙箱，外部发布引擎仍需隔离观察。jchocr 显式 URL 图片输入不是远程推理接口。 |
| [XB-13](docs/requirements/XBERG-INFERENCE.md) | 迁移前后质量 / 资源对照 | [`compare.py`](tests/ocr_fixtures/compare.py) 比较已有文本/布局/耗时报告；[`markdown_media_e2e.rs`](tests/markdown_media_e2e.rs) 的 `speech_transcript` / `timestamp_ms` 提供真实媒体正文、结构和时间戳断言入口。 | 旧 `ocr_compare` 已退役，冻结 Python JSON 不能复测原版性能；未具备 T/O 两区同样本、双侧真实来源、时间戳、耗时及内存的完整可复跑对照，不能倒推历史从未验收。 |
| [H-08](docs/requirements/CONTRACT.md) | 非目标 / 范围约束 | 合同明确不提出专门无障碍开发、维护、测试与验收；通过需求及变更范围审查保留边界，不虚构行为断言。 | 无需补屏幕阅读器、辅助技术等测试来清零引用统计；普通鼠标键盘易用性仍按 H-02，框架自带能力不构成违反非目标。 |
| [U-04](docs/requirements/CONTRACT.md) | 非目标 / 范围约束 | 合同排除角色/标签、专门 Tab 遍历、焦点框与辅助技术要求；不以恒真测试或编号注释冒充验收。 | 不得强加无障碍门槛；普通输入、点击和必要键盘操作仍需按既有功能要求验收，使用 UIA 驱动本身不改变该边界。 |

真实发布仍未获得本次授权，源码中的发布阻断条件不代表已经触发或完成发布。旧版可运行 oracle 与 i7-13700 目标机同机对照条件尚未具备，冻结结果、历史其他机器测量、模拟 worker 或短媒体成功都不能代替；须在所需真实资产与授权具备后，分别记录本次入口、源码/产物身份、实际断言和未验证范围，不因候选映射修复自动升级状态。

### 测试计时报告

完整门内的六个 Python unittest 阶段沿用原发现范围、参数和 `-f`，通过 `scripts.unittest_report` 扩展标准库结果回调；不替换测试框架。局部命令只替换入口模块，其余选择器保持原样：

```powershell
python -m scripts.unittest_report -f scripts.test_gui_smoke
python -m scripts.unittest_report discover -f -s tests/ocr_fixtures -p test_worker_root_isolation.py
```

每次调用自动生成独占的 `.tmp/test-reports/<UTC时间-UUID>/junit.xml` 与 `run.json`。元数据记录实际命令、选择范围、并发、平台、Python/运行器版本、源码 revision 与 dirty 内容身份、观察到的墙钟、退出码及覆盖状态；不可用值明确为 `null`。自维护脚本无独立版本号时以源码身份定位，不编造运行器版本。XML/JSON 按现有 `.tmp/` 忽略规则保留，不进入源码或发布包，不新增 CI 上传或自动历史清理。

测试门每项汇总另打印 `stage_elapsed=<秒>s`，测量既有阶段从启动至所属进程树收尾的墙钟；未取得真实启动/退出证据时打印 `unknown`。这是阶段耗时，不是逐例时间，也不把整条 Cargo 阶段计为纯编译；阶段并行时不得累加为总墙钟。

无头日志面板用例保留两档尺寸、五个工具、空闲/运行状态的完整矩阵及真实键盘/鼠标事件；每次仍推进模拟时钟和动画，只在 Slint 标记画面需重绘时渲染到复用的像素缓冲，不强制重画未变化的画面。测试引擎清场在同一个 PowerShell 进程中依次执行原服务、代理及两遍引擎扫描，保留逐阶段的最新进程查询、所有权过滤、等待间隔和尽力继续语义；会话锁及用户进程保护不变。

| 既有生产者 / 公开入口 | 报告粒度与边界 |
| --- | --- |
| 门内 unittest：`test_markdown_acceptance`、`test_gui_smoke`、`test_test_gate`、`test_review_trusted`、`test_package_windows`、`test_worker_root_isolation` | 每个逻辑测试一项；`startTest`–`stopTest` 实测包含 setup、call、teardown 与单例清理。子测试结果归入父用例，不增加用例数；类/模块夹具的门外错误在元数据中标为未知时长。 |
| 独立 `scripts.test_fetch_7zip` 等 unittest 选择器 | 可经同一入口报告；不擅自加入完整门原选择范围。原始 `python -m unittest` 本身不提供 XML。 |
| `scripts/gui_smoke.py` | 正常 CLI 自动报告所选 S 阶段；数据集阶段含复制/清理，未启动阶段为无时长 skipped。执行顺序及首错停止不变，仍需当次独立桌面授权。 |
| `scripts/markdown_acceptance.py` | 正常 CLI 自动报告所选条目 ID、profile 和组；原 JSON/控制台全集分母不变。NOT RUN 无时长，未选择及 OPTIONAL 不冒充执行；仍按原规则完成所选条目与 broker 清理。 |
| `check_worker_root.py`、`check_gui_worker_connection.py` | 各自单项场景为明确的 command-level 用例，包含其夹具和清理；真实服务/桌面权限不变，不能拿命令耗时冒充内部断言计时。 |
| `tests/ocr_fixtures/compare.py` | `--self-test` 与历史报告比较分别是单项 command-level 报告；后者只比较既有 OCR 数据，不证明当前模型执行。 |
| 根、14 个生产 crate、两个 optional 包的 Rust UT / integration / native / 无头 GUI，以及另行授权的真实 GUI / 引擎 / 模型用例 | 保留原 cargo/libtest 入口。稳定版 libtest 的逐用例 JSON、JUnit 和计时仍需不稳定开关；现有 `--logfile` 只有结果，没有时长，当前不支持逐例计时 XML。 |
| 根默认成员及两个 optional 包的 `cargo test --doc` | 保留独立路径，同属上述稳定版报告缺口；nextest 不覆盖 doctest。 |
| `scripts/test_timing.py` | 原两轮套件/慢例 JSON 分析入口不变；整套耗时不能当逐例时长，重复单跑结果也不替代正常测试路径的完整 XML。 |

已安装的 nextest 不能无损替换现有门：其逐测试独立进程改变进程内夹具复用，`--nocapture` 强制串行，而当前门为四 binary 并发、每 binary 两线程。不会为报告切换 profile、工具链、构建旗标或缓存路径。参照 [nextest 执行规则及 doctest 边界](https://nexte.st/docs/running/) 与 [libtest 输出选项](https://doc.rust-lang.org/rustc/tests/index.html#output-options)。

逐例秒数之和不是运行墙钟或 CPU 时间；共享夹具、构建、调度和并行重叠分别按运行器口径记录。已有结果逐项检查点落盘，失败、中断或门外终止留下 partial 元数据，不重用旧 XML。报告故障可见且不能使测试失败变成功；help/list/状态播种不是测试运行。静态检查、编译和 CI 打包没有测试用例，不计入计时排名。局部报告不代表全仓库报告，完整门和真实桌面/模型执行仍须分别取得当次授权。

## 工程入口

根 `src/main.rs` 只装配 GUI 和既有后台入口，不提供旧根库 façade；`crates/gui` 承接 GUI 业务装配，`crates/ui` 仅编译根 `ui/app.slint`。`crates/file-core` 承接配置、模型、磁盘索引/审计、共享任务/扫描、文件与引擎基础；`crates/recursive-extract` 和 `crates/directory-organizer` 分别承接递归解压与目录整理。MD、Git、Markdown、assets、Xberg、ACP、CodeAgent 与 foundation 均在各自 `crates/` 中；`crates/jchocr` 有自己的 lib/bin/test，可用 `cargo run -p jchocr -- --help`，无需构建 GUI。截图 worker 入口仍为 `optional/snap-ocr-worker/Cargo.toml`，复用真实 foundation/Xberg crate。根 `resources/`、`ui/`、`tests/`、`docs/` 仍是唯一资源/测试位置（CLI 测试归 `crates/jchocr/tests/`）；`scripts/` 是检查/打包入口；`.github/workflows/check.yml` 是 Windows CI。

技术依据和许可见 [第三方说明](THIRD_PARTY_NOTICES.md)；全部需求域及其唯一权威文档见 [需求目录索引](docs/requirements/README.md)，适用全局约束见 [CONTRACT.md](docs/requirements/CONTRACT.md)。
