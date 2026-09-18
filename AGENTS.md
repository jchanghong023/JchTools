# AGENTS.md · JchTools 协作约定

面向在本仓库工作的 AI 代理与协作者。规范性用语遵循 RFC 2119（`MUST` / `SHOULD` / `MAY`）。

## 0. 会话快速上手（每个代理会话按此主线执行）

1. 通读两份权威文档：本文件与 `docs/CONTRACT.md`；其余文档（README 等）仅供参考，`MUST NOT` 作为依据。
2. 明确任务类型：新需求 → 先按第 7 节协议转成合同条目并经用户确认；缺陷修复 → 先写能在修复前失败的回归测试；其余变更 → 确认不违反第 9 节可信基隔离。
3. 动工前记录基线：`cargo test` 与 `python scripts/static_check.py` 的当前状态。
4. 编码遵守第 4 节实现约定与合同相应分区；测试数据一律经 `python scripts/make_tmp.py` 生成。
5. 完成后按 3.3 验收矩阵执行相应层级验证，等 CI 转绿，按第 7 节格式报告（结论先行 + 命令/退出码/关键输出 + CI run 链接）。
6. 收尾：`python scripts/make_tmp.py clean` 清空 `.tmp/`，按第 8 节与 3.2 提交纪律写提交信息。

## 1. 项目背景

- **产品**：JchTools——用户个人使用的 Windows 本地工具箱，交付形态为安装包（当前用户目录、免 UAC、桌面快捷方式/开始菜单/卸载项）+ 便携 ZIP（P-05/E-04，打包脚本见 `scripts/package-windows.ps1`，安装脚本见 `installer/JchTools.iss`）。技术栈 Rust 2021 + Slint 1.17（自绘界面、fluent 风格）、SQLite（rusqlite bundled）、随包 7-Zip 引擎。产品定位与命名规范见合同 P 分区。
- **工具**：当前两个——「递归解压」（X 分区）与「目录整理」（C 分区），各自需求见 `docs/CONTRACT.md`。产品完全离线（P-03）：源码中不得引入任何联网能力。新工具 `MUST` 经 `src/registry.rs` 注册 + 真实页面接入；侧栏与导航 `SHOULD NOT` 写死只服务单个工具的文案或流程。
- **生产方式**：本项目全部产出（代码、测试、文档、CI）由 AI 代理完成；用户不编写任何代码或文字，只在封闭选择、看图判断与真实使用中给出意图和反馈（协作协议见第 7 节）。本文件的纪律条款用于对抗代理的自证偏差。
- **权威文档**：仅两份——`AGENTS.md`（本文件：项目背景与过程纪律，怎么开发、怎么测试、怎么验收）与 `docs/CONTRACT.md`（需求合同：软件必须满足什么，只写需求）。其余文档仅为辅助说明，冲突时以权威文档为准。
- **代码入口**：`src/main.rs`（GUI）、`ui/app.slint`（界面）、`resources/rules.json`（40 项界面规则）、`src/registry.rs`（工具注册表）。

## 2. 临时文件规则（强制）

- 一切测试、验证、截图、日志、下载与中间产物 `MUST` 放在仓库根目录的 **`.tmp/`** 下，例如 `.tmp/ui-test/`、`.tmp/screenshots/run1.png`、`.tmp/bench/`。
- `.tmp/` 已被 `.gitignore` 忽略：`MUST NOT` 提交、`MUST NOT` 打进发布包、`MUST NOT` 用 `git add -f` 强加。
- 测试临时数据 `MUST NOT` 写在仓库其它位置（仓库根、`src/`、`resources/`、`docs/`、`tests/` 都不行）。
- 需要长期保留的样例数据放 `tests/`，或直接用测试内置的临时目录（`tempfile` / `std::env::temp_dir()`）。
- 校验、清理或重建 `.tmp/` 的脚本 `MAY` 直接删除该目录内容，不会影响仓库文件。
- 项目所需的一切临时目录与测试数据统一由 `python scripts/make_tmp.py` 生成（种类可扩展，现有 `testdata`），默认落在 `.tmp/` 下；测试完成后用 `python scripts/make_tmp.py clean` 清空 `.tmp/` 释放磁盘，`MUST NOT` 让 `.tmp/` 无限增长。

## 3. 构建与测试

