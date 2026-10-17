# Rust 诊断日志约定包

```yaml
foundation_status: ready
runtime_verification: run_limited
phase: 1
```

本包是单文件日志补齐的 API 与约定，不是产品需求、全项目覆盖证明或批量调度计划。权威约束为 [P-10](docs/requirements/CONTRACT.md)；本次用户确认单文件 1 MiB、每个落点合计 10 个自有文件、14 天最长保留期，容量可提前淘汰近期历史。合同已独立提交 `9cf901f7`，不与产品代码混合提交。后续只适用技能 Phase 2；不得从本次初始化自动继续补齐业务埋点。

## 1. 范围与单文件边界

- 语言：Rust 2021；运行平台：Windows。适用于主包 `src/` 及 `optional/snap-ocr-worker/src/` 的第一方 Rust 源文件。
- worker 通过 `optional/snap-ocr-worker/src/lib.rs` 的 `#[path = "../../../src/logging.rs"]` 复用同一基础设施；不建立另一套日志系统。
- 不适用：`vendor/`、第三方代码、生成代码、构建产物、`tests/` 夹具、`scripts/` Python/PowerShell、Slint/SQL/资源文件，以及未接入该日志基础设施的 `optional/snap-ocr-core/`。性能日志 `src/perf.rs` / `perf-logs/` 与 GUI 运行日志（S-07/U-10）不是本包诊断文件集，不迁移或重新配置。
- Phase 2 修改前，文件所有者必须自行完整阅读技能 Phase 2 入口与授权边界、`references/parallel-logging-fill.md`、本包以及完整目标文件；已有协调者摘要不能替代阅读。
- 可以只读必要的相关源码、需求及测试，以确定目标文件内的业务语义、异常是否为预期结果、重试/回退、日志归属及重复记录。只有目标文件的日志可修改；需要关联阅读不是排除理由。
- 不改业务语句、控制流、返回值、异常处理、重试次数、超时、协议或关联 ID 传播；不新增捕获/finally/边界包装器，不重写返回表达式。不得改其他文件、依赖、共享配置、sink、初始化入口或性能开关。
- 不委派、不自动安排 AI 复核。默认仅静态自检；若上级指令要求运行验证，先披露冲突，并如实记为 `run_limited` 或 `failed`。

## 2. 共享 API

无需逐文件创建 subscriber 或 logger。主包与 worker 均可用以下导入（两包都导出 `crate::logging`）：

```rust
use crate::logging;
use std::time::Instant;
```

结构化事件使用已经声明的 `tracing` 宏：

```rust
let started = Instant::now(); // 插入在现有、可观测的操作开始点
tracing::info!(
    component = "existing_component",
    event = "existing_operation_started",
    "操作开始"
);
// 保留原有业务语句，不能为日志增加观察路径。
// 以下仅插入现有成功分支：
tracing::info!(
    component = "existing_component",
    event = "existing_operation_completed",
    elapsed_ms = logging::elapsed_ms(started),
    count = 2_u64, // 实际补齐时必须使用该位置已有的安全结果统计
    "操作完成"
);
```

示例里的 component/event/count 是 API 示意，不能据此建立新事件族或虚构结果。实际调用复用目标文件已有组件参数、事件族和真实统计。

已有 `std::io::Error` 的可观测失败分支可以记录类型、原生码和耗时：

```rust
// error: &std::io::Error；started: Instant，均来自现有插入点。
tracing::error!(
    component = "existing_component",
    event = "existing_operation_failed",
    stage = "existing_stage",
    error_type = ?error.kind(),
    error_code = error.raw_os_error(),
    elapsed_ms = logging::elapsed_ms(started),
    "操作失败"
);
```

准确签名与用途：

| API | 用途与边界 |
|---|---|
| `elapsed_ms(started: Instant) -> u64` | 单调时钟毫秒；饱和转换，不能改业务截止时间 |
| `safe_url(value: &str) -> String` | 保留 scheme/host/path，去除 userinfo/query/fragment；无效输入不回显 |
| `safe_error(value: &str) -> String` | 已知凭据/URL 去敏、有界诊断文本；不是任意正文的安全摘要器 |
| `operation_span(component: &'static str, operation: &'static str) -> tracing::Span` | 既有需要独立操作 span 的 API；不以补日志为由重建关联方案 |
| `operation_span_with_id(component: &'static str, operation: &'static str, operation_id: &str) -> tracing::Span` | 仅复用目标中已有且安全的 ID；不搜索或跨文件传播来源 |
| `new_operation_id() -> String` | 既有进程级 ID API；Phase 2 不为覆盖计数引入新关联机制 |
| `init(state_dir: &Path) -> Option<Guard>` | 仅既有生命周期入口调用，Phase 2 不调用 |
| `hold_for_process(guard: Guard)` / `flush_before_exit()` / `FlushOnDrop` | 既有进程持有及退出刷盘方式，Phase 2 不重新装配 |

