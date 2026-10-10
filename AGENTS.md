# AGENTS.md · JchTools 协作约定

面向在本仓库工作的 AI 代理与协作者。规范性用语遵循 RFC 2119（`MUST` / `SHOULD` / `MAY`）。

## 0. 会话快速上手（每个代理会话按此主线执行）

1. 通读本文件与固定需求目录 `docs/requirements/` 中的全部需求文档；该目录是产品需求的唯一权威位置。其余说明文档仅供参考，不得用来覆盖需求。
2. 明确任务类型：新需求 → 先按第 7 节协议转成合同条目并经用户确认；缺陷修复 → 先写能在修复前失败的回归测试；其余变更 → 确认不违反第 9 节可信基隔离。
3. 动工前记录现有验证基线；可复用输入未变的既有结果。新增运行按 3.4 授权边界选择，不机械执行测试；未运行的基线如实标注，不能用编译通过替代测试通过。
4. 编码遵守第 4 节实现约定与合同相应分区；测试数据一律经 `python scripts/make_tmp.py` 生成。
5. 完成后按 3.3 验收矩阵执行 Windows 本地验证并报告命令/退出码/关键输出；CI 仅编译、打包、发布，发布成功须附 release run 与发布链接。已完成且源代码未变的本地验证可复用，不因发布操作重复完整测试。
6. 收尾：`python scripts/make_tmp.py clean` 清空 `.tmp/`，按第 8 节与 3.2 提交纪律写提交信息。

## 1. 项目背景

