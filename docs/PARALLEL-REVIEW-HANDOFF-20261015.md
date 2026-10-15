# 2026-10-15 全业务代码/测试：审查修补接棒

> 本次附件 53 项整改的最终状态见文末「本轮附件整改最终验收」；前文保留各轮历史快照，不代表当前仍有未完成修正。

## 16 路复审后的五项修复（未运行测试）

- Git 隐藏路径预检取“远端相对共同祖先变化”与“两端 blob/mode 差异”的交集，不阻挡两端已提交相同内容的路径。
- Git 合并失败分类尊重继承的 `merge.autoStash`；启用时不把已跟踪的暂存/工作区内容直接判为阻挡，仍检查未跟踪文件的路径冲突。
- CodeAgent 卸载维护移至 `CurUninstallStepChanged(usUninstall)`：取消卸载确认不停止后台，维护失败通过异常阻止文件删除。
- 文档响应没有有效图片元数据时直接返回正文和空媒体集合，保留元数据警告，不进入图片引用扫描。
- `ReviewRemediationAcceptanceTests` 在夹具初始化时通过既有 `make_tmp.py workspace` 建立临时根目录，不依赖其他命令预先留下 `.tmp`。
- 用户明确要求“不测试”；上述后续改动未执行测试或运行期验收，不复用下文历史 PASS 作为此次修改后的通过证据。


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

## 附件：53 项逐项复核与本轮证据边界

本节追加记录本轮附件复核，不改写以上中英文历史表、历史执行结果或当时的未验证状态。逐项事实来源为 `.tmp/review-remediation-20261015/findings.json`，需修正范围以同目录 `fix-todos.json` 为准；下表的“修正”表示已确认的窄幅处置范围，不表示修改已通过运行验证。

共 **53 项**：**28 confirmed、11 partial、2 already-fixed、12 rejected**；其中 **38 项 action=fix（需修正）**、**15 项 action=preserve（保留）**。P3-28 虽为 partial，但只确认 Windows 环境继承事实，不扩张现有合同，属于 preserve。P3-01、P3-14 为用户既有修改，属于 already-fixed，**不计为本轮修补成果**。