Rust 不自动捕获异常类型/堆栈。`tracing` 的 `%`/`?` 使用 Display/Debug，格式器随后转义去敏，但不能把协议、模型、OCR、终端正文或整个错误对象交给它冒称安全。使用已有安全类型分类、错误码、阶段；只有已知不包含敏感正文的诊断文本才用 `error = %logging::safe_error(...)`。普通事件不自动获取堆栈；不能为补日志捕获新的 backtrace。panic hook 只记录 payload 类型与源码位置元数据，再调用原 hook；磁盘不写 panic payload 或堆栈。保留已有安全错误原因、类型/码及允许的诊断上下文；纯透传层不重复完整错误。

## 3. 配置、格式与连续性

`JCH_LOG_LEVEL` 去除首尾空白后按大小写无关的 `debug` / `info` / `warning` / `error` 选择阈值。未设置、非 Unicode、空或非法值均为 INFO；`warn` 和 `trace` 不属于支持值。DEBUG 放行所有四级，不采样；INFO/WARN/ERROR 按阈值抑制低级记录。主包与 worker 共用该选择；只面向既有 `jchtools` / `snap_ocr_worker` target，不引入第三方正文日志。`RUST_LOG` 不接管诊断配置，性能层保持原有独立规则。

每条事件是一条物理行：`timestamp=<UTC RFC3339 微秒精度、Z 时区> level=<级别> pid=<进程> thread_id=<线程> component=<组件> event=<稳定事件名>`，随后是既有 span 链及结构化字段。文本字段的控制字符、反斜杠和引号转义；数值/布尔值保持结构化。组件优先使用目标已有 component 参数或事件族；新站点应显式填写该组件。无显式 component 的既有事件仍按 metadata target 输出头部，span 内的 component 不自动替换头部；不得重命名既有字段来制造另一种所有权。

- 保留稳定事件名、组件和字段含义；同一操作的 started/retried/completed/failed 使用相同 component 与同义统计。新事件沿用已有 snake_case 事件族，只使用本地证据确立的失败阶段。
- 可选上下文直接使用字段，例如 `request_id = %request_id`、`operation_id = %operation_id`、`peer_pid`、`attempt`、`elapsed_ms`；仅当值已在目标可用且允许入盘。不改协议或跨进程传播。
- ERROR：导致当前操作失败的最终错误。WARN：可继续的异常、异常恢复、重试、退化/回退。INFO：本地可确认的关键开始、成功结果及重要状态切换。DEBUG：常规细节。
- 明确预期的可选能力缺失、正常取消及设计内的正常替代路径必须保持项目/目标已有级别，不能因为捕获到错误就自动升 WARN；真正的异常也不能被自行命名为“预期”而降级。必要相关阅读后仍不能确定级别或归属时报告具体 blocker。
- 不记录请求/响应/配置对象、无限 stdout/stderr、截图、OCR/提示/文件正文、认证头、密码/令牌/Cookie/私钥。只记 ID、类型、长度、数量、状态和截断标记；URL 用安全摘要，正文长度不能当作允许记录正文的理由。

## 4. 文件、容量、归属与唯一保留机制

主目录保持 `<state_dir>/logs/`。主目录不可用时，生产回退为当前用户 LocalAppData 下的 `JchTools/Logs`；`test-hooks` 或库测试只回退至 `<state_dir>/logs-fallback/`。两处失败只输出去敏类型/必要码到 stderr，返回 `None`，不终止业务。没有移动真实既有日志，也没有为就绪检查扫描或清理用户日志目录。

每个落点的每个自有诊断文件最大 **1,048,576 字节**，总数最多 **10 个**，包含所有进程的活动文件及全部确认归属的历史。UTF-8 字节、字段分隔和末尾单个 LF 均计入上限；不按字符数限容。最长年龄为修改时间距当前时间 14×24 小时。容量或数量优先，可提前淘汰近期历史。