- **项目类型与技术栈**：自有项目 JchTools，用户个人使用的 Windows 本地工具箱，由 AI Agent 实现和维护。Rust 2021 + Slint 1.17、SQLite（rusqlite bundled）、7-Zip 引擎；产品定位与交付要求见合同 P / E 分区，打包入口为 `scripts/package-windows.ps1` 与 `installer/JchTools.iss`。
- **工具入口**：当前工作树的注册表与 GUI 已接入递归解压、目录整理、MD 整理、转 Markdown、Git 工具、截图 OCR 和模型服务七个入口，对应合同 X / C / M / T / G / O / AH 分区。入口与实现代码存在不代表完整功能已验收，截图 OCR 的真实模型、桌面链路和可选资产交付仍须按 O 分区验证；模型服务按 `docs/requirements/CONTRACT.md` 附录 F 验证，官方 SDK 夹具不代替 F.3 指定的真实 Agent 与模型验收。联网边界只按 P-03 执行，应用联网出口按 P-09 统一走 Windows 系统代理（经代理失败自动回退直连），ACP 模型联网由配置的 Agent 按 AH 分区承接，各功能例外不得互相扩大。新工具 `MUST` 经 `src/registry.rs` 注册 + 真实页面接入；侧栏与导航 `SHOULD NOT` 写死只服务单个工具的文案或流程。
- **生产方式**：本项目全部产出（代码、测试、文档、CI）由 AI 代理完成；用户不编写任何代码或文字，只在封闭选择、看图判断与真实使用中给出意图和反馈（协作协议见第 7 节）。本文件的纪律条款用于对抗代理的自证偏差。
- **权威分工**：`AGENTS.md` 规定开发、测试和验收纪律；固定目录 `docs/requirements/` 保存全部产品需求，`CONTRACT.md` 维护全局约束与既有工具分区，`ALL2MARKDOWN.md` 独立维护转 Markdown（T 分区），`SNAP2TEXT.md` 独立维护截图 OCR（O 分区），`XBERG-INFERENCE.md` 独立维护「Xberg 作为推理提供方」集成域（XB 分区，2026-09-28 确认）。按功能域定位唯一需求来源；明确标注的待确认草案不覆盖已确认条目。旧 `docs/CONTRACT.md` 仅保留迁移链接，兼容静态检查的文件存在性要求，不再维护需求副本。目录内权威文档清单由 static_check 的 `requirements_registry` 检查执法（新增文档须同步本清单与该检查）。其余文档仅为辅助说明。
- **代码入口**：`src/main.rs` → `src/gui.rs`（GUI 启动、回调与后台任务）、`ui/app.slint`（界面）、`src/registry.rs`（工具注册表）；`src/archive.rs`（递归解压）、`src/engine.rs`（整理分析与执行）、`src/md_tools.rs`（MD 合并与拆分）、`src/git_tools.rs`（Git 操作）、`resources/rules.json`（界面规则清单）。转 Markdown 由 `src/markdown.rs`、`src/markdown_document.rs`、`src/markdown_assets.rs` 承接，媒体转录经 Xberg 推理组件（`xberg.exe worker` stdio 协议，XB 分区）承接；截图 OCR 由 `src/snap_ocr_assets.rs`、`optional/snap-ocr-core/`（O-23～O-28 的规格参照物：workspace 内无 crate 依赖它，识别已按 XB-01/XB-02 迁至 Xberg，删除前须确认规格约束另有执法点承接）与 `optional/snap-ocr-worker/`（截图、托盘、服务和结果窗，识别经 Xberg 推理组件）承接，主界面回调仍在 `src/gui.rs`。两资产模块共用的下载/校验/原子落位/推理组件包安装核心在 `src/asset_util.rs`。
- **共享推理与配置入口**：`src/xberg_runtime.rs` / `src/xberg_runtime_windows.rs` 承接共享引擎请求及 Windows 命名管道代理；主程序的 `--xberg-broker` 是内部代理入口。截图服务经 `optional/snap-ocr-worker/src/shared_xberg.rs` 接入，并编译复用主包的 runtime 与 settings 模块。`src/xberg_settings.rs` / `src/app_settings.sql` 在应用状态目录的 `config.sqlite3` 保存 Xberg 目录并承接旧文本配置迁移；功能与生命周期要求以 XB-14～XB-19 为准。
- **ACP 模型服务入口**：`src/acp_api/acp/` 为官方 SDK 会话及回调适配，`http/` 为 Axum OpenAI/SSE 接口，`runtime/` 为 Windows 独立后台与控制管道，`settings.rs` 复用应用 SQLite；主程序内部入口为 `--acp-http-service`。源码与许可归属在 `vendor/SOURCES.json`，`.cargo/config.toml` 解析仓库内依赖。局部入口为 `cargo test --features test-hooks --test acp_settings --test acp_http_contract --test acp_callbacks --test acp_service_process`；其中窗口场景使用 winit-software 与窗口内事件，不移动硬件鼠标。合成 Agent 源码在 `tests/fixtures/acp_api/main.rs`，仅 test-hooks 构建，不进入生产包。
- **子模块文档登记表**：仓库子级 `AGENTS.md` 只承载该子树专属的职责、入口、命令、约定与陷阱；全局规则与全局命令唯一权威在本文件，子文档不重复维护。当前登记两份（全仓库含本文件上限 8 份）：
  - `optional/snap-ocr-worker/AGENTS.md`——截图 OCR 后台进程 crate：托盘/热键/GDI 截图/结果窗/管道服务与 Xberg 客户端的本地命令、与主包的双端同步点及陷阱。
  - `optional/snap-ocr-core/AGENTS.md`——O-23～O-28 冻结规格的纯逻辑参照物 crate：无人依赖的现状、测试命令与删除/改动前置约束。

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
cargo test --features test-hooks                 # 单元与集成测试（真实引擎用例默认 #[ignore]）
python scripts/static_check.py      # 结构/配置/回调/SQL/测试基线/界面规则静态检查
python scripts/test_gate.py fastcheck   # 静态检查 + format + compile，不运行测试；非编译共享预算 60s（见 3.4）
powershell -NoProfile -File .\scripts\acceptance.ps1 -WithEngine   # 单命令验收（见 3.2）
powershell -NoProfile -File .\scripts\package-windows.ps1   # 生成含 7-Zip 的发布 ZIP
```

手工测试集：`python scripts/make_tmp.py testdata`（默认自动生成到 `.tmp/testdata/`，会先清空该目录；也可 `--destination` 另指专门测试目录，仓库内 `.tmp` 之外的位置会被拒绝）。普通处理验收不使用 `--git`：根目录含 `.git` 时按 H-06 拒绝整次处理；`--git` 仅可用于验证该拒绝行为。需要还原普通数据集时重新运行生成命令；测试完成后用 `python scripts/make_tmp.py clean` 清理。

- Windows 构建机需要 Rust `x86_64-pc-windows-msvc` + VS C++ Build Tools + Windows SDK（`rc.exe` 用于把 `resources/app.ico` 嵌入 EXE）。
- 提交前按 3.4 选择必要验证并复用输入未变的通过结果；完整功能验收仍需对应测试与矩阵证据，不因缺少授权而冒报通过。构建输出不得新增 `binding loop` 警告。
- **本机 Xberg 测试引擎（2026-10-03 用户规则，以本次口径为准）**：测试只用最新版本；最新版本已存在时不重复下载；不存在时下载最新版并覆盖老引擎目录。`scripts/xberg_test_engine.py` 在 fulltest 前查询官方 latest、验证已有引擎身份，缺最新版本时校验归档并覆盖固定测试目录；引擎目录固定为 `C:\Users\jiang\Documents\xberg-test\xberg-cli-x86_64-pc-windows-msvc`（2026-10-07 已核验最新发布 `v2026.10.6-0420-run58.1`，归档 SHA-256 经 GitHub release asset digest 核验；旧协议兼容由 `tests/xberg_legacy_protocol.rs` 合成引擎锁定，不依赖本地旧发布物）。（无执法点 · 软法）
- **Xberg 源码参照（E:\xberg）**：xberg 接口的事实参照以本机 `E:\xberg` 源码为准，仅在必要时作权威接口文档查阅（2026-10-03 用户确认）。该源码 `MUST NOT` 编译、构建或产出任何测试/发布物——测试只用上一条的固定引擎目录，产品只用用户配置目录（与 O-31 的「源仓库只读 oracle」边界一致）。（无执法点 · 软法）

> 隔离测试显式启用 `test-hooks`；该特性不在默认特性中，生产与打包构建不启用。它允许测试目录和测试代理覆盖；`debug-assertions` 本身不再授予这些覆盖。测试门同时保持生产构建维度。临时验证工作目录可用 `python scripts/make_tmp.py workspace --destination .tmp/validation` 创建。

## 3.1 7-Zip 引擎

引擎相关需求（官方来源、校验、运行期解析顺序、发布包合规）以 `docs/requirements/CONTRACT.md` 的 E 分区为准，本节只写工程事实：`scripts/fetch-7zip.ps1` 负责获取官方引擎（产物落在 `resources/7zip/`，已被 `.gitignore` 忽略，`MUST NOT` 提交）；构建时 `build.rs` 把引擎压缩编进 EXE（`src/engine_bundle.rs` 负责释放与校验），缺失引擎时构建不失败、仅不内嵌。

## 3.2 测试验收纪律（代码与测试均由 AI 代理产出，以下用于对抗自证偏差）

- **回归测试先行**：修复任何缺陷 `MUST` 先写能在修复前失败的回归测试，并在提交信息记录反证证据（复现命令 + 修复前失败输出摘要）。只有「修复后通过」而没有「修复前失败」证据的修复不算完成。
- **禁止削弱测试**：删除、改名、放宽断言、新增 `#[ignore]` 或平台门禁（`#[cfg(...)]`）`MUST` 同步更新 `scripts/test-baseline.json`（用 `python scripts/static_check.py --update-test-baseline` 重新生成）并在提交信息写明理由；`MUST NOT` 只为了让测试变绿而做上述改动。平台门禁 `MUST` 附带原因注释，且被门禁的行为 `SHOULD` 在另一平台仍有覆盖。
- **提交纪律**：提交信息按变更性质 `MUST` 携带对应记录——缺陷修复附回归反证（复现命令 + 修复前失败输出摘要）；合同增改附用户确认结果；可信基变更附理由与用户批准记录；基线再生成附 `[基线已确认]` 标记；宣称功能验证通过附本地证据，宣称发布成功附发布 run 和版本链接。缺对应记录的提交 `MUST NOT` 合入。
- **完成条件**：见 3.3 验收矩阵；`MUST NOT` 只跑默认 `cargo test --features test-hooks` 就宣称引擎 / UI / 发布相关工作已验证。
- **独立复核**：涉及引擎、删除路径、解压安全（`fsutil` / 覆盖语义）或用户可见行为的实质变更，`SHOULD` 由未参与实现的独立代理会话复跑验证并给出证据格式：命令、环境（OS / rustc / 是否真实引擎）、退出码、关键输出行。
- **本地验收与 CI 职责（2026-10-08 用户确认）**：Windows 本地测试是功能验收依据；CI 仅编译、打包和发布，不运行完整测试、GUI/模型验收、lint 或供应链测试门。`check.yml` 仅手动构建产物，`release.yml` 编译并发布安装包及便携 ZIP，使用 `package-windows.ps1 -SkipTests`；CI 编译成功不能冒充功能测试通过，已通过的本地测试无需为每次发布重复执行。未验证项仍须如实标注。（执法点：两个工作流仅调用跳过测试的生产打包入口）
- **flaky 政策**：`MUST NOT` 重跑到绿。测试间歇性失败必须查因；确属 flaky 的要在提交信息记录现象与原因，不得静默重跑。
- **人工验收边界**：用户已决定不保留人工验收项清单；自动化未覆盖的行为（如真实 TB 级数据、非 150% DPI、网络共享）`MUST NOT` 被代理宣称已验证，只能如实标注「未验证」。
- **UI 测试交互方式（2026-10-09 用户确认）**：本项目 UI 测试 `SHOULD` 优先使用不移动、不占用用户鼠标的方式；除非用户当次显式要求使用 `e2e.md`，`MUST NOT` 执行会抢占用户鼠标的桌面测试流程。该约定不降低真实 GUI E2E 的覆盖及结果断言要求；无法按上述方式完成的项目须如实标注未验证。（无执法点 · 软法）

