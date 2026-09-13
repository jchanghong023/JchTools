---
feature: proxy-status
status: implemented
updated: 2026-09-13
branch: master
---

# 代理工具（proxy-status）

## Report

## 评审决议（2026-09-13）

独立评审已将本计划与当前代码逐条核对，以下前两项由用户确认，其余为评审补入的设计修正，本版已全部合入正文：

1. **显示名定为「代理工具」**（id 仍为 `proxy-status`，不采用「本机代理」）。
2. **端口启发式移除 8080、8888**（本地开发服务器最常用端口，避免开发机上大量误报），仅保留代理特征明显的端口。
3. 评审补入：子进程输出按系统 OEM 代码页解码、`CREATE_NO_WINDOW`、独立 `proxy-busy` 属性、无头测试不 spawn 子进程的接缝、Windows 环境变量大小写合并、`naive` 显式名单、`Event::ProxySnapshot` 变体位置、README 两处文案位置。

## [S1] Problem

用户在配置开发环境、调试网络或切换 VPN 时，经常需要知道：

1. 当前 shell / 系统是否已经设置代理环境变量；
2. Windows 系统代理（WinINET，浏览器与多数桌面程序使用）是否启用、指向何处；
3. 本机是否有常见的 VPN / 代理客户端在运行，以及它们的本地监听端口；
4. 虚拟网卡（TAP / Wintun / WireGuard 等）是否启用。

这些信息散落在注册表、进程列表、网卡列表和各种文档里。用户希望 JchTools 增加第二个工具「代理工具」：一站式展示上述状态，并提供可手动复制执行的 Windows / Linux 代理设置命令，避免到处查资料。

## [S2] Design

### 注册与导航

- 在 `src/registry.rs` 注册第二个工具（`ToolDescriptor` 已有 `category` 字段，无需改 schema）：
  - `id`: `proxy-status`
  - `name`: `代理工具`
  - `category`: `网络`
  - `summary`: `环境变量 · 系统代理 · VPN 进程 · 虚拟网卡 · 设置命令`
- 侧栏继续只遍历 `registry::tools()`；工具数变为 2（仍 ≤5，搜索框不出现；`app.slint` 中搜索框条件为 `tool-count > 5`）。
- 页面路由：`screen 0` 目录整理，`screen 2` 代理工具，`screen 1` 关于（保持不动）。
- `on_select_tool` 按 id 分发：`directory-organizer` → screen 0（保留现有「重置分区并刷新规则」行为）；`proxy-status` → screen 2，并设置 `active-tool-id`；未知 id 忽略。
- 标题栏文案改为根据 screen 显示「目录整理」/「代理工具」/「关于 JchTools」（现为 `screen == 0` 二元表达式，需改三态）。
- 导航高亮：现有 `NavItem` 写死 `root.screen == 0`（`app.slint`），改为按工具 id 映射各自 screen——目录整理项 `screen==0 && id==directory-organizer`，代理工具项 `screen==2 && id==proxy-status`；「关于」仍为 `screen==1`。
- 文案同步：
  - 关于页（`ui/app.slint`）「当前只有目录整理工具」→ 说明已注册「目录整理」「代理工具」两个工具。
  - `README.md` 两处：第 3 行「当前提供第一个工具“目录整理”」、第 41 行「导航只显示目录整理和关于」。
  - `先读我.txt` 经查未见工具数表述；T2 实施时复核一次并记录结论。

### 后端模块 `src/proxy.rs`

纯检测与静态命令参考，不依赖 GUI；`lib.rs` 导出 `pub mod proxy`（模块在 `gui` 特性之外编译，保证 CLI 与非 Windows 构建可用）。

#### 数据模型

```rust
pub struct ProxySnapshot {
    pub env_vars: Vec<EnvVarStatus>,
    pub system_proxy: Option<SystemProxyStatus>,
    pub vpn_processes: Vec<VpnProcess>,
    pub adapters: Vec<AdapterStatus>,
    pub notes: Vec<String>,
}

pub struct EnvVarStatus { pub name: String, pub value: Option<String> }
pub struct SystemProxyStatus { pub enabled: bool, pub server: String, pub override_list: String, pub source: String }
pub struct VpnProcess { pub name: String, pub pid: u32, pub ports: Vec<u16>, pub label: String }
pub struct AdapterStatus { pub name: String, pub description: String, pub status: String, pub virtual_like: bool }

pub struct CommandTip {
    pub platform: &'static str, // "Windows" | "Linux"
    pub group: &'static str,    // 会话环境变量 / 系统代理 / 查看 / 取消
    pub title: &'static str,
    pub command: &'static str,
    pub note: &'static str,
}
```