| 确认归属的名字 | 证据 |
|---|---|
| `jchtools.<YYYY-MM-DD>.<数字PID>-<启动纳秒>-<序号>.log` | 当前进程缓存一次启动标识；`BoundedAppender::segment_path` 的零分段使用此名，PID 复用也不接续旧文件 |
| `jchtools.<YYYY-MM-DD>.<数字PID>-<启动纳秒>-<序号>.<数字分段>.log` | 同一次启动的按日/大小分段；首次写入通过 `create_new` 排他创建，已占用的普通文件只跳过、不追加 |
| `jchtools.<YYYY-MM-DD>.<数字PID>[.<数字分段>].log` | 既有精确格式，仅纳入历史计数及保留期处理，不再创建或续写 |
| `jchtools.log.<YYYY-MM-DD>` | 既有精确旧历史格式，仅纳入历史计数及保留期处理，不创建、不重命名 |

没有证据表明压缩历史、`.bak`、`.backup`、任意 `jchtools.log.*` 或类似名称归本 sink 所有；全部排除。目录、符号链接及其他非普通文件排除，不递归遍历。不把性能日志或别人的 `.log` 算成可删除历史。

`BoundedAppender` 继续负责待写 UTF-8 字节的容量检查、UTC 日期与分段选择，唯一 `retain_owned` 负责历史选择、排序和删除。文件句柄直接来自 `OpenOptions::create_new`，不再交给会追加已有文件的 rolling appender 重开；已有句柄只由本 writer 继续写。后台仍复用 `tracing-appender` 的非阻塞队列和 worker，没有新增日志框架、清理器或编号重命名机制。

所有参与进程在同一落点旁的 `logs.retention.lock`（回退目录相应为 `Logs.retention.lock` / `logs-fallback.retention.lock`）通过 `std::fs::File::lock` 串行化容量检查、创建、保留和写入。锁文件不在诊断目录中，不是日志，不包含业务数据。后台 65,536 字节 BufWriter 批量提交完整事件，避免逐条加锁；没有改 worker 原有关闭等待或业务超时。

完整选择/删除顺序：

1. 进入落点级锁，按当前 UTC 日与缓存的进程启动标识选择零分段；写入前取得待写批次的实际字节数。批次不拆开事件。
2. 当前分段不足以容纳整个批次，或未持有 writer 的候选普通文件已经存在时，递增分段号；不重命名、覆盖、截断或追加前一个 writer 的文件，不把一条记录写到两个文件。
3. 排他创建分段前，`retain_owned` 枚举精确归属的普通文件，先检查所有候选的容量和修改时间，再按 **修改时间升序、完整文件名（同一目录）升序** 稳定决胜。既有超大自有文件会在任何删除前返回失败，需另行批准迁移，绝不删除它来冒充满足限容。
4. 跳过本次将写入的文件；按上述同一顺序删除超期文件，以及为新文件预留一个名额后仍超出 10 的最旧文件。删除失败则该日志操作失败，不绕过上限继续创建或写入。
5. 用 `create_new` 返回的独占文件句柄在锁内写入不超过上限的完整批次。其他进程可能按原保留策略淘汰曾是活动文件的旧分段；原进程下一批次发现路径消失时丢弃旧句柄，再排他创建本轮文件，避免继续向已删除的文件写入。
6. 初始化的既有 `prune_expired` 使用相同锁与同一个 `retain_owned`，不是第二套选择/排序/删除机制。业务继续运行时，到下一个 UTC 日期分段也执行相同保留策略。

单条记录最大 65,536 字节（含 LF）。超大格式化记录由 `summarize_record` 按完整的引号/转义字段解析，保留头部 timestamp/level/pid/thread/component/event、首个可用 operation_id/request_id，并优先保留 span 链之后的事件级 stage/error_type/error_code/elapsed_ms，避免外层同名字段遮蔽真实失败；丢弃大块上下文，附 `truncated=true original_bytes=<原始UTF-8字节数>`。不截断 UTF-8 或引号字段。原有单文本摘要上限仍为 2,048 字节；记录级上限解决多字段/深 span 的累积增长。