## 3.3 验收方法（怎么测试、怎么验收）

「验收通过」= 下表相应本地验证行全部执行且通过；执行权限按 3.4，真实桌面驱动须另行明确指令，不为满足矩阵自动触发。未执行项保持未验证；发布成功另须确认 release 工作流成功及实际版本产物存在，`NOT RUN` 不得报告为通过。

| 场景 / 变更类型 | 必须通过 | 覆盖 |
|---|---|---|
| 任意变更（每次提交） | `cargo test --features test-hooks` + `python scripts/static_check.py` | 合同全部条目对应测试 |
| 引擎 / 解压 / 删除 / 路径安全 | 上行 + `powershell -NoProfile -File .\scripts\acceptance.ps1 -WithEngine` + `tests/gui_flow.rs` | 合同 S / E 分区、C-01 |
| UI（`ui/app.slint` / GUI 装配） | 任意变更行 + `tests/gui_flow.rs` + `scripts/gui_smoke.py` S1–S4 + 两档窗口尺寸目视检查 | 合同 C / U 分区 |
| 递归解压端到端 | `python scripts/make_tmp.py testdata` 生成数据集 → GUI 走「开始解压 → 一段确认 → 跑完」→ 按合同 X / H 分区核对（成功原包及实际分卷按 X-05 删除、已有文件不变、冲突自动改名且后缀不变、失败原包进「解压失败」、Git 整树排除、后续新任务允许重新解压）→ `python scripts/make_tmp.py clean` 清理 | 合同 X / H 分区 |
| 目录整理端到端 | `python scripts/make_tmp.py testdata` 自动生成数据集到 `.tmp/testdata/` → GUI 按默认配置完整走一遍目录整理主流程 → 按合同 C / H 分区核对（Git 整树排除、成功后处理范围内无空目录、再次整理幂等）→ `python scripts/make_tmp.py clean` 清理 | 合同 C / H 分区 |
| 打包 / 发布 / 引擎捆绑 | `scripts/package-windows.ps1` 全程 + 干净目录解包运行 | 合同 E 分区 |