| 编号 | 当前复核结论 | 窄幅处置 / 证据 |
| --- | --- | --- |
| P0 | confirmed；修正 | Release 步骤以 `ISCC /?` 作最后外部命令；用户实际 pwsh/ISCC exit1 为已接受反证，不重复运行帮助命令。改用已安装编译器文件元数据探测，并显式拒绝安装命令非零退出；实际步骤证据见下文。 |
| P1 | confirmed；修正 | extract/transcribe 的 `json!` 序列化 `&Path` 遇无效 UTF-16 可经宏 unwrap 绕过逐文件 Err。应维持逐项失败和正确终态，不让路径序列化 panic。 |
| P2-01 | partial；修正 | merge/add 分支依赖可本地化的人类消息；push porcelain 原因本身未翻译。仅修正实际受影响的分类分支，不泛化为所有 Git 输出都受影响。 |
| P2-02 | confirmed；修正 | Git 固定 600 秒强杀违反 G15 自然结束，重试可能重启慢 push，merge 锁可能遗留。移除 Git 专属固定期限，不以加长期限代替。 |
| P2-03 | confirmed；修正 | Axum 未配置 HTTP/1 timer，默认 30 秒 header 限时失效；半开 header 可阻塞 graceful shutdown。安装 TokioTimer 恢复既有限时语义。 |
| P2-04 | partial；修正 | Git worker unwind 会丢失 GitDone、留下 busy；未证明正常 Git 输入会触发 panic。仅补安全的 worker 终态边界，不声称已发现正常输入 panic。 |
| P2-05 | partial；修正 | jchocr HTTP `read_to_end` 无界，随后 base64 可超 IPC 容量。按 512 MiB 帧承载能力扣除 base64 与元数据开销限制接收；超限为 network/1，不截断输入；沿用 O34 预算合同。 |
| P2-06 | partial；修正 | 未配置的 keepalive 首次约 3 秒后变为 Failed/Error；并非已证实自动来回振荡。保留 NotConfigured 分类。 |
| P2-07 | confirmed；修正 | S18 页表缺 screens 8/9 和 confirm 5。补布局覆盖；CodeAgent 导航须隔离凭据和注册表，不使用生产状态。 |
| P2-08 | confirmed；修正 | A07 既有正例缺 SVG 正文仍可凭健康图片通过。验收必须同时要求两处 OCR 正文标记，不能仅证明图片存在。 |
| P2-09 | confirmed；修正 | PDF 判型使用 lopdf `read_to_end`，XRef/ObjStm 解压无界。改有界探测，预算耗尽保留扩展名判型，不把预算限制误报为确定非 PDF。 |
| P3-01 | already-fixed；保留 | 用户既有 scan.rs `fold_rel` 已覆盖旧式分卷键；zip/rar 带或不带主包已有回归。不重复修补、不计本轮成果。 |
| P3-02 | confirmed；修正 | walk_dir 虽限制递归深度 32，scan_emit 仍递归回放完整树。以显式栈消除回放调用栈风险。 |
| P3-03 | partial；修正 | 共享锁机制正确，争用提示却仅命名 organizer。纠正提示，保留 `organizer.lock` 兼容性，不改锁协议。 |
| P3-04 | rejected；保留 | 实际官方 7Zip 对同名文件/目录报 Cannot create folder，合并前已失败，未出现报告所述 d_1 泄漏。不增加推测性修补。 |
| P3-05 | confirmed；修正 | 合法嵌套成员在过滤前重复检查相同父目录，随后两次 ensure_dir。按 P08 在单包内复用已安全验证的父目录，不放松安全边界。 |
| P3-06 | confirmed；修正 | count_archives 的 relative_string 遇无效 UTF-16 先失败，阻断正常包；扫描却跳过此类项。统一无损路径项的跳过/清点语义。 |
| P3-07 | confirmed；修正 | COPY_NAME 对纯同名桶做哈希，即使去重图无法形成 copy-name 边。避免该无效哈希，不改变真实去重关系。 |
| P3-08 | confirmed；修正 | targets 的 INSERT OR IGNORE 返回值被忽略，keepers 无写入者；SQL 不形成动作不变量。移除无效预留表及其 API。 |
| P3-09 | confirmed；修正 | status_changes 忽略 stdout_truncated，可将合法 NUL 前缀当完整清单。截断须明确失败，不消费部分状态为完整结果。 |
| P3-10 | confirmed；修正 | 两处冲突查询缺 `-z` 且按行 trim，中文路径会呈 quotePath 八进制文本。使用 NUL 分隔保留原文路径。 |
| P3-11 | rejected；保留 | 虽有未使用的 formats 快照，运行时实际查询引擎且已有回归覆盖；未成立当前功能不匹配。 |
| P3-12 | rejected；保留 | 硬编码 12 个媒体成员当前与 manifest 一致，额外检查仍保留 DLL/license 要求；仅未来分歧推测，不据此扩修。 |
| P3-13 | confirmed；修正 | collect_media 遍历每张图片，rewrite_image_links 对每项重新分配并扫描整篇 Markdown，含未引用媒体。改为单次引用扫描和拼接。 |
| P3-14 | already-fixed；保留 | 用户既有 stopping accept 已派发 cancel 且不计晚到连接，并有真实管道回归。不重复修补、不计本轮成果。 |
| P3-15 | confirmed；修正 | envelope clone 后原值未再使用，真实截图 base64 被复制。直接移动请求所有权，移除无用深复制。 |
| P3-16 | confirmed；修正 | broker-state/control 响应持 engine slot 锁调用 FlushFileBuffers，阻塞 cancel/watchdog。响应写出前释放槽锁。 |
| P3-17 | partial；修正 | 当前无存活 write_expected_tag 符号，load_manifest 旧注释已过期；legacy 临时清理仍有效。只纠正过期说明，不删除有效清理。 |
| P3-18 | confirmed；修正 | initialize_staged 丢弃 worker readiness 原因，与 worker_readiness 不同；底层已有安全类别日志。初始化保留具体失败原因，不扩大敏感日志。 |
| P3-19 | confirmed；修正 | 前台 callback shutdown 失败被重写为 AgentDisconnected。保留本地 typed Io 归属，避免重复 finished 清理。 |
| P3-20 | partial；修正 | 未指定 terminal 输出上限时当前无限；协议允许客户端有限默认值。按 AH13 使用默认 1 MiB，显式限额不变，不宣称协议强制此默认。 |
| P3-21 | rejected；保留 | OpenAI/AH04 未要求每个 content 字符串或数组非空，空 cwd 是不同问题。无依据新增 400 校验。 |
| P3-22 | partial；修正 | 非应用启动后全会话轮询，而是首次访问 CodeAgent 后每轮两次打开 SQLite。按 H01 同连接读取保存间隔与安全状态，不改轮询节奏。 |
| P3-23 | confirmed；修正 | 空闲解压/Markdown/Git/整理目录编辑的 invalidate 在 UI 线程执行 is_dir，慢 UNC 会阻塞事件循环。后台合并查询并以代际校验隔离旧结果。 |
| P3-24 | partial；修正 | Git 计数胶囊本为信息展示。仅移除伪交互，不发明不受支持的 filter kind 3。 |
| P3-25 | confirmed；修正 | 快捷键保存启用条件含无关自启草稿，GUI 实际仅保存已应用的自启状态。快捷键页保存条件只依据本页草稿。 |
| P3-26 | rejected；保留 | About 列表当前与全部 8 个已注册工具相符；只有未来漂移可能，无当前遗漏。 |
| P3-27 | rejected；保留 | 堆副本不保证擦除属事实，但未证明明文输出泄漏；零内存残留并非当前需求。不扩张安全合同。 |
| P3-28 | partial；保留 | 未广播 Environment 且旧父进程环境会条件性继承属 Windows 事实；合同仅承诺注册表更新，不承诺 shell 刷新。只记录事实，不新增广播或刷新承诺。 |
| P3-29 | partial；修正 | CA05 的服务持久化是刻意行为；缺少生产维护停止入口会阻塞同 exe 安装/卸载。按 CA08 增加内部维护退出，保留 GUI 关闭语义。 |
| P3-30 | rejected；保留 | `--base64 --json` 被消费为畸形输入，但已返回 2/input 且 stderr 为 JSON；合同未要求 usage 分类，不改现有错误归属。 |
| P3-31 | confirmed；修正 | 托盘线程死亡或 Trigger 投递失败可能留下 busy，无 capture 终态并阻止退出。补这些真实失败终态，不给合法手动选区新增超时。 |
| P3-32 | confirmed；修正 | WorkerStopFailed 丢弃可用原因，尤其 stopped-marker I/O 缺 kind/code 日志。在错误起源记录去敏原因。 |
| P3-33 | rejected；保留 | 虽存在 down/up 派发，未证明用户可见重复菜单；flags=0 已禁止递归菜单。不据推测修改。 |
| P3-34 | rejected；保留 | 历史生成器路径不存在，但已提交冻结表的实际布局独立；未发现当前表/数据缺陷或重新生成要求。 |
| P3-35 | confirmed；修正 | S5 的 `\S+` 拒绝取消日志中合法保留的空格文件名。修正日志匹配，不改变文件名。 |
| P3-36 | confirmed；修正 | 必需隔离变量缺失当前经 GuiRun.error 在启动 GUI 前成为 FAIL/1。应为 NOT RUN/2，不能把缺前置当产品失败。 |
| P3-37 | confirmed；修正 | WM_CLOSE 后 UIA 失效元素竞态可在自然 exit0 时覆盖成功 GuiRun。仅在实际干净退出证据成立时容忍瞬态，保留其他失败保护；fixture 与重跑边界见下文。 |
| P3-38 | confirmed；修正 | confirm_dialog 在批次边界重置“两次不存在”采样，并继续等待已消失 checkbox。保持跨批次连续采样，停止等待失效控件。 |
| P3-39 | rejected；保留 | 共享 15 秒 deadline 属实，但无实际冷启动失败证据或要求更大限时的合同。不凭推测扩大期限。 |
| P3-40 | rejected；保留 | 两个固定路径当前相等，实际引擎消费者使用 EXPECTED_TEST_ENGINE；漂移须未来编辑才发生，无当前错配。 |
| P3-41 | confirmed；修正 | Rust 1.98.1 原生输出 `failed:`，门只识别 `failed `，漏首次失败快速终止，不是最终非零结果丢失。补原生记录识别，保留最终退出判据。 |
| P3-42 | rejected；保留 | 两种交付均包含整份 docs，但合同未禁止历史文档，亦未成立敏感内容指控。不据此裁剪交付。 |