#### 子进程执行约定（所有检测命令统一遵守）

检测需要 spawn `tasklist`、`netstat`、`powershell`：

1. **不闪控制台窗口**：本程序是 `windows_subsystem = "windows"` 的 GUI 进程，spawn 控制台程序会闪黑框。必须设 `std::os::windows::process::CommandExt::creation_flags(0x0800_0000)`（`CREATE_NO_WINDOW`），与 `src/process.rs` 中 7-Zip 的既有写法一致。
2. **输出解码**：中文 Windows 上 `tasklist`/`netstat`/`netsh` 的管道输出是系统 OEM 代码页（zh-CN 为 CP936/GBK），**不是 UTF-8**。不能复用 `src/process.rs` 的管道（它对非 UTF-8 直接报错，且为 7-Zip 专用）。检测代码自行用 `std::process::Command::output()`（输出量小、进程短命，无失控风险），字节按 `MultiByteToWideChar(CP_OEMCP, …)` 解码；为此给 `Cargo.toml` 的 `windows-sys` 增加 `Win32_Globalization` feature，不引入新依赖。
3. **PowerShell 统一强制 UTF-8 输出**：`powershell -NoProfile -NonInteractive -Command "[Console]::OutputEncoding=[Text.Encoding]::UTF8; <查询>"`，输出按 UTF-8 解码。
4. 所有解析函数只接收**解码后的 `&str`**；单元测试使用固定样例文本，不依赖真实环境输出。

#### 检测逻辑（按优先级实现，单项失败记入 `notes`，不整体失败、不 panic）

1. **环境变量**（所有平台）：读取 `HTTP_PROXY`、`HTTPS_PROXY`、`ALL_PROXY`、`NO_PROXY`。实现从一次性环境快照查找（`std::env::vars()` 收集为 map，作为可注入参数以便测试）。**Windows 上环境变量名大小写不敏感**，按规范化名合并为一条（显示大写名），避免同一变量重复成两行；Unix 上大小写形式各自成行。未设置记 `value: None`。
2. **Windows 系统代理**（仅 Windows）：读注册表 `HKCU\Software\Microsoft\Windows\CurrentVersion\Internet Settings` 的 `ProxyEnable`、`ProxyServer`、`ProxyOverride`。复用现有 `windows-sys` / `RegGetValueW` 风格（`Win32_System_Registry` feature 已启用），与 `system_dark()` 同类；`source` 记录该注册表键路径。
3. **VPN / 代理进程 + 端口**（Windows）：
   - 内置常见客户端名称子串匹配（不区分大小写）：`clash`、`mihomo`、`v2ray`、`v2rayn`、`xray`、`sing-box`、`singbox`、`shadowsocks`、`ss-local`、`nekobox`、`hiddify`、`wireguard`、`openvpn`、`surge`、`shadowtls`、`tun2socks`、`easytier`、`proxygen`；`naive` 作为普通子串太泛，改用显式名单 `naive`、`naiveproxy` 匹配。
   - 进程枚举：`tasklist /fo csv /nh`；用既有 `csv` crate 解析得到 `name,pid`（进程名内含逗号由 CSV 引号规则保证）。
   - 端口：`netstat -ano -p tcp`，跳过表头，按空白切列；状态列以 `LISTEN` 为前缀即视为监听（zh-CN 输出为 `LISTENING`，前缀匹配对本地化更健壮），从本地地址列取最后一个 `:` 之后为端口、末列为 PID，按 PID 关联；只保留本机监听端口。
   - 命中进程写入 `VpnProcess`（`label` = 「按进程名匹配」）；未命中名单时，若进程在**代理特征端口**（`7890`、`7891`、`1080`、`1081`、`20171`、`20172`、`10808`、`10809`、`2080`、`33210`、`41091`）监听，也列入并标注「按端口启发式」。**列表不含 8080、8888**（评审确认移除，理由见评审决议）。
   - 已知限制：仅检测 TCP；WireGuard 的 UDP 端口（如 51820）看不到，由 `wireguard` 进程名匹配兜底。
4. **虚拟网卡**（Windows）：`Get-NetAdapter | Select-Object Name,InterfaceDescription,Status | ConvertTo-Json -Compress`（按子进程约定强制 UTF-8；`ConvertTo-Json` 对单个对象与数组两种输出形态都要能解析）；失败则回落 `netsh interface show interface`（OEM 解码）。名称/描述含 `TAP`、`Wintun`、`WireGuard`、`OpenVPN`、`TUN`、`虚拟`（不区分大小写）时 `virtual_like = true`。
5. **非 Windows**：实现环境变量检测；系统代理、进程/端口、网卡检测返回空并写 `notes`（如「当前平台仅完整支持 Windows 检测」）。命令参考仍完整显示 Windows + Linux。平台相关代码用 `cfg` 隔离，保证 Linux CI 可编译。