- 单命令入口：`powershell -NoProfile -File .\scripts\acceptance.ps1`（可选 `-WithEngine` / `-WithGuiSmoke -GuiData <目录>` / `-WithPackage`）。
- 改动过跟踪文件后，提交前须最后用 `git -c core.quotePath=false ls-files -z | grep -zv '^SHA256SUMS.txt$' | xargs -0 sha256sum -b > SHA256SUMS.txt` 重建清单（static_check 会校验其完整性）。
- **需求 ↔ 测试映射**：验证合同条目的测试 `MUST` 在其文档注释中标明合同编号（如 `// 覆盖 C-12`）；每个合同条目至少被一个测试引用。（现状：已有部分编号注释，但全条目覆盖尚未逐项核验，映射执法待落地。`scripts/requirement_coverage.py` 为全部有效编号生成引用审计（fulltest 输出 `.tmp/test-gate/requirements-map.json`），候选引用不是断言或执行证明，未逐项复核真实链路前保持 UNVERIFIED。）
- 合同条目的拆分、合并或重编号 `MUST` 经用户确认。

### 自动化验证要求与当前边界

- 功能开发和功能性修改 `MUST` 同时具备 UT 局部逻辑验证与从真实公开入口到可观察结果的 E2E 验证；跨模块交互按需要增加集成测试，已有有效覆盖可以复用。不得依赖用户手工读代码或人工回归保证质量。（无执法点 · 软法）
- 测试 `MUST` 对应需求及验收条件，覆盖核心成功路径与相关关键失败路径；UT、编译、静态检查、局部模拟和“没有崩溃”不能替代完整链路的结果断言。桩与模拟仅作补充，未经验证的真实边界须明确说明。（无执法点 · 软法）
- 验证报告 `MUST` 区分已实现、验证通过、验证失败和未验证；环境、依赖或权限不足不得报告验收通过。（无执法点 · 软法）
- UT / 集成入口为 `cargo test --features test-hooks`，可按目标运行 `cargo test --features test-hooks --test md_tools`、`cargo test --features test-hooks --test git_tools` 等。`tests/git_tools.rs` 使用真实 git 与本地 bare 远端，不覆盖真实网络及认证。
- Xberg 集成入口为 `cargo test --features test-hooks --test xberg_settings --test xberg_shared_process`（默认 `gui` 特性）。前者覆盖 SQLite 保存、旧配置迁移、独立进程恢复及场景资产选择；后者启动真实 JchTools 代理和命名管道，但使用由 `rustc` 编译的模拟引擎，验证进程复用、转换期间截图响应及请求隔离。它们不覆盖 Windows 重启、真实模型常驻或热键/托盘到结果的桌面 E2E；完整验收仍按 XB 第 5.2 / 6 节执行。
- GUI 链路入口为 `cargo test --features test-hooks --test gui_flow`；该 target 使用真实 winit 窗口，显示时会激活用户焦点，属于门外独立桌面验收。`src/gui.rs` 内另有 MD 真实回调测试，使用 Slint TestingBackend，不等同于真实桌面窗口验证。`scripts/gui_smoke.py` 默认运行 S1–S4（启动、整理与解压）、S15（MD 合并/拆分产物与原文件保护）、S16（Git 逐文件提交到本地 bare 远端）及 S18（两档窗口尺寸下逐页主要按钮边界与重叠判定）；S16 不覆盖真实网络与认证。S6–S9 的配置隔离需要 `cargo build --features test-hooks` 产物，脚本在操作前检查隔离 SQLite 已建立；普通构建不识别测试环境变量。其他扩展阶段通过 `--stages` 选择，不能以底层测试冒充 GUI E2E。`scripts.test_gui_smoke` 的结果判据、布局反例与隔离守卫单测进入本地 fulltest Python 质量门；S5/S14 按最新 T-23 检查取消当前媒体、先前成品不变及无未完成产物。
- 转 Markdown 的依据是 `docs/requirements/ALL2MARKDOWN.md`；按该文档附录 A 验证主包不包含转换专用依赖/模型、未配置时旧工具正常可用、GUI 保存并校验用户指定的 Xberg 运行目录、按需初始化其余组件，以及离线真实 Xberg / OCR / 媒体转换、GUI 入口及两种交付形态；旧 all2markdown 的源码、测试或历史 CI 不能作为集成后的通过证据。实现范围按 T-30 限于迁入，不借迁移改变旧功能，也不擅自搬入旧 Python 架构。
- 纯文档等非功能性修改按实际影响检查内容、引用和需求保留情况，不机械新增功能测试；本仓库已有基线、提交检查与 CI 完成条件仍按 0 / 3.2 / 3.3 执行，未执行项如实标注。
- 可选组件的 UT / 集成测试从仓库根目录分别运行 `cargo test --features test-hooks --manifest-path optional/snap-ocr-core/Cargo.toml --all-targets` 和 `cargo test --features test-hooks --manifest-path optional/snap-ocr-worker/Cargo.toml --all-targets`。根 workspace 的 `default-members = ["."]`，默认 `cargo test --features test-hooks` 不覆盖这些包；本地 `acceptance.ps1` 有二者的显式测试步骤；CI 不执行这些测试，不能把本地默认门通过当作截图组件已验证。旧 `optional/markdown-media-worker` 已按 XB-12 退役（媒体转录迁移到 Xberg 推理组件），其测试与打包步骤一并移除。
- 转 Markdown 的验收承接入口为 `scripts/markdown_acceptance.py`（`--list` 查看条目；默认 `--profile common` 必验新老 Office / PDF / MP4，`--profile full` 保留扩展矩阵，OPTIONAL 不计为通过也不阻塞常用验收），也可经 `acceptance.ps1 -WithMarkdownAcceptance` 接入；运行依赖见 `scripts/requirements-dev.txt`，真实 GUI、Xberg、媒体组件与两种发布目录按脚本参数提供。提供 `JCHTOOLS_TEST_XBERG_DIR` 时，验收先写隔离 SQLite，再用 S17 真实 GUI 初始化本地 notice；不读取生产资产来冒充隔离就绪。缺资产条目为 `NOT RUN`；脚本退出码 0 仅表示全部必验条目 PASS；保留的扩展 OPTIONAL 不计为通过；任一必验条目未执行（含 --only 排除的条目）返回 2，应逐项检查。`scripts/gui_smoke.py --stages S5` 另有转换开始/停止链路，需已配置可用组件，不在默认序列内，不能替代转换产物断言。
- 截图 OCR 的验收覆盖目标见 `docs/requirements/SNAP2TEXT.md` 附录 C。迁至 Xberg 后，worker 已无 `det_paddlex_oracle` / `pipeline_backend_oracle` 测试目标；旧直连引擎路径（`xberg_worker.rs` 客户端、`tests/xberg_client.rs` 协议测试、`examples/xberg_ocr.rs` 无头对照、`mock-xberg-worker` 测试桩）已于 2026-10-04 经用户确认删除，服务仅经 `SharedXbergClient` 使用共享引擎（XB-14）。已有 Slint 测试后端结果窗用例不等于热键/托盘、服务生命周期或多 DPI 的真实桌面 E2E；这些完整链路的自动化覆盖尚未确认。`tests/ocr_fixtures/README.md` 中的旧 `ocr_compare` 命令及 TextSnap 历史结果不作为当前迁入版通过证据。上述入口的授权、技能调用授权及独立桌面边界均按 3.4 执行。
- 本地 `acceptance.ps1` 默认测试 Snap OCR core / worker 并检查资产清单；设置 `JCHTOOLS_SNAP_OCR_ASSET_ROOT` 为完整的已校验资产缓存时，追加真实 worker 从安装位置加载模型的服务测试。未设置时该资产依赖项报告 `NOT RUN`，不能据此声称桌面验收通过。

