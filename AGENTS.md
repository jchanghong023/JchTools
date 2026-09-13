# AGENTS.md · JchTools 协作约定

面向在本仓库工作的 AI 代理与协作者。规范性用语遵循 RFC 2119（`MUST` / `SHOULD` / `MAY`）。

## 1. 项目

- 产品名 **JchTools**（本地工具箱）。`目录整理` 只是当前第一个工具的名字与页面标题，`MUST NOT` 再把它当产品名写进标题、图标、包名或关于页。
- 技术栈：Rust 2021 + Slint 1.17（fluent 风格）、SQLite（rusqlite bundled）、随包 7-Zip 引擎。
- 入口：`src/main.rs`（GUI）、`src/bin/cli.rs`（CLI）、`ui/app.slint`（界面）、`resources/rules.json`（41 项界面规则）、`src/registry.rs`（工具注册表）。
- 后续新工具 `MUST` 通过 `src/registry.rs` + 真实页面接入；侧栏中段与导航 `SHOULD NOT` 写死只服务单个工具的文案或流程。

## 2. 临时文件规则（强制）

- 一切测试、验证、截图、日志、下载与中间产物 `MUST` 放在仓库根目录的 **`.tmp/`** 下，例如 `.tmp/ui-test/`、`.tmp/screenshots/run1.png`、`.tmp/bench/`。
- `.tmp/` 已被 `.gitignore` 忽略：`MUST NOT` 提交、`MUST NOT` 打进发布包、`MUST NOT` 用 `git add -f` 强加。
- 测试临时数据 `MUST NOT` 写在仓库其它位置（仓库根、`src/`、`resources/`、`docs/`、`tests/` 都不行）。
- 需要长期保留的样例数据放 `tests/`，或直接用测试内置的临时目录（`tempfile` / `std::env::temp_dir()`）。
- 校验、清理或重建 `.tmp/` 的脚本 `MAY` 直接删除该目录内容，不会影响仓库文件。

## 3. 构建与测试

```powershell
cargo build                # 开发构建（GUI + CLI）
cargo build --release      # 发布构建
cargo test                 # 单元与集成测试（真实引擎用例默认 #[ignore]）
python scripts/static_check.py      # 结构/配置/回调/SQL 静态检查
powershell -NoProfile -File .\scripts\package-windows.ps1   # 生成含 7-Zip 的发布 ZIP
```

手工测试集：`python scripts/make-testdata.py --destination D:\testzip --git`（`--git` 会建立 git 基线并生成 `恢复.ps1`，测试后可一键回到初始状态）（**会先清空目标目录**，只用于专门的测试目录；仓库内测试请指向 `.tmp/`）。

```bash
bash scripts/check-linux.sh                       # Linux 核心测试
JCHTOOLS_TEST_7ZIP=/abs/path/7zz bash scripts/check-linux.sh   # 含真实解压
```

- Windows 构建机需要 Rust `x86_64-pc-windows-msvc` + VS C++ Build Tools + Windows SDK（`rc.exe` 用于把 `resources/app.ico` 嵌入 EXE）。
- 提交前 `SHOULD` 至少跑 `cargo test` 与 `python scripts/static_check.py`，并确认构建输出没有新增 `binding loop` 警告。

## 3.1 7-Zip 引擎

- 引擎只从官方 release 获取：`scripts/fetch-7zip.ps1`（校验上游 SHA-256，固定版本，拒绝镜像）。下载产物落在 `resources/7zip/`，已被 `.gitignore` 忽略；`MUST NOT` 提交 `7z.exe`/`7z.dll`/`manifest.json`/源码包。
- 构建时 `build.rs` 会把 `resources/7zip` 里的引擎压缩后编进 EXE（`src/engine_bundle.rs` 负责释放与 sha256 校验）。缺失引擎时构建不失败，只是不带内嵌副本。
- 运行期顺序 `MUST` 保持：`<exe>/resources/7zip` → 用户数据目录下已释放的内嵌副本 → 从 EXE 释放。前两者都存在时以外部文件为准（LGPL 可替换）。
- 发布包 `MUST NOT` 含 `7z.exe`/`7z.dll`（打包脚本会检查并报错），但 `MUST` 保留 `licenses/`、`NOTICE.txt` 与上游源码压缩包。

## 4. 代码与界面约定

- 注释、错误信息、界面文案用中文；标识符、模块名、提交信息用英文或中英混排均可，但同一处保持一致。
- 界面颜色 `MUST` 走 `ui/app.slint` 的 `Design` 全局（浅色/深色两套由 `Design.dark` 切换）；不要硬编码与主题冲突的颜色。
- 控件样式分工已固定：面板切换用 `Tab`（下划线页签），规则分区用 `Pill`（实心胶囊），窗口按钮用 `CaptionButton`，进度用 `ProgressBar`。新增同类控件 `SHOULD` 复用这些组件而不是再写一套。
- Slint 布局中 `MUST NOT` 用 `root.width` / `parent.width` 绑定子项自身宽度（会形成绑定环，编译期警告、运行期可能 panic）；需要固定宽度就用常量，需要占满剩余空间用 `horizontal-stretch` 或外层容器。
- 自绘的可交互控件 `MUST` 设置 `accessible-role` 与 `accessible-label`，否则读屏与自动化测试都取不到。
- 进度语义：总量已知（执行阶段）显示百分比 + 进度条；总量未知（扫描/哈希/解压）显示不确定态光带 + 实时计数，`MUST NOT` 编造百分比。
- 窗口默认状态：主窗口启动时 `MUST` 居中显示在当前显示器中央，`MUST NOT` 默认最大化；仅在用户主动最大化或明确偏好时才以最大化启动。
- 窗口缩放适配：布局 `MUST` 适配窗口放大与缩小，控件与文案在常见尺寸范围内完整可读、不裁切、不重叠、不溢出；优先使用拉伸与 `horizontal-stretch`/`vertical-stretch`，`MUST NOT` 把关键内容写死在仅某一固定尺寸下可见的位置或宽度。

## 5. 改名与新工具时的同步清单

产品名、二进制名、图标、状态目录等一旦调整，`MUST` 同步以下位置：

`Cargo.toml`（package/bin 名）· `build.rs`（链接参数与图标资源）· `ui/app.slint`（标题、品牌、关于页）· `src/config.rs`（状态目录）· `src/registry.rs`（工具名）· `scripts/*`（打包、启动、检查脚本）· `.github/workflows/check.yml` · `resources/windows.manifest` · `README.md` / `docs/*` / `先读我.txt` · `SHA256SUMS.txt`（用 `sha256sum -b` 重新生成并逐字节复核）。

## 6. 安全底线

- `MUST NOT` 使用有覆盖语义的移动/复制替换用户文件；删除 `MUST` 走配置的回收站/永久删除策略。
- 任务状态与审计只写本机应用数据目录；`MUST NOT` 上传文件、日志或路径。
- 处理真实用户目录前 `SHOULD` 先用副本验证；`docs/ACCEPTANCE.md` 未通过前不要声称可用于生产。