#### 线程与事件

- 检测在 worker 线程执行，复用 `gui.rs` 的 `async_work`（自带 panic 拦截），通过既有 `mpsc::sync_channel<Event>` 回传，由现有 100ms 轮询 timer 在 UI 线程更新模型，避免 PowerShell/netstat 阻塞 UI。新增 `Event::ProxySnapshot(crate::proxy::ProxySnapshot)` 变体，加在 `src/control.rs` 的 `Event` 枚举上（`emit` 的 match 有 `other` 兜底臂，不影响整理引擎路径）。
- **测试接缝**：`wire_sync` 刻意「不依赖事件循环」，且无头测试工作线程没有事件通道。因此 `State` 增加 `proxy_events: Option<mpsc::SyncSender<Event>>`：`run_with_pre_loop_hook` 建通道后置 `Some`，`initial_state()`（无头测试）保持 `None`；`on_refresh_proxy` 仅在 `Some` 时 spawn 检测。无头测试点 `select-tool("proxy-status")` 或调用 `refresh-proxy` 都不会拉起子进程。
- 触发时机：进入 screen 2 时自动刷新一次 + 「刷新」按钮；**不做结果缓存**（数据量小、2~3 秒即可重测，缓存易过期误导）。检测期间显示 Spinner、刷新按钮禁用，但不阻塞窗口关闭。

#### 命令参考（静态，编译进二进制）

`proxy::command_tips()` 返回精选列表，覆盖：

- **Windows · 会话环境变量**：PowerShell 设置/清除 `HTTP_PROXY`/`HTTPS_PROXY`/`ALL_PROXY`/`NO_PROXY`；cmd `set`/`setx`（注明 setx 影响新进程）。
- **Windows · 系统代理**：`netsh winhttp show/set/reset proxy`；说明 WinINET（浏览器）与 WinHTTP（系统服务）区别；不提供“一键写注册表”执行按钮，只展示可复制命令。
- **Linux · 会话环境变量**：`export` / `unset` 小写与大写；写入 `~/.bashrc` 或 `/etc/environment` 的示例。
- **Linux · 桌面/包管理**：GNOME `gsettings`；apt `Acquire::http::Proxy` 片段。
- **查看**：`env | grep -i proxy`、`netsh winhttp show proxy`、`Get-ItemProperty` 查注册表。

命令只读展示，可复制文本；工具**不自动执行**任何设置命令（符合「手动执行」目标与安全底线）。

### 界面结构（screen 2）

- 顶部操作条：说明文案 +「刷新」按钮；检测中显示 Spinner（`Spinner` 已是现有 busy 指示先例，`app.slint` 已导入）。
- **新增独立属性 `proxy-busy`（禁止复用 `busy`）**：`busy` 是目录整理的任务状态，控制大量 `enabled: !root.busy`，且窗口关闭处理在 `busy` 时弹「任务仍在处理文件」阻止关窗；代理检测用 `proxy-busy`，检测期间关窗口不被误拦。
- 新增 Slint 行结构与模型属性：`ProxyEnvRow`（name / set / value）、`ProxyVpnRow`（name / pid / ports / label）、`ProxyAdapterRow`（name / description / status / virtual-like）、`ProxyCommandRow`（title / command / note / group），对应 `proxy-env-rows` 等模型属性；数据量小，整表重建即可。
- 新增回调（须在 AppWindow 声明并在 Rust 接线，满足 `static_check.py` 的 `ui_callbacks` 一一对应检查）：`refresh-proxy()`、`select-proxy-platform(int)`。
- 分区用 **Pill**（与目录整理规则分区一致）：`状态` / `命令参考`。
- **状态区**四张 Surface 卡片（纵向排列，可滚动）：
  1. **环境变量**：每行变量名 + 是否设置 + 值（空则「未设置」）。
  2. **系统代理**：启用状态、服务器、绕过列表、来源（HKCU…）。
  3. **VPN 进程**：进程名、PID、监听端口、匹配说明；空态「未发现常见 VPN/代理客户端」。
  4. **虚拟网卡**：名称、描述、状态、是否疑似虚拟；空态说明。
- **命令参考区**：平台 Tab（Windows / Linux，切换经 `select-proxy-platform` 由 Rust 重建模型）+ 分组列表；每条含标题、命令（等宽/可选中只读 TextEdit，`app.slint` 摘要区已有先例）与作用备注（自动换行完整显示，不截断）。**一键复制**：整卡可点或点「复制」按钮（CopyChip，带无障碍标签与键盘可达）→ Rust 侧 `copypasta` 写系统剪贴板，成功走蓝色提示条「已复制…」、失败走红色错误条，不静默；代理页面上提示条均可见。
- 所有自绘可交互控件设置 `accessible-role` / `accessible-label`。
- 颜色一律走 `Design` 全局，不硬编码主题色；面板切换用 Tab、分区用 Pill，遵守既有控件样式分工。

