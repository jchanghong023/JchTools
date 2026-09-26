# AGENTS.md · JchTools 协作约定

面向在本仓库工作的 AI 代理与协作者。规范性用语遵循 RFC 2119（`MUST` / `SHOULD` / `MAY`）。

## 0. 会话快速上手（每个代理会话按此主线执行）

1. 通读本文件与固定需求目录 `docs/requirements/` 中的全部需求文档；该目录是产品需求的唯一权威位置。其余说明文档仅供参考，不得用来覆盖需求。
2. 明确任务类型：新需求 → 先按第 7 节协议转成合同条目并经用户确认；缺陷修复 → 先写能在修复前失败的回归测试；其余变更 → 确认不违反第 9 节可信基隔离。
3. 动工前记录基线：`cargo test` 与 `python scripts/static_check.py` 的当前状态。
4. 编码遵守第 4 节实现约定与合同相应分区；测试数据一律经 `python scripts/make_tmp.py` 生成。
5. 完成后按 3.3 验收矩阵执行相应层级验证；需要 CI 结论时按 3.4 跑 slowtest（`check.yml` 现仅手动触发，slowtest 是它的唯一自动入口），等 CI 转绿后按第 7 节格式报告（结论先行 + 命令/退出码/关键输出 + CI run 链接）。
6. 收尾：`python scripts/make_tmp.py clean` 清空 `.tmp/`，按第 8 节与 3.2 提交纪律写提交信息。

## 1. 项目背景

- **项目类型与技术栈**：自有项目 JchTools，用户个人使用的 Windows 本地工具箱，由 AI Agent 实现和维护。Rust 2021 + Slint 1.17、SQLite（rusqlite bundled）、7-Zip 引擎；产品定位与交付要求见合同 P / E 分区，打包入口为 `scripts/package-windows.ps1` 与 `installer/JchTools.iss`。
- **工具入口**：注册表当前接入递归解压、目录整理、MD 整理和 Git 工具，对应合同 X / C / M / G 分区。联网边界只按 P-03（含 Git 工具例外）执行，不以开发说明另行扩大或缩小。新工具 `MUST` 经 `src/registry.rs` 注册 + 真实页面接入；侧栏与导航 `SHOULD NOT` 写死只服务单个工具的文案或流程。
- **生产方式**：本项目全部产出（代码、测试、文档、CI）由 AI 代理完成；用户不编写任何代码或文字，只在封闭选择、看图判断与真实使用中给出意图和反馈（协作协议见第 7 节）。本文件的纪律条款用于对抗代理的自证偏差。
- **权威分工**：`AGENTS.md` 规定开发、测试和验收纪律；固定目录 `docs/requirements/` 保存全部产品需求，当前由 `CONTRACT.md` 统一维护，按其中的功能分区定位需求。旧 `docs/CONTRACT.md` 仅保留迁移链接，兼容静态检查的文件存在性要求，不再维护需求副本。其余文档仅为辅助说明。
- **代码入口**：`src/main.rs` → `src/gui.rs`（GUI 启动、回调与后台任务）、`ui/app.slint`（界面）、`src/registry.rs`（工具注册表）；`src/archive.rs`（递归解压）、`src/engine.rs`（整理分析与执行）、`src/md_tools.rs`（MD 合并与拆分）、`src/git_tools.rs`（Git 操作）、`resources/rules.json`（界面规则清单）。

## 2. 临时文件规则（强制）

- 一切测试、验证、截图、日志、下载与中间产物 `MUST` 放在仓库根目录的 **`.tmp/`** 下，例如 `.tmp/ui-test/`、`.tmp/screenshots/run1.png`、`.tmp/bench/`。
- `.tmp/` 已被 `.gitignore` 忽略：`MUST NOT` 提交、`MUST NOT` 打进发布包、`MUST NOT` 用 `git add -f` 强加。
- 测试临时数据 `MUST NOT` 写在仓库其它位置（仓库根、`src/`、`resources/`、`docs/`、`tests/` 都不行）。
- 需要长期保留的样例数据放 `tests/`，或直接用测试内置的临时目录（`tempfile` / `std::env::temp_dir()`）。
- 校验、清理或重建 `.tmp/` 的脚本 `MAY` 直接删除该目录内容，不会影响仓库文件。
- 项目所需的一切临时目录与测试数据统一由 `python scripts/make_tmp.py` 生成（种类可扩展，现有 `testdata`），默认落在 `.tmp/` 下；测试完成后用 `python scripts/make_tmp.py clean` 清空 `.tmp/` 释放磁盘，`MUST NOT` 让 `.tmp/` 无限增长。

