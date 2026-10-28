# AGENTS.md · snap-ocr-worker（截图 OCR 后台进程）

本文件只承载本子树（`optional/snap-ocr-worker/`）专属的职责、入口、约定与陷阱；全局开发纪律、测试门、`.tmp/` 规则与需求权威分工见仓库根 [AGENTS.md](../../AGENTS.md)，不在本文件重复。产品需求以 `openspec/specs/` 的已批准原生规格为准：截图交互、结果窗与隐私见 [openspec/specs/screenshot-ocr/spec.md](../../openspec/specs/screenshot-ocr/spec.md)（O 分区），推理承接、共享进程与生命周期见 [openspec/specs/shared-inference/spec.md](../../openspec/specs/shared-inference/spec.md)（XB 分区），诊断日志见合同 P-10；本文件不复制需求条目。

## 职责

截图 OCR 的当前用户会话后台进程（XB-25 随主包交付，O-11 不随主界面关闭而退出）：Win32 托盘与全局热键、GDI 截图与冻结框选、Slint 结果窗/设置窗、命名管道单实例服务。识别不自研，全部经 Xberg 推理承接。

| 模块 | 职责 |
|---|---|
| `src/service.rs` | 服务主体：单实例、模型生命周期/重载、settings.json、注册表开机自启、错误分类（O-30） |
| `src/service/protocol.rs` | 当前用户会话专属命名管道，只传状态/控制消息，不传截图或文字 |
| `src/service/tray.rs` | 原生 Win32 消息循环：托盘图标、全局热键、冻结框选窗（独立于 Slint） |
| `src/capture_win.rs` | 鼠标所在显示器物理像素 GDI 截图；内存 BGR→PNG，不落盘（O-29） |
| `src/result_window.rs` | Slint 结果窗/设置窗（`ui/result.slint`，O-21/O-22） |
| `src/shared_xberg.rs` | 共享引擎客户端（经主包 runtime 代理发 `ocr_snapshot` / `snapshot_state`）；错误与模型状态类型（`ClientError`/`SnapshotState`）也定义在此 |

**识别路径注意**：服务的识别走 `SharedXbergClient`（XB-14 唯一共享引擎）。旧直连 `xberg worker` 子进程的完整路径（`src/xberg_worker.rs` 客户端、`examples/xberg_ocr.rs` 无头对照、`tests/xberg_client.rs` 协议测试、`src/bin/mock-xberg-worker.rs` 测试桩）已于 2026-10-04 经用户确认删除，本 crate 内不存在第二条引擎路径。

## 入口与命令

以下命令从仓库根目录执行，需要根文档所列 Windows Rust 构建条件。本包是 workspace 非默认成员，默认根构建/测试不覆盖；本地 `acceptance.ps1` 显式执行测试与生产目标 lint，CI 不运行功能测试。独立测试前先在同一 target 目录构建带 `test-hooks` 的 worker EXE，生命周期测试会校验其能力握手，库测试产物不能替代它。列出命令不表示本次已验证：

```powershell
cargo build --manifest-path optional/snap-ocr-worker/Cargo.toml --bin snap-ocr-worker --features test-hooks
cargo test --manifest-path optional/snap-ocr-worker/Cargo.toml --all-targets --features test-hooks
cargo clippy --manifest-path optional/snap-ocr-worker/Cargo.toml --lib --bin snap-ocr-worker --features test-hooks -- -D warnings
```

- `src/main.rs` 只接受 `--capabilities`（版本握手，打印协议版本 JSON）、`--xberg-broker`（经主程序代理的内部入口）、`--service` / `--service--autostart`（后台服务）；无参启动退出码 2 并提示从主界面启动。bin 目标仅 `snap-ocr-worker`（产品）。
- 已有 `test-hooks` 隔离生命周期测试，但不等同于热键/托盘到结果窗的真实桌面 E2E、多 DPI 或真实模型常驻验证；这些边界未执行时仍如实标注未验证。