```powershell
cargo build                # 开发构建（GUI）
cargo build --release      # 发布构建
cargo test                 # 单元与集成测试（真实引擎用例默认 #[ignore]）
python scripts/static_check.py      # 结构/配置/回调/SQL/测试基线/界面规则静态检查
powershell -NoProfile -File .\scripts\acceptance.ps1 -WithEngine   # 单命令验收（见 3.2）
powershell -NoProfile -File .\scripts\package-windows.ps1   # 生成含 7-Zip 的发布 ZIP
```

手工测试集：`python scripts/make_tmp.py testdata --git`（默认自动生成到 `.tmp/testdata/`，会先清空该目录；`--git` 建立 git 基线并生成 `恢复.ps1`，测试后可一键回到初始状态；也可 `--destination` 另指专门测试目录，仓库内 `.tmp` 之外的位置会被拒绝；测试完成后用 `python scripts/make_tmp.py clean` 清理）。

```bash
bash scripts/check-linux.sh                       # Linux 核心测试
JCHTOOLS_TEST_7ZIP=/abs/path/7zz bash scripts/check-linux.sh   # 含真实解压
```

- Windows 构建机需要 Rust `x86_64-pc-windows-msvc` + VS C++ Build Tools + Windows SDK（`rc.exe` 用于把 `resources/app.ico` 嵌入 EXE）。
- 提交前 `SHOULD` 至少跑 `cargo test` 与 `python scripts/static_check.py`，并确认构建输出没有新增 `binding loop` 警告。

## 3.1 7-Zip 引擎

引擎相关需求（官方来源、校验、运行期解析顺序、发布包合规）以 `docs/CONTRACT.md` 的 E 分区为准，本节只写工程事实：`scripts/fetch-7zip.ps1` 负责获取官方引擎（产物落在 `resources/7zip/`，已被 `.gitignore` 忽略，`MUST NOT` 提交）；构建时 `build.rs` 把引擎压缩编进 EXE（`src/engine_bundle.rs` 负责释放与校验），缺失引擎时构建不失败、仅不内嵌。

## 3.2 测试验收纪律（代码与测试均由 AI 代理产出，以下用于对抗自证偏差）

- **回归测试先行**：修复任何缺陷 `MUST` 先写能在修复前失败的回归测试，并在提交信息记录反证证据（复现命令 + 修复前失败输出摘要，CI 转绿后附 run 链接）。只有「修复后通过」而没有「修复前失败」证据的修复不算完成。
- **禁止削弱测试**：删除、改名、放宽断言、新增 `#[ignore]` 或平台门禁（`#[cfg(...)]`）`MUST` 同步更新 `scripts/test-baseline.json`（用 `python scripts/static_check.py --update-test-baseline` 重新生成）并在提交信息写明理由；`MUST NOT` 只为了让测试变绿而做上述改动。平台门禁 `MUST` 附带原因注释，且被门禁的行为 `SHOULD` 在另一平台仍有覆盖。
- **提交纪律**：提交信息按变更性质 `MUST` 携带对应记录——缺陷修复附回归反证（复现命令 + 修复前失败输出摘要）；合同增改附用户确认结果；可信基变更附理由与用户批准记录；基线再生成附 `[基线已确认]` 标记；宣称完成附 CI run 链接。缺对应记录的提交 `MUST NOT` 合入。
- **完成条件**：见 3.3 验收矩阵；`MUST NOT` 只跑默认 `cargo test` 就宣称引擎 / UI / 发布相关工作已验证。
- **独立复核**：涉及引擎、删除路径、解压安全（`fsutil` / 覆盖语义）或用户可见行为的实质变更，`SHOULD` 由未参与实现的独立代理会话复跑验证并给出证据格式：命令、环境（OS / rustc / 是否真实引擎）、退出码、关键输出行。
- **CI 权威**：本地验证通过只是临时结论；对应 CI（`.github/workflows/`）run 转绿之前 `MUST NOT` 宣称变更已完成，宣称完成 `MUST` 附 CI run 链接；本地自报证据（提交信息、终端输出）视为线索而非判决。
- **flaky 政策**：`MUST NOT` 重跑到绿。测试间歇性失败必须查因；确属 flaky 的要在提交信息记录现象与原因，不得静默重跑。
- **人工验收边界**：用户已决定不保留人工验收项清单；自动化未覆盖的行为（如真实 TB 级数据、非 150% DPI、网络共享）`MUST NOT` 被代理宣称已验证，只能如实标注「未验证」。
- **变异测试**：发布前或每周 `SHOULD` 触发 `.github/workflows/mutants.yml`（cargo-mutants，范围 `engine` / `fsutil`）；存活 mutant 超预算即失败，新增引擎逻辑时 `SHOULD` 关注存活报告并把可杀的 mutant 用新测试杀掉。