## 3. 构建与测试

以下命令在仓库根目录的 Windows PowerShell 中执行；Rust / MSVC / Windows SDK 条件见下文。Python 脚本需 Python 及对应依赖；Git 工具测试需 PATH 中可用的 git。完整验收和发布仍受 3.4 授权约束，列出入口不代表本次已经执行。

```powershell
cargo run --bin JchTools   # 启动 GUI（默认 gui 特性）
cargo build                # 开发构建（GUI）
cargo build --release      # 发布构建
cargo test                 # 单元与集成测试（真实引擎用例默认 #[ignore]）
python scripts/static_check.py      # 结构/配置/回调/SQL/测试基线/界面规则静态检查
python scripts/test_gate.py fastcheck   # 三级测试门之快速门（AI 可自主，≤60s 硬超时；其余两级见 3.4）
powershell -NoProfile -File .\scripts\acceptance.ps1 -WithEngine   # 单命令验收（见 3.2）
powershell -NoProfile -File .\scripts\package-windows.ps1   # 生成含 7-Zip 的发布 ZIP
```

手工测试集：`python scripts/make_tmp.py testdata`（默认自动生成到 `.tmp/testdata/`，会先清空该目录；也可 `--destination` 另指专门测试目录，仓库内 `.tmp` 之外的位置会被拒绝）。普通处理验收不使用 `--git`：根目录含 `.git` 时按 H-06 拒绝整次处理；`--git` 仅可用于验证该拒绝行为。需要还原普通数据集时重新运行生成命令；测试完成后用 `python scripts/make_tmp.py clean` 清理。

- Windows 构建机需要 Rust `x86_64-pc-windows-msvc` + VS C++ Build Tools + Windows SDK（`rc.exe` 用于把 `resources/app.ico` 嵌入 EXE）。
- 提交前 `SHOULD` 至少跑 `cargo test` 与 `python scripts/static_check.py`，并确认构建输出没有新增 `binding loop` 警告。

## 3.1 7-Zip 引擎

引擎相关需求（官方来源、校验、运行期解析顺序、发布包合规）以 `docs/requirements/CONTRACT.md` 的 E 分区为准，本节只写工程事实：`scripts/fetch-7zip.ps1` 负责获取官方引擎（产物落在 `resources/7zip/`，已被 `.gitignore` 忽略，`MUST NOT` 提交）；构建时 `build.rs` 把引擎压缩编进 EXE（`src/engine_bundle.rs` 负责释放与校验），缺失引擎时构建不失败、仅不内嵌。

## 3.2 测试验收纪律（代码与测试均由 AI 代理产出，以下用于对抗自证偏差）

