# 2026-10-15 全业务代码/测试：审查修补接棒

## 后续缺陷修复与运行验证

- **最终结果：第三次同命令 fulltest PASS，退出码 0。** `total=1294.1s / compile_excluded=545.6s / budgeted=748.5s / limit=900.0s`。静态/格式/质量、全部适用 Windows 生产/optional 单元与集成/文档测试、真实 7-Zip、真实 Xberg 媒体、安装包与便携包全部通过；不包含下述独立授权边界。
- 运行时日志位于 `.tmp\test-gate\fulltest`，打包 `095-package.log` 曾确认安装包和 `JchTools-Windows-x64-20261015-075033.zip` 成功生成；收尾清理后这些临时日志与产物不再保留。本文件保留命令、退出码、关键输出、反证及计时；完成上述验收时尚未提交、推送或发布。
- 后续 8 路 diff 复核指出的 13 项缺陷已修补：测试夹具路径、PowerShell 5.1 两个验收入口的根数组展开、解压正式空目录保留、复合流暂存父容器所有权、停止后连接准入、Markdown 跨行代码/无效普通链接、OPC 部件大小写/合法冒号、引用容器围栏、C05 失败优先、精确 cfg 属性、UIA 按钮类型。
- 每组行为修复的修复前失败与修复后结果已汇总在本文件。Xberg 首次尝试因生产截图后台占用引擎而停在启动前，不作为反证。经本次允许临时停止并恢复后台后，实际观察到 54 个停止后的不完整连接阻止代理退出，再修正准入。
- Rust 验证退出码 0：`cargo test -p jchtools-file-core -p jchtools-markdown --features test-hooks --lib review_fix_ -- --nocapture --test-threads=1`（6 项）、目录整理 `signature_detection_cancellation_is_not_swallowed`（1 项）、根 package 的 `core` / `contract_engine` / `split` / `markdown_convert_gate` / `xberg_shared_process` 中 `review_fix_`（9 项）。
- 公开入口验证覆盖 `detect_extension → prepare → apply`、真实 7-Zip `extract_run_at`、`markdown::run` 的真实代理/合成引擎和实际命名管道停止/PID 回收。断言包含准确产物/源文件字节、空目录/用户父目录保留、临时容器清理、终态交付；合成模型不等于真实模型验收。
- Python 三个相关模块的 169 项均有通过结果：转 Markdown/可信检查 143 项通过且输入未变；7 项打包夹具首次受宿主 PowerShell 7 模块路径污染而失败，修正进程级环境后 `python -m unittest scripts.test_package_windows` 26 项在原宿主环境下通过。复用现有 `_ps51_module_path` 策略，未改系统环境或断言。
- Rust/Python 改动文件已在写入者全部停止后集中格式化；生产 Python Ruff 与 UIA LSP 类型检查通过。`python scripts/static_check.py --update-test-baseline` 已登记 1158 项，随后静态检查退出码 0；既有死 SQL helper 的唯一实现一致性测试按原审查决定移除，无新增 ignore。
- 第一次授权 `python scripts/test_gate.py fulltest --authorized` **失败**：静态、格式、Python 质量、dev/test/release 编译已通过；Clippy 在解压局部命名/末尾 continue 和 ACP 错误所有权三处失败。计时 `total=390.9s / compile_excluded=341.4s / budgeted=49.5s / limit=900s`，完整覆盖尚未进入。仅修正三处实际失败，未增加 allow 或缩减门，第三次已通过。
- 三处 Clippy 修正后，同一生产 Clippy 命令通过。ACP 既有测试随后暴露格式文本偶然依赖；已以局部 Layer/Visitor 原始字段捕获替换，未重钉格式器引号。准确去敏协议错误、单条 ERROR、event/component/stage/真实 I/O 类型和正文不泄露断言全部通过；解压公开回归 3 项复跑通过。基线仍为 1158 项，仅登记新测试体哈希。
- 第二次同命令 fulltest **失败**于 `dedup_graph::independent_keeper_does_not_delete_existing_hardlink_pair`（计划删除 2 个，预期 1 个）；并行被终止/未执行项不记通过。计时 `total=431.2s / compile_excluded=273.9s / budgeted=157.3s / limit=900s`。原生 NTFS 实验确认 A/B 为同物理文件、句柄 mtime 均 10 秒，但 B 目录项仍报创建时刻。共享 `fsutil::snapshot_with` 现复用已有句柄信息统一取 size/mtime/ctime/identity/links，不增加系统调用、扫描或哈希，不补写夹具时间隐藏问题。`dedup_graph` 7 项和既有 1601/纪元前时间边界 6 项退出码 0。
- 真实焦点/桌面、真实 OpenCode 模型和发布仍在门外，未报告通过。生产截图后台已由现有 GUI 恢复，控制管道实际返回 `ok=true / model=ready / exiting=false`，快捷键仍为 `Ctrl+Alt+J`，开机自启仍为 false；显式重复启动干净退出 0。已核对原配置保留，不再启动验收或增加检查。
- **收尾失误：主代理误将 `make_tmp.py clean --destination <本次子目录>` 当成定向清理。当前脚本的 `--destination` 仅适用于 testdata/workspace，clean 始终清理整个 `.tmp`；因此其他临时目录、完整日志和打包产物也被移除。** `.tmp/test-gate-packages/installed` 的正在运行程序遭文件占用拒绝删除，命令退出码 1，已立即停止，不再尝试全量清理。代码、`target` 编译缓存及应用状态目录不在该命令清理范围内。未为了补造已删除产物重新执行门，也未宣称临时日志仍可获取。
- 下文为最初技能阶段的历史覆盖及未执行清单；不得把其静态结果替代本次运行证据。