## 3.3 验收方法（怎么测试、怎么验收）

「验收通过」= 下表相应行全部执行且通过 + 对应 CI run 转绿（3.2 CI 权威）；`NOT RUN` 不得报告为通过。

| 场景 / 变更类型 | 必须通过 | 覆盖 |
|---|---|---|
| 任意变更（每次提交） | `cargo test` + `python scripts/static_check.py` | 合同全部条目对应测试 |
| 引擎 / 解压 / 删除 / 路径安全 | 上行 + `powershell -NoProfile -File .\scripts\acceptance.ps1 -WithEngine` + `tests/gui_flow.rs` | 合同 S / E 分区、C-01 |
| UI（`ui/app.slint` / GUI 装配） | 任意变更行 + `tests/gui_flow.rs` + `scripts/gui_smoke.py` S1–S3 + 两档窗口尺寸目视检查 | 合同 C / U 分区 |
| 递归解压端到端 | `python scripts/make_tmp.py testdata --git` 生成数据集 → GUI 走「开始解压 → 一段确认 → 跑完」→ 按合同 X 分区逐条核对（成功原包回收、失败原包进「解压失败」、重跑不再重试、目录不残留压缩包）→ `恢复.ps1` 还原 → `python scripts/make_tmp.py clean` 清理 | 合同 X 分区 |
| 目录整理端到端 | `python scripts/make_tmp.py testdata --git` 自动生成数据集到 `.tmp/testdata/` → GUI 按默认配置完整走一遍目录整理主流程 → 按合同 C 分区逐条核对结果 → `恢复.ps1` 还原 → `python scripts/make_tmp.py clean` 清理 | 合同 C 分区 |
| 打包 / 发布 / 引擎捆绑 | `scripts/package-windows.ps1` 全程 + 干净目录解包运行 | 合同 E 分区 |

- 单命令入口：`powershell -NoProfile -File .\scripts\acceptance.ps1`（可选 `-WithEngine` / `-WithGuiSmoke -GuiData <目录>` / `-WithPackage`）。
- 改动过跟踪文件后，提交前须最后用 `git -c core.quotePath=false ls-files -z | grep -zv '^SHA256SUMS.txt$' | xargs -0 sha256sum -b > SHA256SUMS.txt` 重建清单（static_check 会校验其完整性）。
- **需求 ↔ 测试映射**：验证合同条目的测试 `MUST` 在其文档注释中标明合同编号（如 `// 覆盖 C-12`）；每个合同条目至少被一个测试引用。（现状：现有测试尚未标注合同编号，标注与执法均待落地）
- 合同条目的拆分、合并或重编号 `MUST` 经用户确认。

## 4. 代码与界面实现约定

产品行为类要求（控件分工、进度语义、无障碍、窗口行为、缩放适配、配色令牌、命名规范）一律以 `docs/CONTRACT.md` 的 U / P 分区为准，本节不重复。本节只写实现层约定：

- 注释、错误信息、界面文案用中文；标识符、模块名、提交信息用英文或中英混排均可，但同一处保持一致。
- 界面颜色走 `ui/app.slint` 的 `Design` 全局（浅色/深色两套由 `Design.dark` 切换），不硬编码与主题冲突的颜色。
- Slint 布局中 `MUST NOT` 用 `root.width` / `parent.width` 绑定子项自身宽度（会形成绑定环，编译期警告、运行期可能 panic）；固定宽度用常量，占满剩余空间用 `horizontal-stretch` 或外层容器。
- 含 `TouchArea` 的自绘控件 `MUST` 带 `accessible-role` 与 `accessible-label`（读屏与自动化测试依赖）。
- 本节颜色 / 布局绑定 / 无障碍规则由 `python scripts/static_check.py` 机器检查（十六进制色只允许出现在 `Design` 全局内、布局内禁止 `root/parent.width` 宽度绑定、含 `TouchArea` 的组件必须带 `accessible-role`）。