### 错误与边界

- 单项检测失败：该项显示「检测失败：原因」，其它项继续；失败只进 `notes` 与卡片内文案，**不写 `error_text`、不 panic、不中断其它检测**。
- 进程/网卡列表为空：明确空态文案，不显示为错误。
- 不上传、不联网、不改系统设置；只读检测（注册表读 HKCU、`tasklist`/`netstat`/`Get-NetAdapter` 均无需管理员权限）。
- 不把「未检测到」表述成「系统一定没有代理」——文案用「未发现常见…」「当前进程环境未设置…」。
- 无头 GUI 测试不依赖真实环境内容：不断言本机进程、端口、注册表的实际值。

### 测试边界

- `proxy.rs` 单元测试：
  - 环境变量解析（注入 map）：覆盖 Windows 大小写合并与 Unix 分行两种形态；
  - 已知进程名匹配：含 `naive`/`naiveproxy` 显式名单命中、普通进程名（如本地开发服务器）不误报；
  - `tasklist` CSV 与 `netstat` 监听行的**解码后样例文本**解析（样例含中文表头/中文进程名）；
  - `command_tips()` 至少各含一条 Windows 与 Linux 条目，且无空标题/空命令；
  - 非 Windows 路径不 panic。
- 注册表测试：Windows 下可测读到结构（只读）；非 Windows 跳过。
- GUI 无头测试（`src/gui.rs` 的 `gui_tests`）：
  - `tool_count == 2`（**同步修改现有断言 `src/gui.rs` 中 `initial_surface_lists_defaults`**）；
  - `select-tool("proxy-status")` 后 `screen == 2` 且 `active-tool-id` 正确；切回目录整理后高亮/路由不串；
  - 测试环境 `proxy_events == None` 时 `refresh-proxy` 为 no-op（不 spawn 子进程）；
  - `search_tools_filters_registry` 现有断言预期不受影响（「目录」仍只匹配目录整理）；若受影响，说明原因后调整。
- 静态检查：`python scripts/static_check.py` 的 `ui_callbacks` 要求 AppWindow 声明的回调与 `ui.on_*` 接线一一对应，新增回调必须两侧同步。

## [S3] Out of Scope

- 自动设置/清除代理（一键写环境变量、注册表、WinHTTP）。
- CLI 子命令（用户明确选择仅 GUI）。
- 读取第三方客户端配置文件解析精确混合端口/规则。
- Linux 进程/网卡完整检测（仅保证命令参考与环境变量在 Linux 上可用）。
- 持续后台监控、托盘常驻、开机自启。
- 修改目录整理工具行为。

## Tasks

- [x] T1: 实现 `src/proxy.rs` 检测与命令参考 + 单元测试 — acceptance: 环境变量解析（含大小写合并）/进程匹配（含 `naive` 显式名单）/tasklist+netstat 解码样例解析有测试；`command_tips()` 含 Windows 与 Linux 条目且无空标题/空命令；子进程统一遵守「`CREATE_NO_WINDOW` + OEM 解码（PowerShell 强制 UTF-8）」约定（`Cargo.toml` 的 `windows-sys` 增加 `Win32_Globalization` feature）；非 Windows 不 panic (covers: S2)
- [x] T2: 注册表与文档文案 — acceptance: `registry.rs` 含 `proxy-status`（name 代理工具 / category 网络）；关于页与 `README.md`（第 3 行、第 41 行）不再写「只有/第一个目录整理」；`先读我.txt` 复核并记录结论：全文无「只有目录整理」类工具数表述，无需修改 (covers: S2)
- [x] T3: GUI screen 2 + 选择工具路由 + 刷新回调 — acceptance: 侧栏出现「代理工具」且按 id 高亮互不串；点击进入 screen 2、标题栏三态文案；刷新走 worker 回传 `Event::ProxySnapshot`；使用独立 `proxy-busy`（检测期间关窗不被误拦）；无头测试覆盖 `tool_count==2`、路由、无 sender 不 spawn；accessible 标签齐全 (covers: S2)
- [x] T4: 验证 — acceptance: `cargo test` 与 `python scripts/static_check.py` 通过；构建无新增 binding loop 警告；本 spec 文件待提交时纳入版本控制（当前工作区未跟踪，提交时 `git add docs/compose/spec/proxy-status.md`） (covers: S2)