## 结论与权限边界

- 工作区：`D:\code1111111111\JchTools\JchTools`。修补已留在当前工作树；未提交、推送、发布。初始工作树无改动，基线 `606ba5bd9aea975f53b52a8b58b96da6c3eb663e`。
- 审查/修补阶段已完成；不存在未完成worker、分配源文件遗漏、未决定二轮或未闭合接口依赖。**不是测试验收通过或已证明修复。**
- 初始208个自有源/测试/配置文件，26个功能任务；并发上限32，首次实际并行26；10项有具体原因的定向二轮，另按实际依赖复查。新增OCR回归文件与相应README也已审查。
- 覆盖根入口/构建源、14生产crate、2 optional OCR crate、Slint UI、自有内联/集成/夹具测试、测试/验收/打包/安装/CI脚本和类型桩。排除第三方vendor、生成物、缓存及二进制样例；二进制夹具仅只读参照。
- 主代理只分区/协调/核对结果，未审查源实现或接管修复；使用写入并执行的Eval/agent调度脚本，独立上下文，每文件同时仅一修改所有者。下列检查是子代理实际报告，非主代理重跑。
- **技能按设计没有运行任何测试、构建、fastcheck或最终验收。** 新增/修改测试、红/绿反证、真实入口smoke、项目强制门全部交独立验收Agent。宿主/项目要求运行证明与技能边界冲突，验证阶段保持待执行，不声称已满足。
- fulltest/slowtest及重跑需当次明确授权；真实焦点/桌面/模型/网络/发布权限不继承。桌面入口另获指令，不占用硬件鼠标。

## 完整覆盖与二轮决定

