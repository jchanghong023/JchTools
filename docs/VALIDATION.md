# 验证记录

## 1. 本次 Windows 11 实测（2026-09-11，产品名 JchTools，原 MyTools 已重命名）

环境：Windows 11 Pro 26200，AMD Ryzen 9 9950X，屏幕 2560x1440 @150%，
Rust 1.98.0 stable `x86_64-pc-windows-msvc`，VS 2022 BuildTools（MSVC 14.44）+ Windows SDK 10.0.26100。

| 项目 | 命令 / 方式 | 结果 |
|---|---|---|
| 编译（dev） | `cargo build` | 通过：`target/debug/JchTools.exe`、`jchtools-cli.exe` |
| 编译（release） | `cargo build --release` | 通过（约 2 分钟，`lto=thin`） |
| 测试 | `cargo test` | 50 通过 / 0 失败 / 8 忽略（真实引擎用例默认 ignore；2026-09-12 复跑为 55 通过 / 0 失败 / 14 忽略，见 §4） |
| 静态检查 | `python scripts/static_check.py` | 6 项 PASS，1 项 SKIP（本机 bash 启动器不可用）；机器可读明细现由脚本写入 `.tmp/static-check.json`（本行记录时输出在 `docs/static-check.json`，该快照已随输出位置调整删除） |
| 界面 | 启动 + 像素/UIA 实测 | 无系统标题栏；自绘按钮可拖动/最大化/还原/最小化/关闭，键盘与读屏可访问 |
| 界面（重排后） | GUI 真实任务 + 无障碍树轮询 | 处理规则同屏 9 行（行高 50px，旧版 88/108px 只能显示 2~3 行）；分析阶段显示往返进度光带，执行阶段显示百分比从 6%→29% 单调增长并伴随 “224 / 3776 项” 计数；空闲时进度条隐藏 |
| 功能 | 临时目录 + `jchtools-cli` | 解压、嵌套解压、同名冲突保留两份、内容去重、版本取舍、垃圾/临时/零字节清理、空目录清理、归类、扩展名纠正、CSV 报告全部通过 |
| 性能 | release 版，5004 文件 / 521.51 MiB | 扫描 1.32s；内容去重（BLAKE3 + 逐字节复核）1.61s；峰值工作集 < 10 MiB |
| 内嵌 7-Zip | 独立目录 + 真实 zip | 把 `jchtools-cli.exe` 单独放进 `.tmp/standalone/`（旁边没有任何引擎文件），解压成功 1 包 / 产生 2 个文件；引擎被释放到 `%LOCALAPPDATA%\JchTools\data\engine\26.02-83967f1b\`，两个文件的 sha256 与清单逐一相符；第二次运行 mtime 不变（未重写） |
| 引擎优先级 | 伪造本地 `resources/7zip`（内容不是真引擎、但清单哈希自洽） | 运行结果是「失败 1 包 / 错误 1 项」，证明优先使用了随包目录而不是内嵌副本（LGPL 可替换） |

窗口与外观实测：

- 窗口 1680x1080 物理像素居中于 2560x1440：左右留白各 440，上下留白各 180（对称）。
- 应用图标：EXE 资源图标与运行时窗口图标实测都等于 `resources/app.ico`/`app-icon.png` 的图案（逐像素比对一致；`WM_GETICON` 返回的 256px 图标采样点为 (63,94,230)/(30,142,232)，与源图相同）。
- 侧栏导航图标：逐行墨迹分析确认每个导航行都是「16px 图标簇（x 46..70 物理像素）+ 间隔 + 文本」，四种图形（文件夹/清单/滑杆/信息圈）都按预期绘制。
- 非客户区 0：client 与 window 同尺寸，确认没有系统标题栏；边缘仍可拖拽缩放（`resize-border-width: 6px`）。
- 文字对比度（按 WCAG 相对亮度计算）：正文/标题约 15:1，次要说明约 10.5:1，主色文字约 8.3:1，主色底白字约 6.3:1。

本次测试发现并修复的缺陷：

1. `src/process.rs`：Windows 上 7-Zip 用 CRLF 输出，`\r` 与 `\n` 各触发一次扫描，每行后多出一个空行，导致 `7z l -slt` 解析在第一个条目就中断、所有解压失败。现在 CR 之后的 LF 会被吞掉。
2. `src/archive.rs`：解压结果的隐藏/系统属性判定会一路检查到根目录之上，`%TEMP%`（位于隐藏的 `AppData` 之下）会让全部解压结果被跳过。现在只检查根目录以下的层级。
3. `ui/app.slint`：标题区与计划/设置行缺少宽度约束，长文本被挤成窄列多行（标题副标题曾折成 6 行）。已用显式宽度与横向拉伸修正。
4. 界面重绘：主色 `#4f46e5`、自绘浅色/深色两套配色、渐变页面与卡片阴影；自绘控件补齐 `accessible-role/label`。

## 2. 仍未验证

- 随包 7-Zip 引擎：`resources/7zip` 里的引擎文件不入库（`.gitignore`），需要先运行 `scripts/fetch-7zip.ps1`（联网、校验上游 SHA-256）再构建；未获取引擎时构建仍成功但**不内嵌**，此时解压需要 `--engine <完整 7z.exe>`。当前源码树的实测构建内嵌了本机引擎（26.02），正式发布应使用脚本固定的 26.03。
- 回收站容量不足、网络盘、RAR 多版本与多卷、超大固实包、多 TB 规模、高 DPI/无 GPU 显示、并发外部修改的竞态。
- `tests/archive.rs` 的 19 个真实引擎用例仍为 `#[ignore]`；本次只用临时目录做了端到端解压验证。
- 性能数字来自本机 NVMe 与刚写入（大概率仍在系统缓存）的文件，不等于多 TB 冷启动基准。

## 3. 交付物最初在 Linux 上执行的静态检查

最初交付环境为 Linux x86_64（Debian 13），没有 rustc、cargo、Windows、PowerShell 或 7-Zip，
当时只运行了本节静态检查；上表是之后在 Windows 11 上补做的编译、运行与功能验收。
下表各行数字为当时基线快照，可能低于当前基线（当前界面规则为 41 项）。

