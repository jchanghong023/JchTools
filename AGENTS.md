# AGENTS.md · JchTools 协作约定

本仓库由 AI 代理实现和维护；规范性用语遵循 RFC 2119。实现应满足当前需求，不扩大范围或依赖用户读代码、手工回归。

## 0. 会话快速上手（每个代理会话按此主线执行）

1. 按 [README 需求与工程入口](README.md#需求与工程入口)选择功能域，补读适用的全局约束及交叉引用；普通任务不要预先通读全部规格。`openspec/specs/` 是唯一已批准需求基线。
2. 新需求先形成提案并确认批准；缺陷对照有效规格，先写修复前失败的回归测试；不为纯实现变化制造需求变更。
3. 动工前记录已有验证基线及未运行项；完成后按影响和当次授权验证，报告命令、退出码、关键输出和未验证范围。历史结果不能代替本轮门内所选检查。（无执法点 · 软法）
4. 遵守临时数据、测试授权和可信基隔离规则；不清理其他会话的文件。提交时按变更性质记录必要理由和证据。

## 1. 项目背景

JchTools 是 Windows 本地工具箱：Rust 2021、Slint、SQLite 和 7-Zip。GUI 入口为 `src/main.rs` → `crates/gui/src/gui.rs`，工具注册表为 `crates/gui/src/registry.rs`，界面源为 `ui/`；独立 OCR CLI 为 `crates/jchocr/`。新增 GUI 工具须注册并接入真实页面，不要把通用导航写成只服务某个工具；jchocr 不是 GUI 工具。

[OpenWiki 工程导航](openwiki/quickstart.md)提供按任务索引的架构、工作流、验证和交付说明；Wiki 可能过时，规格、源码及实际测试结果分别是需求、实现及验证的依据。新增需求能力时同步本权威分工、README 需求入口及 `scripts/static_check_impl/misc.py` 的 `requirements_registry` 登记；登记不代表实现或验收通过。子级协作文件只维护子树专属约定。

## 2. 临时文件规则（强制）

- 测试、验证、截图、日志、下载及中间产物放在仓库 `.tmp/`；项目临时目录与测试数据通过 `python scripts/make_tmp.py` 创建。测试内置 `tempfile` / `std::env::temp_dir()` 夹具及 `tests/` 中长期样例按原约定使用。
- `.tmp/` 不提交、不打进发布包；结束后只清理本会话创建的子目录，确认没有并行会话使用时才可运行 `python scripts/make_tmp.py clean`。（无执法点 · 软法）
- `python scripts/make_tmp.py testdata` 会先清空目标目录；普通处理验收不加 `--git`，该选项只用于验证 Git 根目录拒绝行为。

## 3. 构建与测试

Windows PowerShell 常用入口：

```powershell
cargo run --bin JchTools
cargo build
python scripts/test_gate.py fastcheck
python scripts/test_gate.py fulltest --authorized  # 须获当次明确授权
python scripts/test_gate.py slowtest --authorized  # 须获当次明确授权
```

构建需要 `x86_64-pc-windows-msvc`、VS C++ Build Tools 和 Windows SDK；打包使用 `scripts/package-windows.ps1`。隔离测试显式启用 `test-hooks`，生产和打包构建不启用。其他入口、依赖和实际覆盖见 [OpenWiki 验证](openwiki/testing/verification.md)与[构建交付](openwiki/operations/build-and-delivery.md)。

### 3.2 测试验收纪律（代码与测试均由 AI 代理产出）

- 缺陷修复 `MUST` 先写修复前失败的回归测试，并在提交信息记录复现命令和失败摘要；仅有修复后通过不算完成。
- 不得为通过而删除、改名、放宽测试或新增 `#[ignore]` / 平台门禁；确需变更时更新 `scripts/test-baseline.json`（`python scripts/static_check.py --update-test-baseline`），提交信息写明理由，平台门禁须注明原因。不得静默重跑到绿；flaky 须查因并记录。
- 产品功能变更按行为、风险及已批准验收条件选择最小充分自动化验证，覆盖相关成功和关键失败路径；需要真实入口及结果的场景不能用编译、模拟或无崩溃代替。只测试本项目 Rust 产品，不测试 Python、PowerShell 脚本或测试工具自身；Python 驱动产品验收及静态、类型检查保留。（无执法点 · 软法）
- 引擎、删除路径、解压安全或用户可见行为的实质变更，`SHOULD` 由独立代理复核并记录命令、环境、退出码和关键输出。验证合同条目的测试注明需求编号，每条合同至少有一个测试引用；候选引用不是断言或执行证据。
- 提交信息按变更性质记录：缺陷反证、合同增改理由、可信基变更理由、宣称通过的本地证据，以及宣称发布成功的 release run 与版本链接。未执行项不得报告通过。
- UI 测试优先使用不占用用户鼠标的方式；真实桌面驱动须当次另行明确授权，抢占鼠标的流程还须用户当次明确要求使用 [e2e.md](e2e.md)。无头 MCP 不替代真实窗口验收，运行时须按该文档隔离状态、CodeAgent 凭据及注册表。（无执法点 · 软法）

### 3.3 验收方法（怎么测试、怎么验收）

纯文档等非功能性修改只检查内容、差异、引用和需求保留，不运行产品构建或测试。功能修改按影响选测；以下是独立完整 Windows 验收矩阵，不要求普通提交或影响范围门后追加全量。未执行或缺环境的项目标为未验证，局部通过不能冒报完整验收。（无执法点 · 软法）

| 场景 | 完整验收所需证据 |
|---|---|
| 功能验收基线 | 根 workspace 和两个 optional 包的全量 nextest、各自 doctest 及 `python scripts/static_check.py`；默认过滤真实桌面 |
| 引擎、解压、删除及路径安全 | 基线 + `acceptance.ps1 -WithEngine` + `tests/gui_flow.rs` |
| UI 源或 GUI 装配 | 基线 + `tests/gui_flow.rs` + `scripts/gui_smoke.py` S1–S4、S18 的两档窗口布局断言 |
| 递归解压端到端 | 测试数据 → GUI 开始、确认、完成 → 按 X 与适用 H/S、共用附录断言 |
| 目录整理端到端 | 测试数据 → GUI 分析、计划、确认、执行 → 按 C 与适用 H/S、共用附录断言 |
| 打包、发布及引擎捆绑 | slowtest 完整打包及安装包/ZIP 内容校验 + 干净目录运行；真实桌面及发布另获授权 |

完整基线的手工入口如下；逐条记录退出码，直接执行完整验收须遵守下节的授权边界，真实桌面不因矩阵自动获授权。PowerShell 手工 nextest 须显式限制线程数：

```powershell
$threads = [Math]::Max(1, [Math]::Min(28, [Environment]::ProcessorCount))
cargo nextest run --all-targets --features test-hooks --test-threads $threads
cargo test --doc --features test-hooks
cargo nextest run --manifest-path optional/snap-ocr-core/Cargo.toml --all-targets --features test-hooks --test-threads $threads
cargo test --manifest-path optional/snap-ocr-core/Cargo.toml --doc --features test-hooks
cargo nextest run --manifest-path optional/snap-ocr-worker/Cargo.toml --all-targets --features test-hooks --test-threads $threads
cargo test --manifest-path optional/snap-ocr-worker/Cargo.toml --doc --features test-hooks
python scripts/static_check.py
```

独立入口为 `powershell -NoProfile -File .\scripts\acceptance.ps1`，可按场景加 `-WithEngine` / `-WithPackage`；`-WithDesktopTests`、`-WithGuiSmoke` 和 `-WithMarkdownAcceptance` 不创造桌面授权。转 Markdown、截图及共享引擎的具体验收以对应规格为准；局部测试、模拟引擎或历史证据不证明真实模型及桌面链路。

### 3.4 三级测试门与执行权限

三级门以 [P-12/P-15](openspec/specs/application-contract/spec.md) 为准，操作及覆盖边界见 [OpenWiki 验证](openwiki/testing/verification.md)：

- `fastcheck`：静态、类型、格式和编译，不运行测试。`fulltest`：上述检查及受影响的适用 Windows 非打包产品测试，全程离线；缺少引擎、模型或缓存时报告 FAIL / UNVERIFIED，不联网补齐。`slowtest`：执行一次相同受影响功能覆盖，并按范围进行在线准备、发布构建和本地双产物打包校验。
- `fulltest` / `slowtest` 每次运行 `MUST` 获得当次明确授权，`--authorized` 只是声明；绕过入口直接运行内部阶段同样遵守授权。真实桌面、真实模型调用、CI/远程流水线和发布须另行授权；三级门不触发 CI、远程流水线、发布、ComputerUse、全局鼠标或焦点抢占测试。未执行的独立项目不冒报通过，也不因此判门失败。（授权为无执法点 · 软法）
- 影响范围由上游 cargo-delta 和 Cargo 依赖图决定，选中 crate 的适用测试每轮实际执行；已知动态输入盲点须报告。分析失败、基线不可信或工具缺失时报告 FAIL / UNVERIFIED，仅继续无依赖全局静态检查，不自动改跑全量。仅原生确认全包受影响或另行明确完整验收才全量。
- 阶段失败后继续其余可运行部分，保留失败明细和本轮独占日志，汇总实际范围、数量及耗时；不复用旧成功结果，不清空其他运行证据。CI 仅编译、打包或发布，不运行测试/质量门，成功不等于功能验收。
- fulltest 的固定 Xberg 测试引擎为 `C:\Users\jiang\Documents\xberg-test\xberg-cli-x86_64-pc-windows-msvc`；`E:\xberg` 仅作只读接口参照，`MUST NOT` 构建或产出测试/发布物。真实引擎、模型与媒体按规格核验，缺失时如实报告。（无执法点 · 软法）

### 3.5 构建与缓存纪律

保留有效 Cargo `target/` 和增量缓存，不用 `cargo clean` 伪造提速；单轮门保持 toolchain、target、profile、features、flags、linker 和检查判据。改变链接器前须验证完整质量门兼容性；有效缓存不得按时间或目录名猜测为废弃。性能打点及稳定字段见 `crates/foundation/src/perf.rs`，诊断日志按 [P-10](openspec/specs/application-contract/spec.md) 维护。

## 4. 代码与界面实现约定

- 保留无关内容和行为，使用最小充分实现；注释、错误和界面文案用中文，关键步骤及异常分支写日志。
- UI 颜色使用 `ui/theme.slint` 的 `Design`；Slint 子项自身宽度不得绑定 `root.width` / `parent.width`，避免绑定环。产品行为以全局规格的 H/P/U 分区为准。
- 处理真实用户目录前优先用副本验证。改产品名、二进制名、图标或状态目录时同步 Cargo、资源、UI、注册表、脚本、CI、README 和相关规格。

## 7. 用户协作协议（本项目用户零创作）

不得要求用户写代码、文档或阅读代码 diff；必须请用户决策时使用附推荐项的封闭选择、并排截图的是/否判断或可直接复制的命令。报告先给结果，再给必要证据和未验证项。

## 8. OMP mini OpenSpec 需求维护

- `openspec/specs/<capability>/spec.md` 是唯一已批准基线；未批准需求只进入 `openspec/changes/<change-name>/proposal.md` 及对应 delta specs，不因实现或局部测试自动批准。（无执法点 · 软法）
- 用户可见行为变化先形成可验收断言及原生提案，确认批准后更新对应基线和交叉引用；需求删除或变更须用户明确批准。保留已有编号、有效语义和验收条件。代码与规格不一致时修代码，不为迁就代码削弱规格；仅修复违约或调整实现方式不制造需求变更。（无执法点 · 软法）
- 原生结构使用 `### Requirement:`、SHALL/MUST 及 `#### Scenario:` 的 WHEN/THEN。新增能力优先复用已有功能域；入口、命令或开发纪律变化同步本文。
- 查看：`omp openspec list --specs`；验证基线：`omp openspec validate --specs --strict --no-interactive`；验证提案：`omp openspec validate --changes --strict --no-interactive`。原生检查不等于功能验收或执行授权。

## 9. 可信基与防共谋

可信基包括本文件、`scripts/static_check.py` 及 `scripts/static_check_impl/`、`scripts/test-baseline.json`、`scripts/acceptance.ps1`、`scripts/gui_smoke.py` 及 `scripts/gui_smoke_impl/`、`scripts/make_tmp.py` 及 `scripts/make_tmp_impl/`、`scripts/markdown_acceptance.py` 及 `scripts/markdown_acceptance_impl/`，以及 `openspec/` 内原生需求与配置。

任何变更 `MUST NOT` 同时修改产品可执行实现（`src/`、`crates/`、`optional/`、`ui/`、`resources/`）和可信基；可信基变更须独立提交并注明理由。纯需求引注修正不是产品实现变更，保留已有无关改动。（执法待落地）

向本文新增 `MUST` 级条款时，同一变更须落地执法脚本，否则显式标注「无执法点 · 软法」；`SHOULD` 每月审计一次软法。

<!-- OPENWIKI:START -->
## OpenWiki

This repository has a generated `openwiki/` knowledge base. Use it as optional context when investigating unfamiliar architecture or behavior, or when source inspection leaves an important uncertainty. Use `openwiki_search` and `openwiki_read` to find relevant sections; if those tools are unavailable, consult `openwiki/quickstart.md` and its links. Source code and tests are authoritative when the Wiki is outdated.
<!-- OPENWIKI:END -->