- **回归测试先行**：修复任何缺陷 `MUST` 先写能在修复前失败的回归测试，并在提交信息记录反证证据（复现命令 + 修复前失败输出摘要，slowtest 触发的 CI run 转绿后附 run 链接）。只有「修复后通过」而没有「修复前失败」证据的修复不算完成。
- **禁止削弱测试**：删除、改名、放宽断言、新增 `#[ignore]` 或平台门禁（`#[cfg(...)]`）`MUST` 同步更新 `scripts/test-baseline.json`（用 `python scripts/static_check.py --update-test-baseline` 重新生成）并在提交信息写明理由；`MUST NOT` 只为了让测试变绿而做上述改动。平台门禁 `MUST` 附带原因注释，且被门禁的行为 `SHOULD` 在另一平台仍有覆盖。
- **提交纪律**：提交信息按变更性质 `MUST` 携带对应记录——缺陷修复附回归反证（复现命令 + 修复前失败输出摘要）；合同增改附用户确认结果；可信基变更附理由与用户批准记录；基线再生成附 `[基线已确认]` 标记；宣称完成附 slowtest 触发的 CI run 链接（见下条「CI 权威」）。缺对应记录的提交 `MUST NOT` 合入。
- **完成条件**：见 3.3 验收矩阵；`MUST NOT` 只跑默认 `cargo test` 就宣称引擎 / UI / 发布相关工作已验证。
- **独立复核**：涉及引擎、删除路径、解压安全（`fsutil` / 覆盖语义）或用户可见行为的实质变更，`SHOULD` 由未参与实现的独立代理会话复跑验证并给出证据格式：命令、环境（OS / rustc / 是否真实引擎）、退出码、关键输出行。
- **CI 权威**：本地验证通过只是临时结论；对应 CI（`.github/workflows/`）run 转绿之前 `MUST NOT` 宣称变更已完成，宣称完成 `MUST` 附 CI run 链接；本地自报证据（提交信息、终端输出）视为线索而非判决。`check.yml` 现仅 `workflow_dispatch` 手动触发（push / PR 不触发），其 run 只能由 slowtest（`python scripts/test_gate.py slowtest --authorized`）或人类明确手动 dispatch 产生；未跑 slowtest 的变更只能如实标注「未验证」，`MUST NOT` 宣称完成。（无执法点 · 软法：无法机器判定「是否宣称完成」，靠会话纪律 + 3.4 的 `--authorized` 入口守卫）
- **flaky 政策**：`MUST NOT` 重跑到绿。测试间歇性失败必须查因；确属 flaky 的要在提交信息记录现象与原因，不得静默重跑。
- **人工验收边界**：用户已决定不保留人工验收项清单；自动化未覆盖的行为（如真实 TB 级数据、非 150% DPI、网络共享）`MUST NOT` 被代理宣称已验证，只能如实标注「未验证」。

## 3.3 验收方法（怎么测试、怎么验收）

「验收通过」= 下表相应行全部执行且通过 + slowtest 触发的对应 CI run 转绿（3.2 CI 权威）；`NOT RUN` 不得报告为通过。

| 场景 / 变更类型 | 必须通过 | 覆盖 |
|---|---|---|
| 任意变更（每次提交） | `cargo test` + `python scripts/static_check.py` | 合同全部条目对应测试 |
| 引擎 / 解压 / 删除 / 路径安全 | 上行 + `powershell -NoProfile -File .\scripts\acceptance.ps1 -WithEngine` + `tests/gui_flow.rs` | 合同 S / E 分区、C-01 |
| UI（`ui/app.slint` / GUI 装配） | 任意变更行 + `tests/gui_flow.rs` + `scripts/gui_smoke.py` S1–S4 + 两档窗口尺寸目视检查 | 合同 C / U 分区 |
| 递归解压端到端 | `python scripts/make_tmp.py testdata` 生成数据集 → GUI 走「开始解压 → 一段确认 → 跑完」→ 按合同 X / H 分区核对（成功原包及实际分卷按 X-05 删除、已有文件不变、冲突自动改名且后缀不变、失败原包进「解压失败」、Git 整树排除、后续新任务允许重新解压）→ `python scripts/make_tmp.py clean` 清理 | 合同 X / H 分区 |
| 目录整理端到端 | `python scripts/make_tmp.py testdata` 自动生成数据集到 `.tmp/testdata/` → GUI 按默认配置完整走一遍目录整理主流程 → 按合同 C / H 分区核对（Git 整树排除、成功后处理范围内无空目录、再次整理幂等）→ `python scripts/make_tmp.py clean` 清理 | 合同 C / H 分区 |
| 打包 / 发布 / 引擎捆绑 | `scripts/package-windows.ps1` 全程 + 干净目录解包运行 | 合同 E 分区 |

- 单命令入口：`powershell -NoProfile -File .\scripts\acceptance.ps1`（可选 `-WithEngine` / `-WithGuiSmoke -GuiData <目录>` / `-WithPackage`）。
- 改动过跟踪文件后，提交前须最后用 `git -c core.quotePath=false ls-files -z | grep -zv '^SHA256SUMS.txt$' | xargs -0 sha256sum -b > SHA256SUMS.txt` 重建清单（static_check 会校验其完整性）。
- **需求 ↔ 测试映射**：验证合同条目的测试 `MUST` 在其文档注释中标明合同编号（如 `// 覆盖 C-12`）；每个合同条目至少被一个测试引用。（现状：已有部分编号注释，但全条目覆盖尚未逐项核验，映射执法待落地。）
- 合同条目的拆分、合并或重编号 `MUST` 经用户确认。