可复现命令：`python scripts/static_check.py`。

| 项目 | 结果与边界 |
|---|---|
| Cargo 清单 / Windows manifest | TOML 与 XML 解析通过，声明的二进制源码路径存在 |
| 配置与 GUI 规则 | 49 个配置字段与 49 个 UI 设置一致（当时为 49；现基线为 41 项界面规则 + 6 个隐藏字段 = 47 Config 字段），类型和枚举选项检查通过 |
| GUI 回调 | 导出窗口上声明的回调均有对应 Rust 处理函数（当时 23 个） |
| Rust 词法结构 | 19 个 Rust 文件括号结构检查通过；不是 Rust 语法、类型、借用或宏编译检查 |
| SQLite | schema 可解析，46 个可具体化 DML 语句在空 schema 上 EXPLAIN 预编译通过 |
| Bash | 有 bash 时执行 `bash -n scripts/check-linux.sh`；本机无可用 bash 时记为 SKIP |
| 工程范围 | 必需文件存在，Rust 实现没有 todo!/unimplemented!；提供 59 个测试函数的源码 |

机器可读明细由脚本运行时写入 `.tmp/static-check.json`（不再落 `docs/`，历史快照已删除）。

## 4. 2026-09-12 图形界面手工验收（`D:\testzip` 测试数据集）

环境同上；使用 `python scripts/make-testdata.py --destination D:\testzip --git` 生成的数据集
（163 个文件 / 132 MiB，16 组用例），**每次运行前都用目录里的 git 基线还原**（`git checkout -- .` + `git clean -fd` 并补回隐藏/系统属性与空目录）。
界面用 Windows UIA 真实驱动：读无障碍树定位控件、真实鼠标点击与键盘输入，不是脚本内部调用。

| 步骤 | 结果 |
|---|---|
| `python scripts/static_check.py` | 7 项 PASS（按当时脚本口径；其中 bash 语法检查实为 SKIP，并非通过） |
| `cargo test` | 55 通过 / 0 失败 / 14 忽略 |
| `JCHTOOLS_TEST_7ZIP=<repo>/resources/7zip/7z.exe cargo test --test archive -- --ignored` | 14 通过 / 0 失败（真实引擎用例） |
| `cargo build --release` + 启动 | 通过；自绘标题栏、最小化/最大化/还原、侧栏导航、四页界面均可操作 |
| 第一轮：默认规则 | 解压成功 38 包 / 失败 6 包（损坏 2、加密 1、恶意条目 2、超深 1）；计划 60 项（删除 13 / 移动 29 / 空目录复查 18）；执行后“已回收 63 项”，单项取消与空目录复查都按预期阻止了不该执行的删除；导出 CSV 124 行、任务记录可回看 |
| 第二轮：开启 冲突=询问、清理临时/备份、检测真实类型+修正扩展名、消除单链层级、包含隐藏/系统 | 逐次弹出冲突对话框并可应用到后续全部；`其实是PNG.txt`→`图片/…png`、`其实是ZIP.dat`→`压缩包/…zip`、`没有扩展名的PDF`→`…pdf`；`单链/a/b/c/payload.txt` 提升为 `文档/05-空目录与单链/payload.txt`；隐藏/系统文件被归类后**属性保持不变** |

本轮发现并修复的缺陷：