日志内部错误只经现有去敏 stderr 报告，不向失败的 sink 递归写入。既有队列过载计数与告警保留；DEBUG 不主动采样，但不可写文件、队列过载及异常退出不构成无条件落盘承诺。主动退出仍使用既有 Guard / `flush_before_exit`，本次并发验证不意味着所有压力下的无限等待保证。

### 4.1 性能数据的启动隔离

按 P-10，业务诊断日志和性能数据均不能跨进程运行混写。启用既有 `perf-tracing` 时，本程序的性能文件落在 `<state_dir>/perf-logs/<PID>-<启动纳秒>-<序号>/jchtools-perf.log.<UTC日期>`；目录以排他创建预留，同日重启换目录，不追加上次数据。独立 `perf::init` 与组合 `logging::init` 共用同一 appender 构造规则，性能读取端仅访问当前启动目录。

共享代理每次启动或替换 Xberg 引擎，都为其预留 `perf-logs/xberg/<唯一启动标识>/`，不复用同一个 broker 下上次引擎的落点。若目录不能独占创建，沿用关闭性能采集、不影响业务的语义，绝不回退共享性能文件。默认开关、每日轮转、span/字段、去敏及诊断日志的原有容量和留存规则均不改变。


## 5. 生命周期静态证据

- 主 GUI：`src/gui.rs::run_with_engine_overrides` 先解析普通/测试状态目录，再调用 `logging::init`，然后创建窗口、加载设置与运行 GUI；没有移动入口。
- 主程序后台：`src/main.rs` 先识别 `--acp-http-service` / `--xberg-broker`，解析状态目录，初始化并持有 Guard，再 serve；退出路径仍显式刷盘。
- worker：`optional/snap-ocr-worker/src/main.rs` 的代理及 service 分支沿用相同初始化/Guard/退出路径。没有改变命令解析或 help/version 行为。
- 依赖：主包及 worker 的 Cargo.toml 已声明 `tracing`、`tracing-subscriber`、`tracing-appender`；主包 `src/lib.rs` 与 worker path 模块均导出共享 API。未增加依赖或第二套框架。
- 机制：`selected_level` 与 `init` 证明阈值选择；`DiagnosticEvent`/`DiagnosticFields`/`header`/`quoted` 证明单行、UTC 微秒时间戳及字段去敏；`BoundedAppender`/`summarize_record` 证明 UTF-8 记录/文件容量；`product_log_name`/`retain_owned`/`prune_expired` 证明确认归属、唯一保留与排序。
- 既有 `application_started` / stopping / stopped / panicked、fallback、retention 及 queue 事件身份保留。本次没有新增或移动业务日志站点，没有边界包装器或跨文件关联传播；仅令既有启动事件的 level 字段反映实际配置并报告打开的初始分段。

## 6. Phase 2 覆盖与报告

逐文件覆盖三类已在目标显式可见的站点：A 错误/异常分支，B 关键/边界调用及本地可观察结果，C 重要状态变化。复用足够的既有事件，只补缺失的安全目标/阶段/结果元信息；不为每个普通 helper 配一组 INFO 事件，不叠加已足够说明失败的纯传播记录。

开始和完成/失败事件在现有插入点允许时记录 `elapsed_ms`；无本地失败处理路径、必须新增捕获/finally 或重写返回表达式才能得到的结果，报告为 **unobservable outcome**，不制造观察路径。明确分支有缺口但没有可用 API/仅新增日志的插入点，或必要关联阅读后业务级别仍无法确定，报告 **blocker**。其他文件站点是 scope exclusion；没有相关阅读不能凭想象把目标内站点排除。

报告只列：covered sites、unobservable outcomes、scope exclusions、具体 blockers、实际静态检查及真实 runtime 状态；不声称全部项目可观测，不自动追加独立复核或第二轮覆盖统计。

## 7. 本次验证证据与边界

上级仓库规则要求运行验证，覆盖技能静态限制，已在运行前披露。本次仅执行隔离日志基础设施；因此 `runtime_verification: run_limited`，不能写 `not_run_by_design`。不运行 GUI、真实网络、模型、截图或完整测试门。

