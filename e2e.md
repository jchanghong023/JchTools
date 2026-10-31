# e2e.md · 端到端测试操作指南（面向 AI 代理）

本文件是 AGENTS.md 3.2「UI 测试交互方式」点名的桌面测试授权对象：**本文描述的会抢占用户鼠标/焦点的桌面流程，MUST 由用户当次明确要求执行本文后才可运行**。规范用语遵循 RFC 2119。全局纪律（原生需求维护、提交、可信基、三级门）唯一权威在 `AGENTS.md`；本文只解决「怎么测」，不新增或改写任何需求。

## 一、总原则与授权边界

1. **与三级门的关系**：`python scripts/test_gate.py <fastcheck|fulltest|slowtest>`（AGENTS.md 3.4）是常规验证入口；三级门 `MUST NOT` 触发全局鼠标或焦点抢占测试。`gui_flow` 全部用例、`scripts/gui_smoke.py` 与 `scripts/markdown_acceptance.py` 的真实桌面 driver、注册全局热键的 `check_worker_root.py` 服务探测在门内一律标记 `NOT_RUN_SEPARATE_USER_INSTRUCTION_REQUIRED`，保留本文的独立入口。fulltest / slowtest 每次运行 `MUST` 获得当次明确授权（`--authorized` 只是入口声明）。
2. **无窗口链路可自主运行**（不创建真实窗口、不抢焦点/鼠标）：`cargo test --features test-hooks` 的默认单元/集成测试（含库内 Slint TestingBackend 用例，如 `crates/gui/src/gui.rs` 内的 MD 回调测试）、`tests/gui_log_panel.rs` 的真实 Headless 控件交互与布局回归（`cargo test --features gui,test-hooks --test gui_log_panel`）、`python scripts/static_check.py`、`tests/slint_gate.rs` 零诊断门禁，以及本文第二节的无头 MCP 端到端（`SLINT_BACKEND=headless` 使用进程内 TestingBackend，不创建 OS 窗口）。
3. **必须用户当次明确授权的链路**（真实窗口会激活/抢占用户桌面）：
   - `cargo test -p jchtools --features test-hooks --test gui_flow`（真实 winit 窗口）；
   - `scripts/gui_smoke.py`（pywinauto/UIA 真实桌面 driver，需要可交互桌面会话）；
   - `scripts/markdown_acceptance.py` 的 GUI 条目（pywinauto driver）；
   - `acceptance.ps1 -WithDesktopTests` 追加的真实 GUI 用例与 `JCHTOOLS_SNAP_OCR_ASSET_ROOT` 就绪时的 worker 热键服务探测（`snap-ocr-worker-root`）；
   - 任何 `ComputerUse`、全局鼠标注入或焦点抢占流程。
   `acceptance.ps1 -WithGuiSmoke` / `-WithMarkdownAcceptance` 参数本身不创造授权，`MUST` 与 `-WithDesktopTests`（即本次用户明确授权）同时出现，脚本会拒绝单独传入。
4. **flaky 政策**：`MUST NOT` 重跑到绿。间歇性失败必须查因；确属 flaky 时在提交信息记录现象与原因。
5. **软件渲染 ≠ 无焦点隔离**：`SLINT_BACKEND=winit-software`（或 `sw`）仍是真实 winit 窗口、仍会激活焦点；只有第二节的无头后端才不创建窗口。

## 二、Slint MCP 无头端到端（本文核心能力）

启用根包 feature `mcp-server = ["gui", "slint/mcp", "jchtools-ui/mcp-debug-info"]` 后，JchTools 二进制内嵌 Slint MCP 服务器（slint 1.18.1，`vendor/i-slint-backend-testing-1.18.1/mcp_server.rs`）。运行期由 `SLINT_MCP_PORT` 激活；首次显示窗口后 HTTP 服务监听 `127.0.0.1:<port>`。无头模式下全部交互注入进程内事件，不碰真实键鼠。

### 2.1 构建

```powershell
# 隔离调试构建：mcp-server 提供 MCP；test-hooks 让 JCHTOOLS_TEST_STATE_DIR 生效（见 2.2）
cargo build -p jchtools --bin JchTools --features "mcp-server,test-hooks"
```

