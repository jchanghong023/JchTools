# 验证记录

## 1. 本次 Windows 11 实测（2026-09-11，产品名 JchTools，原 MyTools 已重命名）

环境：Windows 11 Pro 26200，AMD Ryzen 9 9950X，屏幕 2560x1440 @150%，
Rust 1.98.0 stable `x86_64-pc-windows-msvc`，VS 2022 BuildTools（MSVC 14.44）+ Windows SDK 10.0.26100。

| 项目 | 命令 / 方式 | 结果 |
|---|---|---|
| 编译（dev） | `cargo build` | 通过：`target/debug/JchTools.exe`、`jchtools-cli.exe` |
| 编译（release） | `cargo build --release` | 通过（约 2 分钟，`lto=thin`） |
| 测试 | `cargo test` | 50 通过 / 0 失败 / 8 忽略（真实引擎用例默认 ignore） |
| 静态检查 | `python scripts/static_check.py` | 6 项 PASS，1 项 SKIP（本机 bash 启动器不可用），明细见 `static-check.json` |
| 界面 | 启动 + 像素/UIA 实测 | 无系统标题栏；自绘按钮可拖动/最大化/还原/最小化/关闭，键盘与读屏可访问 |
| 界面（重排后） | GUI 真实任务 + 无障碍树轮询 | 处理规则同屏 9 行（行高 50px，旧版 88/108px 只能显示 2~3 行）；分析阶段显示往返进度光带，执行阶段显示百分比从 6%→29% 单调增长并伴随 “224 / 3776 项” 计数；空闲时进度条隐藏 |
| 功能 | 临时目录 + `jchtools-cli` | 解压、嵌套解压、同名冲突保留两份、内容去重、版本取舍、垃圾/临时/零字节清理、空目录清理、归类、扩展名纠正、CSV 报告全部通过 |
| 性能 | release 版，5004 文件 / 521.51 MiB | 扫描 1.32s；内容去重（BLAKE3 + 逐字节复核）1.61s；峰值工作集 < 10 MiB |

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

- 随包 7-Zip 引擎：本仓库 `resources/7zip` 只有 README，解压需要 `--engine <完整 7z.exe>`，或先运行 `scripts/package-windows.ps1` 生成内嵌引擎的发布包。
- 回收站容量不足、网络盘、RAR 多版本与多卷、超大固实包、多 TB 规模、高 DPI/无 GPU 显示、并发外部修改的竞态。
- `tests/archive.rs` 的 8 个真实引擎用例仍为 `#[ignore]`；本次只用临时目录做了端到端解压验证。
- 性能数字来自本机 NVMe 与刚写入（大概率仍在系统缓存）的文件，不等于多 TB 冷启动基准。

## 3. 交付物最初在 Linux 上执行的静态检查

最初交付环境为 Linux x86_64（Debian 13），没有 rustc、cargo、Windows、PowerShell 或 7-Zip，
当时只运行了本节静态检查；上表是之后在 Windows 11 上补做的编译、运行与功能验收。

可复现命令：`python scripts/static_check.py`。

| 项目 | 结果与边界 |
|---|---|
| Cargo 清单 / Windows manifest | TOML 与 XML 解析通过，声明的二进制源码路径存在 |
| 配置与 GUI 规则 | 49 个配置字段与 49 个 UI 设置一致，类型和枚举选项检查通过 |
| GUI 回调 | 导出窗口上声明的回调均有对应 Rust 处理函数（当前 23 个） |
| Rust 词法结构 | 19 个 Rust 文件括号结构检查通过；不是 Rust 语法、类型、借用或宏编译检查 |
| SQLite | schema 可解析，46 个可具体化 DML 语句在空 schema 上 EXPLAIN 预编译通过 |
| Bash | 有 bash 时执行 `bash -n scripts/check-linux.sh`；本机无可用 bash 时记为 SKIP |
| 工程范围 | 必需文件存在，Rust 实现没有 todo!/unimplemented!；提供 59 个测试函数的源码 |

机器可读明细在同目录 `static-check.json`。

## 4. 校验清单

`SHA256SUMS.txt` 覆盖 44 个交付文件（含 `resources/app.ico`、`resources/app-icon.png`、`scripts/make-icon.py`），按二进制逐字节校验 44/44 一致。注意：Git-for-Windows 自带的 `sha256sum -c` 会以文本模式读文件，对二进制资产误报 FAILED；请用 Linux/macOS 的 `sha256sum -c`，或按二进制读取自行复核。