### 已实际取得的证据及其边界

- **测试覆盖主张纠正：**原报告“均无测试”不成立，已存在有效覆盖，例如运行时引擎查询、用户既有旧式分卷与停止连接回归；但既有覆盖不等于已覆盖本轮新缺陷，仍需针对性回归。测试源码存在、静态审计、AST/格式/类型结果均不能记为行为 **PASS**。
- **初始 static：**实际退出码为 **1**，当次仅发现 **9 个用户新增测试尚未登记 baseline**。这是当次静态门结果，不是产品行为失败，也不是 baseline 已更新或静态门通过的证明。
- **Python 首轮：**实际执行 **24 tests，exit1，14 failures、8 errors**（`artifact://86`）。其中 close 的 5 个场景存在 fixture 同步问题，随后已纠正；不能把 fixture 错误计为产品 bug，亦不能把首轮全部失败都归因于 fixture。
- **close 纠正后重跑：**`.tmp/review-remediation-20261015/red-close-race.log` 记录 **5 tests，exit1，仅 1 failure**；clean 场景得到 **FAIL/1 而非期望 PASS/0**，构成明确行为反证；其余 4 个失败保护场景通过。这一局部结果不代表整个 Python 集合通过，也不代表关闭竞态修补后已通过。
- **P0 实际步骤：**已用 pwsh 执行更新后的实际 workflow step；已安装的用户目录 ISCC 文件元数据探测 **exit0**（FileVersion `0.0.0.0`）。模拟 choco **exit7** 时显式 throw，步骤为非零退出。没有重复运行 `ISCC /?`，没有触发 CI 或发布；本地步骤结果不等于 CI/发布成功。
- **当前完成边界：**本节记录时，Rust 与最终 fulltest 尚未运行完成，未取得可写为完成通过的最终结果。上述局部实测只证明各自观察到的现象；本节不将任何待验证修正记为 PASS，不以历史报告或测试源码替代本轮运行证据。
- **验收切片实现状态（负责人回报，未执行验证）：**P2-08 已实现 A07 双 OCR marker 并保留原媒体核验；P3-36 仅缺隔离键归 NOT RUN，非法路径、实际执行失败及未知异常仍为 FAIL，委托入口合并有效环境并复用 guarded 集；P3-37 仅在未强杀、实际 exit0 且 Job 成功时容忍关闭瞬态，已退出后不再访问窗口。已补非法路径、未知异常及已退出 stale 窗口回归，修正旧测试隔离 fixture；这些是实现/测试源码状态，不是运行 PASS。protocol3 和 P3-39 未改。

### 本轮局部运行追加证据（非 fulltest 结论）

以下仅追加已观察结果，保留上文历史表及当时状态；53 项分类与数量不变，2 项用户既有 already-fixed 不计本轮成果。日志路径均相对 `.tmp/review-remediation-20261015/`。本轮结果不继承文件开头历史 fulltest 的通过结论。

- **执行入口与编译边界：**已绿 Rust suite 由实际 Cargo 产物运行，记法为 `actualCargo artifact review_remediation_ --nocapture --test-threads=2`；`actualCargo artifact` 指当次构建得到的测试可执行文件，不是另行执行的 Cargo 子命令。CodeAgent 隔离入口使用 `--test-threads=1`。`rootall-target` 的 `--no-run` 编译实际 **25.65s、exit0**，只证明编译，不是测试执行通过。`baseline-update.log` 仅记录 `1234 tests` 登记，登记命令 exit0 **不等于静态全 PASS**。
- **Python 已观察结果：**`python -m unittest scripts.test_gui_smoke scripts.test_markdown_acceptance scripts.test_test_gate` 为 **223 tests / 24.026s / exit0**（`green-python.log`）；保留首轮与 close 定向红的历史证据，不将本次单测等同真实桌面验收。

| 本轮实际测试产物 / 日志 | 已观察结果 |
| --- | --- |
| GUI / `green-jchtools_gui.log` | 7 PASS，0.28s；含 Git/转换 worker unwind 终态与目录探测响应/代际隔离。注入的 panic 输出不等于 suite 失败。 |
| jchocr URL / `green-jchocr.log` | 4 PASS，3.89s；含三种超预算响应及正常小响应对照。 |
| core / `green-core.log` | 4 PASS，0.30s；P3-07 四种同名桶。 |
| PDF / `green-contract_paths.log` | 5 PASS，0.86s；普通/加密对照、输入预算、加密对象流预算及加载错误传播。 |
| 深链扫描 / `green-scan_depth.log` | 2 PASS，3.98s。 |
| assets / `green-jchtools_assets.log` | 5 PASS，0.23s；初始化原因分类。 |
| ACP terminal / `green-acp_callbacks.log` | 3 PASS，1.58s；默认、显式及零输出预算。 |
| ACP local Io / `green-jchtools_acp_api.log` | 1 PASS，0.30s；本地 shutdown 错误消费者归属。 |
| ACP Stop/Apply / `green-acp_service_process.log` | 2 PASS，35.62s；半开 header 下停止/应用端点。 |
| Xberg / `green-xberg_shared_process.log` | 2 PASS，19.84s；未读状态响应不阻塞请求、取消和退出。 |
| Markdown unit / `green-jchtools_markdown.log` | 2 PASS，0.01s；多图字面量/占位符/警告及稀疏引用。 |