- `mcp-server` 是调试/测试专用 feature，生产与发布构建 `MUST NOT` 启用（见根 `Cargo.toml` [features] 注释与 `scripts/package-windows.ps1` 的构建维度）。
- `mcp-server` 已联动 `crates/ui/build.rs` 的 `with_debug_info(true)`（`mcp-debug-info` feature），元素树查询可用；`MUST NOT` 依赖 `SLINT_EMIT_DEBUG_INFO` 环境变量。缺失调试信息的构建里 `get_element_tree` 只会返回无名的根元素并在响应中带 `debugInfoMissing: true`。

### 2.2 隔离运行（避免污染真实用户配置）

GUI 进程默认把状态写入真实用户目录（`ProjectDirs::from("","","JchTools").data_local_dir()`，含 `config.sqlite3`），且已有配置可能唤起截图 OCR / CodeAgent等常驻进程。`JCHTOOLS_TEST_STATE_DIR` **仅在启用 `test-hooks` feature 的构建中生效**（`crates/foundation/src/xberg_settings.rs` 的 `test_directory_override` 以 `cfg!(any(test, feature = "test-hooks"))` 门控），必须为绝对路径。因此无头调试 `MUST` 用 2.1 的双 feature 构建并布置：

```powershell
# 全部产物放 .tmp/（AGENTS.md §2）；状态目录与截图同此
python scripts/make_tmp.py workspace --destination .tmp\mcp-e2e\state
$env:JCHTOOLS_TEST_STATE_DIR = (Resolve-Path .tmp\mcp-e2e\state).Path   # 隔离 config.sqlite3 / 日志
$env:JCHTOOLS_SNAP_OCR_TEST_ASSET_ROOT = (Resolve-Path .tmp\mcp-e2e\state).Path + '\snap-assets'  # 隔离资产根（gui_smoke 同款守卫变量）
$env:JCHTOOLS_TEST_CODEAGENT_CREDENTIALS = (Resolve-Path .tmp\mcp-e2e\state).Path + '\absent-credentials.json'
$env:JCHTOOLS_TEST_CODEAGENT_REGISTRY = 'Software\JchTools\Tests\CodeAgent\mcp-e2e'
$env:SLINT_BACKEND = "headless"    # 进程内 TestingBackend，不创建真实窗口、不抢用户焦点/鼠标
$env:SLINT_MCP_PORT = "8765"       # 激活内嵌 MCP 服务器（仅 127.0.0.1）
target\debug\JchTools.exe
```

- `SLINT_BACKEND=headless` 需要软件渲染器，workspace 的 `slint` 依赖已启用 `renderer-software`。
- 启动成功的标志是 stderr 出现 `Slint MCP server listening on http://127.0.0.1:8765/mcp`（从终端启动可见）。
- 结束无头 GUI 可用 Ctrl+C 或 `taskkill /PID <GUI pid>`（无头窗口没有关闭按钮）；GUI 退出不等于常驻后台退出。随后在**同一套隔离环境**中用本次受检 EXE 执行 `target\debug\JchTools.exe --codeagent-service --stop`，等待本轮隔离 CodeAgent 后台正常退出，不对真实用户后台按映像名清场。
- 端口冲突时换端口重试不算 flaky；`SLINT_MCP_PORT` 值非法时进程会打印 `SLINT_MCP_PORT: invalid port number` 且不启动服务器。
- CodeAgent 的状态目录、合成/缺失凭据路径与测试注册表三个覆盖必须成套设置；仅设置 `JCHTOOLS_TEST_STATE_DIR` 不隔离凭据和注册表。上述缺失凭据路径用于观察去敏失败状态，不读取真实 Token。
- 自动化启动使用 `scripts.gui_smoke.start_owned_process`：暂停创建 → 绑定独占 Windows Job → 恢复执行；stdout / stderr 写入 `.tmp/` 下本轮文件。业务交互前还须确认本轮 GUI PID、角色、新日志启动事件与隔离 SQLite，旧数据库存在不能证明当前程序启用了 `test-hooks`。结束时等待 GUI 自身退出、显式停止本轮后台并关闭 owner，不用输出管道 EOF 代替进程退出判定。
- 解压和目录整理按 H-06 拒绝 Git 项目及其内部目录。先用 `make_tmp.py` 在 `.tmp/` 生成样本，再由测试内置 `tempfile` 夹具复制到无 Git 祖先的自有临时目录；状态、日志、截图仍放 `.tmp/`，结束删除该夹具，不为测试绕过产品保护。

