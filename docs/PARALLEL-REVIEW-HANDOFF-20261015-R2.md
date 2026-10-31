# 全仓库并行审查修复交接（2026-10-15，本轮）

## 状态与权限

- 工作区：`D:/code1111111111/JchTools/JchTools`。开始时工作树无 staged、unstaged 或 untracked 改动。
- 用户范围：全仓库，只修复确定的功能问题；调度并发上限为宿主最大值32，实际按功能依赖和单文件独占所有权启动。
- 用户已选择授权产品与可信基分开本地提交；不推送、不发布。主代理只分区、调度、处理所有权与交接，不自行复审或修补产品源码。
- **审查修复完成不等于功能验收通过。** 完整门、真实桌面/模型、完整打包交付、CI和发布均未执行。
- **验证边界冲突已公开报告。** 技能禁止测试/构建，但更高优先级开发者要求修复前复现、行为修改后实际运行；本轮执行了下表最小先红后绿和真实路径smoke，不能声称“未运行任何测试”。没有启动最终验收。

## 实际覆盖

自有生产域已按职责覆盖：foundation、file-core、递归解压、目录整理、MD、Git、Xberg、资产、转Markdown、CodeAgent、jchocr、截图core/worker、主GUI与入口。工程域覆盖Cargo/build.rs、安装/打包/发布配置、测试门、验收/夹具脚本、可信基与需求引用。

vendor的751个保留包完成来源/文件一致性盘点；12个既有本地修补包、六个产品相关包，加其余733包分八域审查公开入口/输入/错误/目标与依赖边界。**这是功能风险边界审查，不是全量逐行审计。** 未穷举生成Windows/ICU/Unicode数据、native FFI、密码学、SIMD、字体/几何/数值系数正确性。非Windows条件保留源码仅核对构建适用性，不宣称产品支持其它平台。二进制夹具/字体/图标不当作源码逐字审查。

## 产品与验收工具修复

|文件/域|确定缺陷与最小修复|实际证据|
|---|---|---|
|`crates/file-core/src/scan.rs`|旧ZIP/RAR分卷按ASCII折叠重复入队；改用既有ordinal身份键|真实scan，ZIP/RAR有主包/缺主包四组：修前101（2项而非1），修后0，1 test通过，原文件不变|
|`crates/recursive-extract/src/archive.rs`、`tests/archive.rs`|gzip记录成员名与源同名时丢后缀；按现有主体冲突命名保留`.gz`|局部及真实7-Zip公开流程均101→0；外层成功删源1，内层保留字节并正常进入深度隔离，未冒称内层解码成功|
|`crates/md-tools/src/md_tools.rs`|普通/扩展Disk、UNC路径未识别同一输出；扩展根还错误探测未补分隔符的前缀|真实scan→覆盖merge双向路径及UNC文本控制，修前101，修后0，2 tests；真实共享未验证|
|`crates/git-tools/src/git_tools.rs`、`tests/git_tools.rs`|子目录加`diff.relative=true`漏检根目录无关staged文件，却被恢复合并提交/推送；inspect后统一从仓库root操作|真实Git及本地bare，101→0，1 test；index tree/HEAD/remote/MERGE_HEAD/工作区均保留|
|`crates/xberg/src/xberg_runtime_windows.rs`、`tests/xberg_shared_process.rs`|停止后拒绝已有任务cancel连接；停止后连接不加入退场等待屏障，保留cancel dispatch和新业务拒绝|真实broker/Windows管道、合成引擎：取消101→0；既有不完整连接安全退场0；协议仍为3|
|`crates/assets/src/snap_ocr_assets.rs`|HTTP代理407未走P-09既有直连回退；仅实际用代理的407可回退|真实loopback代理→origin JSON，101→0；恰好两次GET，非代理407/普通503/HTTPS CONNECT控制通过|
|`crates/jchocr/src/jchocr.rs`、`crates/jchocr/tests/{jchocr.rs,fixtures/asset_failure.rs}`|资产无效错误从inference/1改为xberg/3，保留细节并给设置恢复指引|真实CLI→broker→管道，合成引擎，101→0；退出3、stdout空、stderr失败JSON与恢复指引|
|`optional/snap-ocr-worker/src/service.rs`|自启值REG_DWORD/超长REG_SZ令服务启动退出；显式Absent/Corrupt/Command区分，真实I/O错误不吞|真实隔离HKCU回归101→0；类型/原字节不变，正常/缺失/关闭/DACL拒绝控制通过；未注册热键|
|`crates/gui/src/gui.rs`（仅本轮两个hunk）|Git终态早于首拍时branch/upstream/current/retry字段永久空或过期；GitDone丢共享状态前定格完整终态|既有真实Git/bare+Slint TestingBackend回调101（空branch）→0/1 test；无OS窗口或视觉验收，外来hunk未提交|
|`scripts/markdown_acceptance.py`、`scripts/test_markdown_acceptance.py`|验收仍只接受broker协议2，拒绝真实协议3；同步为3并保留旧版本拒绝|真实解析路径、底层管道替身：exit1→0，8 tests|
|`scripts/test_timing.py`、`scripts/test_test_gate.py`|失败枚举被静默删套件，套件或单例失败/超时仍退出0；保留报告并传播非零|真实list_suites/main、外部进程替身：修前exit1/4失败，修后exit0/2 tests；ruff检查/格式检查0|

