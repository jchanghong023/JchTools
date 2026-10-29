## MODIFIED Requirements

### Requirement: P-12

JchTools MUST 遵守本条行为：自动化验收须覆盖已确认的必验需求范围，按需求编号建立可核对的需求与测试映射。

#### Scenario: P-12 完整行为与适用边界
- **WHEN** 用户启动或使用 JchTools、执行文件任务或进行相应交付验收，且涉及本条规定的条件
- **THEN** 自动化验收须覆盖已确认的必验需求范围，按需求编号建立可核对的需求与测试映射。完整功能验收以实际 Windows 全量测试及对应矩阵证据为准；本地增量门通过仅证明所选用例通过，不得冒报完整验收，相关输入未变的有效本地验证可复用。CI 仅编译、打包、发布，不运行功能测试或质量门；CI 成功不代表功能验收，也不替代独立桌面、模型或交付验收。转 Markdown 的默认格式范围按 openspec/specs/markdown-conversion/spec.md 附录 A 的常用新老 Office、PDF、MP4 执行；已有扩展格式测试保留为可选，不计为通过，也不阻塞常用格式验收及发布。GUI 测试从真实公开入口覆盖全部可经界面触发或观察的需求及跨功能、设置链路，断言实际业务结果、状态和相关产物，覆盖核心成功与相关失败、停止和取消路径；内部逻辑、协议、资产及交付约束由关联的 UT、集成和交付验证共同覆盖，不遗漏非 GUI 约束。底层单测、模拟、仅完成点击或没有崩溃不能代替对应真实 GUI E2E 与可观察结果断言，未覆盖或未执行项不得报告通过。
  - 本地三级门分离日常功能回归与发布交付验收：fastcheck 保持静态检查、格式与编译，不运行测试；fulltest 的全量模式包含 fastcheck 及全部适用的 Windows 非打包 UT / integration / doc / e2e，保留可选组件、真实 7-Zip / Xberg / 媒体及隔离 GUI 覆盖，但不要求 ISCC、不生成安装包或便携 ZIP、不执行 `package-windows.ps1`、`package-windows-unit`、`fetch-7zip-unit`、仅为发布验证的 `release-target-build` 或在线全量供应链审计 `pyquality-pip-audit`。fulltest 通过不证明 P-05 / E 分区的打包交付验收或实时漏洞库审计通过。slowtest 将相应 fulltest 覆盖执行一次，并保留原在线供应链审计及 P-15 的完整本地发布构建与双产物校验；不先执行 fulltest 再重复同一功能覆盖。在线审计依赖实时外部漏洞库，归 slowtest，原命令及失败判据不变。
  - 按用户最新批准策略，本地门默认增量，`--full` 仅在独立明确的全量验收时选择，不常规用于本地或 CI；CI 不安装测试或质量依赖及 cargo-nextest，不准备本地测试引擎目录，不运行功能测试、lint/质量门、真实模型或桌面，保留生产依赖和交付资产，以 `package-windows.ps1 -SkipTests` 打包。`check.yml` 手动编译打包并保留构建产物，`release.yml` 手动编译打包发布；发布仍须单独授权。本地静态检查和编译仍执行，业务实现、断言和 feature / profile / target 维度不变；正常保留 Cargo target 与增量编译缓存，不建立自有成功账本。fulltest / slowtest 使用 [cargo-nextest](https://nexte.st/)（至少 0.9.116，采集需要 `NEXTEST_BINARY_ID`）；普通 Rust 阶段默认实验性 [cargo-affected](https://github.com/max-sixty/cargo-affected/tree/v0.4.1) `run` 选择受影响测试，`--full` 和依赖真实引擎、媒体等外部状态的阶段使用 `cargo nextest run` 全量执行；文档测试始终使用 `cargo test --doc`。统一根 `.config/nextest.toml`、零重试及首错停止；默认过滤真实桌面，另获授权的桌面验收可显式忽略默认过滤，不运行 ignored 模型测试。PowerShell 验收与未使用 `-SkipTests` 的打包使用全量 nextest 和 Cargo 文档测试。cargo-affected 0.4.1 官方称极早期且支持 Windows MSVC；增量通过不证明完整覆盖。
  - 默认增量门用原生 `cargo affected status --report-json` 检查 coverage 基线；缺失或失效且 Git 工作树干净时，原生 `cargo affected collect` 一次采集整个 workspace（含两个 optional 包），真实执行普通 Rust 测试，本轮不重复三个普通 Rust 阶段；真实引擎、媒体及文档测试仍全量执行。有效基线由原生 `run` 按 Git 基线选测；脏源码按采集 SHA 差异选择，不保证任意脏树重复运行均零选中。缺失或失效基线且工作树脏时明确提示并保守回退全量，不使用 `--allow-dirty` 或自动提交；基线维护仍用原生 `collect` / `collect --diff`。工具使用隔离插桩构建目录，不清正常 target、不全局更改 RUSTFLAGS；`--full` 不自动采集，仍为显式全量路径。基线采集和零选中均不证明本轮全部功能覆盖。本次用户授权仅本地提交本会话变更以建立干净基线，不推送、不触发 CI 或发布。
  - 根 `Cargo.toml` 的 cargo-affected metadata 对非 Rust 输入、`build.rs`、vendor、脚本、夹具及 `.cargo` 配置等保守选择全部测试；独立实验 `scripts/test_affected_selection.py` 检查真实选测行为。该工具仍处于早期，不保证选择绝对精确，增量通过不证明完整覆盖。
  - 普通测试并行度按 `min(15, 可用CPU核心数)` 动态计算，CPU 数不可用或为零时取 1。门计划器为 nextest 和 cargo-affected 转发显式 `--test-threads`，PowerShell 验收及打包同样计算并传参；根 nextest 配置的 `test-threads = 'num-cpus'` 本身不能表达 15 上限，手工运行若需相同上限必须显式传参。物理共享引擎组最多一个测试；Cargo 编译 jobs 不因测试并行度改变。
  - Python 测试以 pytest、[pytest-testmon](https://testmon.org/) 和 [pytest-xdist](https://pytest-xdist.readthedocs.io/en/stable/distribution.html) 执行：testmon 缓存依赖并跳过未受影响测试，本地可跟踪套件使用 `--testmon`，以 `TESTMON_DATAFILE=.tmp/testmon/<stage>.testmondata` 和 `--testmon-env` 按阶段隔离；xdist 并行执行选中用例。适用阶段使用 `pytest -p scripts.pytest_gate -x -n <动态并行度> --dist loadscope --max-worker-restart 0`，门计划器按上述 CPU 上限计算，同一 class / module 分配同一 worker，崩溃不自动重启；本地全量 `--full` 使用 `--no-testmon`，但适用阶段仍保留 xdist 并行；涉及静态资源或子进程等外部状态且无法可靠跟踪的套件每轮保守全量。controller 与各 worker 的日志和报告分别隔离。原生选择数据库不是完整验收证据。仓库当前无自有 TypeScript 工程，不安装 Bun 或创建空任务；将来有自有 TypeScript 时使用用户指定 Bun 1.4.4 的 `bun test --changed` 本地增量，不为此引入统一框架。
  - 阶段顺序执行，保留共享 Xberg / 媒体和真实 / 随包 7-Zip 的安全隔离，编译及产物生产者有序；实际高风险 `trusted-` / `test-gate-` / `static-` 检查与测试须先通过，才执行普通组。清单发现不执行测试体，不代替测试通过；保留编译、产物、发现与执行的真实依赖。
  - 本次用户明确批准取消三级门的脚本硬时间限制、编译观测扣时及自研失败监督。记录阶段和总墙钟作为观测，不以耗时阈值判败。连续两轮默认 `python scripts/test_gate.py fulltest --authorized` 须在代码和环境不变下均通过，以真实输出分别汇报选测、运行情况及实际耗时，证明第二轮大幅减少；增量 PASS 不代表全量覆盖。
  - 测试失败停止由 pytest `-x` 和 Nextest `--max-fail=1:immediate` 原生执行，允许运行器自身清理；入口以标准子进程错误传播非零结果，不要求提前强杀进程树或报告阻塞时硬退出。每轮和独立命令使用包含进程标识及唯一运行标识的独占日志目录，阶段日志、测试结果与打包计时不得跨轮复用、追加或覆盖；未执行项如实报告，不以历史记录代替本轮证据。
  - fulltest 和 slowtest 每次运行须获当次明确授权，入口声明不创造授权，不静默重跑到绿；Windows-only 边界不引入 WSL。CI / 发布、抢占焦点的真实桌面 driver 及真实模型调用仍须门外独立授权；门外项目未执行不冒报通过，不因其独立授权边界而判门失败。环境或工具缺失的覆盖缺口如实标注，不能以编译或门通过代替未执行项的验收。

### Requirement: P-15

JchTools MUST 遵守本条行为：Windows 完整安装包构建脚本记录每个实际步骤及总墙钟耗时，至少覆盖准备、外部资产获取或离线校验、字体与引擎校验、截图 worker 构建、清单准备、主程序构建、产物组装、许可收集、打包前校验、安装包编译、便携 ZIP 压缩；成功、失败与未执行步骤的状态如实记录，异常终止也保留已经发生的耗时，不把整条 Cargo 命令的时间冒充纯编译时间。

#### Scenario: P-15 完整行为与适用边界
- **WHEN** 用户启动或使用 JchTools、执行文件任务或进行相应交付验收，且涉及本条规定的条件
- **THEN** Windows 完整安装包构建脚本记录每个实际步骤及总墙钟耗时，至少覆盖准备、外部资产获取或离线校验、字体与引擎校验、截图 worker 构建、清单准备、主程序构建、产物组装、许可收集、打包前校验、安装包编译、便携 ZIP 压缩；成功、失败与未执行步骤的状态如实记录，异常终止也保留已经发生的耗时，不把整条 Cargo 命令的时间冒充纯编译时间。重复构建时，同内容的资产和清单保持稳定构建路径及时间戳；内容真实变更仍执行原校验并触发必要重编。记录只写开发验证用的 `.tmp/`，不改变产品日志；CI 仅编译、打包、发布，不运行测试或质量门。
  - 完整发布交付验证归入 slowtest：保留 `release-target-build` 原发布全目标构建命令、`package-windows-unit` 与 `fetch-7zip-unit` 全部专项测试，以及 `scripts/package-windows.ps1` 全程生成安装包 EXE 和便携 ZIP；保留引擎内嵌、许可证与合规材料、独立 7-Zip 引擎不泄漏、随包运行资产及两种产物原有内容校验和断言。此分层只移动执行归属，不删除或弱化检查，不改变独立桌面、真实模型或 CI / 发布的授权边界。