### 自动化验证要求与当前边界

- 功能开发和功能性修改 `MUST` 同时具备 UT 局部逻辑验证与从真实公开入口到可观察结果的 E2E 验证；跨模块交互按需要增加集成测试，已有有效覆盖可以复用。不得依赖用户手工读代码或人工回归保证质量。（无执法点 · 软法）
- 测试 `MUST` 对应需求及验收条件，覆盖核心成功路径与相关关键失败路径；UT、编译、静态检查、局部模拟和“没有崩溃”不能替代完整链路的结果断言。桩与模拟仅作补充，未经验证的真实边界须明确说明。（无执法点 · 软法）
- 验证报告 `MUST` 区分已实现、验证通过、验证失败和未验证；环境、依赖或权限不足不得报告验收通过。（无执法点 · 软法）
- UT / 集成入口为 `cargo test`，可按目标运行 `cargo test --test md_tools`、`cargo test --test git_tools` 等。`tests/git_tools.rs` 使用真实 git 与本地 bare 远端，不覆盖真实网络及认证。
- GUI 链路入口为 `cargo test --test gui_flow`；`src/gui.rs` 内另有 MD 真实回调测试。它们使用 Slint 测试后端，不等同于真实桌面窗口验证。`scripts/gui_smoke.py` 的 S1–S4 覆盖启动、整理与解压，未覆盖 MD / Git 的完整桌面链路；Git 从 GUI 启动到推送结果的完整 E2E 覆盖尚未确认。后续相应功能变更应补齐所需链路，不能以底层测试冒充 GUI E2E。
- 纯文档等非功能性修改按实际影响检查内容、引用和需求保留情况，不机械新增功能测试；本仓库已有基线、提交检查与 CI 完成条件仍按 0 / 3.2 / 3.3 执行，未执行项如实标注。

## 3.4 三级测试门与执行权限

统一入口 `python scripts/test_gate.py <fastcheck|fulltest|slowtest>`；三级语义固定，`MUST NOT` 按需要改写层级含义，也 `MUST NOT` 把耗时或远程阶段塞进更低层级。平台范围按合同 P-07 仅 Windows：不设任何跨平台/跨 WSL 验证阶段。

- **fastcheck**：static_check + rustfmt + clippy + cargo test（含 binding loop 扫描）；总墙钟硬上限 60 秒，超时即失败并终止整个进程树，`MUST NOT` 把超时报成成功；AI 代理 `MAY` 自主执行，但其通过不代表完整验证。（执法点：test_gate.py 的预算终止与退出码）
- **fulltest**：当前平台（Windows，唯一支持平台）全部本地验证——Python 质量门（与 CI 同命令）+ fmt/clippy + `acceptance.ps1 -WithEngine -WithGuiSmoke`；`MUST NOT` 触发远程流水线。不含发布打包自检（打包只在 slowtest）。每次运行 `MUST` 有人类明确授权。（执法点：test_gate.py 的 `--authorized` 入口守卫；越过入口直接执行内部命令属规避行为 · 无执法点 · 软法）
- **slowtest**：fulltest 全部阶段 + 发布打包自检（`scripts/package-windows.ps1` 全程，验证引擎内嵌、许可证合规、无引擎泄漏与两种交付产物；本地阶段未全部 PASS 时不执行）+ 远程 `check.yml`（该工作流仅 `workflow_dispatch`，由本门经 `gh workflow run` 触发并轮询到最终状态，`MUST NOT` 把「已触发」当「通过」；push / PR 不自动触发）。每次运行 `MUST` 有人类明确授权。（执法点：同上 `--authorized` 入口守卫；同上软法）
- `release.yml` 是真实发布（自动打时间戳 tag 并发布产物），`MUST NOT` 纳入 slowtest 自动触发；发布需用户单独明确该次目标。（执法点：test_gate.py 不包含该阶段）
- 历史授权、上一次授权、CI 配置或脚本注释 `MUST NOT` 视为本次授权；环境或工具缺失只能如实标注 UNVERIFIED，`MUST NOT` 当作通过或静默跳过。（无执法点 · 软法）