- **P3-07 有效红更正：**`red-organizer-corrected.log` 的四例实际读取计数均为 **8，期望 0**；现 `green-core.log` 四例通过，证明这些桶的读取计数回到 0。不得用早期不等价夹具代替该反证。
- **PDF 有效红/绿：**`red-encrypted-pdf.log` 中普通加密对照 PASS，两个预算/加载错误传播场景 FAIL（1 PASS / 2 FAIL）；这三例现均在上述 5 PASS 内，不把正常加密文件误当预算红。
- **P1 文档与媒体须分别记账：**原 `red-markdown_convert_gate.log` 的 document 场景被已有 Xberg **PID 6604** 占槽拦截，不能计为 document 路径缺陷反证；media 场景实际发生 `json!` 路径序列化 unwrap panic，是有效行为红。`green-markdown_convert_gate.log` 中 document/media 两条逐项失败隔离与合法 **U+FFFD** 对照均 PASS，重复元数据/未引用坏数据场景亦 PASS；但另一多图场景仍被许可证 notice 原子就位 **os error 5** 拦截，当前该日志整体为 **3 PASS / 1 FAIL**，不能报多图集成全绿。
- **Markdown 性能同输入对照：**`markdown-perf-before-result.json` 与 `markdown-perf-after-result.json` 使用同一真实 **262144 B** 正文、**16 vs 256** 张未引用图，每输入 5 次采样。累计分配中位数 before 为 **10751673 / 136624953 B**，ratio **12.707320339820603**；after 为 **264458 / 289898 B**，ratio **1.0961967495783829**。图片数增至 16 倍不再引起近似逐图复制整篇正文的分配增长；这不是零分配或任意输入的复杂度证明。解析耗时中位数 before **8716500 / 124702200 ns**、after **293800 / 440700 ns**，仅是本机这组输入的观测，不泛化为通用耗时保证。
- **CodeAgent 前提失败与有效红分开：**`green-codeagent.log` 名称不代表全绿：整体 **3 PASS / 3 FAIL**。保存间隔、独立 maintenance stop、持维护锁时 serve 退出三例确实 PASS；coldlegacy 升级、运行中升级、卸载最初三例均在 ISCC 编译 `[Code]` 分号注释处被 Pascal 语法拦截，尚未安装/启动 legacy，**不算产品行为红**。只将两行注释改为 `//` 后，ISCC 成功且实际安装升级完成；`red-maintenance-cold-valid.log` 在 `tests/codeagent.rs:1042` 观察到旧 fixture 被启动，**18.95s / exit101**，是有效 consumer 红。不能据已有 stop/serve 局部 PASS 声称升级/卸载已绿。
- **其他仍红的局部日志：**`green-git_tools.log` 为 **7 PASS / 1 FAIL**，停止场景断言收到 CRLF 而期望 LF；`green-gui_log_panel.log` 为 **1 PASS / 1 FAIL**，静态计数悬停/按压像素反馈断言失败；`green-contract_engine.log` 为 **0 PASS / 1 FAIL**，消费可见跳过记录查询得到 `QueryReturnedNoRows`。这些结果不因文件名带 green 而计通过，后续修正与重跑由主控另补。
- **本轮完成边界：**这里只证明上述局部观测；partial 项的合同限定保持不变。本轮完整 fulltest、真实桌面/焦点、真实模型及发布未取得可在此登记为通过的结果；最终 fulltest 结果由主控补录，历史 PASS、编译成功、baseline 更新和局部 suite 均不能替代。

### 最新局部映射补录（38 fix / 15 preserve，最终门待主控）

本节是上述局部日志的后续快照，不覆盖历史红/绿记录，也不继承文件开头旧 fulltest PASS。分类仍为 **53 = 28 confirmed + 11 partial + 2 already-fixed + 12 rejected**；action 仍为 **38 fix / 15 preserve**。逐项机器可读映射为 `.tmp/review-remediation-20261015/local-verification-map.json`，包含每项红/after日志及合同边界；`fix-todos.json` 只用于修正项清单，不作执行证据。下表日志均相对此目录，局部绿不表示整体闭环。

