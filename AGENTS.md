# AGENTS.md · JchTools 协作约定

面向在本仓库工作的 AI 代理与协作者。规范性用语遵循 RFC 2119（`MUST` / `SHOULD` / `MAY`）。

## 0. 会话快速上手（每个代理会话按此主线执行）

1. 阅读本文件，按 `openspec/specs/` 中的功能域及 [README 需求与工程入口](README.md#需求与工程入口) 定位任务，再按需读取相关规格、全局约束、依赖与交叉引用；普通任务不预先通读全部规格，全项目任务逐域覆盖。`openspec/specs/` 是唯一已批准需求基线，其他说明不得覆盖它。
2. 明确任务类型：新需求 → 按第 7、8 节先形成原生需求提案并确认批准状态；缺陷修复 → 对照有效规格先写能在修复前失败的回归测试；其余变更 → 确认不违反第 9 节可信基隔离。不为实现方式变化制造需求变更。
3. 动工前记录现有验证基线；可复用输入未变的既有结果。新增运行按 3.4 授权边界选择，不机械执行测试；未运行的基线如实标注，不能用编译通过替代测试通过。
4. 编码遵守第 4 节实现约定与合同相应分区；测试数据一律经 `python scripts/make_tmp.py` 生成。
5. 完成后按 3.3 验收矩阵执行 Windows 本地验证并报告命令/退出码/关键输出；CI 仅编译、打包、发布，不运行测试或质量门，发布成功须附 release run 与发布链接。已完成且相关输入未变的本地验证可复用。
6. 收尾：删除本会话在 `.tmp/` 下自建的子目录（有并行会话使用 `.tmp/` 时 `MUST NOT` 全量 `clean`；无执法点 · 软法），按第 8 节与 3.2 提交纪律写提交信息。

## 1. 项目背景

- **项目类型与技术栈**：自有项目 JchTools，用户个人使用的 Windows 本地工具箱，由 AI Agent 实现和维护。Rust 2021（最低工具链 `1.98.1`）+ Slint `=1.18.1`、SQLite（rusqlite bundled）、7-Zip 引擎；工程门禁与验收脚本使用 Python / PowerShell。产品定位与交付要求见合同 P / E 分区，打包入口为 `scripts/package-windows.ps1` 与 `installer/JchTools.iss`。
- **工具入口**：当前工作树的注册表与 GUI 已接入递归解压、目录整理、MD 整理、转 Markdown、Git 工具、截图 OCR、模型服务和 CodeAgent；CodeAgent 的功能与验收按 CA 分区维护。入口与实现存在不代表完整验收；截图与模型服务须分别按 O / AH 验收矩阵核对，官方 SDK 夹具不代替真实 Agent 与模型。新工具 `MUST` 经 `crates/gui/src/registry.rs` 注册 + 真实页面接入；侧栏与导航 `SHOULD NOT` 写死只服务单个工具的文案或流程。
- **生产方式**：本项目全部产出（代码、测试、文档、CI）由 AI 代理完成；质量不能依赖用户手工阅读代码或人工回归。用户只在封闭选择、看图判断与真实使用中给出意图和反馈（第 7 节），不编写代码或文字。本文件的纪律用于对抗代理自证偏差。
- **权威分工**：`AGENTS.md` 维护开发、测试与验收纪律；`openspec/specs/<capability>/spec.md` 是唯一已批准需求基线，`openspec/changes/<change-name>/proposal.md` 与对应 `specs/<capability>/spec.md` 只承载待批准提案及需求增量。按功能域读取，不复制子需求：
  - `application-contract`：全局与共用约束（H / P / S / R / U / E 及共用附录 B～E）。
  - `recursive-extraction`：递归解压（X）。
  - `directory-organization`：目录整理（C 及大类映射附录 A）。
  - `markdown-organization`：MD 合并与拆分（M）。
  - `git-file-publishing`：Git 逐文件提交推送（G）。
  - `markdown-conversion`：文档、图片与媒体转 Markdown（T）。
  - `screenshot-ocr`：截图 OCR、确定性布局及 jchocr（O）。
  - `shared-inference`：共享 Xberg、设置与后台（XB）。
  - `acp-model-service`：独立模型服务（AH 及真实 Agent 验收）。
  - `codeagent-token-maintenance`：CodeAgent Token 自动维护（CA）；入口为 `crates/codeagent/src/codeagent.rs` 与独立 GUI 页面，后台内部入口为 `--codeagent-service`，安装/卸载维护入口为 `--codeagent-service --stop`（CA-08）。
  - 跨域关系仅引用对应规格；新增能力时同步本清单、README 入口和 `scripts/static_check_impl/misc.py` 的 `requirements_registry` 登记。登记及原生验证不代表软件实现或功能验收通过。
- **代码入口**：根 `src/main.rs` → `crates/gui/src/gui.rs`（GUI 启动、回调与后台任务），`crates/gui/src/registry.rs` 为工具注册表，`ui/app.slint` 仍在根目录、由 `crates/ui/build.rs` 生成独立 UI crate。递归解压在 `crates/recursive-extract/src/{archive,engine}.rs`，目录整理在 `crates/directory-organizer/src/{engine,planner,dedup_components}.rs`，两者共用 `crates/file-core/src/{task,scan}.rs` 的任务与扫描合同；MD 整理、Git 操作分别在 `crates/md-tools/src/md_tools.rs`、`crates/git-tools/src/git_tools.rs`，规则清单仍为 `resources/rules.json`。转 Markdown 由 `crates/markdown/src/{markdown,markdown_document,markdown_assets}.rs` 承接，媒体转录经 Xberg 推理组件（`xberg.exe worker` stdio 协议，XB 分区）承接；截图资产在 `crates/assets/src/snap_ocr_assets.rs`，资产共用的下载/校验/原子落位/推理组件包安装核心在同 crate 的 `asset_util.rs`。`optional/snap-ocr-core/` 仍是 O-23～O-28 的规格参照物：workspace 内无 crate 依赖它，识别已按 XB-01/XB-02 迁至 Xberg，删除前须确认规格约束另有执法点承接；`optional/snap-ocr-worker/` 承接截图、托盘、服务和结果窗，识别经 Xberg 推理组件，主界面回调仍在 GUI crate。
- **共享推理与配置入口**：`crates/xberg/src/{xberg_runtime,xberg_runtime_windows}.rs` 承接共享引擎请求及 Windows 命名管道代理；主程序的 `--xberg-broker` 是内部代理入口。截图服务经 `optional/snap-ocr-worker/src/shared_xberg.rs` 接入，通过真实 crate 依赖共享 Xberg runtime 与 foundation 的日志/设置，不再把主包源文件重复编入。`crates/foundation/src/{xberg_settings.rs,app_settings.sql}` 在应用状态目录的 `config.sqlite3` 保存 Xberg 目录并承接旧文本配置迁移；功能与生命周期要求以 XB-14～XB-19 为准。
- **jchocr 入口**：独立 package `jchocr` 的 `crates/jchocr/src/bin/jchocr.rs` → `crates/jchocr/src/jchocr.rs`，集成测试在 `crates/jchocr/tests/jchocr.rs`，需求唯一见 O-33～O-37；复用应用 SQLite 和共享 Xberg 代理，不是新增 GUI 工具，不适用第 1 节的新增页面注册要求。安装包将命令复制到当前用户已有 PATH 的 WindowsApps 目录，不修改 PATH；便携包只随附程序。局部验证入口为 `cargo test -p jchocr --features test-hooks --test jchocr`；无需为该独立 CLI 改动运行真实桌面驱动或完整测试门。
- **ACP 模型服务入口**：`crates/acp-api/src/acp_api/acp/` 为官方 SDK 会话及回调适配，同层 `http/` 为 Axum OpenAI/SSE 接口，`runtime/` 为 Windows 独立后台与控制管道，`settings.rs` 复用 foundation 持有的同一份应用 SQLite schema；主程序内部入口为 `--acp-http-service`，安装/卸载维护停止入口为 `--acp-http-service --stop`。源码与许可归属在 `vendor/SOURCES.json`，`.cargo/config.toml` 解析仓库内依赖。局部入口为 `cargo test -p jchtools --features test-hooks --test acp_settings --test acp_http_contract --test acp_callbacks --test acp_service_process --test acp_prompt_summary`；其中 `acp_service_process` 的两项 `real_gui` 用例虽不移动硬件鼠标，仍会激活真实窗口，按 3.4 另获当次桌面授权后运行，不能直接把整个 target 当成无头验证。合成 Agent 源码在 `tests/fixtures/acp_api/main.rs`，仅 test-hooks 构建，不进入生产包。
- **子模块文档登记表**：仓库子级 `AGENTS.md` 只承载该子树专属的职责、入口、命令、约定与陷阱；全局规则与全局命令唯一权威在本文件，子文档不重复维护。当前登记两份（全仓库含本文件上限 8 份）：
  - `optional/snap-ocr-worker/AGENTS.md`——截图 OCR 后台进程 crate：托盘/热键/GDI 截图/结果窗/管道服务与 Xberg 客户端的本地命令、与主包的双端同步点及陷阱。
  - `optional/snap-ocr-core/AGENTS.md`——O-23～O-28 冻结规格的纯逻辑参照物 crate：无人依赖的现状、测试命令与删除/改动前置约束。

## 2. 临时文件规则（强制）

- 一切测试、验证、截图、日志、下载与中间产物 `MUST` 放在仓库根目录的 **`.tmp/`** 下，例如 `.tmp/ui-test/`、`.tmp/screenshots/run1.png`、`.tmp/bench/`。
- `.tmp/` 已被 `.gitignore` 忽略：`MUST NOT` 提交、`MUST NOT` 打进发布包、`MUST NOT` 用 `git add -f` 强加。
- 测试临时数据 `MUST NOT` 写在仓库其它位置（仓库根、`src/`、`crates/`、`resources/`、`docs/`、`tests/` 都不行）。
- 需要长期保留的样例数据放 `tests/`，或直接用测试内置的临时目录（`tempfile` / `std::env::temp_dir()`）。
- 校验、清理或重建 `.tmp/` 的脚本 `MAY` 直接删除该目录内容，不会影响仓库文件。
- 项目所需的一切临时目录与测试数据统一由 `python scripts/make_tmp.py` 生成（种类可扩展，现有 `testdata`），默认落在 `.tmp/` 下；测试完成后清理本会话自建子目录，仅在确认无并行会话使用 `.tmp/` 时才用 `python scripts/make_tmp.py clean` 全量清空释放磁盘，`MUST NOT` 清空其他会话仍在使用的内容（无执法点 · 软法），`MUST NOT` 让 `.tmp/` 无限增长。

## 3. 构建与测试

以下命令在仓库根目录的 Windows PowerShell 中执行；Rust / MSVC / Windows SDK 条件见下文。Python 自维护脚本在 `scripts/`、类型桩在 `typings/`，依赖见 `scripts/requirements-dev.txt`，Ruff / BasedPyright 配置见 `pyproject.toml`；Git 工具测试需 PATH 中可用的 git。完整验收和发布仍受 3.4 授权约束，列出入口不代表本次已经执行。

```powershell
cargo run --bin JchTools   # 启动 GUI（默认 gui 特性）
cargo build                # 开发构建（GUI）
cargo build --release      # 发布构建
cargo nextest run --all-targets --features test-hooks   # 根与全部生产 crate 的单元/集成测试，默认过滤真实桌面
cargo test --doc --features test-hooks                  # 根 workspace 文档测试
python scripts/static_check.py      # 结构/配置/回调/SQL/测试基线/界面规则静态检查
python scripts/test_gate.py fastcheck   # 静态/类型/格式检查及编译，无测试（见 3.4）
python scripts/test_gate.py fulltest --authorized   # 当次授权后本地增量非打包功能门；独立明确全量验收时加 --full
python scripts/test_gate.py slowtest --authorized   # 另获当次授权后本地发布交付门；独立明确全量验收时加 --full
powershell -NoProfile -File .\scripts\acceptance.ps1 -WithEngine   # 单命令验收（见 3.2）
powershell -NoProfile -File .\scripts\package-windows.ps1   # 生成完整安装包与便携 ZIP、逐步骤墙钟计时
```

手工测试集：`python scripts/make_tmp.py testdata`（默认自动生成到 `.tmp/testdata/`，会先清空该目录；也可 `--destination` 另指专门测试目录，仓库内 `.tmp` 之外的位置会被拒绝）。普通处理验收不使用 `--git`：根目录含 `.git` 时按 H-06 拒绝整次处理；`--git` 仅可用于验证该拒绝行为。需要还原普通数据集时重新运行生成命令；测试完成后用 `python scripts/make_tmp.py clean` 清理。

- Windows 构建机需要 Rust `x86_64-pc-windows-msvc` + VS C++ Build Tools + Windows SDK（`rc.exe` 用于把 `resources/app.ico` 嵌入 EXE）。
- 提交前按 3.4 选择必要验证并复用输入未变的通过结果；完整功能验收仍需对应测试与矩阵证据，不因缺少授权而冒报通过。构建输出不得新增 `binding loop` 警告。
- **本机 Xberg 测试引擎**：选版遵守 XB-10。`scripts/xberg_test_engine.py` 在 fulltest 前查询官方 latest、验证已有引擎身份，缺最新版本时校验归档并覆盖固定测试目录 `C:\Users\jiang\Documents\xberg-test\xberg-cli-x86_64-pc-windows-msvc`；旧协议由 `tests/xberg_legacy_protocol.rs` 的合成引擎锁定，不依赖本地旧发布物。（无执法点 · 软法）
- **Xberg 源码参照（E:\xberg）**：xberg 接口的事实参照以本机 `E:\xberg` 源码为准，仅在必要时作权威接口文档查阅（2026-10-03 用户确认）。该源码 `MUST NOT` 编译、构建或产出任何测试/发布物——测试只用上一条的固定引擎目录，产品只用用户配置目录（与 O-31 的「源仓库只读 oracle」边界一致）。（无执法点 · 软法）

> 隔离测试显式启用 `test-hooks`；该特性不在默认特性中，生产与打包构建不启用。它允许测试目录和测试代理覆盖；`debug-assertions` 本身不再授予这些覆盖。测试门同时保持生产构建维度。临时验证工作目录可用 `python scripts/make_tmp.py workspace --destination .tmp/validation` 创建。

生产功能按 `crates/` 下 14 个真实 package 隔离，根 package 只保留主 EXE 与合成 ACP fixture，没有旧库 façade。默认构建/测试覆盖根与这些生产成员；可选截图包仍使用 3.3 的额外显式入口。Cargo 缓存复用以真实 fresh/dirty 输出为证据，保持现有 profile、target、features、RUSTFLAGS，不用清缓存或改变构建维度伪造提速。`.cargo/config.toml` 的 `jobs` 面向本地多核宿主设定（2026-10-14 由 2 调至 16：默认 32 曾触发 Windows 提交内存耗尽 os error 1455，16 经全量脏重建实测在 94.9GB 提交上限内安全）；CI 4 核 runner 由两个 workflow 显式 `CARGO_BUILD_JOBS=4` 覆盖，其他机器可用同名环境变量覆盖；并行度变化不改特性、目标、profile、链接器或受检覆盖。Windows 打包的主构建显式选择 `-p jchtools -p jchocr --bins`；引擎生成由 file-core、截图内嵌清单由 assets、Slint 生成由 ui 各自的 build script 负责，根 `build.rs` 仅承接 Windows 资源与 manifest。`package-windows.ps1` 在 `.tmp/package-windows-<独占标识>/timings.jsonl` 以无 BOM UTF-8 追加各实际阶段和 total 的完整墙钟快照；读取最后一条完整 JSON 行取得状态，异常留下的不完整末行不作为结果。失败保留已写入记录，输出打印该路径；这些记录不是纯编译时间，也不进入发布包。

## 3.1 7-Zip 引擎

引擎相关需求（官方来源、校验、运行期解析顺序、发布包合规）以 `openspec/specs/application-contract/spec.md` 的 E 分区为准，本节只写工程事实：`scripts/fetch-7zip.ps1` 负责获取官方引擎（产物落在 `resources/7zip/`，已被 `.gitignore` 忽略，`MUST NOT` 提交）；构建时 `crates/file-core/build.rs` 把同一份引擎压缩编入 file-core 并最终链接进 EXE（`crates/file-core/src/engine_bundle.rs` 负责释放与校验），缺失引擎时构建不失败、仅不内嵌。打包校验读取当前 Cargo artifact 绑定的 file-core build-script OUT_DIR 状态，不按时间猜缓存；同字节引擎输入与内容寻址截图清单不会因打包输出目录不同而改写构建输入。

## 3.2 测试验收纪律（代码与测试均由 AI 代理产出，以下用于对抗自证偏差）

- **回归测试先行**：修复任何缺陷 `MUST` 先写能在修复前失败的回归测试，并在提交信息记录反证证据（复现命令 + 修复前失败输出摘要）。只有「修复后通过」而没有「修复前失败」证据的修复不算完成。
- **禁止削弱测试**：删除、改名、放宽断言、新增 `#[ignore]` 或平台门禁（`#[cfg(...)]`）`MUST` 同步更新 `scripts/test-baseline.json`（用 `python scripts/static_check.py --update-test-baseline` 重新生成）并在提交信息写明理由；`MUST NOT` 只为了让测试变绿而做上述改动。平台门禁 `MUST` 附带原因注释，且被门禁的行为 `SHOULD` 在另一平台仍有覆盖。
- **提交纪律**：提交信息按变更性质 `MUST` 携带对应记录——缺陷修复附回归反证（复现命令 + 修复前失败输出摘要）；合同增改附变更理由；可信基变更附理由；宣称功能验证通过附本地证据，宣称发布成功附发布 run 和版本链接。缺对应记录的提交 `MUST NOT` 合入。
- **完成条件**：见 3.3 验收矩阵；`MUST NOT` 只跑根 workspace 的 nextest 与 doctest 就宣称引擎 / UI / 发布相关工作已验证。
- **独立复核**：涉及引擎、删除路径、解压安全（`fsutil` / 覆盖语义）或用户可见行为的实质变更，`SHOULD` 由未参与实现的独立代理会话复跑验证并给出证据格式：命令、环境（OS / rustc / 是否真实引擎）、退出码、关键输出行。
- **本地验收与 CI 职责**：Windows 完整功能验收须有全量测试与对应矩阵证据；增量通过仅证明所选测试。按用户最新批准策略，`check.yml` 与 `release.yml` 手动触发，仅编译、使用 `package-windows.ps1 -SkipTests` 打包，后者发布安装包及便携 ZIP；保留生产依赖和交付资产，不安装测试或质量依赖及 nextest，不准备本地测试引擎目录，不运行功能测试、lint/质量门、真实模型或桌面。CI 成功不代表功能验收，也不代替独立授权的真实桌面或模型验收；发布仍须单独授权，未执行项如实标注。
- **flaky 政策**：`MUST NOT` 重跑到绿。测试间歇性失败必须查因；确属 flaky 的要在提交信息记录现象与原因，不得静默重跑。
- **人工验收边界**：用户已决定不保留人工验收项清单；自动化未覆盖的行为（如真实 TB 级数据、非 150% DPI、网络共享）`MUST NOT` 被代理宣称已验证，只能如实标注「未验证」。
- **UI 测试交互方式（2026-10-09 用户确认）**：本项目 UI 测试 `SHOULD` 优先使用不移动、不占用用户鼠标的方式；除非用户当次显式要求使用 `e2e.md`，`MUST NOT` 执行会抢占用户鼠标的桌面测试流程。该约定不降低真实 GUI E2E 的覆盖及结果断言要求；无法按上述方式完成的项目须如实标注未验证。（无执法点 · 软法）

## 3.3 验收方法（怎么测试、怎么验收）

「验收通过」= 下表相应本地验证行全部执行且通过；执行权限按 3.4，真实桌面驱动须另行明确指令，不为满足矩阵自动触发。未执行项保持未验证；发布成功另须确认 release 工作流成功及实际版本产物存在，`NOT RUN` 不得报告为通过。

| 场景 / 变更类型 | 必须通过 | 覆盖 |
|---|---|---|
| 任意变更（每次提交；纯文档除外，见下文） | 根及两个 optional 包的全量 `cargo nextest run` + `cargo test --doc`（入口见下文）+ `python scripts/static_check.py`；默认过滤真实桌面 | 合同全部条目对应测试 |
| 引擎 / 解压 / 删除 / 路径安全 | 上行 + `powershell -NoProfile -File .\scripts\acceptance.ps1 -WithEngine` + `tests/gui_flow.rs` | 合同 S / E 分区、C-01 |
| UI（`ui/` 界面源 / GUI 装配） | 任意变更行 + `tests/gui_flow.rs` + `scripts/gui_smoke.py` S1–S4 + S18（两档窗口尺寸布局判定） | 合同 C / U 分区 |
| 递归解压端到端 | `python scripts/make_tmp.py testdata` → GUI 完整走开始、确认、完成链路 → 按 `openspec/specs/recursive-extraction/spec.md`、全局 H / S 与共用附录逐项断言 → `python scripts/make_tmp.py clean` | X 与适用全局条目 |
| 目录整理端到端 | `python scripts/make_tmp.py testdata` → GUI 完整走分析、计划、确认、执行链路 → 按 `openspec/specs/directory-organization/spec.md`、全局 H / S 与共用附录逐项断言 → `python scripts/make_tmp.py clean` | C 与适用全局条目 |
| 打包 / 发布 / 引擎捆绑 | slowtest 中 `scripts/package-windows.ps1` 全程及双产物原内容校验 + 干净目录解包运行（真实桌面部分另获授权）；fulltest 不证明打包验收 | 合同 P-05 / P-15 / E 分区 |

- 单命令入口：`powershell -NoProfile -File .\scripts\acceptance.ps1`（可选 `-WithEngine` / `-WithPackage`；本次另获真实桌面验收授权时加 `-WithDesktopTests`，`-WithGuiSmoke -GuiData <目录>` 与 `-WithMarkdownAcceptance` 均要求该声明，参数本身不创造授权）。
- **需求 ↔ 测试映射**：验证合同条目的测试 `MUST` 在其文档注释中标明合同编号（如 `// 覆盖 C-12`）；每个合同条目至少被一个测试引用。（现状：已有部分编号注释，但全条目覆盖尚未逐项核验，映射执法待落地。`scripts/requirement_coverage.py` 逐文件扫描需求并在 fulltest 输出 `.tmp/test-gate/requirements-map.json`，编号解析现已纳入 AH / CA 分区。候选引用不是断言或执行证明；逐项复核真实链路前保持 UNVERIFIED。）
### 自动化验证要求与当前边界

- 功能开发和功能性修改 `MUST` 按变更行为、风险与已批准验收条件选择最小充分的自动化验证；局部逻辑使用 UT，验收依赖跨模块或真实公开入口到可观察结果时使用集成或 E2E 验证，已有有效覆盖与相关输入未变的通过结果可以复用。不机械要求每次改动新增所有测试层，不为小改动重建测试体系或原行为；本节矩阵与规格中的明确验收项、3.4 授权边界保持不变。不得依赖用户手工读代码或人工回归保证质量。（无执法点 · 软法）
- 只测试本项目 Rust 产品；不测试 Python、PowerShell 脚本或选测工具自身，保留 Python 驱动的 Rust 产品验收，静态检查和类型检查不受此限制。（无执法点 · 软法）
- 测试 `MUST` 对应需求及验收条件，覆盖核心成功路径与相关关键失败路径；UT、编译、静态检查、局部模拟和“没有崩溃”不能替代完整链路的结果断言。桩与模拟仅作补充，未经验证的真实边界须明确说明。（无执法点 · 软法）
- 验证报告 `MUST` 区分已实现、验证通过、验证失败和未验证；环境、依赖或权限不足不得报告验收通过。（无执法点 · 软法）
- 以下手工 nextest 命令在 PowerShell 中均须追加 `--test-threads $([Math]::Max(1,[Math]::Min(28,[Environment]::ProcessorCount)))`；原生配置的 `num-cpus` 不单独实施 28 上限。
- 根 workspace UT / 集成入口为 `cargo nextest run --all-targets --features test-hooks`，文档测试另运行 `cargo test --doc --features test-hooks`；默认原生过滤排除真实桌面。可按目标运行 `cargo nextest run --features test-hooks --test md_tools`、`cargo nextest run --features test-hooks --test git_tools` 等。`tests/git_tools.rs` 使用真实 git 与本地 bare 远端，不覆盖真实网络及认证。
- Xberg 集成入口为 `cargo nextest run --features test-hooks --test xberg_settings --test xberg_shared_process`（默认 `gui` 特性）。前者覆盖 SQLite 保存、旧配置迁移、独立进程恢复及场景资产选择；后者启动真实 JchTools 代理和命名管道，但使用由 `rustc` 编译的模拟引擎，验证进程复用、转换期间截图响应及请求隔离。它们不覆盖 Windows 重启、真实模型常驻或热键/托盘到结果的桌面 E2E；完整验收仍按 XB 第 5.2 / 6 节执行。
- GUI 链路入口为 `cargo test -p jchtools --features test-hooks --test gui_flow`；该 target 使用真实 winit 窗口，显示时会激活用户焦点，属于门外独立桌面验收。`crates/gui/src/gui.rs` 内另有 MD 真实回调测试，使用 Slint TestingBackend，不等同于真实桌面窗口验证。`scripts/gui_smoke.py` 默认运行 S1–S4（启动、整理与解压）、S15（MD 合并/拆分产物与原文件保护）、S16（Git 逐文件提交到本地 bare 远端）及 S18（两档窗口尺寸下逐页主要按钮边界与重叠判定）；S16 不覆盖真实网络与认证。S6–S9 的配置隔离需要 `cargo build -p jchtools --bin JchTools --features test-hooks` 产物，脚本在操作前检查隔离 SQLite 已建立；普通构建不识别测试环境变量。其他扩展阶段通过 `--stages` 选择，不能以底层测试冒充 GUI E2E。S5/S14 按最新 T-23 检查取消当前媒体、先前成品不变及无未完成产物。
- 不抢占桌面的 GUI 交互入口另见 [e2e.md](e2e.md) 的无头 MCP 流程：调试/测试构建使用 `mcp-server,test-hooks`，运行时设置 `SLINT_BACKEND=headless` 并按该文档成套隔离状态目录、CodeAgent 凭据路径和测试注册表，再用 `SLINT_MCP_PORT` 激活本机接口。该入口不替代真实窗口验收，不改变 3.4 授权边界；生产与发布构建不启用 `mcp-server`。
- 转 Markdown 的验收依据及交付边界唯一见 `openspec/specs/markdown-conversion/spec.md`，共享来源、内置发现及音频可选资产见 `openspec/specs/shared-inference/spec.md`；按附录 A 验证实际安装包/ZIP、离线真实 OCR/媒体转换、GUI 入口及旧源迁移。构建装配入口为 `scripts/prepare-xberg-runtime.ps1`，由 `package-windows.ps1` 调用，运行期不查询最新或下载基础组件；旧 all2markdown 的源码、测试或历史 CI 不能作为当前通过证据。实现范围按 T-30，不借迁移改变旧功能或搬入旧 Python 架构。
- 纯文档等非功能性修改只按实际影响检查内容、差异、引用和需求保留情况，不运行产品构建或测试，不为文档补建功能测试。既有验证基线保留，可信基隔离与提交纪律仍按 0 / 3.2 / 9 执行；未执行的功能验收如实标注，不以文档检查冒充通过。
- 可选组件的 UT / 集成测试从仓库根目录分别运行 `cargo nextest run --features test-hooks --manifest-path optional/snap-ocr-core/Cargo.toml --all-targets` 和 `cargo nextest run --features test-hooks --manifest-path optional/snap-ocr-worker/Cargo.toml --all-targets`；分别以相同 manifest 路径及 `--features test-hooks` 运行 `cargo test --doc`。根 workspace 的 `default-members` 包含根及全部 14 个生产 crate，根 nextest 覆盖搬迁后的内联测试和 jchocr 集成测试，但不覆盖这两个 optional 包；本地 `acceptance.ps1` 和全量本地门均有二者的显式测试步骤，CI 不运行测试；增量门通过不能冒报截图组件完整验收。静态测试发现统一覆盖所有本项目 package 的 `build.rs`、`src/`、`tests/`、`examples/`、`benches/`，保留 ignore、cfg 与测试体哈希判据，不扫描 vendor 或 Cargo 生成源码。旧 `optional/markdown-media-worker` 已按 XB-12 退役（媒体转录迁移到 Xberg 推理组件），其测试与打包步骤一并移除。
- 转 Markdown 的验收承接入口为 `scripts/markdown_acceptance.py`（`--list` 查看条目；默认 `--profile common` 必验新老 Office / PDF / MP4，`--profile full` 保留扩展矩阵，OPTIONAL 不计为通过也不阻塞常用验收），本次另获真实桌面验收授权时也可经 `acceptance.ps1 -WithDesktopTests -WithMarkdownAcceptance` 接入；运行依赖见 `scripts/requirements-dev.txt`，真实 GUI、Xberg、媒体组件与两种发布目录按脚本参数提供。提供 `JCHTOOLS_TEST_XBERG_DIR` 时，验收先写隔离 SQLite，再用 S17 真实 GUI 初始化本地 notice；不读取生产资产来冒充隔离就绪。缺资产条目为 `NOT RUN`；脚本退出码 0 仅表示全部必验条目 PASS；保留的扩展 OPTIONAL 不计为通过；任一必验条目未执行（含 --only 排除的条目）返回 2，应逐项检查。`scripts/gui_smoke.py --stages S5` 另有转换开始/停止链路，需已配置可用组件，不在默认序列内，不能替代转换产物断言。
- 截图 OCR 的验收覆盖目标见 `openspec/specs/screenshot-ocr/spec.md` 附录 C。迁至 Xberg 后，worker 已无 `det_paddlex_oracle` / `pipeline_backend_oracle` 测试目标；旧直连引擎路径已退役，服务仅经 `SharedXbergClient` 使用共享引擎。已有 Slint 测试后端结果窗用例不等于热键/托盘、服务生命周期或多 DPI 的真实桌面 E2E，这些链路的自动化覆盖尚未确认。`tests/ocr_fixtures/README.md` 中的旧 `ocr_compare` 命令对应的 example 已不存在，历史结果不能作为当前通过证据。补齐当前共享引擎的同图对照与真实入口自动化，是后续功能验收的必要工作；执行权限及独立桌面边界按 3.4。
- 本地 `acceptance.ps1` 默认测试 Snap OCR core / worker 并检查资产清单；本次另获真实桌面验收授权并加 `-WithDesktopTests`，同时设置 `JCHTOOLS_SNAP_OCR_ASSET_ROOT` 为完整的已校验资产缓存时，追加真实 worker 从安装位置加载模型的热键服务测试。未同时满足时该项报告 `NOT RUN`，不能据此声称桌面验收通过。

## 3.4 三级测试门与执行权限

统一入口 `python scripts/test_gate.py <fastcheck|fulltest|slowtest>`；各级含义按本节维护。以下规则于 2026-10-10 经用户批准；2026-10-20 用户明确要求分离日常功能回归与发布交付验收，据此将打包相关验证从 fulltest 移入 slowtest（合同 P-12 / P-15）。平台范围按合同 P-07 仅 Windows，不设跨平台或跨 WSL 验证阶段。

本地门编排静态/类型检查、编译、发现与测试；默认测试增量，显式 `--full` 全量。正常保留 Cargo `target/` 与增量编译缓存，保持原静态与编译命令、断言及 feature / profile / target 维度，缓存单位仅为 Cargo / cargo-nextest 运行的单个 Rust 测试用例：fulltest 和 slowtest 可共享此前成功结果；文件 A 变更时，使用 cargo-affected 与 Cargo 的逐用例依赖证据使依赖 A 的用例失效并重跑，已证实与 A 无关且其他影响输入仍有效的用例继续复用，不因全仓源码快照变化使所有用例失效。新增、失败、中断、依赖无法确定、影响输入或外部环境失效的用例保守实际运行，失败、未验证或中断不得冒充成功。不得缓存或跳过整门、编译、静态检查或其他非测试检查阶段，非测试阶段每轮实际执行；fastcheck 不运行测试，不得声称用例缓存命中。`--full` 全量实际执行，不复用用例结果。真实外部状态须现场验证，发布和打包须实际交付并校验当前产物，不以旧结果免除必要检查。每轮报告实际运行及缓存命中数量、失败和阶段耗时。阶段顺序执行，编译生产者有序；高风险检查和测试先于普通组执行，失败不阻止无依赖的后续阶段，清单发现不算测试。共享 Xberg / 媒体及真实 / 随包 7-Zip 保持安全隔离。每轮独占分配日志目录，仅错误时输出保留的日志路径，失败明细是本轮证据；成功明细隐藏且不持久保留为日志（受控 nextest 原生缓存记录按下述例外保留）；增量 PASS 仅证明所选用例通过，不是完整功能验收。

fulltest / slowtest 依赖 [cargo-nextest](https://nexte.st/) 0.9.116 或更新版本（`cargo install cargo-nextest --locked`，coverage 采集需要 `NEXTEST_BINARY_ID`）；fastcheck 不要求 nextest。默认普通 Rust 测试阶段使用实验性 [cargo-affected](https://github.com/max-sixty/cargo-affected) `cargo affected run --report-json <路径> -- <nextest参数>`；`--full` 和真实引擎、媒体等外部状态阶段始终 `cargo nextest run` 全量；文档测试始终使用 `cargo test --doc`。共用根 `.config/nextest.toml`、`--retries 0 --no-fail-fast --color never --no-tests fail --status-level fail --failure-output immediate --show-progress none`。默认过滤真实桌面，另获授权的 `-WithDesktopTests` 可显式忽略默认过滤，绝不同时运行 ignored 模型测试。PowerShell 验收及未使用 `-SkipTests` 的打包运行全量 nextest 和 Cargo 文档测试。

测试并行度按可用逻辑核心数动态取 `min(28, 可用逻辑核心数)`，不可用或零时取 1；门计划器为 nextest 及 cargo-affected 转发 `--test-threads`，PowerShell 验收和未使用 `-SkipTests` 的打包同样计算并传参。根 nextest 配置的 `test-threads = 'num-cpus'` 不能表达上限，手工调用须按 3.3 显式传参；它与 Cargo 编译 jobs=16 无关。物理共享引擎组仍最多一个测试；jchocr 仅 `engine_asset_failure_is_an_explained_xberg_error` 和 `review_remediation_small_url_preserves_successful_ocr_text` 使用该 engine-session 组，三个 `review_remediation_url_{content_length,chunked,without_length}_budget_plus_one_is_network_error` 容量用例使用另一个单并发组，独立 jchocr CLI 测试可并行；worker 既有隔离不变。

cargo-affected 0.4.1 官方称极早期、支持 Windows MSVC。本地开发先运行 `python scripts/native_test_tools.py`，在 `.tmp/native-tools/cargo-affected/bin` 安装固定上游 SHA `08000ff968eebb238a26a3a4de38fe314e8ca05e` 加兼容补丁的 `0.4.1+jchtools.5`：兼容 nextest 默认过滤和显式 filter 的最小筛选，为包含 `test = false` 目标的已识别 workspace 测试目标构造完整 sentinel；`collect --diff` 使用与 run/status 相同的 metadata 规则及所有可达 SHA 差异，沿用原有过滤清单，仅对实际采集结果更新 SHA；使用 fingerprint/SHA 覆盖索引减少选测查询；并包含 Windows LLVM profile 路径、上游 Windows 源码暂存目录碰撞及过长路径兼容补丁。该工具只供本地开发，不进入 CI 或产品包，不修改全局 Cargo bin 或正常 target；缺失或版本不符时提示安装，不静默使用旧版。

默认增量门先以原生 `cargo affected status --report-json` 检查 coverage 基线；缺失或失效且 Git 工作树干净时，原生 `cargo affected collect` 一次采集整个 workspace（含两个 optional 包），采集真实执行测试，之后仍运行三个普通 affected 阶段：原生工具跳过已有有效覆盖且未变化的测试，无 coverage 或不适合插桩的测试保守普通执行，不重复已有效覆盖测试。jchocr 的 `review_remediation_url_{content_length,chunked,without_length}_budget_plus_one_is_network_error` 三个 384 MiB 容量边界用例仅从插桩 collect/status 排除，普通 affected 和 `--full` 仍执行，保留 30 秒上限和断言；子进程损坏的 profraw 不伪造 coverage，缺覆盖的测试由原生工具保守重跑。真实引擎、媒体及文档测试仍全量执行。有效基线随已提交 HEAD 前移而无当前 equal 锚且工作树干净时，使用原生 `collect --diff` 更新；脏源码按采集 SHA 差异选择，不保证任意脏树重复运行均零选中，dirty 不使用 `--allow-dirty`，基线缺失或失效时明确提示并保守回退全量。原生 SHA / DB 是原生选测状态，逐用例结果缓存不得取代或伪造选测状态；插桩使用隔离构建目录，不清理正常 target、不全局更改 RUSTFLAGS。`--full` 不自动采集，保留显式全量路径。本次用户授权本会话变更仅本地提交，不推送、不触发 CI 或发布；其他授权边界不变。基线采集和零选中都不代表本轮全部功能覆盖。

根 `Cargo.toml` 的 affected metadata 对非 Rust 输入、`build.rs`、vendor、脚本、夹具及 `.cargo` 配置等保守选择全部测试；该工具仍处于早期，不能保证选择绝对精确，增量不能证明完整覆盖。仓库无自有 TypeScript 工程，不安装 Bun 或创建空任务。

本次提速验收须在代码和环境不变下连续两轮运行默认 `python scripts/test_gate.py fulltest --authorized`，两轮均通过并以真实输出分别报告选测、运行情况及实际耗时，证明第二轮大幅减少；不设脚本硬时间阈值。不得将采集或零选中冒报为全部功能验收。


### 测试代码质量配置（2026-10-11 用户批准）

- 纯测试、测试夹具与测试替身只要求语法/导入有效、编译成功和实际测试执行；保留全部断言、发现范围、隔离与失败报告，不要求严格格式、Clippy 风格/文档规则、Python lint/类型/Bandit/Vulture，也不将测试专属编译告警升级为错误。
- 生产库、测试运行器、验收与部署脚本仍使用既有严格质量配置；不能仅因名称包含 `test` 或位于测试目录就豁免。混合生产/测试 Rust 文件只在测试专属模块或声明上应用宽松属性。
- 本地门的静态检查按上述角色划分；测试编译、断言及全量模式范围保留，打包相关验证按下述 fulltest / slowtest 分层归属执行。`scripts/quality_scope.py` 维护纯测试文件边界并提供生产 Rust 格式检查、全部 Python 语法检查；Python 工具配置与本地计划器使用同一角色边界。CI 不运行质量检查。


- **fastcheck**：静态检查 + format + compile，不运行任何测试，保留既有检查定义、判据与测试源码。AI 仅在相关改动成批完成后有具体验证需要时选择运行，不逐文件运行；每轮实际执行检查；不运行测试或报告用例缓存命中。通过只证明本级检查通过，不代表完整功能验收。不得以两次缓存后生产编译代替静态或格式检查，也不设置预热阶段。
- **fulltest**：含 fastcheck 同项；`--full` 执行所有适用的 Windows 非打包 Rust 产品 UT / integration / doc / e2e，包括可选组件、真实 7-Zip / Xberg / 媒体及适用的无头/不抢占用户桌面的本地用例；默认本地增量按上述规则选择。不要求 ISCC，不生成安装包或便携 ZIP，不执行 `scripts/package-windows.ps1`、仅为发布验证的 `release-target-build` 或在线全量供应链审计 `pyquality-pip-audit`；通过不证明打包交付或实时漏洞库审计通过。
- **slowtest**：含相应模式的 fulltest 覆盖一次（测试用例可按缓存规则复用有效成功结果） + `pyquality-pip-audit` 原在线审计命令 + `release-target-build` 原命令 + 完整本地 `scripts/package-windows.ps1`；`--full` 全量测试，默认本地增量不能冒报完整验收。保留原引擎内嵌、许可证与合规材料、引擎不泄漏、随包运行资产以及安装包 EXE / 便携 ZIP 的全部原内容校验与断言。在线供应链审计依赖实时外部漏洞库，归入 slowtest，不删除或放宽审计。不先跑 fulltest 再重复同一功能覆盖；P-07 无跨平台/WSL 需求，不存在的跨平台增量标记 `SKIPPED_NOT_APPLICABLE`，不得伪造测试通过。
- **门内边界与独立桌面验收**：所有门均 `MUST NOT` 触发 CI、远程流水线、发布、ComputerUse、全局鼠标或焦点抢占测试。`gui_flow` 全部用例、`acp_service_process` 两项 `real_gui` 用例、`scripts/gui_smoke.py` / `scripts/markdown_acceptance.py` 的真实桌面 driver，以及注册全局热键的 `check_worker_root.py` 服务探测保留原测试/断言及独立入口，门中标记 `NOT_RUN_SEPARATE_USER_INSTRUCTION_REQUIRED`；不得因其在门外而判本门 FAIL，也不得冒报 PASS。软件渲染不隔离焦点；执行须用户当次另行明确指令，并遵守 3.2 的桌面交互限制。库内 Slint TestingBackend、OCR 隔离 desktop / window station 用例和其他适用本地 e2e 仍在完整门内。（执法点：`local_gate_plan.py` 的阶段分类与测试选择；真实授权为会话软约束）
- **授权**：fulltest / slowtest 每次运行 `MUST` 获得当次明确授权；用户明确授权本次任务所需的全级运行及必要重跑时，仅在该任务范围内有效，不继承历史、上一次、CI 配置或脚本注释的授权。`--authorized` 只是当次明确授权的入口声明，不创造授权；绕过入口直接运行内部阶段遵守同一纪律。不得静默重跑到绿、规避失败证据或扩大真实桌面、模型及发布权限。（执法点：`test_gate.py` 的 `--authorized` 入口守卫；授权真实性为软约束）
- **失败与计时（本次用户明确批准）**：当前要求替代旧的首错停止：fastcheck / fulltest / slowtest 失败后继续其余可运行阶段及用例，最终完整汇总并返回非零；必要前置缺失使依赖项不可运行时明确报告 FAIL、UNVERIFIED 或 NOT RUN，不能冒报通过。用户主动中断不强制继续。使用 Nextest 原生非 fail-fast 行为，不自研失败监控或强杀；取消脚本硬时间限制和编译观测扣时，记录实际阶段和总墙钟供观测，不按耗时阈值判败。代码和环境不变下连续两轮 `python scripts/test_gate.py fulltest --authorized` 均须通过，分别汇报真实选测、运行情况及实际耗时；增量 PASS 不代表全量覆盖，`--full` 仅用于独立明确的全量验收，不常规用于本地或 CI。（无执法点 · 软法：连续两轮及报告要求）
- **三级门失败明细（本次用户明确批准）**：成功用例及正常 skip 隐藏，不向控制台输出其名称、输出或逐条耗时，也不持久保留其明细日志或全用例 JUnit 明细；只展示和保存失败 / 错误用例的名称、诊断及耗时。仅为逐用例缓存，受控的 cargo-nextest 本机原生结果存储可保留成功测试身份及逐条耗时；这些记录不输出到控制台、不作为明细日志，成功 stdout / stderr 的录制上限设为 `max-output-size = "0B"`，不得暴露凭据。保留不含成功用例身份的整轮状态、阶段与总耗时、实际执行及选测 / 缓存数量等真实聚合汇总；缺失前置的项目如实标为 UNVERIFIED / NOT RUN。stdout 可临时捕获供诊断或 metadata 消费，成功后不持久保留；LLVM coverage / 依赖数据库是原生选测必要状态，不是明细日志，不删除或伪造。（无执法点 · 软法）
- **日志进程隔离（2026-10-17 用户确认）**：每次门进程启动、重跑及同进程内再次创建的门运行，`MUST` 使用包含进程标识与唯一运行标识的独占日志目录；独立命令重跑同样隔离。临时阶段 stdout、保留的失败诊断与本轮测试结果文件和打包计时状态文件 `MUST NOT` 跨运行复用、追加或覆盖，嵌套验证也不能改写外层或上一轮证据；成功明细不持久保留为日志；受控 nextest 原生缓存记录按上述例外保存。启动时输出本轮目录；不得为换日志清空历史目录。执法点：`gate_runtime.py` 独占运行目录及日志排他创建、`test_gate.py` 独立命令和计划路径传递。
- **环境受限历史条目**：转 Markdown C08/C09（安装版 / 便携版在独立 Windows 用户会话中真实完成转换）在缺少该会话时仍属未验证；既有条目、覆盖与断言不删除。其真实桌面 driver 按上述独立验收边界报告，不再以旧 slowtest 的 CI 阶段例外处理；另行获授权且取得隔离会话后按原验收要求执行。门内其余适用条目缺环境或工具时如实标记 `UNVERIFIED` / `NOT RUN` 等覆盖缺口，不冒报 PASS 或静默跳过。
- **CI / 发布保持独立**：`check.yml` 手动编译打包并保留构建产物，`release.yml` 手动编译打包发布；二者均不运行测试或质量门；二者都不属于本地三级门。真实发布须用户单独明确该次目标。独立触发流水线时，除非用户当次明确要求等待结果，默认只触发并报告本次 run 链接、标记未完结 / 未验证，不借历史 run 冒报成功；发布成功仍须确认对应 run 最终成功及实际产物。（等待行为与发布授权为软约束）

## 3.5 构建与缓存纪律

- **profile 约定**：日常/受检构建用 Cargo 默认 `dev` profile（`incremental = true`、`codegen-units = 256`、`debug = 1` 行号级调试信息；保持默认，不关闭增量、不加昂贵优化）。受检 `release` profile 同时就是打包配置（`lto = "thin"`、`codegen-units = 1`、`strip`、`overflow-checks`、`debug-assertions`）：slowtest 的 `release-target-build` 保留 `cargo build --all-targets --release` 原命令，与 `package-windows.ps1` / CI 的 `--release --bins` 走同一 profile，不另拆打包 profile（拆分须同步本文件、打包脚本与 CI，属待所有者裁定项）。仅为发布验证的 release 全目标构建与本地 release 打包归 slowtest，不进入 fastcheck / fulltest；不改变原命令、profile 或 CI / 发布独立边界。（分级执法点：`local_gate_plan.py`）
- **linker 选择**：仓库 `.cargo/config.toml` 仅设 `+crt-static`；实际链接器还合并用户 Cargo 配置。本机 `C:\Users\jiang\.cargo\config.toml` 已设 `rust-lld`，本次保留有效配置并复用缓存，不切换回 `link.exe`。其他构建环境未设置覆盖时使用 MSVC 默认 `link.exe`；改变链接器前以完整质量门（含全部测试目标、FFI、build.rs）验证兼容，不兼容即回退并留痕。
- **缓存保护**：三级门正常复用当前 `target/` 和增量缓存，无 `cargo clean`、删除、迁移或重建缓存阶段；不得靠清缓存改变受检条件。门外确需全量重建时给出具体理由，优先定点清理；清理前确认无 cargo 进程持有 build-dir 锁。
- **配置维度稳定**：单轮质量门内 toolchain、`RUSTFLAGS`/`CARGO_ENCODED_RUSTFLAGS`、`.cargo/config*`、`CARGO_TARGET_DIR` 保持不变；固定各级既有 feature 模式 / target triple / profile 矩阵（发布验证维度按上述约定归 slowtest），同一条门命令在修复循环、必要重跑与最终完整验证之间保持同一变体，门覆盖的维度集合不得临时缩小。不得临时减少 features、改变 flags、切换 target 或关闭既有质量检查；各级 dev / release 等矩阵维度按定义复用正常缓存。
- **磁盘清理顺序**：废弃 triple/profile 的整目录 → 旧 toolchain 产物 → 自建临时工具产物（如 `target/miri`）；当前有效增量缓存 MUST NOT 删除；feature 差异在 `target/` 内无独立目录，禁止按目录名/时间戳/体积猜测「旧 feature 缓存」，无法证明废弃的一律保留。
- **timings 诊断**：构建耗时占主导时用 `cargo build --timings` 定位串行瓶颈（大 crate、build.rs、proc-macro、链接阶段），Top 阻塞单元与建议写入质量门报告；宿主机实时防护（如 Windows Defender 覆盖 `target/`）仅作为环境建议披露——不改系统设置、不据此跳过任何检查。
- **性能打点与基准**：运行期耗时打点在 `crates/foundation/src/perf.rs`，由默认关闭的 `perf-tracing` 特性控制（`cargo run --features perf-tracing` 启用；诊断日志（P-10）使 tracing 依赖常开，本特性只门控性能打点代码与 `perf-logs` 层）；性能日志落状态目录 `perf-logs/<进程启动标识>/`、目录内按天轮转，独立与组合初始化共用 `perf::appender`；Xberg 每次引擎启动单独预留 `perf-logs/xberg/<唯一启动标识>/`。与界面运行日志（S-07）及诊断日志互不影响；span 名称与字段名是跨版本可比较口径，只增不改名、不改语义（清单见该文件头注释）。
- **诊断日志入口**：`crates/foundation/src/logging.rs` 常开初始化，GUI、内部后台服务、共享代理、截图 worker 与 jchocr 复用 foundation 的同一实现；文件名包含缓存的真实进程启动标识，排他创建，不在重启或 PID 复用时续写旧文件。记录范围、保留策略和失败退化由合同 P-10 唯一维护。`tests/perf_probe.rs` 是默认 `#[ignore]` 的性能基准（`cargo test -p jchtools --features test-hooks --release --test perf_probe -- --ignored --nocapture`，规模经 `JT_PERF_GROUPS` / `JT_PERF_COPIES` / `JT_PERF_EMPTY_DIRS` 调整）：只测耗时与计数，不断言具体秒数，不进入默认验收门。

## 4. 代码与界面实现约定

产品行为类要求（控件分工、进度语义、窗口行为、缩放适配、配色令牌、命名规范）一律以 `openspec/specs/application-contract/spec.md` 的 U / P / H 分区为准，本节不重复。本节只写实现层约定：

- 实现 `SHOULD` 采用满足当前明确需求或已证实缺陷的最小充分设计，保留无关内容、格式、行为与现有改动；不为假设的未来需求新增抽象、兼容层、框架或无关重构。完成请求结果及适用的必要验证后停止，不扩展相邻任务；用户指定的更窄边界优先。
- 注释、错误信息、界面文案用中文；标识符、模块名、提交信息用英文或中英混排均可，但同一处保持一致。
- Key steps and exception branches in this project must have logs.
- 界面颜色走 `ui/theme.slint` 的 `Design` 全局（浅色/深色两套由 `Design.dark` 切换），不硬编码与主题冲突的颜色。
- Slint 布局中 `MUST NOT` 用 `root.width` / `parent.width` 绑定子项自身宽度（会形成绑定环，编译期警告、运行期可能 panic）；固定宽度用常量，占满剩余空间用 `horizontal-stretch` 或外层容器。
- 本节颜色 / 布局绑定规则由 `python scripts/static_check.py` 机器检查（十六进制色只允许出现在 `Design` 全局内、布局内禁止 `root/parent.width` 宽度绑定）。无障碍不设开发或验收要求，按 H-08 / U-04 执行。

## 5. 改名与新工具时的同步清单

产品名、二进制名、图标、状态目录等一旦调整，`MUST` 同步以下位置：

根及 `crates/*/Cargo.toml`（package/bin 名）· 根 `build.rs`（链接参数与图标资源）· `ui/` 界面源（标题在 `app.slint`；品牌徽章在 `widgets.slint`；关于页在 `pages/about.slint`）· `crates/file-core/src/config.rs` / `crates/foundation/src/xberg_settings.rs`（共享状态目录）· `crates/gui/src/registry.rs`（工具名）· `scripts/*`（打包、启动、检查脚本）· `.github/workflows/check.yml` · `.github/workflows/release.yml` · `resources/windows.manifest` · `README.md` · `AGENTS.md` §1 与 `openspec/specs/application-contract/spec.md`（P 分区命名与定位）。

## 6. 安全底线

安全需求（不覆盖语义、删除策略、不上传等）以 `openspec/specs/application-contract/spec.md` 的 S 分区与 P-03 为准，本节不重复。代理执行纪律：

- 处理真实用户目录前 `SHOULD` 先用副本验证。

## 7. 用户协作协议（本项目用户零创作）

- 代理 `MUST NOT` 要求用户编写代码、文字或文档，`MUST NOT` 让用户阅读代码 diff；需要用户输入时 `MUST` 转换为以下形式之一：附推荐项的封闭选择题、新旧并排截图的是/否判断、一条可直接复制运行的命令。
- 新需求 `MUST` 先复述为可验收的行为断言，进入 `openspec/changes/<change-name>/proposal.md` 及对应能力的需求增量，明确批准状态后再改变已批准基线；具体规则见第 8 节。（无执法点 · 软法）
- 向用户报告结果 `MUST` 先给结论（通过 / 失败 / 受阻），证据（命令、退出码、关键输出行）附后；未验证的事项 `MUST NOT` 表述为已完成。
- 本节为行为协议，无法机检，靠会话纪律与用户在交互中纠偏执行。

## 8. OMP mini OpenSpec 需求维护

- `openspec/specs/<capability>/spec.md` 是唯一已批准需求基线；未批准想法只进入原生 change 的 proposal 与 delta specs，不混入基线，不冒充已完成归档。按相对独立功能域组织，跨域引用而不复制；需求、验收条件与实现和验证状态分开。（批准状态无自动执法点 · 软法）
- 需求及验收条件的有效语义不得因格式、拆分、去重或实现缺口丢失；既有编号保留。用户可见行为变化 `MUST` 对应规格中的行为断言与场景，变更理由进入相应提案及提交记录；需求删除或改变须有用户明确批准，不能以行数或引用数量代替语义完整性。（无执法点 · 软法）
- 代码与有效规格不一致时 `MUST` 以规格为准，按第 3.2 节的缺陷流程修代码；`MUST NOT` 为迁就代码改写、削弱或删除需求。仅修复对既有要求的违背、重构或改变实现方式且需求不变时，不制造提案或需求增量。（无执法点 · 软法）
- 原生结构使用 `### Requirement:`、SHALL/MUST 以及 `#### Scenario:` 中的 WHEN/THEN；模型身份、确定性定义、边界例与验收矩阵仍是相应规格的规范内容。需求引用及验证证据按第 3.3 节维护，引用存在不代表验收通过。
- 行为要求变化时 `MUST` 同步对应规格及交叉引用；入口、命令或开发纪律变化时同步本文件。新能力优先复用合适功能边界；具体需求、提案和实现规划不复制进本文件。（无执法点 · 软法）
- 仓库配置使用 requirements-only 的 `omp-spec`，只维护 proposal 与 specs；需求管理不引入强制的软件实施流程。日常查看使用 `omp openspec list --specs`；基线检查使用 `omp openspec validate --specs --strict --no-interactive`，待批准变更检查使用 `omp openspec validate --changes --strict --no-interactive`。原生检查仅证明文档结构，不代替 Windows 功能验收，也不授予模型、桌面、完整门或发布权限。

## 9. 可信基与防共谋

- 可信基文件：`AGENTS.md`、`scripts/static_check.py`（含 `scripts/static_check_impl/`）、`scripts/test-baseline.json`、`scripts/acceptance.ps1`、`scripts/gui_smoke.py`（含 `scripts/gui_smoke_impl/`）、`scripts/make_tmp.py`（含 `scripts/make_tmp_impl/`）、`scripts/markdown_acceptance.py`（含 `scripts/markdown_acceptance_impl/`）及 `openspec/` 内原生需求与配置。（`resources/rules.json` 与 `crates/file-core/src/config.rs` 结构上由 static_check 的 config_schema 检查互锁，且加规则时二者本就合法同变，不列入可信基。）
- 任何变更 `MUST NOT` 同时修改产品可执行实现（`src/`、`crates/`、`optional/`、`ui/`、`resources/`）与可信基文件；可信基变更 `MUST` 独立提交并注明理由。纯需求引注修正不改变可执行行为，不属于产品实现变更；已有无关产品改动仍须保留并独立处理。（执法：CI 按 diff 文件清单检测混合提交 · 待落地）
- 向本文件新增 `MUST` 级条款时，同一变更 `MUST` 落地对应执法脚本检查，否则该条 `MUST` 显式标注「无执法点 · 软法」；`SHOULD` 每月审计一次无执法点的条款，补齐执法或降级措辞。