### 2.3 JSON-RPC over HTTP 调用示例

实现是 MCP Streamable HTTP 的单端点变体：只接受 `POST http://127.0.0.1:8765/mcp`（路径 `/` 也接受），`Content-Type` 必须以 `application/json` 开头（否则 415）；每个请求一个 JSON-RPC 2.0 对象（批次数组返回 -32600）；通知（无 `id`）返回 202 空体；本实现不校验 `Accept` 头、不下发会话头（无需 `Mcp-Session-Id`，但 CORS 预检允许携带）；带 `Origin` 头时必须是 `localhost`/`127.0.0.1`/`::1`（否则 403）。`initialize` 返回 `protocolVersion: "2025-06-18"`。窗口句柄与元素句柄结构相同（`{"index":"0","generation":"0"}`，protobuf JSON 字符串数值，零值字段可省略）但**不可互换**：窗口句柄只来自 `list_windows`，元素句柄来自 `get_element_tree` / `find_elements_by_id` / `query_element_descendants` / `get_window_properties` 的 `rootElementHandle`。

```powershell
# initialize（curl.exe，Windows 自带；-H "Origin: http://localhost" 可选）
curl.exe -s http://127.0.0.1:8765/mcp -H "Content-Type: application/json" -d '{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{\"protocolVersion\":\"2025-06-18\",\"capabilities\":{},\"clientInfo\":{\"name\":\"e2e\",\"version\":\"0\"}}}'

# tools/list —— 工具全集与 inputSchema 以此为准
curl.exe -s http://127.0.0.1:8765/mcp -H "Content-Type: application/json" -d '{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/list\"}'

# list_windows → 取 windowHandles[0]
curl.exe -s http://127.0.0.1:8765/mcp -H "Content-Type: application/json" -d '{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"tools/call\",\"params\":{\"name\":\"list_windows\",\"arguments\":{}}}'

# take_screenshot → 返回 base64 PNG 的 image 内容块 + sizeBytes
curl.exe -s http://127.0.0.1:8765/mcp -H "Content-Type: application/json" -d '{\"jsonrpc\":\"2.0\",\"id\":4,\"method\":\"tools/call\",\"params\":{\"name\":\"take_screenshot\",\"arguments\":{\"windowHandle\":{\"index\":\"0\"}}}}'
```

交互工具示例（先 `get_window_properties` 拿 `rootElementHandle`，或 `find_elements_by_id` 按 `ComponentName::element-id` 定位）：

```powershell
# 点击元素（省略 action/button 默认左键单击；可选 action=DoubleClick、button=Right/Middle/...）
curl.exe -s http://127.0.0.1:8765/mcp -H "Content-Type: application/json" -d '{\"jsonrpc\":\"2.0\",\"id\":5,\"method\":\"tools/call\",\"params\":{\"name\":\"click_element\",\"arguments\":{\"elementHandle\":{\"index\":\"3\"}}}}'

# 文本输入（TextInput 元素 set_element_value，或 dispatch_key_event 逐键）
curl.exe -s http://127.0.0.1:8765/mcp -H "Content-Type: application/json" -d '{\"jsonrpc\":\"2.0\",\"id\":6,\"method\":\"tools/call\",\"params\":{\"name\":\"dispatch_key_event\",\"arguments\":{\"windowHandle\":{\"index\":\"0\"},\"text\":\"j\"}}}'
```

**工具全集（18 个，名称与参数已从 `mcp_server.rs` 核实）**：

