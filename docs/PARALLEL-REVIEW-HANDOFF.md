# 全代码并行审查修复交接

## 状态与工作区

- 工作区：`D:\code1111111111\JchTools\JchTools`，Windows 10.0.26200 x64。
- 初始工作树无既有改动。全仓代码分给25个独立审查负责人；后续3个独立验证准备负责人。本轮累计28个，符合用户最后指定的30上限。各产品文件单一修改负责人，现全部停止写入。
- 审查修复已落盘；整体验收未通过。最终 `python scripts/static_check.py` 返回1，失败项为 `test_baseline`；其他项通过，285个跟踪文件的SHA256与工作树一致。
- 无commit/push、依赖安装、需求条目修改、测试基线再生成或完整测试门执行。可信基脚本与产品改动须按项目约定分开提交，不能混成一笔。
- 本文只是交接，不是需求权威。需求仍以 `docs/requirements/` 四份文档为准。

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

1. **基线审批受阻**：`scripts/test-baseline.json`未改。项目§10要求先给新旧基线并排做是/否确认；用户禁止本轮提问，不能把笼统权限授权冒充该特定比较确认。验收Agent取得该确认后再独立更新基线与提交记录；不删除回归、不放宽断言让static_check变绿。
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
