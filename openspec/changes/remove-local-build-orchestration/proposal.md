# Proposal

## Why

2026-10-29 用户明确要求删除现有本地构建编排系统的全部相关内容，后续不再使用；随后明确批准本地使用各语言原生增量测试；此前 CI 全量测试策略由本次明确批准的 CI 仅编译、打包、发布策略覆盖；补充批准 pytest-testmon 缓存依赖并选测、pytest-xdist 并行执行选中用例，测试并行度为 `min(15, 可用CPU核心数)`。本次用户明确批准取消脚本硬检查时间及自研失败即停实现，以原生运行器和标准错误传播取代，并授权本会话变更仅作本地提交以建立干净基线，不推送、不触发 CI 或发布。上述明确要求共同构成本变更批准依据，真实桌面、模型和发布仍各需独立授权。

## What Changes

- 本地三级门直接执行检查、编译与测试；默认增量以实验性 cargo-affected / nextest 和 pytest-testmon 选择适用测试，`--full` 使用 nextest 和 pytest 全量，Cargo 文档测试始终全量。外部状态相关测试保守全量，增量通过不冒报完整验收。
- 默认增量门先以原生 `cargo affected status --report-json` 检查 coverage 基线；缺失或失效且 Git 工作树干净时，原生 `cargo affected collect` 一次采集整个 workspace（含两个 optional 包），真实执行普通 Rust 测试，本轮不重复三个普通 Rust 阶段；真实引擎、媒体及文档测试仍全量执行。有效基线由原生 `run` 按 Git 基线选测；脏源码按采集 SHA 差异选择，不保证任意脏树重复运行均零选中。缺失或失效基线且工作树脏时明确提示并保守回退全量，不使用 `--allow-dirty` 或自动提交；维护基线仍用原生 `collect` / `collect --diff`。工具使用隔离插桩构建目录，不清正常 target、不全局更改 RUSTFLAGS，不造成功账本；`--full` 不自动采集。基线采集和零选中均不证明本轮全部功能覆盖。
- Python 适用阶段采用 `pytest -p scripts.pytest_gate -x -n <动态并行度> --dist loadscope --max-worker-restart 0`，由计划器按 CPU 可用数和 15 上限计算；本地 `--full` 用 `--no-testmon` 但保留适用阶段并行；controller 和 worker 日志分离。
- nextest 配置使用 `num-cpus`，脚本驱动的 nextest、cargo-affected 转发参数以及 PowerShell 验收和打包均显式按同一动态上限传 `--test-threads`；CPU 数不可用或为零时取 1。原生配置本身不能表达 15 上限，物理共享引擎组仍最多一个测试，Cargo 编译 jobs 不变。
- 根 `Cargo.toml` 的 cargo-affected metadata 对非 Rust 输入、构建脚本、vendor、脚本、夹具及 `.cargo` 配置保守选择全部测试；独立实验 `scripts/test_affected_selection.py` 检查真实选测行为，实验工具不保证绝对精确。
- CI 仅保留生产依赖和交付资产，以 `package-windows.ps1 -SkipTests` 打包；不安装测试或质量依赖及 nextest，不准备本地测试引擎目录，不运行功能测试、lint/质量门、真实模型或桌面。`check.yml` 手动编译打包并保留构建产物，`release.yml` 手动编译打包发布，发布仍须独立授权；CI 成功不代表功能验收。仓库无自有 TypeScript 工程，不增加空任务或 Bun 依赖；将来有自有 TypeScript 时使用 Bun 1.4.4 的 `bun test --changed`。
- 保留正常 Cargo target 和增量编译缓存、测试安全隔离、生产者顺序、高风险优先和逐轮独占日志；阶段顺序执行。删除硬时间门、编译扣时及自研监督与失败通知，pytest `-x`、Nextest `--max-fail=1:immediate` 负责原生失败停止，入口传播标准子进程错误；实际墙钟仅供观测。
- 保留测试断言、构建维度、当次授权以及真实桌面和模型的独立边界；更新代码、测试和现有说明，不改业务 Rust、UI 或第三方材料。

## Capabilities

### New

无。

### Modified

- `application-contract`：完整替换 P-12 的本地门执行与证据规则，并同步 P-15 的 CI 边界，保留其他验收要求。

## Impact

影响本地门脚本及其测试、工程说明、CI 配置和应用合同。验收应核对旧系统相关实现和配置已删除、全量覆盖与执行顺序未缩减、增量选择及安全隔离边界明确，并按当次授权在代码和环境不变下连续运行两轮默认 `python scripts/test_gate.py fulltest --authorized`，要求均通过并以真实输出分别汇报选测、运行情况及实际耗时，证明第二轮大幅减少；增量 PASS 不代表全量覆盖，`--full` 仅用于独立明确的全量验收，不常规用于本地或 CI，不设耗时阈值；未运行的功能验收不得报告通过。编排代码位于产品目录之外，不改变可信基隔离。本变更不自动归档。