|任务 / 最终所有者|覆盖范围|二轮决定与闭环|
|---|---|---|
|R01 / R01Pass2|共享设置 SQLite、日志、代理与性能生命周期，以及相关测试（10文件）|是：定向二轮独立核对并恢复原所有者最终角色修补证据：jchocr 实际可执行身份识别、后台 flag 优先与产品品牌保持；无额外修改，角色后 rustfmt 已为0。|
|R02 / R02Pass2|文件核心数据、规则、哈希、配置与数据库不变式，以及 core 集成测试（12文件）|是：独立定向二轮确认 OOXML 关联/C-04/死代码切换，并补正关联 URI authority/空段/目录尾段缺陷；真实比较行为回归保留、当前生产 Rust 格式检查成功。|
|R03 / R03Pass1|文件路径、扫描、控制、进程、任务和文件系统安全，以及路径与边界测试（10文件）|是：定向二轮共享根/祖先及规范编号 marker 边界已落地，Git 优先和无分配 UUID 比较保持安全契约；R04/R05 最新消费者事实一致，全部当前文件静态检查成功。|
|R04 / R04Pass1|递归解压、7-Zip 内嵌与获取运行接口，以及归档、分卷和合同测试（9文件）|是：定向二轮完成实际根/后代拒绝、Git 优先、同卷无覆盖编号备用容器和可取消选择；12项回归与4文件最终静态命令完整，最新共享边界复核闭合。|
|R05 / R05Pass1|目录整理计划、去重、执行及分类和性能探针测试（8文件）|是：定向 C-04 双硬链接保护、候选外已知硬链接与工作区空目录遍历完成；R02 强合同旧断言、R03 最新根/marker 和 R04 规范同卷 fallback 均已闭环。|
|R06 / R06Pass1|Markdown 合并、无损硬限制拆分与测试（4文件）|是：定向取消 cutover 已完成且 35 个自有调用/2 个 GUI 同任务 control 确认，无兼容包装或剩余依赖阻塞。|
|R07 / R07Pass1|逐文件 Git 提交推送、重试停止、staged 保护与测试（4文件）|否：staged 删除/rename 源保护和双流日志都有对应行为回归，无未解决接口影响。|
|R08 / R08Pass1|共享 Xberg 请求、Windows broker、协议隔离及相关合成引擎和测试（11文件）|否：退出竞态采用私有 RAII 覆盖已存在 15 秒有界交付；可观察终态回归明确、OCR 最新依赖复核兼容，无证据需二轮。|
|R09 / R09Pass1|资产下载、校验、原子落位与截图资产安装（6文件）|否：暂存文件扩展名碰撞修复局部，取消保留回归明确且格式检查成功；无契约变化。|
|R10 / R10Pass1|所有转 Markdown 实现、格式清单、媒体 E2E 和合成媒体夹具（12文件）|否：普通链接 title 保护局部修复，label 真图片路径与公开批处理回归均保留，无未决接口问题。|
|R11 / R11Pass1|真实 GUI 装配、注册表、Slint UI/生成入口及 GUI 流程/日志/编译合同测试（11文件）|否：一轮全部 GUI/UI 范围完整；MD 同任务取消 API 与关闭分支已修正，最新 R08 有界 drain 的全部 GUI 生命周期消费者兼容且无同步主线程等待。|
|R12 / R12Pass1|CodeAgent DPAPI/注册表维护、持久配置及后台状态与测试（4文件）|否：首次注册表写入和后台零等待争用修复均已完成，最新共享 opener 消费复核闭合，原断言保留、最终格式检查成功。|
|R13 / R13Pass2|jchocr 公共 CLI、进程入口和独立集成测试（5文件）|否：CLI 一轮安全诊断修补完整；最终 R01 实际文件名角色和 broker 优先规则已针对两个初始化路径复核兼容，无本地第二轮修补需要。|
|R14 / R14Pass1|ACP 官方客户端会话、回调、终端、公共类型及回调/提示摘要测试（9文件）|否：一轮去敏真实 I/O 原因修复完整；手写 SSE 依赖迁入最新 assert_stream_end，逐事件断言全部保留、帧尾覆盖闭合。|
|R15 / R15Pass1|ACP OpenAI HTTP、SSE、连接取消、DTO 与 HTTP 契约/断连测试（6文件）|否：一轮无确定缺陷；已逐一复核 R17 最新完整帧消费者依赖，无兼容问题。|
|R16 / R16Pass1|ACP 设置、Windows 服务进程、IPC、发现环境及生命周期测试（8文件）|否：一轮无确定缺陷；全部 7 个 finish_stream 与发现消费者对最新夹具复核兼容。|
|R17 / R17Pass1|ACP 合成 Agent/消费者夹具以及真实 OpenCode 验收测试与辅助代码（3文件）|是：定向二轮提取唯一 tail-completion 判据，R14 实际手写消费者已接入，R15/R16 旧消费者复核兼容，未改变产品协议。|
|R18 / R18Pass1|截图 OCR 冻结逻辑规格参照及全部内联测试（10文件）|否：完整冻结规格和内联测试覆盖，未发现确定缺陷，无变更，不重复审查。|
|R19 / R19Pass1|截图 worker 服务、托盘热键、捕获、Xberg 客户端、结果窗及内联测试（12文件）|否：尺寸溢出修复局部；针对 R08 最新有界 handler 交付修复的消费者复查已明确兼容，未决事项仅验收执行。|
|R20 / R20Pass1|OCR 对照/资产/隔离验收脚本及测试（8文件）|否：无需第二轮：schema/代理隔离/旧状态修复均有独立行为回归；R21 最新 gate discovery 和角色归类已完成，剩余既有 noqa 静态诊断与实际验收已明示。|
|R21 / R21Pass1|测试门/Job 运行/计划/计时/报告/质量角色划分及其全部自有测试（9文件）|是：定向二轮将新增 OCR GUI 回归纳入完整门发现和纯测试角色，最终语法/Ruff 检查成功；一轮已补漏 fetch UT，取消清理修复保持有效。|
|R22 / R22Pass1|静态规则检查与需求覆盖映射及可信基检查测试（3文件）|否：词法修复与包引用覆盖都有独立反例；R26 两类破坏性边界回归已纳入，最终语法/Ruff/类型检查成功。基线重生成仍待独立验收。|
|R23 / R23Pass1|真实 GUI smoke 自动化、相关断言单测及 pywinauto/comtypes 类型契约（15文件）|是：定点第二轮返回类型对齐与自有调用显式忽略完成；R24 外部消费者最新定点类型/Ruff已通过，依赖闭合。|
|R24 / R24Pass1|转 Markdown 真实验收运行器与其全部测试（2文件）|否：一轮六项具体回归和语法/Ruff通过；R23 返回类型依赖的显式忽略已落地，最新 basedpyright/Ruff 为0。|
|R25 / R25Pass2|主程序入口、根 Cargo/资源生成、打包/安装/CI/获取引擎/验收入口与相关测试（14文件）|是：定向二轮完整确认默认打包/WithPackage 非桌面 artifact 分类及显式独立桌面入口；沿用未改 classifier，当前 PS/Python/嵌入驱动语法检查为0。|
|R26 / R26Pass1|测试夹具目录工厂、测试引擎准备、公共进程清理辅助及图标生成脚本（5文件）|否：原路径 reparse 与字典 pre-clear 校验修复局部，全部静态检查成功；R22 两类回归依赖已经完成，执行与可信基重登记仍待验收。|

## 实际修补与静态检查

所有命令在Windows执行并限定60秒；0只证明该静态命令，不能替代Rust编译、Slint实际surface或运行证明。已处理的命令错误/格式失败和最终结果均列明，未重复未变输入检查。