foundation、目录整理、CodeAgent、转Markdown、冻结截图core未发现需要修复的确定新缺陷。foundation疑似socks5凭据泄露被实际回归反证（exit0、1 passed），未修改生产逻辑，临时探针已撤回。GUI/入口结论见下方最终记录。

## vendor修复与永久回归位置

下列包均同步其局部说明/摘要，保留上游归档digest。不存在当前产品调用证据的保留API缺陷与已发生的产品故障分开表述，不冒称真实渲染或网络改善已验。

|包目录|源码/回归与修复|实际局部证明|
|---|---|---|
|`ureq-2.12.1`|`header.rs`坏头冒号边界；`chunked/decoder.rs`截断正文UnexpectedEof；`stream.rs`SOCKS IPv6 ATYP；新增`stream/proxy_connect.rs`确定性写入/flush错误传播|四项修前真实失败101；修后公开API smoke 3/3，真实helper/decoder 12/12；透传连接器不是TLS验收|
|`fax-0.2.7`|`src/decoder.rs`拒绝水平游程越宽；`src/encoder.rs`宽行偏移/位置和溢出；内联回归|真实公开decode/encode及TIFF边界，修前101，修后0|
|`image-webp-0.2.4`|`src/decoder.rs`ALPH+VP8合成前验证与ANMF尺寸一致；内联回归|真实动画帧越界/错成功→受控错误，匹配正例通过；与fax隔离入口合计3 tests通过|
|`pxfm-0.1.30`|`src/powf.rs`负零及负奇数指数符号；内联回归|`-0^-3`错误+inf→-inf，精确bits与公开派发/通用后端101→0，1 test|
|`rspolib-0.1.2`|`moparser.rs`MO context顺序、打开错误；`file/{mod,mofile}.rs`保留借用bytes/传播错误；`poparser.rs`引号/状态检查；`tests/parser_invariants.rs`|真实PO/MO公开入口panic/往返失败101→0，4 tests；不声称缺失rspolib_testing的完整UT已跑|
|`zerovec-0.11.8`|`src/ule/{plain,slices}.rs`零大小取模/除零；`tests/const_constructors_smoke.rs`|既有空bytes→空slice契约，修前3失败，修后10通过；非空拒绝/非零步长控制|
|`ttf-parser-0.25.1`|`src/tables/cmap/{format2,format4}.rs`无符号模65536；`tests/tables/cmap.rs`|负delta绕回及高位glyph两项先失败，修后cmap18 tests通过|
|`error-code-3.4.0`|`src/lib.rs`Windows io::Error使用系统类别；`tests/error_code.rs`|Win32 5/Winsock10035及无原生码回退，101→0，2 tests|
|`nu-ansi-term-0.50.3`|`src/difference.rs`显式reset优先于样式优化；`tests/style.rs`及内联回归|公开字符串/字节输出101→0，3 tests；新增内部两项尚未独立跑|
|`socket2-0.6.5`|`src/sys/windows.rs`非零小数毫秒向上取整；内联回归|真实Socket read/write：1ns不再变无限，1ms+1ns为2ms，None/零/饱和控制；101→0，2 tests|
|`tokio-1.52.3`|`src/net/windows/named_pipe.rs`SetNamedPipeHandleState失败先取错误再关尚未移交句柄；`tests/net_named_pipe.rs`|隔离进程真实32次失败打开：修前58→90句柄、101；修后58→58、0，合法byte/message控制通过|
|`rfd-0.15.4`|`src/backend/win_cid/file_dialog{.rs,/dialog_ffi.rs,/path.rs}`扩展UNC保持绝对UNC、无中间字符串分配；path内测试|真实生产helper101→0/2 tests；Windows PathIsRelativeW/PathIsUNCW四种路径smoke0，非GUI/COM验收|
|`tiny-skia-0.11.4`、`tiny-skia-0.12.0`|`src/pixmap.rs`二维pixel横向越界不再跨行；各`tests/pixel_bounds.rs`，0.12登记target|真实双版本owned/borrowed边界4 tests，101→0；有效角点/极大坐标控制|
|`dpi-0.1.2`|`src/lib.rs`PhysicalUnit三常量由错误LogicalUnit类型改为PhysicalUnit；内联回归|修前3项E0308，修后类型/精确值/缩放test与外部smoke0；LSP不可用后全仓引用查找无外部调用需迁移|
|`imagesize-0.14.0`|`src/lib.rs`面积相等时按宽高区分，使Ord/Eq一致；`tests/ordering.rs`及target登记|真实BTreeSet逐项insert及PNG公开探测，101/2失败→0/3通过；面积仍是主排序|
|`rustybuzz-0.20.1`|`src/lib.rs`TATWEEL从重叠0x40改为独立0x80；内联回归|真实buffer/glyph操作串标志101→0，2 tests及公开smoke；未宣称真实渲染改善|
|`unicode-bidi-mirroring-0.4.0`|`src/lib.rs`不再对未排序第二列二分；静态214对无分配反查；内联回归|U+29F5及完整配对双向/普通字符控制，101→0，2 tests及公开smoke|