- **当前未完成：Git local-only 更正夹具后 after PASS，hidden 内容保护 after 仍 FAIL；OCR 最新 5 PASS / 1 FAIL（10.38s），idle 真实 warm/font 恢复已 PASS，非隔离 live-selection 被全局 proxy `AlreadySet` 阻挡，主控 fixture 已参数化、待重跑。本轮 fulltest 尚未运行，最终门结果由主控补录。** 编译成功、0tests过滤、baseline登记和旧门结果均不可替代。
- **NOTICE os5仍未确因：**已排除继承环境（父进程三个相关变量全无），默认asset root忽略STATE_DIR，两个thread lease顺序正确。后续显式assetroot隔离4/4通过只证明该次执行；不能归因杀软，也不能声称外部文件锁已修好。
- **供应链归属纠正：**本轮lopdf真实改动仅reader/CHANGELOG；object_stream/parser/xref的有界实现为原上游，不列作本轮新增修补。
- **补充实物与计数边界：**`ui-visual/`保留idle、hovered、pressed、released、tab-skipped五张PNG；软件渲染和合成Tab输入不等于真实桌面/焦点授权验收。`green-file-core.log`为0 tests/112 filtered，`green-extract-cache.log`为0 tests/55 filtered，均不计PASS。P3-03提示、P3-08死代码删除及P3-17注释修正不捏造行为回归。
- **P2-01 夹具与旧 helper 反证边界：**早期 local-only 的 `a.txt` 为 untracked，不能据此保证先处理 a 再处理已 tracked 的 z；原 `red-git-local-only.log` 的提交顺序断言失败不作为该分类缺陷的有效红。夹具已改成 tracked a→z；完整旧 helper 配合更正后的真实 Git 夹具，经独立 rustc 重放 local-only 与 hidden 两例，分别真实 **exit101**（`git-before-provenance.json` 及两份 `git-before-review_remediation_*.stdout/.stderr`）。当前 `green4-git.log` 为 **9 PASS / 1 FAIL，28.91s**：local-only 已 PASS，hidden 仍 FAIL；不能把文件名中的 green 当作全绿。
- **P2-01 内容丢失实证与保护时机：**`hidden-git-bytes.log` 中原始写入为 `hidden local dirty z must survive unchanged\n`；真实 merge **exit2**，原始 stderr 明确 `Your local changes ... z.txt ... would be overwritten`、`Aborting`、`Merge with strategy ort failed`。但流程结束时 z 的原始 bytes 已是旧内容 `shared committed z\r\n`；索引与 HEAD oid 均为 `eb5050021c4e4e544763dadf58810fe8919d249c`，事后按路径 hash 也等于该 oid。此前不运行 merge 的 miniprobe 得到 actualHash `187f…`、index `eb505…`，证明当时能区分隐藏修改；因此当时 helper 仅在失败后核对内容已太晚，事后 hash==index 不能反证没有本地阻挡或原内容未丢失。具体哪一内部 Git 阶段改写仍未证实，不归因所有 Git 版本。G-10 已就地澄清 merge 前保护隐藏修改、内容及索引标记，尊重 G-17 的 filters/EOL、不加入逐文件清单；不改变 G-11/G-17 或其他成功路径，也不靠删除断言使测试通过。本次源代码切换（负责人回报，尚未取得 after 通过）：fetch 后、真实 merge 前计算一次 `HEAD...FETCH_HEAD` incoming，仅核对其中隐藏索引候选的既有标记/索引条目与按路径 filters/EOL hash；内容有变则在未启动 merge 时停止。普通 status dirty 仍只在 merge 失败后用同一 incoming 分类，新增 wrapper 边界 bytes 快照用于定位改写阶段，不把尚未运行的边界记录写成根因证明。
- **OCR 最新真实路径与 fixture 边界：**`green2-ocr.log` 曾在真实结果窗字体阶段因缺 event loop 失败，不是自动恢复已通过；`green3-ocr.log` 的隔离 idle 已走真实 warm/font 并 PASS（子进程 9.68s），停止原产地安全 kind/code 诊断也 PASS。当前唯一失败是非隔离 live-selection 的全局 proxy `AlreadySet`：多个非隔离测试注册 threading 事件代理，不能算新的生产行为反证。主控已将测试 helper 参数化，仅 idle 独立子进程 `threading=true`，其余 false；该 fixture 调整尚待重跑，生产 worker 行为未因此改动。