## 与主包的耦合边界（改动须双端同步）

- 真实 crate 级共享：本服务依赖 workspace 的 `jchtools-foundation` 与 `jchtools-xberg`，主程序与 jchocr 使用同一组基础/推理实现。日志与设置在 `crates/foundation/src/{logging,xberg_settings}.rs`，runtime 在 `crates/xberg/src/{xberg_runtime,xberg_runtime_windows}.rs`；worker 的 `src/lib.rs` 不再通过 `#[path]` 重复编译主包源文件。测试覆盖特性经依赖显式传播，生产包不启用 `test-hooks`。
- 运行期产物依赖（主包侧）：`crates/assets/src/snap_ocr_assets/manifest.rs` 定义 `WORKER_EXE_NAME`，同目录 `readiness.rs` 承接主包管道名；`crates/gui/src/gui.rs` 以 `--service` 启动本 EXE，`crates/gui/src/gui/snap_ocr.rs` 承接 GUI 管道命令与热键映射。`crates/assets/build.rs` 嵌入根 `resources/snap-ocr-assets.json` 或 `JCHTOOLS_SNAP_OCR_MANIFEST` 指定的真实清单；worker 条目的 size/sha256 由打包脚本按当前构建字节回填，worker 本体不嵌入主 EXE。
- 语义耦合点（两端无共享类型，靠纪律与静态检查防漂移）：命名管道名 `\\.\pipe\jchtools-snap-ocr-<hash>`（服务端在 `src/service/protocol.rs`）；`--capabilities` 版本握手 JSON；`crates/gui/src/gui/snap_ocr.rs` 发出的管道命令与服务端分发表——由 `scripts/static_check.py` 的 `snap_pipe_command_sync` 执法双端同步；热键映射表两侧逐对一致。
- 产品改名/交付同步清单见根 AGENTS.md 第 5 节，不在此重复。

## 本地约定与陷阱

- 仅 Windows（`cfg(windows)` 贯穿服务、截图与托盘）；Slint 锁 `=1.18.1`，与主程序同版本。
- 界面与错误文案中文；错误按 O-30 分类并去敏。截图的隐私边界唯一见 O-29；实现使用内存图像经 base64 传共享引擎，Xberg 子进程 stderr 丢弃，不进日志。
- 识别经共享 Xberg 运行时代理（XB-14 唯一共享进程，不再直连子进程）；取消、超时与进程退出的语义边界见根仓 `crates/xberg/src/xberg_runtime.rs` 与 XB 分区需求，服务侧按 `ClientError` 分类处置并触发重载，不自动重试（XB-08）。
- 测试专用环境变量与常量：管道前缀 `jchtools-snap-ocr-test-`（测试绝不触碰真实管道与 launcher.json）、`JCHTOOLS_SNAP_OCR_TEST_ASSET_ROOT`（测试资产根覆盖）。管道及单实例身份复用 foundation 从当前主 Token 取得的用户 SID 与 SessionId；读取失败返回错误，不回退继承的 `USERNAME` / `USERPROFILE`。
- 日志通过 `jchtools-foundation` 共用 `crates/foundation/src/logging.rs`，记录与保留边界唯一见合同 P-10；修改共享模块时须同时核对主程序、jchocr 和服务调用。
- lint 硬门禁在本 crate `[lints]` 独立声明：`warnings`、clippy pedantic 及 `unwrap_used` / `expect_used` / `dbg_macro` / `todo` / `unimplemented` 全 deny，编辑须维持零告警；本 crate 不设 `perf-tracing` 特性。foundation 在自己的 crate 内处理日志/性能特性，worker 不再维护源码复编需要的 `unexpected_cfgs` check-cfg 例外。
- 源码注释中的任务编号（O-xx / XB-xx / P-xx）用于定位对应已批准需求，改动前按编号查对应需求文档，不把实现注释当成规格本体。