## 3.5 构建与缓存纪律

- **profile 约定**：日常/受检构建用 Cargo 默认 `dev` profile（`incremental = true`、`codegen-units = 256`、`debug = 1` 行号级调试信息；保持默认，不关闭增量、不加昂贵优化）。受检 `release` profile 同时就是打包配置（`lto = "thin"`、`codegen-units = 1`、`strip`、`overflow-checks`、`debug-assertions`）：质量门的 `cargo build --all-targets --release` 与 `package-windows.ps1` / CI 的 `--release --bins` 走同一 profile，不另拆打包 profile（拆分须同步本文件、打包脚本与 CI，属待所有者裁定项）。
- **linker 选择**：保持 MSVC 默认 `link.exe`（`.cargo/config.toml` 仅设 `+crt-static`）。改用 `rust-lld` 等更快链接器前，须以一次完整质量门（含 `--all-targets`、FFI、build.rs）验证兼容；不兼容即回退并留痕。
- **缓存保护**：禁止无理由 `cargo clean`；禁止删除或迁移当前受检配置的 `target/`；清理前确认无 cargo 进程持有 build-dir 锁；确需全量重建时给出具体理由，优先定点清理。
- **配置维度稳定**：单轮质量门内 toolchain、`RUSTFLAGS`/`CARGO_ENCODED_RUSTFLAGS`、`.cargo/config*`、`CARGO_TARGET_DIR` 保持不变；feature 模式 / target triple / profile 矩阵切换属门设计本身，但同一条门命令在修复循环与最终完整验证之间必须保持同一变体，门覆盖的维度集合不得缩小。
- **磁盘清理顺序**：废弃 triple/profile 的整目录 → 旧 toolchain 产物 → 自建临时工具产物（如 `target/miri`）；当前有效增量缓存 MUST NOT 删除；feature 差异在 `target/` 内无独立目录，禁止按目录名/时间戳/体积猜测「旧 feature 缓存」，无法证明废弃的一律保留。
- **timings 诊断**：构建耗时占主导时用 `cargo build --timings` 定位串行瓶颈（大 crate、build.rs、proc-macro、链接阶段），Top 阻塞单元与建议写入质量门报告；宿主机实时防护（如 Windows Defender 覆盖 `target/`）仅作为环境建议披露——不改系统设置、不据此跳过任何检查。

## 4. 代码与界面实现约定

产品行为类要求（控件分工、进度语义、窗口行为、缩放适配、配色令牌、命名规范）一律以 `docs/requirements/CONTRACT.md` 的 U / P / H 分区为准，本节不重复。本节只写实现层约定：

- 注释、错误信息、界面文案用中文；标识符、模块名、提交信息用英文或中英混排均可，但同一处保持一致。
- 界面颜色走 `ui/app.slint` 的 `Design` 全局（浅色/深色两套由 `Design.dark` 切换），不硬编码与主题冲突的颜色。
- Slint 布局中 `MUST NOT` 用 `root.width` / `parent.width` 绑定子项自身宽度（会形成绑定环，编译期警告、运行期可能 panic）；固定宽度用常量，占满剩余空间用 `horizontal-stretch` 或外层容器。
- 本节颜色 / 布局绑定规则由 `python scripts/static_check.py` 机器检查（十六进制色只允许出现在 `Design` 全局内、布局内禁止 `root/parent.width` 宽度绑定）。无障碍不设开发或验收要求，按 H-08 / U-04 执行。

## 5. 改名与新工具时的同步清单

产品名、二进制名、图标、状态目录等一旦调整，`MUST` 同步以下位置：

`Cargo.toml`（package/bin 名）· `build.rs`（链接参数与图标资源）· `ui/app.slint`（标题、品牌、关于页）· `src/config.rs`（状态目录）· `src/registry.rs`（工具名）· `scripts/*`（打包、启动、检查脚本）· `.github/workflows/check.yml` · `.github/workflows/release.yml` · `resources/windows.manifest` · `README.md` / `先读我.txt` · `AGENTS.md` §1 与 `docs/requirements/CONTRACT.md`（P 分区命名与定位）· `SHA256SUMS.txt`（用 `sha256sum -b` 重新生成并逐字节复核）。