| 工具 | 必填参数 | 可选参数 |
|---|---|---|
| `list_windows` | — | — |
| `get_window_properties` | `windowHandle` | — |
| `get_element_tree` | `elementHandle` | `maxElements`（默认 200，上限 1000；截断时 `truncated: true`） |
| `get_element_properties` | `elementHandle` | — |
| `find_elements_by_id` | `windowHandle`, `elementsId` | — |
| `query_element_descendants` | `elementHandle`, `queryStack` | `findAll` |
| `take_screenshot` | `windowHandle` | `imageMimeType` |
| `click_element` | `elementHandle` | `action` / `button` / `modifiers` |
| `drag_element` | `elementHandle`, `target` | `button` / `modifiers` |
| `hover_element` | `elementHandle` | `modifiers` |
| `move_pointer` | `windowHandle`, `position` | `modifiers` |
| `scroll_element` | `elementHandle` | `deltaX` / `deltaY` / `modifiers` |
| `dispatch_pointer_scroll` | `windowHandle`, `position` | `deltaX` / `deltaY` / `modifiers` |
| `invoke_accessibility_action` | `elementHandle`, `action` | — |
| `set_element_value` | `elementHandle`, `value` | — |
| `dispatch_key_event` | `windowHandle`, `text` | `eventType`（默认 `PressAndRelease`） |
| `start_event_recording` | — | — |
| `stop_event_recording` | — | — |

枚举值为 PascalCase 字符串：`ClickAction` = `SingleClick`/`DoubleClick`；`PointerEventButton` = `Left`/`Right`/`Middle`/`Back`/`Forward`/`Other`；`ElementAccessibilityAction` = `Default_`/`Increment`/`Decrement`/`Expand`；`KeyEventType` = `PressAndRelease`/`Press`/`Release`；`modifiers` 是 `{shift, control, alt, meta}` 布尔对象，手势期间按住、结束释放。`queryStack` 是逐条指令的管线，每项恰好一个键：`matchDescendants`（递归）、`matchElementId`、`matchElementTypeName`、`matchElementTypeNameOrBase`、`matchElementAccessibleRole`。交互后验证用 `take_screenshot`；要断言事件真的到达运行期，用 `start_event_recording` → 交互 → `stop_event_recording`（事件日志上限 1024 条，响应含 `droppedCount` / `unknownEventCount`）。

截图落盘约定（MCP 返回的是 base64，不自动写文件）：

```powershell
# 把 take_screenshot 响应中 result.content[0].data 解码到 .tmp/（AGENTS.md §2：截图一律在 .tmp/ 下）
# （管道接在 take_screenshot 的 curl 之后；Windows 自带 curl.exe 输出即 UTF-8 JSON）
python -c "import base64,json,sys,pathlib; resp=json.load(sys.stdin); pathlib.Path('.tmp/mcp-e2e/shot.png').write_bytes(base64.b64decode(resp['result']['content'][0]['data']))"
```

### 2.4 界面美化迭代循环

1. 改 `ui/app.slint` → 重新执行 2.1 构建（增量）→ 2.2 启动 → `take_screenshot` 对照。
2. 自查（无需用户参与）：颜色只允许出现在 `ui/app.slint` 的 `Design` 全局（十六进制色机器检查，`python scripts/static_check.py`）；布局内 `MUST NOT` 用 `root.width` / `parent.width` 绑定子项宽度（binding loop 禁令，同受 static_check 与打包警告扫描执法）；构建输出 `MUST NOT` 出现 `binding loop` 警告。
3. 需要用户裁决时按 AGENTS.md 第 7 节给「新旧并排截图的是/否判断」或附推荐项的封闭选择题；`MUST NOT` 让用户读 diff。
4. 断言界面结构时优先 `get_element_tree` / `query_element_descendants` 的几何与角色数据，截图仅作视觉证据；两者结论冲突时先查渲染差异。MCP 截图用于真实应用进程内的交互与视觉取证，不以隔离控件回归代替应用交互；CodeAgent 仅采集去敏状态，不展示或记录 Token。

### 2.5 软件渲染 vs 生产渲染差异

无头后端用软件渲染器出图（`SLINT_BACKEND=headless` 传空 renderer 名即软件渲染器）；生产窗口默认走 winit + femtovg。抗锯齿、字体光栅化、阴影与半透明叠加在两条管线间存在像素级差异，`MUST NOT` 把无头截图当像素级 golden 与生产对照；布局几何（元素树里的位置/尺寸）是两管线一致的可断言口径。发布验收仍以 `tests/gui_flow.rs` 与 `gui_smoke.py` 的真实窗口链路为准（按第一节授权）。