1. `src/engine.rs` / `src/archive.rs`：一个已经在本任务里被删除的同名同大小文件会让后续流式压缩包（`base.tar.gz` / `.bz2` / `.xz`）整包解压失败并报 `系统找不到指定的文件。 (os error 2)`——`delete_path` 没有把该文件标记为 `active=0`，同名同内容查找又去 `snapshot` 一个已不存在的路径。现在删除后立即失效该行，且查找到的候选文件消失时直接跳过。数据集上 07 组由「成功 9 / 失败 3」变为「成功 13 / 失败 0」，回归用例 `deleted_same_name_file_does_not_abort_a_later_stream_archive` 在修复前失败、修复后通过。
2. `src/archive.rs`：`.tgz` 的解压结果会按 gzip 头里的名字落盘，`base.tgz` 因此被写成一份名字带 `.tgz` 的 tar 流（`base (1).tgz`）。现在流式包的同名成员改用去掉一层压缩后缀的真实名字（`base.tar`），并继续走正常的冲突策略。
3. `src/process.rs`：解压/列包失败时把 7-Zip 最后 12 行输出（含 `Path = …` 之类元数据）整段拼进错误信息，用户看不到原因。现在优先取 stderr 里可读的错误行、去掉 `\\?\` 前缀并截断；没有任何可读输出时明确写「没有输出可读的错误行；压缩包可能已损坏或不完整」。
4. `src/main.rs` / `ui/app.slint`：计划行把状态显示成英文枚举值（`pending`），任务记录显示 UTC 纳秒时间戳与 `finished` 等英文状态，冲突对话框显示 `修改时间(ns)：178918…` 与内部暂存路径。现在统一为中文状态、本地时间（`2026-09-12 12:14:54`）、以及「目标 / 现有文件 / 新解压文件」的可读对照。
5. `src/main.rs` / `ui/app.slint`：导出成功、规则保存成功这类提示以前走红色错误条；现在分为错误条（红色）与提示条（蓝色）。
6. `src/main.rs`：执行阶段的状态栏仍写「扫描 0 个文件」，容易误以为没扫到东西；现在执行阶段显示「执行中：已处理 N / M 项」。
7. `ui/app.slint`：计划行复选框、规则复选框/下拉/数字输入都没有无障碍名称，读屏只报「复选框」；现在都带上规则标题或「删除 04-垃圾与临时/xxx」。数字输入框另有 Slint 1.17 上游问题：纯数字的 `accessible-value` 会被 UIA 当成数值属性，读屏读到 0，这里用 `accessible-description: "当前值 N"` 缓解。
8. `ui/app.slint` / `src/main.rs`：「设置与规则预设」页只列出「应用」分区的 1 条规则（应用主题），却提供导出/导入/恢复按钮；现在该页列出全部 49 条规则（当时为 49；现基线为 41）并按类型给出对应控件。
9. `src/main.rs`：规则之间的依赖（如「修正扩展名」需要先开「检测真实类型」）只在点「开始解压与分析」时才报错；现在改动规则时立即校验并提示。
10. `src/main.rs`：实时日志以前从上往下追加，界面停在最早的一行；现在最新的消息显示在最上面（导出与数据库仍保留完整时间顺序）。

第一轮未覆盖项（分页翻页、原生目录选择器、主题切换、规则导入导出、暂停/取消、关闭确认）已在 §4.1 的第二轮里补齐；回收站容量不足、网络盘、多 TB 规模、非 150% 缩放显示仍未覆盖。

### 4.1 同日第二轮（继续用真实输入驱动界面）

在第一轮基础上把「手工测试集临时扩充到 211 个文件」以制造超过一页的计划，并补测了此前没碰过的流程：

| 流程 | 结果 |
|---|---|
| 原生目录选择器 | 从已输入的目录开始，选完回填主界面并提示「规则或目录已改变」 |
| 主题 | 跟随系统 / 浅色 / 深色三态切换，主界面、设置页、日志面板配色同步 |
| 窗口 | 最大化 / 还原正常，最大化后布局拉伸正确 |
| 规则导出 → 恢复内置默认 → 导入 | 导出到桌面、导入后配置生效（主题、冲突策略、线程数都对上） |
| 计划分页（122 项） | 第 1/2 页内容不重复，首末页按钮状态正确，切筛选回到第 1 页 |
| 暂停 / 取消 | 暂停后按钮变「继续」且任务真的停在原处；取消后不再执行后续操作 |
| 关闭确认 | 任务运行中点关闭会先确认；该确认框盖在解压冲突对话框之上 |

本轮发现并修复：

1. `src/engine.rs`：用户主动取消的*分析*阶段任务被写成 `failed`，任务记录里显示「失败」；现在写 `cancelled` 并记「已取消」，回归用例 `user_cancelled_analysis_records_cancelled_status` 修复前失败、修复后通过。
2. `src/main.rs`：取消任务用红色错误条报告「任务已取消」；现在按中性提示条展示，状态写作「任务已取消；已完成的操作不会自动回滚」。
3. `ui/app.slint`：任务运行中收到关闭请求时，确认框画在冲突对话框*下面*，用户只能看到一张被压住的框；现在确认层画在冲突层之后，并且关闭确认不再要求勾选「我已确认目录、规则及可能的永久删除行为」（按钮直接是「停止并关闭」）。
4. `ui/app.slint` / `src/main.rs`：计划「上一页/下一页」在首末页仍可点、点了没反应；现在按实际页数禁用。
5. `src/main.rs`：`选择目录…` 固定从系统上次位置开始；现在从已经输入的目录开始。
6. `src/main.rs`：导出规则、导入规则、导出报告默认落在系统上次用过的目录（测试里就是被整理的目录）；现在默认桌面（无桌面则主目录），避免把配置/报告写进被整理的树。
7. `src/main.rs`：导入规则成功后没有任何反馈；现在提示「已导入规则：<路径>」。
8. `src/main.rs`：「设置与规则预设」把「应用主题」排在 49 条规则（当时为 49；现基线为 41）的最后，改主题要滚很久；现在排在最前。
9. `ui/app.slint`：自绘的侧栏导航、面板页签、分区胶囊、窗口按钮都不在 Tab 顺序里——键盘用户无法切换页面/分区，也无法用键盘最小化或关闭窗口。现在这些控件内含 `FocusScope`，可 Tab 聚焦、空格/回车激活，并显示主色焦点框（已实测：Tab 到「任务记录」按空格切页、Tab 到「冲突」按空格切分区）。

键盘走查结果（Tab 顺序，带无障碍名称）：任务记录 → 关于 → 目录输入框 → 选择目录… → 处理规则/整理计划/进度与日志 → 六个分区胶囊 → 规则行控件（复选框/下拉/数字输入）→ 开始解压与分析。

用合成输入无法验证的项（需要真实鼠标移动/双击时序，工具限制，非应用缺陷）：自绘标题栏的拖动移动窗口与双击条带最大化——这两项在 §1 的上一轮真实操作中已验证过；窗口缩到最小尺寸（960x620）的布局未在本轮复测。

## 5. 校验清单

当前基线（2026-09-13 多轮评审后，删除「任务记录」页与「设置与规则预设」页后同步）：`cargo test` 135 通过 / 0 失败 / 19 忽略（基线数字以最近一次 `cargo test` 实测为准）；真实引擎用例 19/19 通过（JCHTOOLS_TEST_7ZIP 实测）；`static_check.py` 7 项检查（有 bash 时 7 PASS；无 bash 的 Windows 主机为 6 PASS + 1 SKIP shell_syntax）：41 项界面规则 + 6 个隐藏字段 = 47 Config 字段、32 回调、56 DML、90 测试函数；新增覆盖：同名重复条目 zip（-aou 自动改名两份保留）、GBK 文件名 zip（内容完整落盘）、空 zip / 仅目录条目 7z（合法空包处理）、手工构造合法 zstd 帧解压；config.json 覆盖写、CSV 公式注入转义、分卷识别谓词；GUI 层新增 7 个无头状态测试（Slint 测试后端：初始规则面、分区切换、非法输入回退、主题不失效计划、目录校验、工具搜索、导航）与 1 个计划执行确认流端到端测试（真实回调+真实引擎线程走完整确认流）。以上各节为历次验证记录，其中数字为当次快照，可能低于当前基线。

`SHA256SUMS.txt` 覆盖除自身外的全部 git 跟踪交付文件（含 `resources/app.ico`、`resources/app-icon.png`、`scripts/make-icon.py` 与 CI workflow），以 `sha256sum -b` 二进制模式生成；改动任何被覆盖文件后必须重新生成并整单复核（`sha256sum -c SHA256SUMS.txt` 应全部 OK）。

## 6. 2026-09-14 测试验收体系加固（对抗 AI 代理自证偏差）

背景：本仓库代码与测试均由 AI 代理产出。本轮按「验收纪律」评审结论落地以下机制，全部在本机（Windows 11，Rust stable MSVC）实测。

新增机制与实测结果：

| 机制 | 落点 | 实测 |
|---|---|---|
| 测试基线门禁（防删测试/放宽断言/悄悄加 ignore 或平台门禁） | `scripts/static_check.py` 新增 `test_baseline` 检查 + `scripts/test-baseline.json`（189 项，含 ignore 与 cfg 状态）+ `--update-test-baseline` | PASS；本轮新增 8 个属性测试时门禁先失败、重新生成基线后通过（门禁生效的直接证据） |
| Slint 界面规则机器检查（AGENTS.md 第 4 节） | `static_check.py` 新增 `slint_layout_width`（布局内禁止 root/parent.width 宽度绑定）、`slint_colors`（十六进制色只许在 Design 全局内）、`slint_accessibility`（含 TouchArea 的组件必须带 accessible-role） | 全部 PASS，对现有代码零误报（检查为结构感知：绝对定位下的填充用法不受影响） |
| 产品名一致性 | `static_check.py` 新增 `product_naming`（标题/状态目录=Cargo 清单=JchTools，无 MyTools 残留） | PASS |
| 属性测试（proptest 1.11，随机输入上的需求不变式） | `tests/property.rs`（6 项：safe_relative 不逃逸根目录、绝对路径与 `..` 拒绝、validate_component Windows 兼容不变式、尾随/注入字符拒绝、COM/LPT 保留名边界、CSV 公式注入转义逐格核对）+ `src/process.rs`（2 项：任意 \r\n/\n/\r 混排分行保真、任意字节不 panic） | 6 + 2 全部通过（每项 256 cases） |
| 单命令验收 | `scripts/acceptance.ps1`（static_check → cargo test → binding loop 扫描；可选 -WithEngine / -WithGuiSmoke -GuiData / -WithPackage；日志落 `.tmp/acceptance/`） | `acceptance.ps1 -WithEngine` 全绿：static-check、cargo-test、binding-loop-scan、engine-tests 均 PASS，未选阶段显式 NOT RUN |
| 变异测试（cargo-mutants） | `.github/workflows/mutants.yml`（每周一 03:00 UTC + 手动；范围 `src/engine.rs`、`src/fsutil.rs`；存活预算 SURVIVOR_BUDGET=12，超即失败；archive.rs 因真实引擎用例默认 ignore 会假存活而排除） | 工作流已创建，**尚未首次运行**（需 CI 环境；首次运行后按存活报告补测试并下调预算） |
| CI 补强 | `check.yml` windows job 新增 `python scripts/static_check.py` 步骤 | 待下次 push 验证 |
| 验收清单状态化 | `docs/ACCEPTANCE.md` 改为编号 + 状态（自动化/半自动/需人工/未执行）+ 证据锚点表；「需人工」条目 AI 代理不得宣称通过 | 完成 |
| AGENTS.md 3.2 测试验收纪律 | 红测先行、禁削弱测试、DoD 矩阵、独立复核、flaky 禁止重跑到绿、VALIDATION 只追加、人工项不得代验 | 完成 |

本轮全量数字：`cargo test --all-targets` 167 通过 / 0 失败 / 20 忽略（lib 83、core 76、property 6、gui_flow 1、bins 1）；真实引擎用例 20/20 通过（`JCHTOOLS_TEST_7ZIP=resources/7zip/7z.exe`，本机引擎 26.02）；`static_check.py` 12 项检查全 PASS（有 bash）；测试基线 189 项一致。

过程中的假阳性排查记录（保留供后续参照）：

1. `pump_splits_on_any_line_ending_combination` 首版失败两次，均为**测试生成器歧义而非产品缺陷**：(a) 空 字节流与「单个空行」字节相同，pump 返回 0 行是标准语义；(b)「\r 分隔 + 空行 + \n 分隔」拼接成一个 CRLF（单终止符），是 CRLF 修复合并语义的正确结果。已通过「最后一段非空 + 排除跨空行合并组合」把生成器约束到无歧义区间。
2. `validate_component_rejects_trailing_and_injected_chars` 首版因测试自身把字符序号当字节索引用 `String::insert` 而 panic，与实现无关，已修正为字节边界换算。

仍未验证：mutants 工作流未实际运行（首次运行后需校准存活预算）；`-WithGuiSmoke` / `-WithPackage` 路径本轮未触发（需要 pywinauto 会话与发布打包，留待下一次 GUI/发布变更时执行）。

## 7. 2026-09-14 持续专家团审查 Session 2 · Round 1（基线 98ca7c5）

范围：全仓库三分片并行初审（引擎与文件安全 / 数据规划网络 CLI / GUI 与界面规则），独立只读交叉复审 + 增量复核。

修复与改动（未提交工作区）：

1. `src/archive.rs` `normalize_new_member_attributes`：返回 `bool`；属性剥离未生效时调用方置 `complete=false` + 原包强制保留 + 日志「保留」（修复前失败被 `let _` 吞掉、`complete` 仍为 true，310 行会删除原压缩包，成员沦为扫描不可见的影子文件）。前缀拼接三分支处理 verbatim / UNC / 普通路径；无可剥离属性时早退；修正「剥离失败不影响本次解压结果」的失实注释。
2. `tests/core.rs`：为既有 `#[cfg(unix)]` 符号链接门禁补原因注释（AGENTS.md 3.2 要求）。
3. 新增回归测试 2 个：`tests/archive.rs::long_path_hidden_member_is_stripped_and_archive_completes`（管线级行为锚点，Windows + 真实引擎门控）与 `src/archive.rs::tests::normalize_strips_hidden_on_plain_long_path`（非 verbatim >260 普通路径直测锚点）；`test-baseline.json` 189→191 纯新增。

