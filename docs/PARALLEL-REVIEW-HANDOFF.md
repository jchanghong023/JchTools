# 全代码并行审查修复交接

> 以下早期章节为历史记录；本次全仓并行审查的范围、提交、证据及未验证项以末尾「2026-10-13 全仓并行审查修复」章节为准，不能复用旧轮通过结果证明本轮修改。

## 状态与工作区

- 工作区：`D:\code1111111111\JchTools\JchTools`，Windows 10.0.26200 x64。
- 初始工作树无既有改动。全仓代码分给25个独立审查负责人；后续3个独立验证准备负责人。本轮累计28个，符合用户最后指定的30上限。各产品文件单一修改负责人，现全部停止写入。
- 审查修复已落盘；整体验收未通过。最终 `python scripts/static_check.py` 返回1，失败项为 `test_baseline`；其他项通过，285个跟踪文件的SHA256与工作树一致。
- 无commit/push、依赖安装、需求条目修改、测试基线再生成或完整测试门执行。可信基脚本与产品改动须按项目约定分开提交，不能混成一笔。
- 本文只是交接，不是需求权威。当前需求域及唯一维护位置以 [固定需求目录索引](requirements/README.md) 为准。

## 已实施修复与文件清单

|范围|实际修改文件|修复内容|
|---|---|---|
|目录整理、规则、数据库|`src/engine.rs`、`src/planner.rs`、`src/db.rs`、`src/fsutil.rs`、`src/rules.rs`、新 `src/rules/signature.rs`；`tests/core.rs`、`tests/property.rs`|Windows序数大小写比较；精确卷族去重计数；签名结构校验和取消传播；不可靠检测保留扩展名；既有任务库版本检查、schema6键语义；Windows禁用C0而不是DEL/C1。|
|递归解压、引擎释放|`src/archive.rs`、`src/engine_bundle.rs`；`tests/archive.rs`、`tests/contract_engine.rs`|复合tar流及tgz递归；深度与危险条目隔离；卷族缺卷/碰撞/Unicode边界；整族长名统一主体、完整保留后缀编号补零；落盘前大小验证；只清理本次拥有的staging/part，不删除他人临时文件。|
|Git、进程、代理|`src/git_tools.rs`、`src/process.rs`、`src/system_proxy.rs`；`tests/git_tools.rs`|真实Git合并与禁止子模块递归；停止后不再直连重试；Windows bypass通配符；有界stdout/stderr、启动前取消、Windows Job内启动与后代整树终止。|
|MD与转换|`src/md_tools.rs`、`src/markdown.rs`、`src/markdown_document.rs`；`tests/markdown_convert_gate.rs`、`tests/markdown_media_e2e.rs`、`tests/fixtures/shared_xberg.rs`|MD最终写入/flush阶段取消保留既有结果；缩进块与转义图片标记保留；按所选引擎formats筛选；缺失/空媒体夹具明确失败，不能跳过冒充通过。|
|共享推理、资产、配置|`src/asset_util.rs`、`src/markdown_assets.rs`、`src/snap_ocr_assets.rs`、`src/xberg_runtime.rs`、`src/xberg_settings.rs`；`tests/xberg_assets.rs`、`tests/xberg_settings.rs`|拒绝ADS、ZIP大小写覆盖；Ready预取消处理；缓存worker校验；共用Paddle许可与单场景模型边界；保存下载配置时保留未加载的旧文本配置。|
|GUI、截图服务、日志|`src/gui.rs`、`optional/snap-ocr-worker/src/service.rs`、`src/logging.rs`、`src/perf.rs`；`tests/perf_probe.rs`|各输入框独立选择起点；非法/空/溢出草稿保留并阻止确认；Ready复用前必须确认字体资产；诊断日志初始化失败不触发panic；性能探针独立进程、真实span落盘与flush。|
|验证工具|`scripts/test_gate.py`、`scripts/test_test_gate.py`、`scripts/test_timing.py`、`scripts/gui_smoke.py`、`scripts/test_gui_smoke.py`、`scripts/markdown_acceptance.py`、`scripts/test_markdown_acceptance.py`、`tests/ocr_fixtures/compare.py`|完整门PARTIAL判据、当前dispatch身份、60秒进程树预算、slowtest默认触发不等待；GUI结果/启动/隔离守卫；A18/A25通过同一真实broker的formats及SQLite/PID/会话身份判据；OCR重复锚点不做歧义匹配。|
|依赖、发布、说明|`Cargo.toml`、`Cargo.lock`、`scripts/package-windows.ps1`、`README.md`、`先读我.txt`、`SHA256SUMS.txt`|ISCC前生成共同BUILD-INFO，编译后更新便携真实状态；既有image最小六decoder与roxmltree复用，退役globset/infer移除，未保留试加sevenz/lzma，未升级既有依赖版本；说明同步；最终跟踪文件摘要更新。|

其余所分配代码已审查，没有另行报告需要修复的证据明确缺陷；不代表所有运行行为均已验收。未删除既有测试或削弱断言；审查期间新增的纯接线测试已撤下，不能作为行为证明。

## 实际验证证据

技能本身禁止阶段内测试，但更高优先级宿主要求缺陷反证和行为烟测。主代理因此执行了下述针对性验证；子代理没有代跑最终验收。不能声称“本轮没有运行测试”。所有命令有不超过60秒的墙钟/进程树约束。