## 三、既有测试入口速查

| 入口 | 命令 | 前提/边界 |
|---|---|---|
| 单元/集成测试 | `cargo test --features test-hooks`；按目标拆分如 `cargo test --features test-hooks --test md_tools`、`--test git_tools`、`--test xberg_settings`、`--test xberg_shared_process` | `git_tools` 用真实 git + 本地 bare 远端，不覆盖真实网络认证 |
| 静态检查 | `python scripts/static_check.py` | 结构/配置/回调/SQL/测试基线/界面规则 |
| UI 零诊断门禁 | `cargo test --features test-hooks --test slint_gate` | 编译 `ui/app.slint` 的诊断清零（测试专用 slint-interpreter，不进产品） |
| Headless 日志与布局回归 | `cargo test --features gui,test-hooks --test gui_log_panel` | `tests/gui_log_panel.rs` 使用真实 Slint 控件、进程内指针/键盘与软件渲染，不创建 OS 窗口；覆盖 960×620 / 1120×720、浅色 / 深色，footer 空闲 / 运行 / 完成态按钮可读性、短诊断紧凑布局、长诊断限高滚动及空失败列表几何；不代表生产 GPU 像素或完整发布验收 |
| gui_flow | `cargo test -p jchtools --features test-hooks --test gui_flow` | 真实 winit 窗口，会激活用户焦点——**门外独立授权** |
| gui_smoke | `python scripts/gui_smoke.py --exe target/debug/JchTools.exe --data <make_tmp 数据集> [--stages S1,...]`；`--list-stages` 列阶段 | pywinauto/UIA 真实桌面——**门外独立授权**；隔离构建产物见 S6-S9 行 |
| markdown 验收 | `python scripts/markdown_acceptance.py --list`；默认 `--profile common`，`--only A,A24` 选条目；被测 GUI 须 `--gui-exe` 或 `JCHTOOLS_TEST_GUI_EXE` 显式指定 | GUI 条目为 pywinauto driver——**门外独立授权**；退出码 0=全 PASS / 1=有 FAIL / 2=有 NOT RUN / 3=参数错误 |
| optional 包 | `cargo test --features test-hooks --manifest-path optional/snap-ocr-core/Cargo.toml --all-targets`；`optional/snap-ocr-worker/` 同式 | 不在 default-members，本地 `acceptance.ps1` 有显式步骤，CI 不执行 |
| 单命令验收 | `powershell -NoProfile -File .\scripts\acceptance.ps1`（`-WithEngine` / `-WithPackage` / 授权后 `-WithDesktopTests [-WithGuiSmoke -GuiData <目录>] [-WithMarkdownAcceptance]`） | 桌面参数须本次明确授权（见第一节） |
| 性能基准 | `cargo test -p jchtools --features test-hooks --release --test perf_probe -- --ignored --nocapture` | 默认 `#[ignore]`，只测耗时/计数不断言秒数；规模经 `JT_PERF_GROUPS` / `JT_PERF_COPIES` / `JT_PERF_EMPTY_DIRS` |
| 性能打点 | `cargo run --features perf-tracing` | 日志落状态目录 `perf-logs/`，默认关闭 |