| fix编号 | 当前实际状态 | 红 / after证据（无专门红则明确不造） | 观察及保留边界 |
| --- | --- | --- | --- |
| P0 | 实际步骤观测 | 用户已提供反证 → `release-probe-installed.log`, `release-probe-install-failure.log` | 元数据探测exit0；模拟choco exit7明确throw；不重跑ISCC帮助、不触发CI。 |
| P1 | 局部绿 | `red-markdown_convert_gate.log` → `green2-markdown.log` | 4/4，6.30s；文档/媒体逐项隔离及U+FFFD对照、多图准确产物。原document占槽不是路径缺陷红。 |
| P2-01 | local-only 局部绿；hidden 仍红 | `red-git_tools.log`；更正夹具的旧 helper 重放 `git-before-provenance.json` → `green4-git.log`；`hidden-git-bytes.log` | 早期 untracked a 排序前提无效，已改 tracked a→z；两例旧 helper 各 exit101。当前 local-only PASS，hidden FAIL，Git 9/10；真实 merge exit2 后隐藏原内容已变成旧 CRLF，事后核对不足，须 merge 前保护；内部改写时点未证实。 |
| P2-02 | 局部绿，Git suite未全绿 | `red-git_tools.log` → `green-git_tools.log`, `green4-git.log` | 保留早期自然停止断言 CRLF/LF 夹具不匹配记录；最新自然结束停止用例 PASS，Git 整体 9/10，未把 hidden 失败记绿。 |
| P2-03 | 局部绿 | `red-acp_service_process.log` → `green-acp_service_process.log` | 半开header的Stop/Apply两例PASS，35.62s。 |
| P2-04 | 局部绿 | `red-jchtools_gui.log` → `green-jchtools_gui.log` | 注入Git unwind终态/重启PASS；GUI suite7/7，0.28s，不证明正常输入panic。 |
| P2-05 | 局部绿 | `red-jchocr-corrected.log` → `green-jchocr.log` | 三类超预算响应及小URL对照4/4，3.89s。 |
| P2-06 | idle 局部绿，OCR suite未全绿 | `red-ocr.log` → `green-ocr.log`, `green2-ocr.log`, `green3-ocr.log` | 保留握手 process_exited 和缺 event loop 的历史失败；最新隔离 idle 真实 warm/font 恢复 PASS（子进程9.68s）。OCR 5/6；非隔离 live-selection 因 AlreadySet 失败，主控 fixture 参数化待重跑。 |
| P2-07 | 局部绿 | `red-evidence.txt` → `green2-python.log` | 三个Python模块合计223 tests / 23.885s / OK；S18 页面/confirm布局覆盖；不等于真实桌面操作或生产凭据导航验收。 |
| P2-08 | 局部绿 | `red-evidence.txt` → `green2-python.log` | 三个Python模块合计223 tests / 23.885s / OK；A07 双 OCR 正文 marker 断言；非真实模型验收。 |
| P2-09 | 局部绿 | `red-contract_paths.log`, `red-encrypted-pdf.log` → `green-contract_paths.log` | 5/5，0.86s；输入/加密对象流预算及错误传播。上游已有有界实现不冒报本轮新增。 |
| P3-02 | 局部绿 | `red-scan_depth.log` → `green-scan_depth.log` | 深链队列/清点2/2，3.98s。 |
| P3-03 | 非行为修正 | 无专门行为红 → 无行为PASS主张 | 仅锁争用提示；organizer.lock/协议保留，无功能回归PASS主张。 |
| P3-05 | 局部绿 | 无专门行为红 → `green-cache-conflicts.log`, `green-cache-exclusions.log` | 真实7-Zip26.03：64同父兄弟冲突/空目录1/1，0.62s；17成员排除不建父目录1/1，0.47s。 |
| P3-06 | 局部绿 | `red-contract_engine.log` → `green-contract_engine.log`, `green2-extract.log` | canonical-prefix fixture纠正后count/extract跳过无配对代理项1/1，0.06s；早期QueryReturnedNoRows不计绿。 |
| P3-07 | 局部绿 | `red-organizer-corrected.log` → `green-core.log` | 有效红四例读8次、期望0；after四例零读取PASS，0.30s。 |
| P3-08 | 非行为修正 | 无专门行为红 → 无行为PASS主张 | 删除死SQL/API及旧wiring测试，不捏造功能回归；最终baseline主控补。 |
| P3-09 | 单例绿，suite未全绿 | `red-git_tools.log` → `green-git_tools.log` | 截断status在commit/push前失败PASS；Git整体及新增分类边界未闭环。 |
| P3-10 | 单例绿，suite未全绿 | `red-git_tools.log` → `green-git_tools.log` | 首次merge/restart原中文冲突路径PASS；不等于Git全绿。 |
| P3-13 | 局部绿 | 无专门行为红 → `green-jchtools_markdown.log`, `green2-markdown.log`, `markdown-perf-before-result.json`, `markdown-perf-after-result.json` | unit2/2，0.01s；集成4/4，6.30s；同输入分配对照见上文，不伪称原功能测试红。 |
| P3-15 | 所有权修正，无专门行为反证 | 无专门行为红 → `green-xberg_shared_process.log` | envelope直接move；相关Xberg消费者2/2不是专门clone计数/分配证明。 |
| P3-16 | 局部绿 | `red-xberg_shared_process.log` → `green-xberg_shared_process.log` | 未读响应不阻塞请求/取消/停止退役2/2，19.84s。 |
| P3-17 | 非行为修正 | 无专门行为红 → 无行为PASS主张 | 只纠正过期注释，保留有效legacy清理，无行为测试主张。 |
| P3-18 | 局部绿 | `red-jchtools_assets.log` → `green-jchtools_assets.log` | 初始化具体missing/size/digest原因5/5，0.23s。 |
| P3-19 | 局部绿 | `red-jchtools_acp_api.log` → `green-jchtools_acp_api.log` | 本地shutdown typed Io归属1/1，0.30s。 |
| P3-20 | 局部绿 | `red-acp_callbacks.log` → `green-acp_callbacks.log` | 默认1MiB/显式/零限额3/3，1.58s；不是协议强制默认值。 |
| P3-22 | 局部绿 | `red-codeagent.log` → `green2-codeagent.log` | 保存间隔与生效间隔隔离子例PASS，0.04s；CodeAgent整体6/6。 |
| P3-23 | 局部绿 | `red-jchtools_gui.log` → `green-jchtools_gui.log` | 响应性/缺目录/旧代际三例PASS，GUI suite7/7，0.28s；不等于真实UNC桌面。 |
| P3-24 | 局部绿 | `red-gui_log_panel.log` → `green2-ui.log`, `ui-visual-proof.log` | disabled FocusScope真实软件像素、Tab跳过、回调计数PASS；UI2/2，1.71s，截图复跑1/1，2.07s。 |
| P3-25 | 局部绿 | `red-gui_log_panel.log` → `green2-ui.log` | 仅本页草稿决定快捷键保存，保留自启草稿PASS；UI2/2，1.71s。 |
| P3-29 | 局部绿 | `red-maintenance-cold-valid.log` → `green2-codeagent.log` | CodeAgent六项6/6，126.46s，真实coldlegacy/运行中升级/卸载、维护释放及锁保护；保留CA05。 |
| P3-31 | 既有行为局部绿；最新 fixture 待重跑 | `red-ocr.log` → `green-ocr.log`, `green2-ocr.log`, `green3-ocr.log` | 先前托盘死亡/投递失败恢复、有效选区无deadline、不清inference四例 PASS；最新 live-selection 因全局 proxy AlreadySet 失败，不计新生产行为反证。threading fixture 已参数化待测；最新 OCR 5/6，不报全绿。 |
| P3-32 | 局部绿，OCR suite未全绿 | `red-ocr.log` → `green-ocr.log`, `green2-ocr.log`, `green3-ocr.log` | 保留原缺安全 kind/code 的失败；最新停止原产地去敏诊断用例 PASS，OCR 整体 5/6，剩余 AlreadySet fixture 待重跑。 |
| P3-35 | 局部绿 | `red-evidence.txt` → `green2-python.log` | 三个Python模块合计223 tests / 23.885s / OK；取消日志允许合法空格文件名。 |
| P3-36 | 局部绿 | `red-evidence.txt` → `green2-python.log` | 三个Python模块合计223 tests / 23.885s / OK；缺隔离键为 NOT RUN/2；非法路径/执行失败/未知异常仍 FAIL。 |
| P3-37 | 局部绿 | `red-close-race.log` → `green2-python.log` | 三个Python模块合计223 tests / 23.885s / OK；仅实际干净 exit0/Job成功时容忍关闭瞬态；保留其他失败。原五例 fixture 错误不计产品红，更正后 clean 场景 FAIL/1 为有效红。 |
| P3-38 | 局部绿 | `red-evidence.txt` → `green2-python.log` | 三个Python模块合计223 tests / 23.885s / OK；确认关闭的连续不存在采样跨批次保持，不等待失效 checkbox。 |
| P3-41 | 局部绿 | `red-evidence.txt` → `green2-python.log` | 三个Python模块合计223 tests / 23.885s / OK；识别原生 failed: 快速失败，最终非零判据不变。 |