|命令或实际入口|退出码与关键结果|证据边界|
|---|---|---|
|最终 `python scripts/static_check.py`|1；仅 `test_baseline` FAIL，其余PASS，`sums_integrity` 285文件PASS|新增回归/测试变化尚未经过基线确认。初始同命令为0。|
|对已改Rust文件的 `rustfmt --check --edition 2021 --config skip_children=true ...`；Archive三文件 `rustfmt --edition 2021 ...`|0|仅格式；`tests/fixtures/shared_xberg.rs`保持原有压缩书写约定，未整文件重排。|
|`ruff check scripts/test_gate.py scripts/test_test_gate.py scripts/test_timing.py scripts/gui_smoke.py scripts/test_gui_smoke.py scripts/markdown_acceptance.py scripts/test_markdown_acceptance.py tests/ocr_fixtures/compare.py`|0，All checks passed|限定Python变更文件。|
|`python -m unittest scripts.test_test_gate scripts.test_gui_smoke scripts.test_markdown_acceptance`|0；126测试通过|3模块单测，不等同真实门/真实GUI/真实转换验收。|
|最终冻结源码的core source-only消费者；原产品模块通过 `#[path]` 包含，原测试文件完整包含|0；55测试通过，含2条先红的长卷名统一主体回归、序数卷族、取消、签名负例、DB、进程树、引擎释放及公开规划/执行路径|不是全根Cargo构建；没有复制产品函数、stub或用旧jchtools.rlib替代变化实现。|
|同一最终core消费者，真实7-Zip26.03：bz2/tgz、嵌套递归/深度、危险隔离、嵌套分卷碰撞|0；6测试通过|真实公开archive入口与实际产物；不是完整验收矩阵。|
|当前源码独立Git消费者|0；6测试通过|真实本地git/bare远端、冲突内容、子模块远端状态、停止后不重启及bypass通配符；未覆盖真实认证网络。|
|当前源码独立资产/日志消费者|0；10测试通过|ADS、大小写碰撞、预取消、worker完整性、共享许可、模型归属、日志初始化失败；不证明模型内容和推理质量。|
|当前源码MD消费者、formats消费者|0；MD3例及formats1例通过|formats客户端为当前源码，broker使用已在场 `target/debug/JchTools.exe`，不是最终全根重编；协议fixture不证明真实Xberg转换质量。|
|媒体夹具缺失/空输入负例|原消费者各返回101，驱动匹配明确缺失/空输入panic及1 failed后返回0|仅证明fail-closed，不能称真实媒体转换通过。|
|当前源码专用perf-tracing库 + 原完整 `tests/perf_probe.rs`|0；3探针通过，真实落盘analyze/apply/hash/plan等closed spans；实际删除/计划计数断言通过|16组×3副本、64空目录的小规模行为烟测；不是release性能基准。驱动曾错误覆盖TEMP使H06拒绝，纠正仅在临时驱动，不修改Git保护。|
|当前源码库 + 原完整 `tests/xberg_settings.rs` 中 `downloaded_save_preserves_unloaded_legacy_text --exact`|0；独立子进程保存/重载回归通过|此前已证实旧文本配置丢失；仅该消费者，不是全部配置验收。|
|`cargo test --locked --features test-hooks --manifest-path optional/snap-ocr-worker/Cargo.toml --lib reuse_requires_verified_font_assets -- --nocapture --test-threads=1`|0；1测试通过；编译9.05秒|真实当前worker crate，字体Ready复用回归；非热键/托盘/桌面E2E。|
|实际桌面GUI脚本，1120×720及960×620两档、144DPI|0；非法abc草稿保留/阻止确认，改成16后精确1个压缩包确认，无源文件操作|验证已在场GUI EXE；未取得最终全根重编，不能替代最终UI验收。|

已执行的核心、Git、MD、资产等回归保留修复前失败与修复后对应证据；未实际执行的打包、真实推理和完整桌面链路不作反证或通过声称。部分旧库二进制证据后来由当前源码消费者重新验证，不能引用旧binary证明最新修改。临时harness/日志/截图按项目临时目录规则在收尾清理，不作为永久测试或发布物保留。

## 仍受阻或未验证；给验收Agent的下一步

1. **基线承接**：用户已删除项目原§10的人工新旧基线审批要求，基线再生成不再因额外是/否确认受阻；按当前 `AGENTS.md` §3.2记录覆盖点、测试变更理由与反证，不删除回归、不放宽断言让static_check变绿。
2. **根构建/完整Cargo未验证**：初始 `cargo test --features test-hooks`、后续根lib构建/单测编译均达到60秒或驱动55秒树终止预算，未把超时报成功。已用当前原样source closure做必要烟测，但不能替代默认/gui/test-hooks的完整Cargo契约。最终GUI重新构建、`tests/gui_flow.rs`及对应真实桌面链路仍须验收。
3. **真实边界未验证**：A18/A25真实SQLite/Windows会话/PID/命名管道链路；真实Xberg模型、媒体/OCR质量；热键/托盘/服务生命周期与多DPI；真实认证网络；发布ISCC、ZIP、无引擎泄漏和两种交付形态。C08/C09无独立Windows用户会话仍须如实NOT RUN，不能推定通过。
4. **完整门与CI待授权承接**：本轮没运行fastcheck/fulltest/slowtest或最终验收Agent，也没触发远程CI。按项目当次授权规则执行；slowtest默认只触发并给run链接，用户明确要求等待时才等待。CI转绿前不得宣称整体完成。
5. **源码依赖复用边界**：主包logging/runtime/settings被worker以源码编入，验收两端；新 `src/rules/signature.rs`与本交接文件尚为新增文件，后续纳入跟踪/分提交后按当时全部跟踪文件重新生成SHA256SUMS。产品与可信基分别提交；没有兼容shim或退役推理通道恢复。
6. LSP/rust-analyzer在宿主不可用，未安装替代；代码类型/最终链接仍以真实Cargo验收结论为准。根受检target缓存未清理。

## 2026-10-08 无人值守验收承接

以上是前轮历史交接状态。用户随后明确确认 771→832 测试基线及两项 lint 摘要补充，并授权本次完整验收、等待 CI、发布和必要修复；要求不再重复询问。最新格式必验范围为 DOC/DOCX、XLS/XLSX、PPT/PPTX、PDF、MP4；既有扩展矩阵和测试保留为可选，不计通过。