## 3.4 三级测试门与执行权限

统一入口 `python scripts/test_gate.py <fastcheck|fulltest|slowtest>`；各级含义按本节维护，变更需用户明确确认。以下规则按 2026-10-10 用户显式调用 `jch-fastcheck-fulltest-slowtest-gates` 并批准冲突以该技能为准同步。平台范围按合同 P-07 仅 Windows：不设任何跨平台/跨 WSL 验证阶段。

- **fastcheck**：静态检查 + format + compile，不运行任何测试，保留既有检查定义、判据与测试源码；非编译共享预算为 **60 秒**。AI 仅在相关改动成批完成后有具体验证需要时选择运行，不逐文件运行或对输入未变的通过结果反复运行。通过只证明本级检查通过，不代表完整功能验收。不得以两次缓存后生产编译代替静态或格式检查，也不设置预算外预热阶段。
- **fulltest**：含 fastcheck 同项 + 所有适用的 Windows 本地 UT / integration / doc / e2e + 本地发布打包（`scripts/package-windows.ps1`，含既有引擎内嵌、许可证、引擎不泄漏与两种交付产物检查）；非编译共享预算为 **900 秒**。包括可选组件、真实 7-Zip 及适用的无头/不抢占用户桌面的本地用例；不改变既有测试断言、检查判据或依赖质量配置。
- **slowtest**：非编译共享预算为 **1500 秒**。P-07 无跨平台/WSL 需求，本级适用覆盖与 fulltest 相同，只完整执行一次同一覆盖，不先跑 fulltest 再重复执行；不存在的跨平台增量标记 `SKIPPED_NOT_APPLICABLE`，不得伪造测试通过。
- **门内边界与独立桌面验收**：所有门均 `MUST NOT` 触发 CI、远程流水线、发布、ComputerUse、全局鼠标或焦点抢占测试。`gui_flow` 全部用例、`acp_service_process` 两项 `real_gui` 用例、`scripts/gui_smoke.py` / `scripts/markdown_acceptance.py` 的真实桌面 driver，以及注册全局热键的 `check_worker_root.py` 服务探测保留原测试/断言及独立入口，门中标记 `NOT_RUN_SEPARATE_USER_INSTRUCTION_REQUIRED`；不得因其在门外而判本门 FAIL，也不得冒报 PASS。软件渲染不隔离焦点；执行须用户当次另行明确指令，并遵守 3.2 的桌面交互限制。库内 Slint TestingBackend、OCR 隔离 desktop / window station 用例和其他适用本地 e2e 仍在完整门内。（执法点：`local_gate_plan.py` 的 artifact 分类与精确 `--skip`；真实授权为会话软约束）
- **授权**：用户显式调用 `jch-fastcheck-fulltest-slowtest-gates` 即授权完成该次任务所需的全级运行及必要重跑；仍不得静默重跑到绿或规避失败证据。非该显式技能任务中的普通 fulltest / slowtest 每次运行 `MUST` 获得当次明确授权，不继承历史、上一次、CI 配置或脚本注释的授权。`--authorized` 只代表上述显式技能授权或当次明确指令，是入口声明，不创造授权；绕过入口直接运行内部阶段亦须遵守同一会话授权纪律。（执法点：`test_gate.py` 的 `--authorized` 入口守卫；授权真实性为软约束）
- **计时与终止证据**：每级从开始至终止使用一个共享墙钟；setup / static / format / tests / download / packaging / cleanup 均计入非编译预算。只扣除受控 Windows Job 成员中实际观测到 compiler 工作、且没有非编译并行工作的编译独占区间；不得把整条 Cargo、测试或打包命令都视为编译，也不得把缓存命中、等待或预热整体扣除。混合打包只有同步等待纯 `cargo build` 且满足上述观测条件的区间可扣除，其余阶段全收费。成功、失败、超时、缺授权、环境缺失、异常或取消等所有终止路径均输出 `total` / `compile_excluded` / `budgeted` / `limit` / `status`，其中 `budgeted = total - compile_excluded`；超出本级非编译预算即失败并终止所属进程树。
- **环境受限历史条目**：转 Markdown C08/C09（安装版 / 便携版在独立 Windows 用户会话中真实完成转换）在缺少该会话时仍属未验证；既有条目、覆盖与断言不删除。其真实桌面 driver 按上述独立验收边界报告，不再以旧 slowtest 的 CI 阶段例外处理；另行获授权且取得隔离会话后按原验收要求执行。门内其余适用条目缺环境或工具时如实标记 `UNVERIFIED` / `NOT RUN` 等覆盖缺口，不冒报 PASS 或静默跳过。
- **CI / 发布保持独立**：`check.yml` 仍仅手动编译、打包，`release.yml` 仍编译、打包、发布；二者都不属于三级门。真实发布须用户单独明确该次目标，功能验收仍以 Windows 本地结果为准。独立触发流水线时，除非用户当次明确要求等待结果，默认只触发并报告本次 run 链接、标记未完结 / 未验证，不借历史 run 冒报成功；发布成功仍须确认对应 run 最终成功及实际产物。（等待行为与发布授权为软约束）