**gui_smoke 阶段（S1–S18，含义已对照 `scripts/gui_smoke.py` 头注释核实）**：S1 启动并正常退出；S2 目录整理分析（只读到计划）；S3 整理全链路；S4 递归解压全链路（成功原包/分卷删除、冲突改名、嵌套落盘）；S5 转 Markdown 开始/停止（默认序列不含，需 Xberg 就绪与 `JCHTOOLS_S5_MEDIA` 真实媒体）；S6 随包基础就绪（需 `cargo build -p jchtools --bin JchTools --features test-hooks` 产物——普通构建不识别测试环境变量——及预置 `JCHTOOLS_TEST_BUNDLED_XBERG_DIR`）；S7 重启恢复；S8 音频显式下载边界（test-hooks 屏障，不连真实网络）；S9 基础包缺失；S10 输出层级与同名策略（需真实组件）；S11 已有结果跳过；S12 输出子树排除；S13 部分失败与完成统计；S14 运行中关闭确认与安全停止（需 `JCHTOOLS_S5_MEDIA`）；S15 MD 合并/拆分（产物与原文件保护）；S16 Git 逐文件提交推送到本地 bare 远端（不覆盖真实网络/认证）；S17 真实 Xberg 离线检查补写 notice（需 `JCHTOOLS_TEST_XBERG_DIR` 与绝对路径的 `JCHTOOLS_TEST_STATE_DIR` / `JCHTOOLS_MARKDOWN_TEST_ASSET_ROOT`）；S18 两档窗口尺寸逐页布局判定（U-05/P-13）。默认序列 `S1,S2,S3,S4,S15,S16,S18`。除 S6–S9 自隔离外，其余阶段独立调用要求在场 `JCHTOOLS_TEST_STATE_DIR` 与 `JCHTOOLS_SNAP_OCR_TEST_ASSET_ROOT`（缺则拒绝启动并指向 acceptance.ps1；`--allow-isolated-run` 只跳过 CLI 前置检查，绕不过公共启动入口隔离）。

**「什么改动该跑什么」决策表**：

| 改动 | 最低验证 |
|---|---|
| 任意代码/配置 | `cargo test --features test-hooks` + `python scripts/static_check.py` |
| `ui/app.slint` / GUI 装配 | 上行 + 第二节无头 MCP 截图自查（或授权后 `gui_flow` + `gui_smoke` S1–S4/S18） |
| 递归解压 / 目录整理 / 引擎 / 删除路径 | 上行 + 授权后 `acceptance.ps1 -WithEngine` + `gui_flow` |
| 转 Markdown | 上行 + 授权后 `markdown_acceptance.py --profile common`（扩展矩阵 `--profile full`） |
| Git 工具 | 上行（`--test git_tools` 覆盖本地 bare 远端链路） |
| 打包/发布 | `scripts/package-windows.ps1` 全程 + 干净目录解包运行 |

## 四、通用纪律提醒（细节以 AGENTS.md 为准）

- **`.tmp/` 规则**：截图、日志、状态目录、中间产物一律在仓库 `.tmp/` 下（本文第二节目录 `.tmp/mcp-e2e/` 即此约定）；`MUST NOT` 提交、`MUST NOT` 入发布包；测试数据统一 `python scripts/make_tmp.py` 生成；收尾删除本会话自建子目录，并行会话在用时 `MUST NOT` 全量 `clean`。
- **回归测试先行**：缺陷修复 `MUST` 先写修复前失败的回归测试，提交信息附复现命令与失败输出摘要（AGENTS.md 3.2）。
- **测试基线**：删除/改名/放宽断言、新增 `#[ignore]` 或平台门禁 `MUST` 同步 `python scripts/static_check.py --update-test-baseline` 并在提交信息写明理由；`MUST NOT` 只为变绿而削弱测试。该命令会重写 `scripts/test-baseline.json`，属可信基变更，须独立提交。
- **界面硬规则**：十六进制色只允许在 `Design` 全局；布局内禁止 `root/parent.width` 宽度绑定（binding loop）；颜色/布局规则由 `static_check.py` 机器执法。
- **如实报告**：`NOT RUN` / `UNVERIFIED` 不得报告为通过；验证报告区分已实现 / 验证通过 / 验证失败 / 未验证（AGENTS.md 3.2 / 3.3）。

## 五、MCP 客户端接入提示

任何支持 Streamable HTTP 的 MCP 客户端可以把 `http://127.0.0.1:8765/mcp` 注册为 MCP 服务器，获得第二节全部工具的工具化访问（`initialize` 后 `tools/list` 自带每个工具的 inputSchema 与 `initialize.instructions` 的完整工作流说明）。通用 JSON 配置示例（字段名与文件位置按各自客户端文档调整）：

```json
{
  "mcpServers": {
    "jchtools-headless": {
      "type": "http",
      "url": "http://127.0.0.1:8765/mcp"
    }
  }
}
```

约束：服务器只绑定 `127.0.0.1`，须先按 2.2 启动无头进程；一次只服务本机调试会话，`MUST NOT` 暴露给局域网；该构建仅用于调试（feature 边界见 2.1）。