`ident_case`、`strict-num`、`blocking`既有本地补丁未发现新增确定问题。

## 定向二审、依赖补查与静态检查

- Packaging首轮无缺陷；因新增九个修补目录需要有效source身份，**决定二审**。`Cargo.toml`新增精确path-patch/exclude，tiny-skia两版本用package别名；`Cargo.lock`仅移除对应九包registry source/checksum，无升级/依赖边变化。三次离线metadata/resolver均exit0（0.522/0.660/0.437s）；版本/features图/members/profiles等静态比较不变。没有构建，真实编译是否使用新source仍待验收。
- Inventory首轮盘点后，因各修复改变文件和九包source身份，**决定补查**。最终同步结果见末节；不把元数据核对当源码运行证明。
- TrustedReview发现两项明确缺陷，**决定定向二审/修复**：同秒随机UUID任务不能按字典序判代次；验收脚本必须使用独占运行根而非复用留下notice的资产根。先产品本地提交，再改可信基并独立提交；同时官方更新全部受影响测试基线。结果见末节。
- 初次依据各分区实际Coverage/Changes/Checks/Handoff，除上述三项外无具体理由重复二审。随后发现另一会话持续修改多个文件，故对Assets、GitTools、JchOcr、SnapWorker、AcceptanceTools、LocalGates、Xberg另外启动七项**归属与依赖影响补查**；GUI所有者独立核对其外来hooks影响。只生成本轮修复的独立暂存补丁，保留外来代码；实际路径未受影响的通过结果不重跑。最终补查结果见末节。
- Rust文件按修改范围rustfmt/解析；Python生产脚本ruff与语法检查通过情况见前述证据。error-code旧import次序及dpi旧区域格式差异未顺带整理；未据此宣称全仓零告警。