### R01
- 改动文件：crates/foundation/src/{xberg_settings.rs,logging.rs}。
- 修补/结论：默认 INFO 记录设置起止/耗时/关联；open_with_timeout 在 schema 前设置等待，旧 open 保留5s；普通 jchocr 按实际文件名识别，broker 优先，品牌不变。
- 静态检查：rustfmt --check --edition 2021 两个修改源文件=0；角色修补后 logging.rs 同命令=0（第二轮恢复原宿主记录，未重复运行）。
- 测试接棒（全部 **UNVERIFIED**）：logging.rs：settings_operations_are_observable_at_default_info_level、cli_process_role_uses_executable_identity_and_preserves_broker_mode；xberg_settings.rs：caller_selected_busy_timeout_preserves_default_and_allows_fail_fast。

### R02
- 改动文件：crates/file-core/src/{rules.rs,rules/signature.rs}；tests/core.rs。
- 修补/结论：WAV PCM/float 位宽、帧/字节率和完整采样校验；OOXML 唯一内部 officeDocument 关联/MIME/主XML一致性，拒绝 URI authority/空段/目录尾段；C-04 改为保护双硬链接并保留独立删除/字节断言；移除闲置 SQL helper 与其唯一实现一致性测试。
- 静态检查：rustfmt --edition 2021 --check --config skip_children=true rules.rs signature.rs=0；URI修正后 signature.rs=0。core.rs 曾因纯测试/既有格式差异=1，非严格格式必验项；未冒报编译通过。父代理删除前 LSP references 成功：仅声明及唯一测试。
- 测试接棒（全部 **UNVERIFIED**）：signature.rs：wav_pcm_requires_consistent_sample_layout、ooxml_main_relationship_resolves_internal_package_paths；core.rs：review_signature_wav_invalid_pcm_preserves_original_extension、review_signature_ooxml_requires_package_office_document_relationship、permanent_bytes_exclude_hardlinked_sources（加强）。删除 ordering_sql_agrees_with_rust_compare，保留真实 ordinal compare 回归。

### R03
- 改动文件：crates/file-core/src/{fsutil.rs,scan.rs,task.rs}；tests/{contract_paths.rs,organizer_fs_boundaries.rs}。
- 修补/结论：复制清理只在 create_new 建立实际所有权后执行；实际 OS 系统目录取代可覆写 SystemRoot；精确标记工作区/Git 优先/污点传播；普通同名资料纳入；共享 normalizer 拒绝工作区根与后代，覆盖 prepare/count/extract/apply/GUI；UUID 比较用栈缓冲。
- 静态检查：rustfmt --edition 2021 fsutil.rs scan.rs task.rs=0；最终 fsutil.rs task.rs=0（scan未再变）；rustfmt --emit stdout 两个修改测试文件=0，只作解析。
- 测试接棒（全部 **UNVERIFIED**）：fsutil.rs：workspace_detection_requires_matching_owner_and_plain_content_directory、workspace_namespace_and_selected_root_boundaries_are_exact；contract_paths.rs：system_directory_protection_ignores_overridden_systemroot；organizer_fs_boundaries.rs：ordinary_workspace_named_directory_is_processed_without_touching_marked_workspace、ordinary_workspace_named_file_is_processed、prepare_rejects_selected_marked_workspace_and_descendants。新 helper 回归从 contract_paths 搬至内联，原断言保留，公开反证可独立移植基线。

### R04
- 改动文件：crates/recursive-extract/src/{archive.rs,engine.rs}；tests/{contract_engine.rs,split.rs}。
- 修补/结论：根/重复根条目计入限额；去掉 P-08 外部指纹重查，保留入队 UNIQUE；清理失败显示残留且保留 OWNER；普通同名成员允许解出，实际工作区保护/Git优先；固定父被文件/链接/Git阻挡时同卷独占编号备用容器，不触碰占用，不删旧父；每个候选 checkpoint，无百万上限。
- 静态检查：rustfmt --edition 2021 --check archive.rs engine.rs=0；最终 archive.rs=0；rustfmt --config disable_all_formatting=true --check contract_engine.rs split.rs=0，最后修改的 contract_engine.rs 单独=0。
- 测试接棒（全部 **UNVERIFIED**）：archive.rs 9项：root_directory_entries_count_toward_entry_limit、staging_cleanup_failure_reports_location_and_reason、queued_fingerprint_is_not_rechecked_before_extraction、ordinary_workspace_named_members_are_extracted、marked_workspace_destination_uses_a_new_directory、git_boundary_precedes_workspace_marker_reads、staging_avoids_file_and_git_container_occupants、staging_preserves_preexisting_plain_container、cancelled_staging_does_not_create_a_container。contract_engine.rs：ordinary_workspace_named_directory_remains_in_extraction_scope、extraction_rejects_selected_marked_workspace_and_descendants。split.rs：valid_empty_archive_survives_occupied_staging_container（合成空包引擎，不是真实7-Zip证明）。