## 3.5 构建与缓存纪律

- **profile 约定**：日常/受检构建用 Cargo 默认 `dev` profile（`incremental = true`、`codegen-units = 256`、`debug = 1` 行号级调试信息；保持默认，不关闭增量、不加昂贵优化）。受检 `release` profile 同时就是打包配置（`lto = "thin"`、`codegen-units = 1`、`strip`、`overflow-checks`、`debug-assertions`）：质量门的 `cargo build --all-targets --release` 与 `package-windows.ps1` / CI 的 `--release --bins` 走同一 profile，不另拆打包 profile（拆分须同步本文件、打包脚本与 CI，属待所有者裁定项）。本地 release 打包属于 fulltest 与 slowtest 的适用覆盖，不因使用 release 而移到独立 CI 或仅限 slowtest。
- **linker 选择**：保持 MSVC 默认 `link.exe`（`.cargo/config.toml` 仅设 `+crt-static`）。改用 `rust-lld` 等更快链接器前，须以一次完整质量门（含 `--all-targets`、FFI、build.rs）验证兼容；不兼容即回退并留痕。
- **缓存保护**：三级门正常复用当前 `target/` 和增量缓存，无 `cargo clean`、删除、迁移或重建缓存阶段；不得靠清缓存改变受检条件。门外确需全量重建时给出具体理由，优先定点清理；清理前确认无 cargo 进程持有 build-dir 锁。
- **配置维度稳定**：单轮质量门内 toolchain、`RUSTFLAGS`/`CARGO_ENCODED_RUSTFLAGS`、`.cargo/config*`、`CARGO_TARGET_DIR` 保持不变；固定既有 feature 模式 / target triple / profile 矩阵，同一条门命令在修复循环、必要重跑与最终完整验证之间保持同一变体，门覆盖的维度集合不得缩小。不得为满足预算临时减少 features、改变 flags、切换 target 或关闭既有质量检查；既有 dev / release 等矩阵维度按原定义复用正常缓存。
- **磁盘清理顺序**：废弃 triple/profile 的整目录 → 旧 toolchain 产物 → 自建临时工具产物（如 `target/miri`）；当前有效增量缓存 MUST NOT 删除；feature 差异在 `target/` 内无独立目录，禁止按目录名/时间戳/体积猜测「旧 feature 缓存」，无法证明废弃的一律保留。
- **timings 诊断**：构建耗时占主导时用 `cargo build --timings` 定位串行瓶颈（大 crate、build.rs、proc-macro、链接阶段），Top 阻塞单元与建议写入质量门报告；宿主机实时防护（如 Windows Defender 覆盖 `target/`）仅作为环境建议披露——不改系统设置、不据此跳过任何检查。
- **性能打点与基准**：运行期耗时打点在 `src/perf.rs`，由默认关闭的 `perf-tracing` 特性控制（`cargo run --features perf-tracing` 启用；诊断日志（P-10）使 tracing 依赖常开，本特性只门控性能打点代码与 `perf-logs` 层）；性能日志落状态目录 `perf-logs/`、按天轮转，与界面运行日志（S-07）及诊断日志互不影响；span 名称与字段名是跨版本可比较口径，只增不改名、不改语义（清单见该文件头注释）。
- **诊断日志（P-10）**：`src/logging.rs` 常开初始化，GUI 与 `--xberg-broker` 代理进程的关键步骤告警与错误写入状态目录 `logs/`（按天轮转、保留 14 天、panic 入盘），覆盖进程间通信、网络出口与关键任务；只写本地盘、不上传、不含文件正文，初始化失败安静退化不影响业务。记录范围与边界以合同 P-10 为准。`tests/perf_probe.rs` 是默认 `#[ignore]` 的性能基准（`cargo test --features test-hooks --release --test perf_probe -- --ignored --nocapture`，规模经 `JT_PERF_GROUPS` / `JT_PERF_COPIES` / `JT_PERF_EMPTY_DIRS` 调整）：只测耗时与计数、不断言具体秒数，不进入默认验收门。

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
- 新需求 `MUST` 先复述为可验收的行为断言（进入 `docs/requirements/` 中对应功能域的需求草案，见第 8 节），经用户逐条确认后才可动工；影响用户可见行为的歧义 `MUST` 先以封闭问题澄清，`MUST NOT` 默认假设。
- 向用户报告结果 `MUST` 先给结论（通过 / 失败 / 受阻），证据（命令、退出码、关键输出行）附后；未验证的事项 `MUST NOT` 表述为已完成。
- 本节为行为协议，无法机检，靠会话纪律与用户在交互中纠偏执行。

## 8. 固定需求目录（docs/requirements/）

- `docs/requirements/` 是用户意图的唯一权威目录，当前由 `CONTRACT.md`（全局及 X / C / M / G 等分区）、`ALL2MARKDOWN.md`（T 分区）、`SNAP2TEXT.md`（O 分区）与 `XBERG-INFERENCE.md`（XB 分区）按功能域维护需求：每行一个编号行为断言，**只写需求、不写实现状态**；需求按第 7 节确认后进入实现（O 分区已于 2026-09-27 确认，XB 分区已于 2026-09-28 确认），「功能是否正常」以已确认合同覆盖为准，不以代理单方面理解为准。
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
