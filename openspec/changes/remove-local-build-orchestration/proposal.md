# Proposal

## Why

2026-10-29 用户明确要求删除现有本地构建编排系统的全部相关内容，后续不再使用；随后明确批准本地使用原生增量测试；此前 CI 全量测试策略由本次明确批准的 CI 仅编译、打包、发布策略覆盖。最新要求仅测试本项目 Rust 产品，保留 Python 驱动的 Rust 验收，不测试脚本及选测工具自身。测试并行度为 `min(15, 可用CPU核心数)`。失败后继续其余可运行阶段和用例，最终完整汇总并非零退出；取消脚本硬检查时间和自研失败监控，使用原生运行器。真实桌面、模型和发布仍各需独立授权。

## What Changes

- 本地三级门直接执行检查、编译与 Rust 产品测试；默认增量以实验性 cargo-affected / nextest 选择适用测试，`--full` 使用 nextest 全量，Cargo 文档测试始终全量。外部状态相关测试保守全量，增量通过不冒报完整验收。
- 只测试本项目 Rust 产品，不测试 Python、PowerShell 脚本或选测工具自身；保留以 Python 驱动的 Rust 产品验收，静态检查与类型检查不受影响。
- 默认增量门先以原生 `cargo affected status --report-json` 检查 coverage 基线；缺失或失效且 Git 工作树干净时，原生 `cargo affected collect` 一次采集整个 workspace（含两个 optional 包），真实执行测试，之后仍运行三个普通 affected 阶段：原生工具跳过已有有效覆盖且未变化的测试，无 coverage 或不适合插桩的测试保守普通执行，不重复已有效覆盖测试。jchocr 的 `review_remediation_url_{content_length,chunked,without_length}_budget_plus_one_is_network_error` 三个 384 MiB 容量边界用例仅从插桩 collect/status 排除，普通 affected 和 `--full` 仍执行，保留 30 秒上限和断言；子进程损坏的 profraw 不伪造 coverage，缺覆盖的测试由原生工具保守重跑。真实引擎、媒体及文档测试仍全量执行。有效基线随已提交 HEAD 前移而无当前 equal 锚且工作树干净时，使用原生 `collect --diff` 更新；脏源码按采集 SHA 差异选择，不保证任意脏树重复运行均零选中，dirty 不使用 `--allow-dirty`，基线缺失或失效时明确提示并保守回退全量。原生 SHA / DB 是唯一选测状态，不自建成功账本；工具使用隔离插桩构建目录，不清正常 target、不全局更改 RUSTFLAGS；`--full` 不自动采集。基线采集和零选中均不证明本轮全部功能覆盖。
- 本地开发使用 `python scripts/native_test_tools.py` 安装 `.tmp/native-tools/cargo-affected/bin` 私有工具，固定上游 SHA `08000ff968eebb238a26a3a4de38fe314e8ca05e` 加兼容补丁的 `0.4.1+jchtools.2`，兼容 nextest 默认过滤和显式 filter 的最小筛选，并包含 Windows LLVM profile 路径、上游 Windows 源码暂存目录碰撞及过长路径兼容补丁。缺失或版本不符时提示安装，不静默使用旧版；工具不进入 CI 或产品包，不修改全局 Cargo bin 或正常 target。
- nextest 配置使用 `num-cpus`，脚本驱动的 nextest、cargo-affected 转发参数以及 PowerShell 验收和打包均显式按同一动态上限传 `--test-threads`；CPU 数不可用或为零时取 1。原生配置本身不能表达 15 上限，物理共享引擎组仍最多一个测试，Cargo 编译 jobs 不变。
- 根 `Cargo.toml` 的 cargo-affected metadata 对非 Rust 输入、构建脚本、vendor、脚本、夹具及 `.cargo` 配置保守选择全部测试，实验工具不保证绝对精确。
- CI 仅保留生产依赖和交付资产，以 `package-windows.ps1 -SkipTests` 打包；不安装测试或质量依赖及 nextest，不准备本地测试引擎目录，不运行功能测试、lint/质量门、真实模型或桌面。`check.yml` 手动编译打包并保留构建产物，`release.yml` 手动编译打包发布，发布仍须独立授权；CI 成功不代表功能验收。仓库无自有 TypeScript 工程，不增加空任务或 Bun 依赖；将来有自有 TypeScript 时使用 Bun 1.4.4 的 `bun test --changed`。
- 保留正常 Cargo target 和增量编译缓存、测试安全隔离、生产者顺序、高风险优先和逐轮独占分配日志目录，仅错误时输出保留的日志路径；阶段顺序执行。失败后继续所有可运行阶段与用例，使用 Nextest 原生非 fail-fast 行为，最终完整汇总并非零退出；必要前置缺失的依赖项明确报告 FAIL、UNVERIFIED 或 NOT RUN，用户主动中断不强制继续。删除硬时间门、编译扣时及自研失败监控和强杀；实际墙钟仅供观测。
- 按用户最新明确批准，三级门隐藏成功及正常 skip 用例，不向控制台输出其名称、输出或逐条耗时，也不持久保留成功用例明细；只展示和保存失败 / 错误用例的名称、诊断与耗时。保留不含成功用例身份的整轮状态、阶段及总耗时、实际执行和选测 / 缓存数量等真实聚合汇总。stdout 可临时捕获供诊断或 metadata 消费，成功后不持久保留；LLVM coverage / 依赖数据库是选测必要状态，不当日志删除或伪造。
- 保留测试断言、构建维度、当次授权以及真实桌面和模型的独立边界；更新代码、测试和现有说明，不改业务 Rust、UI 或第三方材料。

## Capabilities

### New

无。

### Modified

- `application-contract`：完整替换 P-12 的本地门执行与证据规则，并同步 P-15 的 CI 边界，保留其他验收要求。

## Impact

影响本地门脚本及其测试、工程说明、CI 配置和应用合同。验收应核对旧系统相关实现和配置已删除、全量覆盖与执行顺序未缩减、增量选择及安全隔离边界明确，并按当次授权在代码和环境不变下连续运行两轮默认 `python scripts/test_gate.py fulltest --authorized`，要求均通过并以真实输出分别汇报选测、运行情况及实际耗时，证明第二轮大幅减少；增量 PASS 不代表全量覆盖，`--full` 仅用于独立明确的全量验收，不常规用于本地或 CI，不设耗时阈值；未运行的功能验收不得报告通过。编排代码位于产品目录之外，不改变可信基隔离。本变更不自动归档。