### R05
- 改动文件：crates/directory-organizer/src/{planner.rs,engine.rs}；tests/{dedup_graph.rs,classify_shape.rs}。
- 修补/结论：取消签名测试 include_bytes 路径纠正；可靠硬链接保护与 keeper 无关，候选外已知同物理路径也保护，不扩大哈希/内容读取；最终空目录遍历使用同一工作区谓词；R02旧断言、R03边界、R04备用命名已闭环。
- 静态检查：修改源文件 rustfmt=0；最终 rustfmt --check --config disable_all_formatting=true planner.rs engine.rs dedup_graph.rs classify_shape.rs=0（解析，不是编译/测试）。
- 测试接棒（全部 **UNVERIFIED**）：planner.rs 既有 signature_detection_cancellation_is_not_swallowed；dedup_graph.rs：independent_keeper_does_not_delete_existing_hardlink_pair、hardlink_outside_name_candidates_still_protects_its_peer；classify_shape.rs：ordinary_work_directory_is_processed_but_owned_workspace_is_preserved、numbered_workspace_wrapper_protects_only_marked_children。

### R06
- 改动文件：crates/md-tools/src/md_tools.rs；tests/md_tools.rs。
- 修补/结论：输出排除改 Windows ordinal，避免 Unicode 展开误排；分隔线不误作 Setext；fence 支持 Tab 尾缀；scan_markdown/plan_splits clean cutover 增 Control，扫描逐项/规划起止及每512边界检查；35个自有调用及2个GUI同任务Control迁移，无包装兼容层。
- 静态检查：最终 rustfmt --check --edition 2021 生产 md_tools.rs=0；AST迁移及当前caller定向检查完成。
- 测试接棒（全部 **UNVERIFIED**）：md_tools.rs 集成新增：scan_keeps_distinct_unicode_name_when_excluding_output、merge_keeps_thematic_break_before_setext_underline、merge_closes_fence_with_trailing_tabs、scan_honors_cancelled_control_before_reading_inputs、split_plan_honors_cancelled_control_before_planning；既有测试仅迁移参数。

### R07
- 改动文件：crates/git-tools/src/git_tools.rs；tests/git_tools.rs。
- 修补/结论：保护 staged 删除后同名重建，以及 rename 新旧全部提交路径；add/commit非零退出保留 stdout/stderr 两条界面日志。
- 静态检查：生产源 rustfmt --check=0；测试 rustfmt --emit stdout=0，仅解析。
- 测试接棒（全部 **UNVERIFIED**）：staged_deletion_with_recreated_worktree_file_is_protected、staged_rename_with_recreated_source_is_protected、add_and_commit_failures_preserve_both_output_streams。

### R08
- 改动文件：crates/xberg/src/xberg_runtime_windows.rs；tests/xberg_shared_process.rs。
- 修补/结论：私有 RAII active-handler 计数覆盖终态写入/既有15s刷出；停止不再在 pending 清空但终态未交付时退出。协议和任务 drain 不变，无新无限等待；OCR/GUI消费者最终兼容。
- 静态检查：生产 rustfmt --check 与测试 --emit stdout 完成无诊断；工具未显示数值退出码，不能虚报0。
- 测试接棒（全部 **UNVERIFIED**）：background_stop_waits_for_outstanding_terminal_delivery：未读完成终态时停止仍保活，准确终态读取后退出。未读/断开客户端有界退出仍须实际证明。

### R09
- 改动文件：crates/assets/src/snap_ocr_assets.rs。
- 修补/结论：下载临时名在完整目标名后追加 .part，避免目标本身为 .part 时清理删除有效目标。
- 静态检查：rustfmt --check --edition 2021=0。
- 测试接棒（全部 **UNVERIFIED**）：内联 cancelled_download_preserves_destination_with_part_extension。

### R10
- 改动文件：crates/markdown/src/markdown_document.rs；tests/markdown_convert_gate.rs。
- 修补/结论：普通链接 title 中字面图片引用不再加媒体前缀；label内真实嵌套图片仍落位，正文/源保护不变。
- 静态检查：生产 rustfmt --check --edition 2021=0。
- 测试接棒（全部 **UNVERIFIED**）：内联 ordinary_link_titles_remain_literal_and_label_images_are_materialized；修改公开 batch_preserves_image_titles_and_commits_angle_bracket_media，保留原标题/目标/产物/源文件断言。

### R11
- 改动文件：crates/gui/src/gui.rs；ui/app.slint；tests/gui_flow.rs。
- 修补/结论：容量小数有界逐位换算避免中间u64溢出拒绝合法值；失败分页保留隔离错误/全部卷位置，Tooltip显完整信息；Git说明不套应用系统代理；MD同任务Control与中性取消、停止关闭分支接入。
- 静态检查：最终 rustfmt --edition 2021 --check gui.rs gui_flow.rs=0。Slint未编译/运行，Tooltip/布局/模态优先仍待实际surface。
- 测试接棒（全部 **UNVERIFIED**）：gui.rs：capacity_rules_accept_exact_byte_fractions_without_intermediate_overflow、fail_list_retains_partial_quarantine_error_and_all_moved_locations、cancelled_md_preparation_does_not_request_overwrite_or_report_error、md_terminal_transitions_preserve_stop_and_close_confirmation。gui_flow.rs 两项关闭测试仅更新过时注释，全部断言/名称保留。