## 验证缺口与下一位Agent

1. 本轮**没有运行**fulltest/slowtest、完整项目套件或最终验收。每次完整门须另获明确授权；桌面/焦点/全局热键/鼠标、真实Agent/模型和发布各守独立授权。
2. 根Xberg/Git/Assets定向Cargo曾在共享锁/编译阶段60s超时，**未执行测试**且未重试同一超时入口；后续用真实源码/依赖窄入口取得证据。Xberg永久根target、Git/Assets普通Cargo整合及worker完整crate仍待验收。
3. 若上游standalone旧锁或缺失dev依赖阻塞（fax_derive、rspolib_testing、ureq/ANSI旧锁等），不要升级/安装/弱化断言制造通过。使用真实源码隔离入口并说明边界；根`cargo test -p <vendor依赖>`并非通用入口，因为它们不是workspace member。
4. 最终依赖图全部21个本地补丁、永久vendor回归及自有新增回归需由acceptance Agent按授权复核；运行覆盖不能由metadata或编译代替。真实TLS/公网/模型、GUI/网络共享、安装/便携交付及C08/C09独立Windows会话均未验证。
5. 临时harness/注册表键由各所有者清理；本轮误置`vendor/pxfm-0.1.30/target`已单独删除，不动根target或原增量缓存；协调脚本目录在交接后单独清理，不全量clean其他会话.tmp。
6. 审查期间出现本轮调度外的持续写入，涉及GUI、转换、托盘、安装/工作流、需求及大量测试。原始全仓库覆盖与这些后来新增的外来代码必须区分：只核对其对本轮修复/运行证明的影响，没有把外来新功能全部重审或夹带提交。最终以独立owned.patch提交本轮修复，外来hunk保留未提交；可信测试基线针对本轮已提交源码生成，不能声称匹配仍在变化的整个工作树。

## 最终协调记录

### GuiEntry

完成。已审主入口、完整GUI注册/装配和Slint、UI导出、四个集成target及所有工具回调。仅gui.rs自身两hunk：GitDone最终同步branch/upstream/current/retry/retry_wait；增强既有真实Git回调，worker终态后才首泵。真实源码+匹配缓存rustc编译0，实际Git/bare/TestingBackend回调修前101(branch空!=master)，修后0/1pass；无OS窗口/焦点。Cargo58.229s编译超时未测不重跑；fmtcheck0。外来hooks/helper曾导致E0425，随后落地；最新A1DF上GitDone/本用例未变，外来testhook默认None未启用，无须重测。只暂存.tmp/gui-entry-git-terminal/owned.patch（git apply --cached --check退出0），不要整文件暂存；checks.txt和前后日志保留待消费。外来review_remediation_*测试未纳入审查/提交，不依赖这些helper。最新依赖图编译、视觉/真实桌面及最终验收未验证。

### AssetsRecheck2

完成：.tmp/assets-recheck2-owned-20261015/owned.patch，仅HTTP代理407分类及原loopback回归两hunk；cached--check0。外来worker初始化helper+5tests不涉下载路径，原101→0及503/CONNECT控制继续有效，未重跑/改源码。

### GitToolsRecheck2

完成：.tmp/git-tools-recheck2-owned-20261015/owned.patch，仅入口文档/info.root两hunk及原子目录diff.relative回归；cached--check0。外来timeout test-hooks默认不改deadline，原helper/依赖未变，原真实Git/bare101→0继续有效；未重跑。

### JchOcrRecheck2

完成：.tmp/jchocr-recheck2-owned-20261015/owned.patch，只含asset_invalid生产分支、原完整CLI回归及asset_failure合成夹具，cached--check0。排除外来Harness::command重构、Read导入、URL容量与remediation测试，工作树未改。外来harness对本路径调用等价，broker外来停止改动不涉正常OCR请求，原CLI101→0/1pass证据有效，无重跑。真实模型/全验收未验证。