- `cargo test --features test-hooks` 已完整执行；根库 372 项通过、Git 49 项通过。真实 7-Zip 26.03 的 archive 显式 ignored 集 38 项通过；Snap core 61 项及 worker 58 项通过。先前 32 路并发下 Git 命令受宿主进程启动拥塞而触发看门狗，未当作通过；后续统一 `RUST_TEST_THREADS=4`，未改变断言、功能或超时。
- GUI 生产和 test-hooks 二进制已重编。真实 GUI 整理、解压、MD、Git 本地 bare 推送、取消与设置重启链路，以及两档窗口尺寸的按钮边界检查已执行。真实模型 worker 从已校验隔离资产加载就绪。
- 首次 fulltest 扩展格式矩阵为 39 PASS、0 FAIL、5 NOT RUN，其中 A18/A25 为断开 broker 管道、A24 缺合成媒体工具，C08/C09 缺独立 Windows 用户会话；完整门结论为 UNVERIFIED，不能写成验收通过。现默认 common 范围按用户要求执行；full 原矩阵保持。
- 更新真实 7-Zip Git 冲突用例预期：合同 X-04 要求改名目录 `repo (1)` 成功，既有 `.git` 和用户文件字节不变。原“失败隔离”预期与合同不符；新用例保留旧树保护并新增改名产物、计数及原包删除断言。832 总数不变。
- Python 三验证模块新增回归后 133 项通过；修正 UTF-8 Git 状态读取与 Windows PID 文件可见但共享句柄尚未释放的瞬态读取竞态。真实后代退出、无关进程存活及原预算断言均保留。
- 新增 common 内容判据先红：`python -m unittest scripts.test_markdown_acceptance.CommonFormatAcceptanceTests.test_mp4_duration_without_transcript_is_rejected scripts.test_markdown_acceptance.CommonFormatAcceptanceTests.test_old_office_outputs_cannot_exchange_document_markers`，修正前退出 1、2 failed；修正后要求起止时间戳后的中文正文及每份对应的完整 DOC/XLS/PPT 标记。独立代理执行 CommonFormatAcceptanceTests 与 A24SceneNoteTests，退出 0、7 tests OK。

独立恢复的历史版本反证（原产品函数/模块原样消费者，非桩；不替代根验收）：

| 缺陷 | 复现命令与旧版关键失败 | 当前结果 |
|---|---|---|
|首次下载配置保存丢失尚未加载的旧目录|`powershell -NoProfile -ExecutionPolicy Bypass -File .tmp/release-validation/red-evidence/run-settings-evidence.ps1`；旧模块 Cargo 101，`left: None`、`right: Some("…旧用户目录")`，父回归 1 failed|当前模块 Cargo 0，独立保存/读取子进程及父回归通过|
|复合后缀错误拆分|`powershell -NoProfile -ExecutionPolicy Bypass -File .tmp/release-validation/red-evidence/run-fsutil-evidence.ps1`；旧函数测试 101、3 failed，report.tar.pdf/x.part0.rar/a.zip.7z.001 主体错误|当前函数测试 0、3 passed|

最终 slowtest、对应 CI 与 release 结果以本次实际运行链接及结论为准。本段不声明这些后续阶段已通过。独立 Windows 用户会话 C08/C09 保持 NOT RUN；真实网络认证、TB 级数据和其它 DPI 不在本机通过证据内。