### R12
- 改动文件：crates/codeagent/src/codeagent.rs；tests/codeagent.rs。
- 修补/结论：成功解密后创建缺失 Environment 键；仅服务间隔/状态读写用 Duration::ZERO，争用保留有效间隔/状态恢复补写；公共settings仍5s。
- 静态检查：最新生产和测试 rustfmt --check=0；最终共享opener消费静态确认。
- 测试接棒（全部 **UNVERIFIED**）：强化 codeagent_native_registry_and_detached_service：缺键失败不创建/首次成功/重复不写；独占SQLite期间两个Token轮换，保留原12s持锁/恢复/去敏断言。

### R13
- 改动文件：crates/jchocr/src/jchocr.rs；crates/jchocr/tests/jchocr.rs。
- 修补/结论：CLI OCR 起止/耗时/统一关联诊断，不含输入内容；最终role=jchocr和内部role=xberg-broker初始化兼容，公开 logging API/品牌不变。
- 静态检查：生产 rustfmt --check=0；未重复未变检查，第二轮只作最终依赖复核。
- 测试接棒（全部 **UNVERIFIED**）：ocr_call_logs_correlated_start_and_result_without_input_content；R01角色回归须联合验收。

### R14
- 改动文件：crates/acp-api/src/acp_api/acp/callbacks.rs；tests/acp_callbacks.rs。
- 修补/结论：文件I/O错误保留去敏阶段/ErrorKind/OS码，协议错误不变；手写SSE DONE之后调用R17唯一完整帧尾判据，保留全部逐事件/内容/PID断言。
- 静态检查：callbacks.rs 最终 rustfmt --check --config skip_children=true=0；测试 --emit stdout=0。
- 测试接棒（全部 **UNVERIFIED**）：内联 file_read_failure_logs_io_cause_without_text；修改 server_tool_events_remain_text_only，拒绝不完整帧/终态后额外数据。

### R15
- 改动文件：无。
- 修补/结论：HTTP/SSE/断连全部分配文件审查无确定缺陷；R17最新完整帧消费者全部调用点复查兼容。
- 静态检查：无文件变化，不运行新命令；只读定向依赖复核。
- 测试接棒（全部 **UNVERIFIED**）：无新增/修改；既有 HTTP/断连测试未运行。

### R16
- 改动文件：无。
- 修补/结论：ACP settings/IPC/发现/生命周期全部分配文件审查无确定缺陷；R17全部7个成功流消费及JSON/取消路径最终兼容。
- 静态检查：无文件变化，不运行新命令；只读定向依赖复核。
- 测试接棒（全部 **UNVERIFIED**）：无新增/修改；real_gui仍须单独授权。

### R17
- 改动文件：tests/fixtures/acp_api/main.rs。
- 修补/结论：finish_stream 完整消费HTTP定界，拒绝DONE后非空白尾部；提取唯一 pub(crate) assert_stream_end，R14手写消费者已接入，旧finish_stream语义不再复制。
- 静态检查：最终 rustfmt --check --config disable_all_formatting=true=0，只作语法。
- 测试接棒（全部 **UNVERIFIED**）：consumer::tests::{sse_completion_accepts_complete_http_frames,sse_completion_rejects_events_after_done}，含Content-Length/chunked正例及晚到文本/错误反例。

### R18
- 改动文件：无。
- 修补/结论：OCR冻结参照完整规格/内联测试无确定缺陷；不将旧oracle历史报告作当前真实截图通过证据。
- 静态检查：无文件变化，无新检查命令。
- 测试接棒（全部 **UNVERIFIED**）：无新增/修改；optional core须显式运行测试。

### R19
- 改动文件：optional/snap-ocr-worker/src/capture_win.rs。
- 修补/结论：BGR尺寸乘积使用 checked_mul，拒绝两阶段溢出，不分配巨型缓冲；最终R08有界交付/worker停止消费兼容。
- 静态检查：rustfmt --edition 2021 --config skip_children=true=0；消费者定向复核，无重复命令。
- 测试接棒（全部 **UNVERIFIED**）：bgr_dimensions_reject_overflow_without_allocating。

### R20
- 改动文件：tests/ocr_fixtures/{check_worker_root.py,check_gui_worker_connection.py,test_worker_root_isolation.py,test_gui_worker_connection.py,README.md}。
- 修补/结论：schema指向共享唯一路径；代理入口固定为能力校验的隔离worker，防继承生产broker逃逸；GUI使用UIA invoke而非焦点/硬件点击；完整旧状态带后缀亦拒绝，防假PASS；README同步。
- 静态检查：四个变更Python ast.parse=0；Ruff check 两个checker=1，仅GUI既有unused noqa E402（第23行），未无关注释清理。实际UIA/隔离服务未运行。
- 测试接棒（全部 **UNVERIFIED**）：worker isolation新增 test_settings_use_shared_schema_and_isolated_database、test_inherited_production_broker_cannot_escape_isolation；新 test_gui_worker_connection.py 两项切页前后旧状态拒绝回归。R21已纳入discovery/纯测试角色。