- `cargo test --features test-hooks --lib logging::tests::`：退出码 0，主测试集 12 passed / 0 failed；包括 UTF-8/LF 容量、恰好 1 MiB 后分段、总数 10、未知压缩文件保护、超大事件的身份/错误元数据保留、既有超大日志不破坏，以及已有去敏/备用目录/panic/关联用例。子进程隔离测试不重复计作额外合同覆盖。
- `python .tmp/logging-foundation/smoke.py`：首次并发场景失败，末尾事件缺失；静态检查发现逐条锁导致队列未在框架既有 1 秒关闭等待内排空。改为后台批量写入后，同一六组阈值和 12 进程×1,000 条并发场景退出码 0；DEBUG 80 条完整保留，warning/error 正确抑制低级，未设/非法值回 INFO。最终还从公开 `init`/`tracing` 入口验证 100 层 span 下超大事件的截断与事件级错误字段优先级；最终输出 `PASS multiprocess=12 owned_files=10 maximum_bytes=343437 final_event_present=true`。未改业务/worker 超时或收窄场景。该 throwaway driver 与产物在收尾清除，保留此失败→修正→通过记录。
- `cargo check --manifest-path optional/snap-ocr-worker/Cargo.toml --features test-hooks --lib`：退出码 0，共享 worker 编译通过。
- `cargo clippy --features test-hooks --lib -- -D warnings`：退出码 101，报告 58 个 lint 错误，包含 ACP/解压/资产/Git/GUI 等非本次目标文件，以及日志文件既有的分号、引用传递、filter match 风格问题。未修改其他文件来消除它们；本次调整过的 filter 已改为 let-else，但未据此声称全仓 Clippy 通过。
- `rustfmt --edition 2021 --check src/logging.rs`：退出码 0，范围仅共享基础设施文件。
- 超大记录的同名 span 字段优先级回归：`cargo test --features test-hooks --lib logging::tests::oversized_record_preserves_identity_and_error_metadata -- --exact` 修正前退出码 101，断言失败 `缺少诊断字段：stage="连接失败"`；摘要改为保留末尾事件级错误字段，不丢失最终失败上下文。最终完整日志局部测试集退出码 0，包含此例通过。保留该回归断言，不修改或放宽预期。

静态机制无未解决的初始化缺口；运行验收只涵盖上述实际范围。真实用户日志库存未扫描、清理或迁移；若实际落点包含超大自有历史，初始化会拒绝该落点并按既有备用策略退化，该库存迁移需另行授权，不可借本包 ready 抹去运行期 blocker。完整质量门、真实 GUI/模型/网络链路与发布验收未执行。

## 8. 2026-10-11 独立日志补齐批次

本节与上节日志基础设施的历史验证记录分开：本批只补现有业务调用中的诊断事件、阶段、类型/码、耗时及安全统计，不修改日志基础设施、业务返回结果、重试次数、超时、协议、界面运行日志或需求可信基。按文件独占分配 task 工作池；已有完整覆盖的文件保持不变，不为纯 helper 或透传层重复添加日志。

本批禁止所有测试（含单文件、冒烟、E2E）、全仓格式化、全仓静态检查及三级门禁；历史通过结果不算本批证据。检查只限本批修改文件的非测试检查。跨文件类型、运行时落盘、真实网络、模型、截图与桌面行为均未验证，不据此宣称 P-10 全链路或功能验收通过。

对于没有本地错误分支、已合并或丢弃具体错误类型/码的结果，以及 HTTP/SSE 响应是否实际送达，不新增业务捕获、处理、重试或协议传播来制造详细诊断。日志专用判断只观察现有调用返回，不改变调用次数或原业务结果；已有错误映射闭包保留原返回表达式，仅插入安全日志。

本批完成 33 个工作池文件项：25 个文件补充日志，8 个文件已有充分覆盖而保持不变；主代理另为结果窗既有前台切换重试/任务栏回退补充日志，共修改 26 个 Rust 文件。本文件仅追加独立批次说明；需求、可信基、依赖及日志基础设施未修改。

文件级检查：逐个修改文件执行 `rustfmt --edition 2021 --config skip_children=true --emit stdout <文件>`，26 次退出码均为 0、stderr 均为空；格式化结果仅保留在内存，未写回源码，不递归处理模块。该证据仅证明单文件解析成功，不是格式一致、跨文件类型、编译或运行验证。精确差异逐项静态审阅；文件级去除诊断宏/局部计时后的 token 对照辅助核查，其余差异仅为诊断分类/阶段及日志专用分支。测试、冒烟、E2E、构建、仓库级检查和门禁均未运行。
