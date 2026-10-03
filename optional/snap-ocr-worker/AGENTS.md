# AGENTS.md · snap-ocr-worker（截图 OCR 后台进程）

本文件只承载本子树（`optional/snap-ocr-worker/`）专属的职责、入口、约定与陷阱；全局开发纪律、测试门、`.tmp/` 规则与需求权威分工见仓库根 [AGENTS.md](../../AGENTS.md)，不在本文件重复。产品需求以固定需求目录为准：截图交互、结果窗与隐私见 [SNAP2TEXT.md](../../docs/requirements/SNAP2TEXT.md)（O 分区），推理承接、共享进程与生命周期见 [XBERG-INFERENCE.md](../../docs/requirements/XBERG-INFERENCE.md)（XB 分区），诊断日志见合同 P-10；本文件不复制需求条目。

## 职责

截图 OCR 的当前用户会话后台进程（XB-25 随主包交付，O-11 不随主界面关闭而退出）：Win32 托盘与全局热键、GDI 截图与冻结框选、Slint 结果窗/设置窗、命名管道单实例服务。识别不自研，全部经 Xberg 推理承接。

| 模块 | 职责 |
|---|---|
| `src/service.rs` | 服务主体：单实例、模型生命周期/重载、settings.json、注册表开机自启、错误分类（O-30） |
| `src/service/protocol.rs` | 当前用户会话专属命名管道，只传状态/控制消息，不传截图或文字 |
| `src/service/tray.rs` | 原生 Win32 消息循环：托盘图标、全局热键、冻结框选窗（独立于 Slint） |
| `src/capture_win.rs` | 鼠标所在显示器物理像素 GDI 截图；内存 BGR→PNG，不落盘（O-29） |
| `src/result_window.rs` | Slint 结果窗/设置窗（`ui/result.slint`，O-21/O-22） |
| `src/shared_xberg.rs` | 共享引擎客户端（经主包 runtime 代理发 `ocr_snapshot` / `snapshot_state`） |
| `src/xberg_worker.rs` | 直接驱动 `xberg worker` 子进程的客户端（id 关联、600 秒超时、取消/超时=终止进程、JobObject kill-on-close） |
| `src/bin/mock-xberg-worker.rs` | 协议测试桩，仅供 `tests/xberg_client.rs` 经 `CARGO_BIN_EXE_*` 引用 |

**识别路径注意**：服务的实际识别走 `SharedXbergClient`（XB-14 唯一共享引擎）；`service.rs` 内以 `use crate::shared_xberg::SharedXbergClient as XbergWorkerClient` 别名引用，勿据名字误判为独立子进程路径。`xberg_worker.rs` 的独立子进程客户端现服务于 `examples/xberg_ocr.rs` 无头对照入口；`tests/xberg_client.rs` 用 mock 子进程验证其协议，不经过 `SharedXbergClient`，不能作为共享引擎或真实 OCR 验收（与根文档口径一致）。

## 入口与命令

本包在 workspace `members` 内但不在 `default-members`，仓库根的 `cargo build` / `cargo test` **不构建、不测试本包**；一切命令走 `--manifest-path`（与 `check.yml`、`acceptance.ps1` 同口径）：

```powershell
cargo test --manifest-path optional/snap-ocr-worker/Cargo.toml --all-targets
cargo clippy --manifest-path optional/snap-ocr-worker/Cargo.toml --all-targets -- -D warnings
cargo run --manifest-path optional/snap-ocr-worker/Cargo.toml --example xberg_ocr -- <Xberg组件目录> <image.png> <输出.json>
```

- `src/main.rs` 只接受 `--capabilities`（版本握手，打印协议版本 JSON）、`--xberg-broker`（经主程序代理的内部入口）、`--service` / `--service--autostart`（后台服务）；无参启动退出码 2 并提示从主界面启动。bin 面：`snap-ocr-worker`（产品）与 `mock-xberg-worker`（测试专用）。
- `examples/xberg_ocr.rs` 是同图对照的开发验收入口（需真实组件目录与 PNG），不是产品链路。
- 热键/托盘到结果窗的真实桌面 E2E、多 DPI、服务生命周期不在现有自动化内；未执行时如实标注未验证。

## 与主包的耦合边界（改动须双端同步）

- 无 crate 级依赖（双向都没有 path dependency）。源码级复用：`src/lib.rs` 以 `#[path = "../../../src/…"]` 把主包 `logging.rs`、`xberg_runtime.rs`、`xberg_settings.rs` 编进本 crate——改这三个共享文件同时影响主程序与本服务。
- 运行期产物依赖（主包侧）：`src/snap_ocr_assets.rs` 定义 `WORKER_EXE_NAME` 与管道名、`src/gui.rs` 以 `--service` 启动本 EXE 并维护热键映射表、`build.rs` 生成 `resources/snap-ocr-assets.json`（worker 条目打包时回填 size/sha256）。
- 语义耦合点（两端无共享类型，靠纪律与静态检查防漂移）：命名管道名 `\\.\pipe\jchtools-snap-ocr-<hash>`；`--capabilities` 版本握手 JSON；`src/gui.rs` 发出的管道命令与服务端分发表——由 `scripts/static_check.py` 的 `snap_pipe_command_sync` 执法双端同步；热键映射表两侧逐对一致。
- 产品改名/交付同步清单见根 AGENTS.md 第 5 节，不在此重复。

## 本地约定与陷阱

- 仅 Windows（`cfg(windows)` 贯穿服务、截图与托盘）；Slint 锁 `=1.17.1`，与主程序同版本。
- 界面与错误文案中文；错误按 O-30 分类并去敏，不含截图内容或用户路径。截图字节只在内存经 base64 传子进程（O-29）；Xberg 子进程 stderr 直接丢弃，不进日志。
- worker 协议为 stdio JSON 行；取消即终止子进程（无单请求取消），超时与进程退出同路径终止、不自动重试（XB-08）。
- 测试专用环境变量与常量：管道前缀 `jchtools-snap-ocr-test-`（测试绝不触碰真实管道与 launcher.json）、`JCHTOOLS_SNAP_OCR_TEST_ASSET_ROOT`（测试资产根覆盖）、`MOCK_XBERG_MODE`（mock bin 模式）；`USERNAME` / `USERPROFILE` 用作管道身份哈希回退。
- 日志经主包 `logging.rs` 落状态目录 `logs/`（按天轮转、保留 14 天，P-10），初始化失败安静退化。
- lint 硬门禁在本 crate `[lints]` 独立声明：`warnings`、clippy pedantic 及 `unwrap_used` / `expect_used` / `dbg_macro` / `todo` / `unimplemented` 全 deny，编辑须维持零告警；本 crate 不设 `perf-tracing` 特性，但保留共享 `logging.rs` 引用 cfg 所需的 `unexpected_cfgs` check-cfg 声明。
- 源码注释中的任务编号（O-xx / XB-xx / P-xx）是事实规格来源，改动前先查对应需求文档。