### R21
- 改动文件：scripts/{test_gate.py,test_timing.py,local_gate_plan.py,quality_scope.py,test_test_gate.py}。
- 修补/结论：Job初始化/resume取消关闭句柄、登记和所属暂停进程；startup计入同一deadline；完整门补fetch UT；OCR test_*.py发现与新GUI测试角色接入，仅fulltest/slowtest，fastcheck不执行。
- 静态检查：变更文件AST语法=0；Ruff check/format --check test_gate.py test_timing.py local_gate_plan.py=0；最后 local_gate_plan.py quality_scope.py=0。真实Job取消证明未运行。
- 测试接棒（全部 **UNVERIFIED**）：test_test_gate.py：CompleteGatePythonCoverageTests.{test_fetch_7zip_tests_are_scheduled_only_in_complete_gates,test_ocr_fixture_discovery_includes_connection_and_isolation_tests}；OwnedCommandLaunchCancellationTests.{test_resume_interrupt_reaps_process_and_closes_owned_job,test_job_configuration_interrupt_closes_new_handle}；TimingBudgetAndEncodingTests.test_process_startup_is_charged_to_requested_timeout。

### R22
- 改动文件：scripts/{static_check.py,requirement_coverage.py,test_review_trusted.py}。
- 修补/结论：测试/cfg发现遮蔽字符串；模块堆叠/嵌套cfg；嵌套块注释不截断体哈希；所有自有package build/src/tests/examples/benches引用纳入且仍UNVERIFIED；承接R26两个破坏边界回归。
- 静态检查：三文件ast.parse=0；两个生产脚本Ruff check/format --check/basedpyright=0；最后追加回归仅再parse=0。没有执行collector/全仓static或重钉baseline。
- 测试接棒（全部 **UNVERIFIED**）：test_review_trusted.py 7项：test_test_attribute_in_raw_string_is_not_discovered、test_module_cfg_survives_literal_braces_and_stacked_attributes、test_nested_comment_braces_do_not_hide_assertion_changes、test_literal_file_cfg_is_not_a_gate、test_all_package_test_locations_supply_candidate_references、test_junction_inside_tmp_and_ancestor_are_rejected_before_clear、test_missing_or_mutated_rec_dictionary_preserves_existing_bundle。

### R23
- 改动文件：scripts/{gui_smoke.py,test_gui_smoke.py}；typings/pywinauto/{__init__.pyi,application.pyi,base_wrapper.pyi}。
- 修补/结论：SQLite URI用as_uri转义#/%，metadata失败也关闭连接；Desktop桩回实际模块，去虚假成员/别名；set_edit_text返回BaseWrapper，GUI三个调用显式忽略，R24外部调用同步。
- 静态检查：AST=0；最终Ruff check/format --check、basedpyright变更桩+gui_smoke=0。首次返回类型检查4项unusedCallResult已由R23/R24修正，R24最终type检查=0。
- 测试接棒（全部 **UNVERIFIED**）：test_gui_smoke.py：TaskRecordTests.test_task_records_accept_uri_reserved_characters_in_state_path、test_unreadable_metadata_closes_the_observer_connection。

### R24
- 改动文件：scripts/{markdown_acceptance.py,test_markdown_acceptance.py}。
- 修补/结论：按围栏字符/长度/闭合屏蔽字面图片；A25独立条目副本；C05扫描线程异常/无完成轮/未结束不再假PASS；消费R23返回类型显式忽略。
- 静态检查：两个变更文件ast.parse=0；生产Ruff check/最终format --check=0；最新消费者basedpyright/Ruff=0。
- 测试接棒（全部 **UNVERIFIED**）：test_markdown_acceptance.py：MediaPostconditionTests.{test_shorter_fence_inside_long_code_block_is_not_a_media_reference,test_longer_closing_fence_and_unclosed_fence_preserve_literal_links}；FormatSweepCoverageTests.test_sweep_does_not_mutate_registered_item；PythonProcessScanTests.{test_c05_scanner_exception_is_reported_not_silently_lost,test_c05_unfinished_scanner_cannot_report_verified_modules,test_c05_scanner_without_completed_round_cannot_report_verified_modules}。