15项preserve仅保留既有判定：P3-01、P3-14为用户既有already-fixed；P3-28为partial但不扩张注册表合同至环境广播；其余P3-04/11/12/21/26/27/30/33/34/39/40/42为rejected。不伪造这些项的新测试，也不计为本轮成果。真实桌面/焦点、真实模型、发布及本轮最终fulltest均未在本节登记通过。

### 最新 Git / OCR 局部闭环更正与 fulltest 停止边界

本节仅追加后续实际局部结果，不改写上文记录时的失败、fixture 问题或“待重跑”状态。分类保持 **53 = 28 confirmed + 11 partial + 2 already-fixed + 12 rejected**，处置保持 **38 fix / 15 preserve**；两项用户既有 already-fixed 不计本轮成果。日志路径相对 `.tmp/review-remediation-20261015/`，机器映射同步至 `local-verification-map.json`。

| 最新局部日志 | 实际日志结果 | 退出码证据边界 |
| --- | --- | --- |
| `green5-git.log` | 完整 Git **10 PASS / 0 FAIL**，**28.36s**；57 filtered，`test result: ok`。 | 日志未显式记录进程退出码；不补造 exit0。 |
| `green6-git-hidden.log` | 清理临时 debug 后仅 hidden 单例 **1 PASS / 0 FAIL**，**5.67s**；66 filtered，`test result: ok`。不是第二次完整十例。 | 日志未显式记录进程退出码；不补造 exit0。 |
| `green4-ocr.log` | OCR 生命周期 **6 PASS / 0 FAIL**，**10.43s**；55 filtered，`test result: ok`。隔离子进程输出与外层重复测试名称不可重复计数。 | 日志未显式记录进程退出码；不补造 exit0。 |

| 编号 | 最新局部状态及对应实测 | 历史及合同边界 |
| --- | --- | --- |
| P2-01 | **局部通过**：`green5-git.log` 中 local-only、hidden、localized dirty/conflict/prestaged deletion 及无关 dirty 重试均 PASS；hidden 清理 debug 后在 `green6-git-hidden.log` 再单例 PASS。 | 保留早期 untracked a 排序夹具无效、更正夹具旧 helper 两例各 exit101、`green4-git.log` 9/10 以及 `hidden-git-bytes.log` 内容丢失实证。后续通过不推翻当时失败；保护发生于 merge 前，保留 filters/EOL、内容和索引标记及无逐文件清单边界，不泛化到所有 Git 输出或版本。 |
| P2-02 | **局部通过**：`green5-git.log` 的自然结束停止用例 PASS，所在完整集合 10/10。 | 早期停止断言 CRLF/LF 不匹配仍保留；不以新超时代替自然结束，不将局部集合等同 fulltest。 |
| P3-09 | **局部通过**：`green5-git.log` 截断 status 在任何 commit/push 前失败用例 PASS，所在完整集合 10/10。 | 截断不能作为完整清单消费；早期 suite 非全绿记录不变。 |
| P3-10 | **局部通过**：`green5-git.log` 首次 merge / restart 原中文冲突路径两例 PASS，所在完整集合 10/10。 | NUL 分隔原文路径合同不变；不靠显示转义文本代替路径。 |
| P2-06 | **局部通过**：`green4-ocr.log` 隔离 idle 保存配置后恢复 PASS，子进程 **9.72s**；生命周期集合 6/6。 | 保留 process_exited、缺 event loop 及此前 AlreadySet fixture 失败；真实 warm/font 局部恢复不扩张为真实模型验收。NotConfigured 分类限定不变。 |
| P3-31 | **局部通过**：`green4-ocr.log` 托盘死亡 capture 恢复、Trigger 投递失败恢复、合法选区无 capture deadline、不清 inference 四例均 PASS；生命周期集合 6/6。 | 此次 live-selection 已 PASS，不再把此前全局 proxy AlreadySet fixture 失败写成最新状态；该历史仍可追溯。不新增选区超时、不改变状态/退出/取消语义，保留既有 status 字段及 task 字符串口径，不新增 busy 键。 |
| P3-32 | **局部通过**：`green4-ocr.log` 停止原产地安全诊断 PASS，隔离子进程 **0.41s**；生命周期集合 6/6。 | 先前缺安全 kind/code 的失败保留；仅去敏原产地诊断，不扩大敏感日志。 |

**fulltest 已尝试，但当前未通过，不是“尚未运行”，也不是整体完成。** 按主控最新交接，`fulltest-5` 停止于 **optional 生产 Clippy `struct_excessive_bools`**；本次仅获授权读取上述三份局部日志，未读取 fulltest 日志，不补造该门退出码或其他阶段结果。后续 phase/生产状态表示修正尚无本节可登记的通过证据，不能从这三份既有局部日志预写为通过，也不得削弱检查。上文“本轮 fulltest 尚未运行”仅保留为当时快照，当前以此追加状态为准。

这些结果仅闭环所列 Git/OCR 局部执行；不改变其余项目的证据归属，不新增真实桌面/焦点、真实模型、CI/发布成功声明，不干预用户桌面或停止真实用户 Xberg。历史 PASS、编译成功、baseline 登记、日志文件名中的 green 及本节局部通过均不能替代本轮完整 fulltest 通过。

## 本轮附件整改最终验收