## 5. 改名与新工具时的同步清单

产品名、二进制名、图标、状态目录等一旦调整，`MUST` 同步以下位置：

`Cargo.toml`（package/bin 名）· `build.rs`（链接参数与图标资源）· `ui/app.slint`（标题、品牌、关于页）· `src/config.rs`（状态目录）· `src/registry.rs`（工具名）· `scripts/*`（打包、启动、检查脚本）· `.github/workflows/check.yml` · `.github/workflows/release.yml` · `resources/windows.manifest` · `README.md` / `先读我.txt` · `AGENTS.md` §1 与 `docs/CONTRACT.md`（P 分区命名与定位）· `SHA256SUMS.txt`（用 `sha256sum -b` 重新生成并逐字节复核）。

## 6. 安全底线

安全需求（不覆盖语义、删除策略、不上传等）以 `docs/CONTRACT.md` 的 S 分区与 P-03 为准，本节不重复。代理执行纪律：

- 处理真实用户目录前 `SHOULD` 先用副本验证；未经用户亲自验收前 `MUST NOT` 声称可用于生产使用。

## 7. 用户协作协议（本项目用户零创作）

- 代理 `MUST NOT` 要求用户编写代码、文字或文档，`MUST NOT` 让用户阅读代码 diff；需要用户输入时 `MUST` 转换为以下形式之一：附推荐项的封闭选择题、新旧并排截图的是/否判断、一条可直接复制运行的命令。
- 新需求 `MUST` 先复述为可验收的行为断言（进入 `docs/CONTRACT.md` 草案，见第 8 节），经用户逐条确认后才可动工；影响用户可见行为的歧义 `MUST` 先以封闭问题澄清，`MUST NOT` 默认假设。
- 向用户报告结果 `MUST` 先给结论（通过 / 失败 / 受阻），证据（命令、退出码、关键输出行）附后；未验证的事项 `MUST NOT` 表述为已完成。
- 本节为行为协议，无法机检，靠会话纪律与用户在交互中纠偏执行。

## 8. 需求合同（docs/CONTRACT.md）

- `docs/CONTRACT.md` 是用户意图的唯一权威载体：每行一个编号行为断言，**只写需求、不写实现状态**；「功能是否正常」以合同覆盖为准，不以代理单方面理解为准。
- 用户可见行为变更 `MUST` 对应至少一行合同；合同行数单调不减；增改 `MUST` 经用户确认并记录于提交信息，`MUST NOT` 由代理单方面增删。
- 代码与合同不一致时 `MUST` 以合同为准、按缺陷流程修代码（先红后绿，见 3.2）；`MUST NOT` 为迁就代码现状而改写、削弱或删除合同条目。
- 合同条目与测试的映射规则见 3.3（测试注释标合同编号；矩阵检查待落地）。

## 9. 可信基与防共谋

- 可信基文件：`AGENTS.md`、`scripts/static_check.py`、`scripts/test-baseline.json`、`scripts/acceptance.ps1`、`scripts/gui_smoke.py`、`scripts/make_tmp.py`、`docs/CONTRACT.md`。（`resources/rules.json` 与 `src/config.rs` 结构上由 static_check 的 config_schema 检查互锁，且加规则时二者本就合法同变，不列入可信基。）
- 任何变更 `MUST NOT` 同时修改产品代码（`src/`、`ui/`、`resources/`）与可信基文件；可信基变更 `MUST` 独立提交、提交信息注明理由并获用户批准。（执法：CI 按 diff 文件清单检测混合提交 · 待落地）
- 向本文件新增 `MUST` 级条款时，同一变更 `MUST` 落地对应执法脚本检查，否则该条 `MUST` 显式标注「无执法点 · 软法」；`SHOULD` 每月审计一次无执法点的条款，补齐执法或降级措辞。

## 10. 基线保护

- `scripts/test-baseline.json` 的再生成 `MUST` 先以新旧并排提交用户做是/否判断，对应提交信息 `MUST` 带 `[基线已确认]` 标记；`MUST NOT` 静默更新后直接提交。（执法：脚本校验提交标记 · 待落地）