## 6. 安全底线

安全需求（不覆盖语义、删除策略、不上传等）以 `docs/requirements/CONTRACT.md` 的 S 分区与 P-03 为准，本节不重复。代理执行纪律：

- 处理真实用户目录前 `SHOULD` 先用副本验证；未经用户亲自验收前 `MUST NOT` 声称可用于生产使用。

## 7. 用户协作协议（本项目用户零创作）

- 代理 `MUST NOT` 要求用户编写代码、文字或文档，`MUST NOT` 让用户阅读代码 diff；需要用户输入时 `MUST` 转换为以下形式之一：附推荐项的封闭选择题、新旧并排截图的是/否判断、一条可直接复制运行的命令。
- 新需求 `MUST` 先复述为可验收的行为断言（进入 `docs/requirements/CONTRACT.md` 草案，见第 8 节），经用户逐条确认后才可动工；影响用户可见行为的歧义 `MUST` 先以封闭问题澄清，`MUST NOT` 默认假设。
- 向用户报告结果 `MUST` 先给结论（通过 / 失败 / 受阻），证据（命令、退出码、关键输出行）附后；未验证的事项 `MUST NOT` 表述为已完成。
- 本节为行为协议，无法机检，靠会话纪律与用户在交互中纠偏执行。

## 8. 固定需求目录（docs/requirements/）

- `docs/requirements/` 是用户意图的唯一权威目录，当前合同文件为 `CONTRACT.md`：每行一个编号行为断言，**只写需求、不写实现状态**；「功能是否正常」以合同覆盖为准，不以代理单方面理解为准。
- 用户可见行为变更 `MUST` 对应至少一行合同；合同行数单调不减；增改 `MUST` 经用户确认并记录于提交信息，`MUST NOT` 由代理单方面增删。
- 代码与合同不一致时 `MUST` 以合同为准、按缺陷流程修代码（先红后绿，见 3.2）；`MUST NOT` 为迁就代码现状而改写、削弱或删除合同条目。
- 合同条目与测试的映射规则见 3.3（测试注释标合同编号；矩阵检查待落地）。

- 需求或预期用户可见行为变化时，`MUST` 检查并同步固定目录内对应需求文档；新独立功能域可新增文档，已有合适分区则就地维护，不按行数或代码目录机械拆分。同一需求只能有一个权威维护位置，跨文档用引用表达关系；具体需求和规划不重复写入本文件。（无执法点 · 软法）
- 固定目录或权威体系缺失时，`MUST` 先补齐再继续实现；只变实现方式且需求不变时不制造需求变更，不改写需求来合理化缺陷。入口、命令或开发规则变化时同步本文件。（无执法点 · 软法）

## 9. 可信基与防共谋

- 可信基文件：`AGENTS.md`、`scripts/static_check.py`、`scripts/test-baseline.json`、`scripts/acceptance.ps1`、`scripts/gui_smoke.py`、`scripts/make_tmp.py`、`docs/requirements/` 内全部需求文档及旧路径链接文件 `docs/CONTRACT.md`。（`resources/rules.json` 与 `src/config.rs` 结构上由 static_check 的 config_schema 检查互锁，且加规则时二者本就合法同变，不列入可信基。）
- 任何变更 `MUST NOT` 同时修改产品代码（`src/`、`ui/`、`resources/`）与可信基文件；可信基变更 `MUST` 独立提交、提交信息注明理由并获用户批准。（执法：CI 按 diff 文件清单检测混合提交 · 待落地）
- 向本文件新增 `MUST` 级条款时，同一变更 `MUST` 落地对应执法脚本检查，否则该条 `MUST` 显式标注「无执法点 · 软法」；`SHOULD` 每月审计一次无执法点的条款，补齐执法或降级措辞。

## 10. 基线保护

- `scripts/test-baseline.json` 的再生成 `MUST` 先以新旧并排提交用户做是/否判断，对应提交信息 `MUST` 带 `[基线已确认]` 标记；`MUST NOT` 静默更新后直接提交。（执法：脚本校验提交标记 · 待落地）