**本轮 fulltest 通过，退出码 0。** 53 个唯一编号已逐项核对，分类账与局部证据账一致：28 confirmed、11 partial、2 already-fixed、12 rejected；按复核后的合同范围完成 38 项修正，15 项保留（12 项不成立、2 项用户既有修复、1 项不扩张现有合同）。历史失败、反证不充分处及真实边界保留在上文，不将这些限定改写为更强的结论。

### 最终命令与实测

门内实际叶子命令：`python scripts/test_gate.py fulltest --authorized`。本轮经临时捕获器记录为 `fulltest-9.log`；包装器只从已有 GitHub CLI keyring 向子进程内存注入 `GITHUB_TOKEN`，不改仓库、系统配置、构建维度，不打印或持久化凭据。

| 最终计时字段 | 实际输出 |
| --- | --- |
| status / exit | PASS / 0 |
| total | 902.0 s |
| compile_excluded | 8.7 s |
| budgeted | 893.3 s |
| limit | 900.0 s |

关键输出：`fulltest PASS total=902.0s compile_excluded=8.7s budgeted=893.3s limit=900.0s`、`CAPTURE_EXIT=0`。总墙钟不等于非编译计费时间；本次没有放宽 900 秒预算，没有改变 profile、features、target、RUSTFLAGS、链接器或缓存目录。

本次门的静态/格式/类型/Clippy、全部适用本地 UT/integration/doc/e2e、两个 optional 组件、真实 7-Zip、固定目录 Xberg 真实媒体链路及本地发布打包阶段均返回 PASS。几个直接相关的完整集合如下；这是各集合实际执行数，不把 baseline 登记数当成执行数：

| 本次 fulltest 日志 | 实际结果 |
| --- | --- |
| `048-root-test-build-jchtools_gui-lib.log` | 128 PASS / 0 FAIL；16.05 s |
| `070-root-test-build-git_tools.log` | 67 PASS / 0 FAIL；145.12 s |
| `075-root-test-build-codeagent.log` | 8 PASS / 0 FAIL；63.00 s |
| `086-snap-ocr-worker-test-build-snap_ocr_worker-lib.log` | 61 PASS / 0 FAIL；28.52 s |
| `096-package.log` | package exit0；installer、ZIP、可选组件本地落位 PASS |

上述逐阶段日志位于 `.tmp/test-gate/fulltest/`。本地生成 `.tmp/test-gate/package/JchTools-Setup-x64.exe`、`JchTools-Windows-x64-20261015-170540.zip` 和 `optional-components-v0.1.2/`；Inno Setup 输出 `Successful compile`。这些是本地打包结果，**不是发布成功或干净用户会话 GUI 验收**，可选组件没有上传。

### 门内直接失败的处理与测试基线理由

- **Clippy：**CodeAgent 错误码使用有符号等位转换，ACP 结果使用 `copied()`，GUI 条件等价简化；不加 lint 豁免。OCR 的冗余 busy/capture-pending bool 改为 `Idle / Selecting / Processing` 单一活动阶段，保留旧回执守卫、外部 status/task 字段及原退出语义，不新增选区超时。阶段迁移后的六条生命周期局部回归 6/6、exit0，最终完整 worker 集合如上。
- **GUI 旧同步文案测试：**异步目录探测后，旧 `root_edited_reports_missing_directory` 和 `switching_to_md_or_git_shows_neutral_status` 将“回调返回时已完成 I/O/某段文案”当成合同，分别在实际运行中失败。按不保留实现/文案钉死测试的规则删除这两条，不重新钉死文案；保留更强的 R-04/H-02 消费行为回归：编辑立即撤销 ready、后台最终报告缺目录、慢探测时编辑/导航响应、同路径旧代际结果不覆盖新结果。最终 GUI 128/128 通过。
- **CodeAgent 共享冲突：**`fulltest-7` 在立即覆盖、内容校验已成功后，删除替换后的隔离镜像得到 Windows error32；持锁者未确认，不归因杀软或后台仍活着。保留立即可写、内容相等及最终已删除断言；仅后置删除的共享冲突沿用既有有界等待，其它 I/O 错误立即失败。更正后针对性单例 exit0，最终完整 CodeAgent 8/8 通过。
- **外部下载：**`fulltest-8` 全部适用测试通过，打包访问 GitHub 官方 7-Zip 元数据时被匿名 API rate limit 拒绝，整体 exit1。先用与打包相同的 PowerShell 客户端验证已有认证可读取 26.03 官方元数据，再完整执行上述 `fulltest-9`；没有忽略下载、使用镜像、关闭摘要验证或把前次失败当通过。
- **baseline：**最终由 `python scripts/static_check.py --update-test-baseline` 登记 1234 条 Rust 测试。除上述两条同步文案测试，先前已说明的死 SQL/API wiring 测试随死实现移除；真实失败断言、平台发现范围和 ignore 边界没有为通过而放宽。可信基与产品实现按仓库规则分开提交，合同变更理由为 AH-13 有限默认终端保留、O-34 IPC 承载预算、CA-08 安装维护自然退出及 G-10 隐藏修改保护，不能把代码现状倒写成需求削弱。

### 完成边界与证据保留

真实桌面/焦点、`gui_flow`、ACP `real_gui`、桌面 driver、OCR 全局热键探针及真实 OpenCode 模型验收仍按门的 `NOT_RUN_SEPARATE_USER_INSTRUCTION_REQUIRED` 报告；没有启动 CI、远程流水线或发布。软件渲染和合成输入不扩张成真实桌面证明，媒体组件的真实链路不扩张成真实 Agent 验收。

前文的临时文件名用于标识本轮观测，独占验证目录按仓库约定清理后不保证继续存在；本节、逐项 53 项表、各永久回归及提交记录为持久交付。保留共享测试门日志与本地打包产物，不全量清空 `.tmp/` 或删除用户/其他会话目录。