### R25
- 改动文件：scripts/{package-windows.ps1,acceptance.ps1,test_package_windows.py,test_fetch_7zip.py}。
- 修补/结论：默认打包先编译artifact再用既有classifier，不执行gui_flow/两项ACP real_gui，保留引擎并报NOT RUN；WithPackage不隐含桌面授权，显式入口保留；fetch夹具自包含，不依赖忽略的manifest。
- 静态检查：两个测试Python compile=0；PS Parser.ParseFile package/acceptance=0；从Python AST提取两驱动仅Parser.ParseInput=0。首次外层shell展开$的检查命令=1，改正确引用后=0；没执行驱动/测试。
- 测试接棒（全部 **UNVERIFIED**）：test_package_windows.py：test_default_package_tests_use_existing_non_desktop_classifier、test_acceptance_package_keeps_desktop_authorization_explicit；test_fetch_7zip.py既有落位/mtime/损坏恢复用例修改夹具，断言保留。

### R26
- 改动文件：scripts/make_tmp.py。
- 修补/结论：resolve前拒绝原目标/祖先reparse，避免清空另会话目录；OCR bundle清空前校验既有固定rec inference.yml字典。
- 静态检查：compile语法、Ruff check/format --check、basedpyright=0。未执行factory受检业务/任何测试。
- 测试接棒（全部 **UNVERIFIED**）：回归在R22独占 test_review_trusted.py：junction及其child拒绝/目标sentinel保留；missing/mutated字典在clear前拒绝，ONNX/font即使匹配也不能假通过。

## 验收接棒顺序与缺口

1. 阅读根AGENTS与对应功能需求；不改需求来迁就旧实现。上述每个新增/修改测试文件及行为都未执行，必须真实验证，不能以format/AST/类型检查冒充PASS。
2. 在隔离基线中仅移植相关回归，保存修复前行为失败；接口迁移/私有观察接线必要时等价适配，但编译错误不算行为反证。再执行当前修补对应UT/integration及真实公开入口场景。
3. 特别缺口：复制误删原公开跨卷/Windows共享拒绝场景没有完整永久回归；既有 copy_noreplace_refuses_existing_target_without_truncation 不证明调用方cleanup所有权。须在本机两卷证明失败前后目标保护，不用缺环境跳过/伪PASS凑证据。
4. 测试基线未重生成。R22新collector规则影响门禁/体哈希；其仅静态推断xberg_runtime.rs五项cfg从null变为any(test,feature=test-hooks)，未穷举。另有本轮新增/修改/合法删除测试。独立审查差异后再更新scripts/test-baseline.json，不机械重钉或弱化。
5. 删除例外：R02删除unused ordering_sql和唯一实现一致性测试 ordering_sql_agrees_with_rust_compare；父代理LSP确认仅声明/该测试，当前穷举检索无生产调用，真实ordinal compare行为回归保留。这是合法死代码/实现细节测试cutover，非制造变绿（未运行测试）。no-tests-deleted=NO仅此例外，其他原测试未删、断言未弱化；C-04旧相反断言加强为合同保护。
6. 静态残留：R20 GUI checker既有unused noqa E402使scoped Ruff=1，未无关注释清理；R02纯测试严格格式差异不属于项目必验，语法/编译/执行仍不得略去。子代理未挂载LSP，不能据此声称Rust类型验证；父代理仅为删除闲置导出使用过references。
7. 按根§3.3验收矩阵选择Windows测试/静态检查、optional显式入口、引擎/路径安全以及获授权GUI/模型/交付验证。实际Tooltip/布局/模态、终态停止竞态、Job取消、DPAPI/注册表/SQLite维护、UIA能力及隔离服务须实跑。真实网络/认证、DPI/TB/网络共享/独立用户会话未验证；旧报告/CI/历史release不可替代。
8. 可信基与产品变更必须分拆未来提交：static_check.py、make_tmp.py、gui_smoke.py、acceptance.ps1及必要test-baseline属于可信基；本轮各所有者独立修补但未提交。提交要有理由、反证及真实本地证据。授权外不自动提交/CI/发布。
9. 已同步OCR诊断README；权威需求未修改。宿主永久修补文档/变更记录流程在运行smoke后仍待验收方完成，本报告只是修补事实/未验证状态，不是需求完成声明。
10. 会话独占.tmp协调脚本/状态/中间报告在本交付后删除；不清空其他会话.tmp。不启动最终验收，也不因接棒重复已完成静态审查。

### 核心可选局部入口（本轮未运行）

- `cargo test -p jchtools --features test-hooks --test core --test contract_paths --test organizer_fs_boundaries --test contract_engine --test split --test dedup_graph --test classify_shape`
- `cargo test -p jchtools --features test-hooks --test md_tools --test git_tools --test markdown_convert_gate --test xberg_shared_process --test codeagent`
- `cargo test -p jchocr --features test-hooks --test jchocr`；`cargo test -p jchtools --features test-hooks --test acp_callbacks --test acp_http_contract --test acp_http_disconnect --test acp_prompt_summary`
- optional：`cargo test --features test-hooks --manifest-path optional/snap-ocr-core/Cargo.toml --all-targets`、worker同入口；R15/R16真实GUI子项不得未授权直接运行，局部命令须遵守现有classifier/精确skip与§3.4。
- Python回归经现有完整门/局部unittest入口执行；R21已将fetch及OCR全部test_*.py接入完整门。不要把上列局部入口等同完整验收，也不要在本技能中启动它们。