首次 `slowtest --authorized --wait` 的本地阶段全部 PASS（acceptance 636.6 秒、package 539.8 秒，常用矩阵 37 PASS / 0 FAIL / 2 NOT RUN / 5 OPTIONAL）；远程 [run 37710985115](https://github.com/jchanghong023/JchTools/actions/runs/37710985115) 在 Python 单测失败，整门 FAIL、退出 1，未触发发布。原因是 GUI 单测的 pytest 未声明，以及 CI 的 `RUNNER~1` 短路径夹具未规范化。用真实 C 盘 8.3 临时路径复现原 PID 回归同一失败（1 failed）；仅将夹具 `Path(temporary)` 规范化，并将五处异常断言等价换为标准库 `assertRaises(RuntimeError)`，产品隔离检查不变。保留五个旧日志、PID、角色、有效记录、缺 SQLite 判据，并增加短路径回归。修正后 Python 134 项通过；独立 AcceptanceBoundaryTests + test_gui_smoke 38 项通过。后续须用新提交重跑门，不能把旧失败 run 写成成功。

第二轮 slowtest 的本地阶段再次全部 PASS，实际安装包与 ZIP 解包后主程序/worker 字节一致、清单大小与 SHA-256 相符、无转换模型或裸 7-Zip 二进制泄漏。远程 [run 37713010730](https://github.com/jchanghong023/JchTools/actions/runs/37713010730) 已通过 Python、两档 Clippy；无默认特性 UT 280 PASS，core 106 PASS / 3 FAIL。日志明确构建时缺完整 7-Zip、未内嵌；三个合法 7z/RAR 签名正例无法获得引擎证据，按产品 fail-closed 保留旧后缀。修正仅在首次编译前调用现有官方 `fetch-7zip.ps1`，不改产品、断言或 ignore；13 条 Cargo 命令及原矩阵完全保留，经独立复核。带真实引擎的 `cargo test --all-targets --no-default-features --features test-hooks` 本地退出 0，三项原失败回归通过；`cargo audit` / `cargo deny check advisories sources` 退出 0。远程后续仍须用修正后的提交验证，发布尚未触发。

后续本地打包曾因 GitHub 匿名 API 限流失败；改为当前登录凭据读取官方元数据后完整本地门通过，凭据只存在进程环境，未落盘。远程 [run 37717029364](https://github.com/jchanghong023/JchTools/actions/runs/37717029364) 已将 core 的 109 项（含原三失败）全部验证通过，继续到 `batch_uses_selected_engine_formats` 时因缺代理宿主 EXE 报 `os error 2`（4 PASS / 1 FAIL）。`JchTools` bin 要求 gui 特性，无默认特性测试及 Clippy 不生成该 EXE；CI 现补充 `cargo build --locked --features test-hooks --bin JchTools` 前置宿主，再执行原核心命令。宿主用 `--xberg-broker`，不进入 GUI；隔离钩子与独立状态目录保留。原 13 条 Cargo 命令不变，新增后 14 条，经独立复核。本机相同无默认特性回归退出 0、1 PASS，远程后续仍待验证。

## 2026-10-08 用户更新 CI 职责并授权发布

用户明确指示停止完整远程测试：“本地测试就够了，ci只用来编译，发布版本包”，随后要求“发布版本直到成功”。此指示覆盖以上历史 CI 权威及重复完整测试规则。功能验收采用已执行的 Windows 本地证据；CI 仅生产编译、打包和发布，两个工作流使用 `package-windows.ps1 -SkipTests`。跳过模式不再编译测试钩子或测试目标，发布记录仍如实标记该构建机测试为 NOT RUN，不把编译等同功能验收。原有本地测试与 832 项基线均保留。

最新已执行本地阶段：acceptance PASS（605.6 秒），完整 package PASS（465.6 秒）；常用格式矩阵 37 PASS / 0 FAIL / 5 OPTIONAL / C08、C09 NOT RUN。产品源码自上述本地验证后未改，本次仅调整 CI/打包入口与职责文档，复用该本地证据。发布结果以实际 release run 和版本资产为准。

## 2026-10-11 slowtest 失败修复

- 用户当次授权 `python scripts/test_gate.py slowtest --authorized` 及定位根因后的必要重跑，要求三个层级失败即停、沿用门内超时，不增加外部终止阈值。
- 首轮退出 1：`test_baseline` 报五项新增、两项改名及十二项测试体指纹变化；没有新增 ignore。四个独立只读复核覆盖全部差异：ACP 会话协商身份与输入拒绝、解压重复成员与卷族不覆盖、整理/Git 安全和合并、GUI 取消及后台日志隔离。两项改名分别消除固定分类容器迁走 Git 树的错误预期、将可能因权限直接跳过的符号链接夹具换为必验的其他真实 reparse 类别；后者不声明覆盖符号链接权限路径。基线按原静态检查入口重建为 1003 项，不改变发现范围和忽略状态。
- `cargo test --features test-hooks --test git_tools real_output_captured_for_success_and_failure_paths -- --exact --nocapture` 首次编译退出 101：`src/archive.rs:433` 的迭代器捕获 `Control` 引用却未声明 `'a`。修正参数生命周期，移除仅复制引用的 `clone`；再次执行已编译通过，真实 Git 回归退出 101，报「push 失败必须采集……：[]」。
- 上述 Git 回归原来读取 `spawn_run` 明确置空的 `RunOutcome.logs`；线程实际输出位于独立共享缓冲区。等待线程完成后接管该缓冲区，保留两条输出流、Git 原始错误与停止状态断言。修正后同命令退出 0：`1 passed; 0 failed`，真实本地 bare 远端及失败路径均执行。
- 第二轮 slowtest 退出 1：前置静态、生产构建与测试目标编译均通过，严格 Clippy 在解码临时名二分查找中拒绝手写 `(left + right) / 2`。改为 `u32::midpoint(left, right)`，不加 lint 豁免。`cargo clippy --lib --bin JchTools --features test-hooks -- -D warnings` 退出 0；随后 `cargo test --features test-hooks --lib decoded_duplicate_volume_families_preserve_partial_existing_set -- --nocapture` 退出 0、`1 passed; 0 failed`，既有半套卷族与两组新增重复卷族字节及共同主体断言均保留。
- 第三轮 slowtest 在 `root-test-build` 退出 1：真正的编译错误为 `failed to mmap ...libjchtools...rlib` / Windows `os error 1455`，不是元数据格式损坏。宿主有 32 逻辑核，约 48 GiB 物理内存及 48 GiB 分页文件；默认 Cargo 全目标并发耗尽提交内存。`.cargo/config.toml` 仅固定 `build.jobs = 2`，不清缓存、不改 profile/特性/目标/链接器/RUSTFLAGS、不调整系统分页设置。相同 `cargo test --all-targets --features test-hooks --no-run --message-format=json` 退出 0、`build-finished success:true`。
- 第四轮 slowtest 首次嵌套失败为 `cancelling_git_move_cancels_cleanup_of_retained_git_tree_ancestors`：`tests/core.rs` 将取消行数写死为 1，实际正确取消了项目原根及祖先两条清理行。移除这一规划内部数量断言，不改为写死 2；增加对该夹具全部空目录清理行均取消的行为断言，保留不重新勾选 Git 移动、独立图片归类、重复重算幂等、Git 字节原样与未创建集合目录的检查。`cargo test --features test-hooks --test core cancelling_git_move_cancels_cleanup_of_retained_git_tree_ancestors -- --exact --nocapture` 退出 0、`1 passed; 0 failed`；基线仍 1003 项，未添加忽略或减少发现范围。
- 完整 slowtest 与三级失败即停结果以随后实际运行证据为准；本段不把局部回归、基线同步或源码复核冒称为完整门通过。产品代码与可信基不得混合提交；本次不提交、不推送、不触发 CI 或发布。

## 2026-10-13 全仓并行审查修复

### 本轮状态与授权边界

- 工作区：`D:\code1111111111\JchTools\JchTools`。本轮初始工作树干净，初始 HEAD 为 `4232fa51bb39cc2038c2b776afab48e81e2f2377`。
- 用户调用 `jch-parallel-review-fix`，要求全仓功能审查及修复、最大工作流并发。首批实际同时分派 32 个独立上下文子代理；随后补充 6 个数据依赖源码分区，累计 38 个任务，峰值不超过 32。
- 本轮审查/修复与源码接口复核完成；**完整 fulltest 首轮已执行但在 Clippy 失败，最终测试及功能验收尚未通过**。本节不将源码审查、库存摘要、Cargo metadata 或局部烟测称为全仓验收通过；完整门后续修复、提交与推送结果由主协调者按下方收尾记录更新。
- 早期技能阶段原本禁止测试/构建；宿主更高优先级要求缺陷反证和变更路径运行证明。主协调者已明确报告冲突，并集中执行定向 red/green 与实际源码烟测；子代理没有代跑测试、构建或最终验收。因此不能声称本轮“没有运行任何测试”。用户最新已明确授权本任务的完整 `fulltest`、全部必要修复与重跑、提交及推送，全流程自动完成，不再重复询问；早期阶段限制不适用于主协调者当前收尾验收。
- 审查阶段没有执行 fastcheck/fulltest/slowtest、完整发布打包、远程 CI、真实桌面驱动、全局鼠标/热键或真实模型验收；未自动安装依赖、清理 Cargo 缓存、升级依赖或改变 target/features/profile/linker。当前收尾已实际运行完整门，采用现行 **900 秒非编译共享预算**，不加早期 60 秒技能阶段的外部终止阈值；CI、发布及门外真实桌面/模型验收不由 fulltest 或提交推送推导。
- 先前用户另行允许可信基修复独立本地提交。实际提交为 `29b1b93b729076c1ce07b3e4cc322c1016af6bde` 和追加单文件提交 `09d3e1831fb5e4b2fdd085d657b069f60ae64fb1`，未 amend、push、触发 CI 或发布；产品与 vendor 源码修复仍留工作树，未夹带进可信基提交。最新授权已包含最终提交与推送，但尚无最终提交或 push 成功证据。
- 用户允许临时停止两项原后台以解除 EXE 锁。已用原有 `target/debug/JchTools.exe` 恢复 `--acp-http-service`（PID **75184**）与 `--codeagent-service`（PID **9940**），创建及身份核对退出 0；后续观测再次确认两进程持续 alive，ACP 在 **127.0.0.1:8765** Listen，监听者为 PID 75184（`local/jch-review-service-restore-observed.json`，退出 0）。这是原后台恢复证据，不是最新修复后的最终 GUI EXE 重编或新 GUI 验收。

### 实际审查覆盖与遗漏

|分区|实际覆盖|深度及保留边界|
|---|---|---|
|R01–R19|全部分配的产品 Rust、SQL、Slint、资源配置、根测试及两个可选组件|各负责人报告逐段/完整范围审查，核对需求与必要消费者；R08 共享推理、R15 ACP SDK、R17 后台没有保留修复。运行验收不由文件已读推导。|
|R20–R25|质量门、可信基、GUI/Markdown/OCR 验证工具、部署与打包、CI、安装器、字体/图标及使用文档|实际入口、判据、隔离、授权和分发审查；未逐份独立解码全部永久二进制媒体夹具，未重演历史验收。|
|R26|全部 751 包来源、许可材料与库存登记|审查阶段组合核账 timeout 保留；最新主协调收尾完成当前全量静态核账，准确计数和边界见本节最终记录。|
|R27|协议/异步 76 包|均有代表性公开实现、类型/模块 routing 或资料读取；活跃协议选定实现另深读，不是全部异步/加密/HTTP 算法审计。|
|R28|图形/媒体 83 包|64 包抽样实现、12 包入口/feature 合同、7 包 build 集成；不是 83 包全部实现逐行审查。|
|R29|格式/压缩 26 包|每包实际选定功能实现与消费者；不涵盖全部编码表、SIMD 或格式算法。|
|R30|原生平台 145 包|23 包有源码入口或选定实现记录；静态模型推断 22 包 Windows 相关，123 包 inactive/other-platform 未深审（其中一包仅短 build.rs）。实际可达性以 Cargo 特性图为准。|
|R31|数据基础 358 包|原轮 30 包选定功能源码、40 包仅 build 脚本、288 包 metadata/feature-only；Windows 图 286 激活/72 未激活，激活余项由 R33–R38 补读，不把原 metadata-only 升格为源码证据。|
|R32|构建/语言 63 包|每包选定函数、宏或 facade；11 个声明 build script 的主要/完整部分。深度不同，未审全部宏后端或语法。|
|R33–R38|追加审查原数据分区的 256 个 Windows-active 包：23 并发、10 解析、57 数学哈希、31 集合内存、35 文本区域、100 错误工具|逐包记录实际选定功能实现及已找到的消费者/未找到调用的边界；激活不等同生产调用，部分具名调用仅为上游 CLI、wasm 或 Unix。与原 R31 的 30 个源码包合计覆盖该分区 286 个激活包，不将原 metadata-only 记录计作源码证据。|

明确未覆盖：所有上游算法逐行、所有特性/平台、全部生成 Unicode/OpenType/Windows API 表、SQLite C 全文、汇编/SIMD 内核、全部宏生成后端，以及未激活依赖的完整运行语义。上述 751 包为依赖分区清单总数，不是 751 包每行源码均已审查；R33–R38 的追加覆盖也仅为逐包选定功能实现。没有重新下载全部原始归档或刷新漏洞公告，不宣称全上游源码或安全认证。按合同 P-07 保持 Windows 产品边界。

最新离线 Cargo metadata 实际退出 0：锁文件及解析图均为 754 项（751 依赖、3 workspace 包），其中 12 个本地 path 修补、739 个 registry 来源。没有同版本 source-replacement 缓存冒充新源码的问题：本地修补经 `[patch.crates-io]` 明确接入，原归档身份与当前有效来源分开登记。

### 本轮保留修复与回归文件

下表同时列出新增或修改的测试文件；源码内联测试包含在对应源码路径中。**除后述实际局部命令外，这些回归均待独立验收执行；修改文件数不等于通过测试数。**

|范围|保留修改路径|实际修复|
|---|---|---|
|整理/去重/命名|`src/engine.rs`、`src/planner.rs`、`src/dedup_components.rs`、`src/rules.rs`、`src/rules/signature.rs`；`tests/dedup_graph.rs`、`tests/classify_shape.rs`|other-name 规则包括同名及副本名；真实无尾斜杠 glob；隔离根不扫描；永久清理前置；摘要按 40 UTF-16 判断且强制降级不恢复来源；无扩展名 dotfile 不误清理/归档；实际 scan/plan_moves 跨度及安全失败原因。`tests/core.rs` 的临时重复新增已迁至库测试，原文件恢复。|
|解压/引擎|`src/archive.rs`、`src/engine_bundle.rs`；`tests/archive.rs`|同包目录冲突不混合其他成员；隔离每卷移动前响应取消；内嵌引擎固定位置被文件占用时使用同级 releases 容器且保护原文件。|
|文件/进程|`src/fsutil.rs`、`src/process.rs`；`tests/contract_paths.rs`|后缀仅裁 ASCII 点/空格，保留合法 Unicode 空白；stream 子进程先挂起入自有 Job，再恢复，终态回收后代和继承管道。|
|MD/转 Markdown|`src/md_tools.rs`、`src/markdown_document.rs`；`tests/md_tools.rs`、`tests/markdown_convert_gate.rs`|完整非数字段自然排序；跳过所有 reparse 类型；超长无换行输入分块响应暂停/取消；图片 title/括号/尖括号定位不错误改写字面量。|
|代理/下载/Git|`src/system_proxy.rs`、`src/snap_ocr_assets.rs`、`src/git_tools.rs`；`tests/git_tools.rs`|精确 bypass、IPv6 与协议回退；取消/落位失败清理本次 part；合并路径不 trim、阶段重试和实际失败输出；停止后不继续提交；仅当前命令系统代理优先于 remote 配置，bypass/关闭保持原优先级。|
|持久化/CodeAgent|`src/hash_cache.rs`、`src/xberg_settings.rs`、`src/codeagent.rs`；`tests/codeagent.rs`|COMMIT 失败 RAII 回滚释放锁；同一 SQLite 快照读取来源及目录；数据库初始故障不阻止后台维护，恢复后热应用间隔；非 Unicode 隔离变量 fail-closed。|
|GUI/日志/ACP|`src/gui.rs`、`src/logging.rs`、`src/acp_api/mod.rs`、`src/acp_api/settings.rs`、`src/acp_api/http/mod.rs`、`src/acp_api/http/stream.rs`；`tests/gui_flow.rs`|精确字节解析、解压快照及任务库关联、同来源 active 目录；转义敏感键去敏、INFO span 关联、后台角色及 mode；设置/HTTP 操作默认 INFO 与流 poll/Drop 保留 HTTP span；GUI 测试子进程隔离仍保留原断言。|
|OCR|`optional/snap-ocr-worker/src/main.rs`、`optional/snap-ocr-worker/src/service.rs`；`optional/snap-ocr-core/src/layout.rs`；`tests/ocr_fixtures/check_worker_root.py`、`tests/ocr_fixtures/compare.py`、新 `tests/ocr_fixtures/test_worker_root_isolation.py`、`tests/ocr_fixtures/README.md`|worker capability 明示 test_hooks；自启写入使用测试专属注册表键；非隔离 worker 在启动服务前拒绝；报告拒绝非法耗时/正文类型；规格参照物按数值平局继续次键排序，修复 signed zero。|
|质量门/可信基|`scripts/test_timing.py`、`scripts/xberg_test_engine.py`、`scripts/gate_runtime.py`、`scripts/test_gate.py`、`scripts/local_gate_plan.py`、`scripts/quality_scope.py`、`scripts/static_check.py`、`scripts/make_tmp.py`、`scripts/requirement_coverage.py`、`scripts/acceptance.ps1`；`scripts/test_test_gate.py`、新 `scripts/test_review_trusted.py`、`scripts/test-baseline.json`、`pyproject.toml`|共享剩余预算、UTF-8/BOM、损坏收据、清理中断失败；恢复既定 release 编译维度；临时目录物理边界；跨注释 test 属性；AH/CA 引用保持 UNVERIFIED；默认禁止门外桌面；新纯测试精准登记，不豁免生产脚本。|
|验收/分发|`scripts/gui_smoke.py`、`scripts/test_gui_smoke.py`、`scripts/markdown_acceptance.py`、`scripts/test_markdown_acceptance.py`、`scripts/media_e2e_fixture.py`、`scripts/package-windows.ps1`、新 `scripts/test_package_windows.py`；`tests/markdown_fixtures/README.md`|完成判据不误用旧快照、数字尾卷白名单；合成图片/Office 结构及媒体 URI、重复输出/覆盖保护；真实 Cargo executable 与对应 OUT_DIR 匹配；嵌套许可材料保留路径和字节、缺失拒绝。|
|依赖/说明|`Cargo.toml`、`Cargo.lock`、`.cargo/config.toml`、`vendor/SOURCES.json`、`THIRD_PARTY_NOTICES.md`、`README.md`、`先读我.txt`、`docs/xberg-local-integration.md`；独立可信基 `AGENTS.md`、`docs/requirements/ACP-MODEL-SERVICE.md`|精确 12 path 修补及原始来源追溯；cfg_aliases notice；八工具/CodeAgent/字体/默认主题、并行临时清理和现行授权说明。需求仅工作目录交叉引用事实同步，不削弱行为条目。|

本地依赖修补为以下 12 个包，全部保持原版本、原归档摘要与既有公开签名；`vendor/SOURCES.json` 登记 39 个真实 source/test/doc/inventory 文件：

|包|保留源码与回归路径（包目录内）|缺陷|
|---|---|---|
|`ureq-2.12.1`|`src/stream.rs`、`src/proxy.rs` 内联回归、checksum|SOCKS Condvar predicate；方括号 IPv6 代理。|
|`image-webp-0.2.4`|`src/decoder.rs` 内联回归、checksum|VP8L 最大合法 16384 维度被掩码成 0。|
|`fax-0.2.7`|`src/decoder.rs` 内联回归、checksum|越界垂直转换不得返回合法 Incomplete 白行；合法边界与 End 保持。|
|`blocking-1.7.0`|`src/lib.rs` 内联 subprocess 回归、CHANGELOG、checksum|空闲 worker 的退出 AND 判据。|
|`ttf-parser-0.25.1`|`src/parser.rs`、`src/lib.rs`、`tests/tables/main.rs`、CHANGELOG、checksum|TTC 长度 checked arithmetic、完整 offset 数组 count；畸形输入返回错误而非 panic/放大数量。|
|`strict-num-0.1.1`|`src/lib.rs`、`tests/signed_zero_hash.rs`、CHANGELOG、checksum|signed zero 相等值的 Hash 必须一致。|
|`zerovec-0.11.8`|`src/ule/{plain,mod}.rs`、`src/zerovec/{slice,mod}.rs`、新 `src/ule/const_constructor_tests.rs`、`tests/const_constructors_smoke.rs`、README、checksum|合法 u8、bool 校验及零尺寸空输入解析/构造；非空 unit 输入仍拒绝。|
|`ident_case-1.0.1`|`src/lib.rs` 内联回归、CHANGELOG、checksum|CamelCase 的 UTF-8 首字符及空结果安全边界。|
|`rspolib-0.1.2`|`src/moparser.rs`、`src/poparser.rs` 内联回归、checksum|MO seek/read_exact 错误传播；PO 读错误/非法 UTF-8 不是成功 EOF，open 失败不 panic。|
|`pxfm-0.1.30`|`src/pow.rs` 内联回归、checksum|零指数及单位底数恒等式先于 NaN 传播。|
|`error-code-3.4.0`|`src/system.rs` 内联真实转换回归、checksum|FormatMessage 缓冲失败直接 Truncated，不转换失败缓冲；私有借用生命周期只绑定输出。|
|`nu-ansi-term-0.50.3`|`src/windows.rs` 内联真实内核回归、checksum|自有 console HANDLE 所有返回路径 RAII 释放。|

### 实际验证与失败证据

以下是**早期技能阶段**的有限命令证据，当时使用 60 秒墙钟约束；两次 root timeout 和来源组合核账 timeout 原样保留，不以缓存预热或缩小矩阵隐藏失败。用户最新已授权完整 fulltest 及必要修复/重跑，当前收尾采用现行 900 秒非编译共享预算，不沿用早期 60 秒外部终止阈值。下述局部绿不替代项目门。

|命令/实际入口|观测结果|证明边界|
|---|---|---|
|各 owner 的 scoped rustfmt、Python AST/Ruff、PowerShell Parser、文档链接静态扫描|已完成的命令按分区收据记录退出 0；部分 Rust 只 `--emit stdout` 解析，不是严格格式检查|无源码改动后的全仓静态门通过声称；LSP 设备不可用。|
|`python -B -m unittest -v scripts.test_review_trusted` 加 R22 两项判据|0；10 项，1.784s|真实目的地/junction/属性/引用/桌面入口判据；不是全门或真实桌面。|
|R20 budget/UTF-8/cached identity/cleanup/BOM 七项目标回归|0；7 项|实际行为及文件解析；不是运行 fulltest/slowtest。|
|`python -B -m unittest scripts.test_package_windows -v`|0；13 项，5.036s；最新动态 resolver 专项另退出 0，0.607s|实际 PS collector/resolver 和许可复制；没有完整打包/安装。|
|R23 十个新回归及既有 CLI 拒绝反例；实际 BMP/PPTX/PNG 生成 smoke|0；11 项；P1/P4 像素一致、ZIP CRC/XML/GUID 合法、URI 源图可达|合成夹具与验收工具，不是真实转换/模型。|
|OCR 隔离 capability 回归及 `compare.py --self-test`|0|不启动生产 worker 服务；不证明真实注册表服务/热键/模型 E2E。|
|当前 19 个真实模块的 source-only `prepare_at`→`apply`→再次 `prepare_at`|0；6.272s；scanned=8、planned_delete=2、moved=5、removed_items=10、permanent_bytes=13、hash_read_bytes=8、execution_read_bytes=0、幂等=true|不链接旧 JchTools rlib、不复制产品函数或 stub。真实磁盘 scan/plan_moves 关闭事件各 3；失败原因落盘且输入保留。不是 GUI/ACP/model/全应用构建。|
|当前 CodeAgent/settings/logging 源码原生 smoke|编译 0、无 warning；运行 0，2.50s，三个 PASS|真实 CurrentUser DPAPI、测试专属 REG_SZ、SQLite 初始故障/恢复、保存间隔、安全状态、停止/锁释放/去敏；不访问真实凭据键，完整原 integration 与最终 GUI 仍未验证。|
|真实新 path `ureq` public API：IPv6 loopback HTTP/SOCKS5|0，1.39s；代理 accept、HTTP 请求、SOCKS 目标、200 和 exact sentinel 均通过|目标探测实际 TimedOut、不等于 ConnectionRefused；业务断言未豁免。真实 JchTools 下载/TLS/公网/Agent 未验证。|
|新 path WebP encoder→decoder；fax Group4 public API；TTC count/RawFace/Face|分别 0；最大维度四形状回环、畸形 fax Err(Invalid) 与合法控制、count None/MalformedFont|旧相应入口均实际退出 101；没有执行旧 fontdb 巨大分配。整 TIFF/字体 GUI/判型消费仍待验收。|
|新 path strict-num 外部 HashMap；zerovec 独立动态公开 API|0；分别 3 项、6 项|旧分别 2 failed/1 control passed、4 failed/2 controls passed；zerovec 独立 const 求值尚待执行，不能用动态测试代替。|
|R33 完整实际 `blocking` 源码的公开 `unblock` subprocess 回归|旧 AND 101、5.98s，worker/idle 各 1 未回收；新 OR 0，parent/child 各 1 passed（0.63s/0.55s），runner 总 1.31s|真实公共 unblock 与实际 Executor counter，不是假 executor；不证明 ACP/全应用链路。详见 `local/jch-review-R33-result.md` 最新 Main 记录。|
|R37 当前 `vendor/ident_case-1.0.1/src/lib.rs` standalone `rustc --edition=2015 --test`|0；4 passed / 0 failed：原 rename_variants/rename_fields 两 ASCII 控制及新 UTF-8/empty 两回归；旧两负例各 101|当前完整源码的四项定点测试，保留原 AsciiExt warning；不等同 root GUI/完整门。详见 `local/jch-review-R37-result.md` 最新 Main 记录。|
|R19 完整 `snap-ocr-core` 源码 direct `rustc --test` 后 exact `layout::tests::signed_zero_coordinates_follow_numeric_tie_breakers`|编译 0、执行 0；1 passed / 61 filtered，runner 总 1.04s；旧 101，BA != AB|仅 signed-zero 数值平局规格回归；61 filtered 不是通过项，不证明包级全测试、真实 Xberg 或桌面 OCR。详见 `local/jch-review-R19-result.md` 最新 Main 记录。|
|完整真实 error-code root + system 原生输出回归|旧 101（128 个 X 泄漏，2 控制通过）；新 0，3 项|真实 WideCharToMultiByte；安全初始化哨兵反证，不声称实际读取了未初始化内存。|
|完整真实 nu-ansi-term Windows 模块 + NUL 原生句柄回归|旧 101（GetHandleInformation 仍为 1，1 控制通过）；新 0，2 项|真实内核关闭及 ERROR_INVALID_HANDLE；不创建控制台窗口，不证明 GUI 调用了 enable_ansi_support。|
|R38 原源码 MO/PO/pow 与新 path 库相同六项 public API 对照；`jch-review-R38-original-library-runner.py` + `jch-review-R38-public-original-probe.rs`|runner 0，5.859813699993538s；原源码 MO/PO/pow 三负例各 101，三正例各 0；新 path 六项均 0|旧负例分别未返回 CorruptedMOData、未返回 Custom SyntaxError、NaN != 1.0。完整实际 src 副本仅替换原快照，依赖/OUT_DIR 用真实 Cargo 产物，无随机旧缓存、stub 或前提失败冒充 red。原完整包全部 UT（含缺失 rspolib_testing 前提）仍未执行。收据 `local/jch-review-r38-original-api-evidence.json`、完整输出 `artifact://449`。|
|`cargo metadata --locked --offline --format-version 1`|0；754 身份、3 workspace、12 path/739 registry；均无重复身份且路径匹配|是实际依赖解析，不是构建或运行。|
|`cargo build --locked --offline --features test-hooks --bin JchTools --message-format=json`|第一次真实 E0106/101，私有生命周期已修；随后同 variant 两次各达到 60s，未产生完整新 root App/lib 收据|12 个新 path leaf 库实际已编；早期未再重跑，最终 GUI/主库构建保持未验证；当前完整门按最新授权及现行预算承接，不能静默重跑到绿。|
|R26 最终 metadata/lock/provenance/current-inventory 组合静态核账|60s timeout，kernel 130，无完成收据；未重跑|changed-only JSON/单文件摘要成功不替代全量核账，不冒报 SOURCE PASS。|

Source-only 核心准备期间三个失败均留证：缺 Windows native import 搜索路径（4.708s、link 1）；runner 自加目录存在检查拒绝 Cargo 的可选 atlmfc 搜索路径（0.005s、未编译）；repo `.tmp` 输入被真实 H-06 Git 祖先保护拒绝（5.110s、runtime 101）。只修协调脚本输入/前提：原样传真实 Cargo native 路径；输入使用项目允许的测试内置 TempDir 例外、状态/日志/产物仍 `.tmp`。没有改产品 Git 保护、移动 `.git`、伪造 fallback 或抑制源码烟测的 135 个未引用 warning。

### 接手验收的未完成项

1. **当前完整编译及静态门**：由主协调者执行已授权的 `python scripts/test_gate.py fulltest --authorized`，包含 fastcheck 同项及现行完整适用覆盖；保留既定 dev/release、features、target、缓存和编译观察口径。采用 900 秒非编译共享预算，不另加外部终止阈值；早期两次 60s root timeout 不当作语法成功或测试成功，也不阻止已获授权的必要修复/重跑。
2. **全部真实回归**：表中新增/修改文件及源码内联测试全部执行。核心、解压、MD、Git、持久化、GUI、ACP、OCR 原 integration 与可选组件尚未完整 green；先红证据只证明对应缺陷，不代表整 target 完成。
3. **主入口**：`cargo test --features test-hooks`；可选 core/worker 分别 `cargo test --features test-hooks --manifest-path optional/snap-ocr-{core,worker}/Cargo.toml --all-targets`。重点 target 为 `dedup_graph`、`archive`、`contract_paths`、`md_tools`、`markdown_convert_gate`、`git_tools`、`codeagent`、`xberg_settings`、`acp_settings`、`acp_http_contract`。实际产物和公开结果断言不可用 mocks/旧 EXE 替代。
4. **vendor 特殊入口**：rspolib normalized dev-dependencies 缺 `rspolib_testing`，包内 tests 直接编译有前提缺口，不自动安装；使用完整真实源码/公开 consumer 后再处理合法包验收。zerovec `autotests=false` 保持原样，`tests/const_constructors_smoke.rs` 须显式执行，独立 `src/ule/const_constructor_tests.rs` 的 const 求值另验。ttf `tests/tables/main.rs` 及 fax 整 TIFF consumer 不以裸 public decoder smoke 替代。
5. **来源与分发**：重新完成 SOURCES/current inventory/provenance 核账，再验证 nested notices、真实 Cargo OUT_DIR、完整内嵌状态、ISCC/ZIP、无引擎泄漏及两种交付运行。metadata 12path/754 身份及局部 PS 回归不能证明完整打包。
6. **授权与门外边界**：用户最新已明确授权本任务完整 fulltest、必要修复/重跑、提交推送的自动收尾，无须再申请或停在阶段边界；没有请求本轮 CI 或发布，不继承历史发布许可。真实桌面/GUI/截图热键及真实模型保持门外独立授权边界；`-WithDesktopTests` 只是入口声明，不创造授权。必要重跑须保留首失败与根因、固定原矩阵，不静默重跑到绿。
7. **基线**：已独立审计并定向登记 1029→1089，60 add/0 remove/16 body 及一项既有 cfg 修正；没有机械 `--update-test-baseline` 吞未知变化。1089 是库存数，**不是 1089 tests PASS**。四项旧 dedup 错误预期按 C-02 修正，TTC panic 预期强化为错误返回；反证/理由保留，不能重新放宽。

全部分区详细收据在本会话 `local/jch-review-R01-result.md` 至 `R38-result.md`；其绝对目录为 `C:\Users\jiang\.omp\agent\sessions\--D--code1111111111-JchTools-JchTools--\2026-10-13T04-07-10-643Z_01a13371-6033-71a0-8cf0-7d310ff7cacc\local`。该目录保留协调源快照及运行收据，不属于产品包。以上本节已列工作区、主要变更、回归文件、命令、实际失败和验收边界；会话 `.tmp` 临时编译/夹具由主协调清理，不留下伪永久测试或扩大清理到其他会话。

### 主协调者最终收尾记录

本表记录主协调者的实际验收和交付结果；尚未发生的阶段保持未完成，不把局部绿或库存数当作 PASS。历史失败及上述局部证据不删除。

|结果字段|当前准确状态／须回填证据|
|---|---|
|完整 fulltest|最终 `python scripts/test_gate.py fulltest --authorized` 退出 0、PASS；`total=1278.6s / compile_excluded=507.4s / budgeted=771.2s / limit=900.0s`。完整适用 Windows 本地门、真实引擎、媒体链路及安装包/便携包打包均通过。最终收据保存 `local/jch-fulltest-final-pass.json`；此前五次失败仍保留。|
|失败修复与必要重跑|直接修复 Clippy 整数字节判定、临停已确认占用的工作区 OCR 后台、ACP 日志字段比较、临时日志 subscriber 的 callsite 兴趣缓存，以及真实 GUID reparse 目录夹具必须为空的前提；不改业务重试次数、预算或断言。最后 MD 回归 27 项及主库整 target 均通过。1089 仍只是基线库存数。|
|覆盖及交付边界|来源核账、门内 UT/integration/doc/e2e、可选组件、真实引擎及打包已通过；生成 `JchTools-Setup-x64.exe` 和 `JchTools-Windows-x64-20261013-183458.zip`。真实桌面/模型 Agent 及完整上游依赖包 UT 不在本次通过结论内。用户要求停止额外历史反证重建与专项测试，未执行部分不改写为 PASS。|
|最终提交|按可信基、依赖修补、应用修复分别提交；测试基线仅更新 ACP 字段比较和 MD 夹具的两个已审查指纹，无删除、ignore 或断言削弱。实际提交标识以本轮 Git 历史和交付回复为准。|
|推送|目标为 `origin/main`；交付前以远端 `refs/heads/main` 与本地 HEAD 一致确认推送结果，未请求 CI 或发布。|
|CI / 发布|本轮未请求、未执行；不作为已授权 fulltest/提交推送的自动增项，也不复用历史 run 声称本轮成功。|