反证证据（红测先行）：把 `normalize` 函数体临时替换为修复前等价形态（裸路径 `SetFileAttributesW` + 吞错 + 记成功）后，直测锚点在本机（Windows 11 26200，LongPathsEnabled=0，rustc 1.98.0）失败：`assertion left == right failed: 隐藏属性必须被剥离  left: 2  right: 0`（`.tmp/expert-review/s2-r1-unit-red.log`，复现命令 `cargo test --lib normalize_strips_hidden_on_plain_long_path`）；恢复实现后同测试通过（`s2-r1-unit-green.log`）。python 独立探针（`.tmp/expert-review/probe_setattr.py`）：378 字符路径 verbatim 形式 `SetFileAttributesW` ret=1 剥离成功、裸形式 ret=0 err=3（ERROR_PATH_NOT_FOUND），机制与修复方向一致。

裁定记录（初审 A-1 部分证伪）：初审报告的「LongPathsEnabled=0 机器上 >260 字符成员剥离必失败」在产品管线中不可达——`Job.root` 经 `fsutil::normalize_root` 的 `fs::canonicalize`（Windows 恒返回 verbatim 路径），管线内该调用实际总是收到 `\?\` 路径（调试实证进入函数的路径 first4=[92,92,63,92]；集成测试在修复前即通过）。保留并修复的是残留问题：剥离失败（ACL 拒绝/杀软锁/TOCTOU 等任意成因）静默吞错且方向不保守，与同文件 207/278 行等影子内容路径「complete=false + 原包保留」的既定口径矛盾。修复后跨轮行为：叶子成员命中隐藏门走「跳过」稳定不动点；嵌套归档每轮重走「合入→剥离失败→保留」，稳定重复、无数据风险。

交叉复审：首轮 fail（指出集成测试注释与裁定矛盾、缺能在修复前失败的直测锚点）→ 按意见整改（注释改写为管线级锚点定位 + 新增直测锚点 + 调用点注释收敛性表述修正）→ 增量复核 pass。

本轮全量数字：`cargo test --all-targets` 169 通过 / 0 失败 / 21 忽略（lib 84、core 76、property 6、gui_flow 1、bins 2）；真实引擎用例 21/21 通过（`JCHTOOLS_TEST_7ZIP=resources/7zip/7z.exe`，引擎 26.02）；`acceptance.ps1 -WithEngine -WithGuiSmoke -GuiData .tmp/gui-data` 全绿（static-check、cargo-test、binding-loop-scan、engine-tests、gui-smoke S1-S3 全 PASS，package 显式 NOT RUN）；`static_check.py` 12 项检查全 PASS；测试基线 191 项一致。

初审 B/C 分片与 A 分片疑点共 7 项按「只处理有证据的功能性问题」口径记录不修（进度分母失败回退旧值、模态遮罩实例无 accessible-role、gui_smoke 关闭按钮歧义、gui_flow 死变量、equal_bytes 错误上抛整包失败方向保守、nettest 过时注释、LIMIT 8 已记录设计）。

## 8. 2026-09-14 持续专家团审查 Session 2 · Round 2（基线 = 98ca7c5 + Round 1 未提交修复）

范围：全范围重审（3 个全新上下文分片）。确认并修复 2 项初审缺陷；交叉复审另捕获并修复 1 项修复本身引入的高危回归。

修复与改动：

1. `src/planner.rs`（中级）：`cleanup_delete=Keep` 的清理命中文件此前只被防「充当去重 keeper」，作为重复项时无守卫——同组 keeper 先注册（本文件按 keep_duplicate 排序在后）即被按 `duplicate_delete` 删除，结果随排序翻转，违反 rules.json「清理方式=保留：不生成任何清理操作」的承诺，且与冲突路径既有防护（跳过清理命中文件）不一致。修复：去重 keeper 分支在 remove_candidate 前加 `cleanup_reason` 守卫（跳过 + 日志）。反证：守卫临时短路 → 新测试 `cleanup_keep_files_are_not_dedup_deletions` RED（`.tmp/expert-review/s2-r2-dedup-red.log`：清理保留的文件不得按重复规则删除；normal.txt mtime 20 经 `ordering_sql(Newest)="mtime DESC"` 先注册 keeper）。
2. `src/gui.rs` + `src/control.rs`（低级）：`Event::WslDistros` 此前无请求归属，处理只按「当前 scope==1」判定——「刷新在途→切走→切回（自动发起第二次拉取）」后晚到的旧列表会误清新请求的 busy，测试进行中重新放行并发操作。修复：事件载荷增加请求代际，`State.wsl_fetch_gen` 两处签发点递增，处理收敛到纯函数 `wsl_distros_event_accepted(gen,latest,scope)`（与 PlanPage/NetTestReport 归属守卫同策略），新增无头测试。
3. 交叉复审捕获（高危，修复 #2 引入）：切页签发点把 sender 提取留在 `if let Some(sender)=state.borrow()...` 的 scrutinee 里，edition 2021 下 `Ref` 临时存活到整个 if-let 语句结束，块内 `state.borrow_mut()` 递增代际必然 `BorrowMutError` panic——生产 GUI 首次切入 WSL 页签即崩；无头测试因 `proxy_events=None` 进不了该块，GUI 冒烟也不点该页签，常规验证全绿属盲区。修复：普通 `let` 提取（Ref 在语句末释放）后再 if-let。反证：临时恢复缺陷模式 → 新测试 `wsl_scope_switch_with_sender_bumps_gen_and_sets_busy` RED（`.tmp/expert-review/s2-r2-borrow-red.log`："RefCell already borrowed"，与复审推断一致）；恢复后 GREEN。该测试注入真实 channel（后台线程真实探测 wsl，发送失败有 `let _=` 兜底，不阻塞测试）。

交叉复审：首轮 fail（C 项借用冲突）→ 按建议修复 + 补测试 → 增量复核 pass。

本轮全量数字：`cargo test --all-targets` 171 通过 / 0 失败 / 21 忽略（lib 86、core 77、property 6、gui_flow 1、bins 2）；真实引擎用例 21/21；`acceptance.ps1 -WithEngine -WithGuiSmoke` 全绿（static-check、cargo-test、binding-loop-scan、engine-tests、gui-smoke 全 PASS；首轮验收因其后又发生代码修改而作废重跑）；`static_check.py` 12 项全 PASS；测试基线 194 项。

初审记录不修（按「只处理有证据的功能性问题」口径）：A 分片 find_identical_elsewhere 候选窗口 LIMIT 32（方向保守）；B 分片 conflict_groups SQL lower() ASCII 折叠（漏删方向保守）、has_unscanned_content 吞 walkdir 错误（执行期实空复查自保）、is_plausible_ip 前导零（展示）、gui keep_duplicate 非默认值显示 index 0（展示）、busybox date %N 优雅降级（可达性判定不受影响）；C 分片 NetTestFailed 无归属（仅 panic 可达，建议后续统一代际）、confirm-kind=3 模态滞留（两出口闭环）、伪 PNG 测试数据（魔数嗅探用例有效）、文本规则字段无就地回退（错误条已提示）。

## 9. 2026-09-14 持续专家团审查 Session 2 · Round 3（基线 = 98ca7c5 + R1/R2 未提交修复）

范围：全范围重审（3 个全新上下文分片）。确认并修复 1 项高级缺陷；B 分片 1 项低级发现按口径记录不修。

修复与改动（全部在 src/gui.rs）：

1. `Event::Failed` 收尾重载（高级，预先存在）：失败/取消收尾块把 `task` 提取放在 `if let Some(task)=state.borrow().task.clone()` 的 scrutinee 里，edition 2021 下该 `Ref` 临时存活到整个 if-let 语句结束，块内 `state.borrow_mut()` 更新 planned 时必然 `BorrowMutError` panic——本会话已有任务后，用户在执行中点「取消」（最常见操作）或任何引擎致命错误都会直接杀死整个应用，任务库停在中间态。该分支此前无任何测试覆盖（gui_flow 只驱动 happy path，gui_tests 无事件泵级用例），故长期存活。修复：块提取为模块函数 `reload_after_failed`，task 先普通 let 提取再 if-let（与 R2 的 WSL 切页修复同款手法），事件泵改为调用该函数。反证：函数体临时写回 scrutinee 借用模式 → 新测试 `failed_reload_updates_summary_without_reborrow_panic` RED（`.tmp/expert-review/s2-r3-failed-red.log`："RefCell already borrowed"）；修复后 GREEN。测试用真实 Database（3 个 Delete 动作 + set("summary")，与 engine.rs:96/129/316 同键同类型）驱动。

裁定记录（不修）：B 分片低级发现——「重复文件删除方式=保留」的规则表文案承诺与 hardlink 模式行为冲突（remove_candidate 对 hardlink 显式绕过 Keep 短路，代码注释表明系有意设计：硬链接不销毁内容、计划可预览可取消勾选），属设置语义与文案口径的真实出入但无数据后果，按「只处理有证据的功能性问题」口径记录。A 分片 4 项与 B/C 分片疑点共 10 项均记录不修（跳过计数 Kept 双计为展示口径、MoveRecycle 未覆写 bin_count 属测试基建、多卷包比例上限按主卷计算默认值下无影响、只读属性不剥导致永久删除路径 loud 失败无静默丢失、Unix 下 name 小写化使「相同名称」实为折叠匹配、moves 落点依赖取消勾选时执行期安全失败、successful_recycle_not_permanent 门禁注释、proxy 环境变量大小写合并顺序、Failed 分支 5 处残余 scrutinee 借用现均无块内 borrow_mut——交叉复审独立核实，防患建议已记录）。

交叉复审：A–F 全部 pass（逐行等价性、借用安全、行为一致性、测试有效性、基线纯新增、6 处残余 scrutinee 独立核实无第 7 处遗漏）。

本轮全量数字：`cargo test --all-targets` 172 通过 / 0 失败 / 21 忽略（lib 87、core 77、property 6、gui_flow 1、bins 2）；真实引擎用例 21/21；`acceptance.ps1 -WithEngine -WithGuiSmoke` 全绿；`static_check.py` 12 项全 PASS；测试基线 195 项。

## 10. 2026-09-14 持续专家团审查 Session 2 · Round 4（基线 = 98ca7c5 + R1-R3 未提交修复）

范围：全范围重审（3 个全新上下文分片）。A、B 分片 0 功能性发现（其嫌疑候选全部源码证伪或落入口径外记录）；C 分片 1 项低级发现并已修复。

修复与改动（scripts/make-testdata.py）：

`--git` 模式的第二次提交（README 生成后）未带内联 git 身份，与 baseline 提交（刻意 `-c user.email=test@local -c user.name=testdata` 以摆脱机器配置依赖）不对称——无全局/系统 git 身份的机器（干净 CI、容器）上整个语料生成完才以 "Author identity unknown" 失败，且 `_测试说明.md` 停留未提交态，随后 `恢复.ps1` 的 `git clean -fd` 会把它删掉，违背该注释的设计目标。修复：第二次提交补齐与 baseline 同款的 `-c core.autocrlf=false -c user.email -c user.name`（`git add -A` 保留）。

反证机制演示（`.tmp/expert-review/gitid-demo`，用 `GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_SYSTEM=/dev/null` 模拟无身份机器）：旧命令形态 `git commit` 失败 "Author identity unknown"；新命令形态提交成功，reflog 显示 author/committer 均为 `testdata <test@local>` 且仓库 config 无 user.*。端到端跑完整语料生成未执行（耗时长；身份解析机制与提交命令同路径，由 `git var GIT_AUTHOR_IDENT` 交叉复核佐证）。

交叉复审：A-D 全部 pass（diff 唯一 hunk 且 add -A 保留、`git -C -c commit` 参数顺序实测合法、机制反证成立、`--git` 以外路径零影响、autocrlf=false 幂等无害）。

本轮全量数字：`acceptance.ps1 -WithEngine -WithGuiSmoke` 全绿（static-check、cargo-test、binding-loop-scan、engine-tests、gui-smoke 全 PASS）；`static_check.py` 12 项全 PASS。首轮验收因主代理 shell 工作目录漂移（cd 入 .tmp 后相对路径失效）未执行即失败，非产品问题，已从仓库根重跑并全绿。

初审记录不修（按口径）：A 分片——archive_members 表只写不读（疑似遗留占位）、equal-bytes 合入会剥离既存目标隐藏属性（符合「成员落位归本工具管理」口径但与字面理解有出入，建议确认）、pump 64KiB 行上限对极端超长路径（保守失败）、scan 根豁免依赖 walkdir min_depth 实现细节（建议留注释）、cleanup_part_residue 并发窗口（无数据风险）；B 分片——dedup 守卫 hardlink 组合无行为锚点、conflict 多副本版本全删（可辩护语义）、DNS 槽位 panic 理论泄漏、probe 50ms 下限越 deadline 有界、proxy 大小写合并顺序、ctrlc 注册失败静默、db.rs/planner.rs 两处注释滞后；C 分片——rfd 对话框竞态已有双保险、NetTestFailed 无归属（已记录）、二次分析中计划页显示旧行（写路径全禁用）、finished 后复选框点击有反馈、数值行无 input-type、PlanLoadFailed 无 task 归属（推演无可达误报）、make-testdata 绝对路径投喂 7z（外部行为假设）。

## 11. 2026-09-14 持续专家团审查 Session 2 · Round 5（基线 = 98ca7c5 + R1-R4 未提交修复）

范围：全范围重审（3 个全新上下文分片）。A、B 分片 0 功能性发现；C 分片 1 项中级发现并已修复（端到端红绿实证）。

修复与改动（scripts/make-testdata.py 生成的 恢复.ps1 模板）：

`--git` 语料有两次提交（baseline + README），生成的 `恢复.ps1` 向上探测仓库时用 `git log -1` 只看 HEAD 主题来匹配 `*test corpus baseline*`——HEAD 恒为 README 提交，一键还原必然 `Write-Error; exit 1`（README 宣称的一键还原图灵上不可用；手工等价命令兜底存在，故定中级）。修复：改为 `git log --pretty=%s` 全历史匹配（变量 $message→$subjects，注释说明「HEAD 永远是后者：必须全历史匹配」）。

反证（真实端到端，非机制模拟）：修复前用当时脚本生成语料到 `.tmp/gui-git-red` 并运行生成的 `恢复.ps1` → exit 1，"no JchTools test-corpus git baseline found above ..."；修复后重新生成到 `.tmp/gui-git-green` 并运行 → exit 0，"restoring ... restored to baseline:"（git clean 移除未跟踪空目录后由脚本补建，链路完整）。演示语料已清理。

交叉复审：A-D 全部 pass（PowerShell 5.1 下 `-like` 对标量/数组的两种形态均正确；.git 损坏时 $null -like 为 false 继续向上探测与原版一致；全历史匹配不违背「只认提交信息匹配本测试基线」的安全意图且误认面仅边际扩大；`_测试说明.md` 对外承诺与新实现一致；非 --git 路径零影响）。

本轮全量数字：`acceptance.ps1 -WithEngine -WithGuiSmoke` 全绿；`static_check.py` 12 项全 PASS。

初审记录不修（按口径）：A 分片——`.jchtools-link-` 前缀残留清扫无归属校验（该前缀已是事实保留命名空间，建议声明而非改删除逻辑）、超长路径回收站系统性降级为永久删除（loud、可配置 fallback，与超长路径其它支持的体验落差）；B 分片——successful_recycle 门禁注释相邻性弱、under_path/conflict 大小写折叠口径分域、手动取消勾选的腾名假设落空（执行期安全兜底）、网络超时硬编码与 CLI 失败提示按 mtime 猜目录（展示层）；C 分片——gui_smoke open_confirm 超时静默返回（失败点后移但无假 PASS）、acceptance 未预检 cargo（理论 PASS 窗口）、PlanLoadSync.completed 空转（注释与实现不符）、gui_smoke 在真实 AppData 累积任务库（耗时仅）。

## 12. 2026-09-14 持续专家团审查 Session 2 · Round 6-7 收敛确认（连续 2 个干净轮次，审查通过）

Round 6（候选干净轮 1）：3 个全新上下文分片独立重审，A/B/C 均 0 功能性发现（A 分片新增 7-Zip 上游源码核验 `-ba` 下归档自述块不输出、`list()` 解析前提成立；C 分片对 gui.rs 全部 30 余处 borrow/borrow_mut 调用点逐一排查，无 if-let scrutinee 新违规）。疑点 10 项全部为「无当前可触发故障」的备案（static_check 花括号计数不感知字符串、archive_members 表只写不读、restore 探测误认面分析等），无代码修改。

Round 7（候选干净轮 2）：3 个全新上下文分片再次独立重审，A/B/C 均 0 功能性发现，且逐项确认前六轮 7 项修复在工作区正确落地、无放松或回退（normalize 契约、planner 守卫、WSL 代际、Failed 收尾、make-testdata 两处）。每轮评审均对已裁定项做「无新证据」确认而非机械跳过。

两轮期间代码状态零变化（工作区 porcelain 快照 sha256 前 16 位 b2040696d742a722 前后一致；`git diff --stat` 9 files, +376/−45；HEAD 98ca7c5）。Round 5 验收（全绿）对应当前代码状态，另按惯例复跑最终验收。

最终全量数字：`acceptance.ps1 -WithEngine -WithGuiSmoke -GuiData .tmp/gui-data` 全绿（static-check、cargo-test、binding-loop-scan、engine-tests、gui-smoke S1-S3 全 PASS，package 显式 NOT RUN——无打包类变更）；`cargo test --all-targets` 172 通过 / 0 失败 / 21 忽略；真实引擎用例 21/21；`static_check.py` 12 项全 PASS；测试基线 195 项。

## Session 2 总结

- 基线 98ca7c5，7 轮审查（R1-R5 修复轮 + R6-R7 收敛确认），最多 3 个并行子代理/轮，每轮全新上下文。
- 修复 7 项：R1 archive.rs 属性剥离契约（初判触发器部分证伪，保留保守失败方向）+ tests/core.rs 门禁注释；R2 planner.rs cleanup_keep 去重守卫（中）+ gui.rs WslDistros 请求代际（低）+ 复审捕获的切页借用冲突（高，修复引入）；R3 gui.rs Event::Failed 收尾借用冲突（高，预先存在，取消任务必崩）；R4 make-testdata --git 第二次提交身份；R5 恢复.ps1 log -1 匹配缺陷（中）。
- 每项修复均有「修复前失败」反证（4 个自动化 RED + 2 个端到端 RED + 1 个机制演示），全部经独立上下文交叉复审（其中 2 轮复审否定初版修复并驱动返工），全部通过 acceptance -WithEngine -WithGuiSmoke 全量验收。
- 新增回归测试 6 个（含 2 个真实引擎用例、3 个无头 GUI 用例），测试基线 189→195 纯新增。
- 另有 1 项 R1 初审发现被证伪（walkdir 根谓词，非本轮）、约 60 项疑点按「只处理有证据的功能性问题」口径记录在 .tmp/expert-review/state.md 供后续轮次参考。
- 全部修复在未提交工作区（无提交/推送授权）。

## 13. 2026-09-15 持续专家团审查 Session 3 · Round 1（基线 = 98ca7c5 + Session 2 全部未提交修复）

范围：全范围重审（3 个全新上下文分片）。B、C 分片 0 功能性发现；A 分片 1 项中级发现并已修复。

修复与改动（src/archive.rs）：

`find_identical_elsewhere` 的「树内已有相同内容则不落盘」快捷路径可匹配**正在解压的原压缩包自身**：原包在自身解压期间 active=1（失活只发生在 delete_path 或整个解压队列结束），quine 型自指包（成员解压字节=整包字节，rsc 式 gzip/zip quine；gzip 头还会记录与包同名的原始文件名——archive.rs 182-184 行注释自证该形态存在）的成员名+大小与原包行完全一致，逐字节确认通过后成员被判「树内已有相同内容」跳过落盘，随后原包按 archive_delete 删除——内容两处皆失（Permanent 模式即丢失；219 行日志在删除后成为假话）。修复：函数增加 `archive_rel` 参数，SQL 预筛加 `AND rel<>?3` 排除当前原包；跨包等价（成员匹配另一个尚未处理的同字节包）经推演安全——后处理的包自身会正常落盘成员，内容不灭失；merge_extracted 的 equal-bytes 分支因 collides_with_source 先行改名而不可能以原包为目标。

反证（断言级）：临时移除 SQL 排除子句 → 新单元测试 `find_identical_elsewhere_never_matches_current_archive` RED，失败消息即「等价查找不得把正在解压的原包自身当作树内副本」（`.tmp/expert-review/s3-r1-quine-red2.log`）；恢复后 GREEN。测试同时锁住快捷路径不回归：对照组 other/pack.zip（非原包的同名同字节文件）必须仍被命中。

交叉复审：A-D 全部 pass（rel 口径同源性——files.rel 与 archive_rel 同出 relative_string 且 BINARY 比较精确；跨包推演安全；equal_bytes 全仓仅三处、merge 分支被 collides_with_source 拦截无第二丢失通道；测试夹具与真实管线同构；基线 195→196 纯新增）。

本轮全量数字：`acceptance.ps1 -WithEngine -WithGuiSmoke` 全绿；`cargo test --all-targets` 173 通过 / 0 失败 / 21 忽略（lib 88、core 77、property 6、gui_flow 1、bins 2）；`static_check.py` 12 项全 PASS；测试基线 196 项。

初审记录不修（按口径）：A 分片 5 项备案（Staging 无 OWNER 崩溃残留、normalize 失败日志口径、取消时归档行已标 failed、summarize_failure 的 \?\ 抹除、LPE=1 测试局限）；B 分片 4 项（守卫顺带关闭清理保留文件的硬链接合并及日志文案、Unix 大小写折叠去重归类、DEFAULT_TIMEOUT_MS 死常量、新测试无平台门禁之确认）；C 分片 6 项（Error 事件瞬态分页文案、finished 后复选框交互取舍、切页拉取分支防御性记录、sql_syntax 字符串跳过、gui_smoke 真实 AppData 并发隔离、seen_tasks 现状）。

## 14. 2026-09-15 持续专家团审查 Session 3 · Round 2-3 收敛确认（连续 2 个干净轮次，审查通过）

Round 2（候选干净轮 1）：3 个全新上下文分片独立重审，A/B/C 均 0 功能性发现（A 分片对 quine 排除修复与 normalize 契约攻击性复核通过；C 分片对 gui.rs 全部 60 处 borrow/borrow_mut 调用点逐点核查无 edition 2021 scrutinee 新违规）。疑点 9 项全部为失败安全或展示层备案（unique_target 255 上限试错失败安全、删包失败汇总口径、取消勾选无联动提示、run_capture_oem 展示等），无代码修改。

Round 3（候选干净轮 2）：3 个全新上下文分片再次独立重审，A/B/C 均 0 功能性发现，并逐项确认全部历史修复（normalize 契约、quine 排除、cleanup_keep 守卫、WSL 代际、Failed 收尾、make-testdata 两处）在工作区正确落地、无放松或回退。疑点 13 项均为备案（clean_orphan_link_temps 命名空间声明建议、LIMIT 32 窗口回退正常落盘、翻页连点跳页为设计已知代价、NetTestFailed 防御纵深等）。

两轮期间代码状态零变化。Round 1 验收（全绿）对应当前代码状态，另按惯例复跑最终验收。

最终全量数字：`acceptance.ps1 -WithEngine -WithGuiSmoke -GuiData .tmp/gui-data` 全绿（static-check、cargo-test、binding-loop-scan、engine-tests、gui-smoke S1-S3 全 PASS，package 显式 NOT RUN）；`cargo test --all-targets` 173 通过 / 0 失败 / 21 忽略（lib 88、core 77、property 6、gui_flow 1、bins 2）；真实引擎用例 21/21；`static_check.py` 12 项全 PASS；测试基线 196 项。

## Session 3 总结

- 基线 = 98ca7c5 + Session 2 未提交修复；3 轮审查（R1 修复轮 + R2/R3 收敛确认），最多 3 个并行子代理/轮，每轮全新上下文。
- 修复 1 项：archive.rs `find_identical_elsewhere` 排除当前原包（中）——quine 型自指包（rsc 式 gzip/zip quine）经「树内已有相同内容」快捷路径双失内容；断言级 RED（s3-r1-quine-red2.log）+ 交叉复审 A-D pass + 验收全绿。
- 新增回归测试 1 个（含对照组锁快捷路径不回归），测试基线 195→196 纯新增。
- 约 15 项疑点按口径记录在 .tmp/expert-review/state.md。
- 全部改动在未提交工作区（无提交/推送授权）。