### SnapWorkerRecheck2

完成：.tmp/snapworker-recheck2-owned-20261015/owned.patch，仅自启显式状态/分类/调用映射/sentinel断言/原回归共7hunk，cached--check0。外来316行仅测试，与自启路径无关，原registry101→0继续有效；未改源码或重跑。

### AcceptanceToolsRecheck2

完成：.tmp/acceptance-tools-recheck2-owned-20261015/owned.patch，仅协议2→3、两处成功夹具和3项原回归，保留CRLF后cached--check0。外来SVG测试删除/新增remediation测试未收入；parser未变，broker3新ack只显式启用，formats未启用，原8tests/ruff0有效，无重跑。

### LocalGatesRecheck2

完成。仅生成.tmp/local-gates-recheck2-owned-20261015/owned.patch，含test_timing说明/枚举失败/结果传播三块及TimingResultIntegrityTests两项；git apply --cached --check退出0，未改index/源码。外来review_remediation测试及gate_runtime解析修改不涉本路径，原先红后绿和ruff0可复用，无需重跑。保留所有外来变更，待CommitProducts消费。

### XbergRecheck2

完成：.tmp/xberg-recheck2-owned-20261015/owned.patch，仅runtime模块说明/停止listener两hunk及原取消回归一hunk；独立cached--check0。原修复/证明路径未变，外来231行测试不影响；原真实IPC取消101→0与退场0继续有效，未重跑。

### VendorInventoryRecheck2

完成。仅vendor/SOURCES.json：九个新修复包切为path身份；登记本轮18包78个实际文件，全局/包级最终21组99项一致，保留旧修复与原始source/归档SHA/VCS。静态JSON/TOML/SHA核对751inventory、768lock、21path+730registry，60个实际非inventory修改文件摘要0 mismatch；最终记录完整/无重复/旧来源保留。无测试/构建/Cargo；pxfm临时target已由主代理清理。功能验收仍待后续。

### CommitProducts

产品独立提交成功：a5f3f507057a2534625778fb1f75029add91d530，99文件，2669新增/229删除；无推送发布。八份owner补丁精确暂存，所有可信路径与scripts/test_review_trusted.py排除；八类外来文件/hunk未夹带，用户工作树未改、无stash/reset/checkout，index清空。组合cached--check/精确暂存/git commit均0；提交记录含每项缺陷前后命令/证据与局限。原tmp及GUI消费目录已清理。可信baseline必须取此提交快照，不吸收未暂存用户GUI/Markdown/tray/验收/需求/安装/workflow/新scan_depth测试等。完整门/最终验收未验证。

### TrustedReviewRecheck2

可信基修复及必要验证完成，仅自身4文件已精确暂存，等待本文一并独立提交。gui_smoke全部任务轮询改为点击前完整frozenset快照，排除所有旧ID；acceptance工厂创建每轮acceptance-UUID独占根，日志/默认state/assets派生且不删除共享目录。原任务轮询修前exit1/RuntimeError超时；重复真实PS初始化修前exit1（复用资产根）。修后 python -B -m unittest scripts.test_review_trusted.TaskPollingTests scripts.test_review_trusted.AcceptanceWorkspaceTests：exit0，3tests，.663s；真实SQLite smoke exit0，旧failed/ready排除，新低序同秒ready成功、新cancelled正确报错。2Python AST exit0，PS由实际初始化回归解析。官方updater在产品a5f3f507的工厂150文件源码快照运行一次exit0：1158→1168，新增10、删除0、既有body变2(GUI真实Git/worker sentinel)，cfg/ignore全保留；绝不吸收外来未提交Rust测试。精确cached patch应用exit0，临时CRLF首次拒绝未改index后已改官方LF。外来gui_smoke S18/确认重试、其它测试和产品hunk未暂存/未覆盖。无完整门/Cargo/桌面/模型/发布。基线仅对应已提交产品，不声称当前并发工作树static_check通过。
