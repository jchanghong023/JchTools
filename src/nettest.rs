//! 网络连通性测试：仅在用户点击时向固定目标站点发起 TCP/443 探测。
//! 不上传数据、不执行任意用户命令；失败时给出可操作的排查提示。
//! Windows 本机与 WSL2 内部分别探测（WSL2 有独立网络命名空间，必须在发行版内测）。

use std::net::{TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// 默认单站探测超时（毫秒）。
pub const DEFAULT_TIMEOUT_MS: u64 = 5_000;

/// 同时最多挂起的 DNS 解析线程数。超过后直接报繁忙并拒绝本次解析，避免无限堆积阻塞线程。
const MAX_PENDING_DNS: usize = 4;
/// 当前挂起（尚未把结果送回 channel）的 DNS 解析线程计数。
static PENDING_DNS: AtomicUsize = AtomicUsize::new(0);

/// 探测目标站点。
pub struct ProbeTarget {
    pub id: &'static str,
    pub name: &'static str,
    pub host: &'static str,
    /// 失败时追加的站点相关提示。
    pub tip: &'static str,
}

/// 固定三站：ChatGPT、Google、GitHub。
pub const TARGETS: &[ProbeTarget] = &[
    ProbeTarget {
        id: "chatgpt",
        name: "ChatGPT",
        host: "chatgpt.com",
        tip: "ChatGPT 在部分地区需代理才能访问；可先检查本机 VPN/代理与系统代理设置。",
    },
    ProbeTarget {
        id: "google",
        name: "Google",
        host: "www.google.com",
        tip: "Google 在部分地区需代理；若仅本机失败而 WSL2 成功，优先核对 Windows 侧代理与防火墙。",
    },
    ProbeTarget {
        id: "github",
        name: "GitHub",
        host: "github.com",
        tip: "GitHub 访问异常时常见原因是 DNS 污染或间歇中断；可尝试更换 DNS 或稍后重试。",
    },
];

/// 单站结果状态（界面文案与此对齐）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeStatus {
    /// 未开始或未得到结论
    Unknown,
    /// 可以连接
    Reachable,
    /// 不可以用
    Unreachable,
}

impl ProbeStatus {
    pub fn label(self) -> &'static str {
        match self {
            ProbeStatus::Unknown => "未测试",
            ProbeStatus::Reachable => "可以连接",
            ProbeStatus::Unreachable => "不可以用",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ProbeResult {
    pub id: String,
    pub name: String,
    pub host: String,
    pub status: ProbeStatus,
    /// 探测耗时（微秒）；失败时为 None。小于 1000 时界面显示「<1 ms」。
    pub latency_us: Option<u64>,
    /// 人类可读的结论/错误摘要。
    pub message: String,
    /// 失败排查提示（可能为空）。
    pub tips: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct NetTestReport {
    /// 探测发生的位置：`Windows`（非 Windows 平台为 `std::env::consts::OS`）或 `WSL2:<发行版>`。
    pub scope: String,
    pub results: Vec<ProbeResult>,
    /// 报告级备注（如 WSL 不可用、部分目标跳过等）。
    pub notes: Vec<String>,
}

/// 本机探测报告的 scope 标签：Windows 平台保持品牌值 `Windows`，
/// 其它平台如实返回 OS 名（run_local 与 UI 的报告匹配共用这一个口径）。
pub fn local_scope_label() -> &'static str {
    if cfg!(windows) { "Windows" } else { std::env::consts::OS }
}

/// 供界面循环探测的目标列表（id / 显示名 / 主机）。
pub fn targets() -> Vec<(&'static str, &'static str, &'static str)> {
    TARGETS
        .iter()
        .map(|t| (t.id, t.name, t.host))
        .collect()
}

/// 按 id 查找内置目标。
pub fn find_target(id: &str) -> Option<&'static ProbeTarget> {
    TARGETS.iter().find(|t| t.id == id)
}

/// 组装失败提示：通用网络排查 + 站点专用提示。
pub fn failure_tips(host: &str, extra: Option<&str>) -> Vec<String> {
    let mut tips = vec![
        "检查本机是否已连接互联网。".to_string(),
        format!("确认能否在浏览器打开 https://{host}。"),
        "若已配置 VPN/代理，请确认客户端在运行且系统代理/环境变量已按需开启。".to_string(),
        "查看「状态」页中的环境变量、系统代理与 VPN 进程是否符合预期。".to_string(),
    ];
    if let Some(extra) = extra {
        tips.push(extra.to_string());
    }
    tips
}

/// 在本机（Windows 主机进程视角）探测 host:443 的 TCP 连通性与耗时（微秒）。
/// 成功仅表示能建立到 443 的 TCP 连接，不代表完整 HTTPS 业务可用。
/// DNS 解析放在独立线程 + `recv_timeout`，避免解析器无限阻塞。
/// DNS 与所有地址 connect 共享同一总 deadline，避免总耗时 = timeout × (1+N)。
pub fn probe_host(host: &str, timeout: Duration) -> Result<u64, String> {
    let start = Instant::now();
    let deadline = start + timeout;
    let addrs = resolve_with_timeout(host, 443, timeout)?;
    let mut last_err = String::from("无可用地址");
    let n = addrs.len().max(1) as u32;
    for addr in addrs {
        // 每地址独立预算：坏 IPv6 不得吃光总超时导致 IPv4 从未被试（双栈黑洞常见误报）。
        let total_remaining = deadline.saturating_duration_since(Instant::now());
        if total_remaining.is_zero() {
            last_err = "探测总超时已耗尽".to_string();
            break;
        }
        let per_addr = total_remaining / n;
        let budget = per_addr.max(Duration::from_millis(50)).min(total_remaining);
        match TcpStream::connect_timeout(&addr, budget) {
            Ok(_) => {
                // 微秒：本机/局域网连接常常 <1ms，用毫秒会全是 0，看起来像坏了。
                let us = start.elapsed().as_micros() as u64;
                return Ok(us);
            }
            Err(e) => last_err = e.to_string(),
        }
    }
    Err(format!("TCP 443 连接失败：{last_err}"))
}

/// 带宿主超时的 DNS 解析：`to_socket_addrs` 本身无超时，放到独立线程里等。
/// 用全局 AtomicUsize 限制同时最多 N 个挂起 DNS 解析；超过时直接报繁忙并拒绝，
/// 避免 DNS 故障环境下每次探测留下永久阻塞的工作线程。
fn resolve_with_timeout(
    host: &str,
    port: u16,
    timeout: Duration,
) -> Result<Vec<std::net::SocketAddr>, String> {
    // 先检查并发上限：若已有 MAX_PENDING_DNS 个挂起解析，不再启动新线程。
    let prev = PENDING_DNS.fetch_add(1, Ordering::SeqCst);
    if prev >= MAX_PENDING_DNS {
        PENDING_DNS.fetch_sub(1, Ordering::SeqCst);
        return Err(format!(
            "DNS 解析繁忙（已有 {MAX_PENDING_DNS} 个解析挂起未完成），请稍后重试"
        ));
    }

    let host = host.to_string();
    // 槽位只允许释放一次：超时返回后工作线程可能仍阻塞，不得再次 fetch_sub。
    // to_socket_addrs 无法取消；残留线程只占 OS 资源，但 PENDING_DNS 会复用。
    let released = Arc::new(AtomicBool::new(false));
    let released_worker = released.clone();
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let result = (host.as_str(), port)
            .to_socket_addrs()
            .map(|iter| iter.collect::<Vec<_>>())
            .map_err(|e| format!("DNS 解析失败：{e}"));
        if !released_worker.swap(true, Ordering::SeqCst) {
            PENDING_DNS.fetch_sub(1, Ordering::SeqCst);
        }
        let _ = sender.send(result);
    });
    let release_slot = |released: &Arc<AtomicBool>| {
        if !released.swap(true, Ordering::SeqCst) {
            PENDING_DNS.fetch_sub(1, Ordering::SeqCst);
        }
    };
    match receiver.recv_timeout(timeout) {
        Ok(Ok(addrs)) if !addrs.is_empty() => Ok(addrs),
        Ok(Ok(_)) => {
            release_slot(&released);
            Err("DNS 未返回地址".to_string())
        }
        Ok(Err(error)) => {
            release_slot(&released);
            Err(error)
        }
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            // 超时后不释放槽位：工作线程可能仍阻塞在 to_socket_addrs；
            // 若此刻 release，会允许新 spawn，使僵尸线程无界堆积。由 worker 完成时释放。
            Err(format!("DNS 解析超时（超过 {} ms）", timeout.as_millis()))
        }
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            release_slot(&released);
            Err("DNS 解析线程异常退出".to_string())
        }
    }
}

/// 把微秒耗时格式化成界面文案；小于 1ms 时显示「<1 ms」。
pub fn format_latency_us(us: u64) -> String {
    if us < 1000 {
        "<1 ms".to_string()
    } else {
        format!("{} ms", us / 1000)
    }
}

/// 对内置三站在本机依次探测。
pub fn run_local(timeout_ms: u64) -> NetTestReport {
    let timeout = Duration::from_millis(timeout_ms.max(1));
    let mut results = Vec::with_capacity(TARGETS.len());
    for target in TARGETS {
        match probe_host(target.host, timeout) {
            Ok(us) => results.push(ProbeResult {
                id: target.id.into(),
                name: target.name.into(),
                host: target.host.into(),
                status: ProbeStatus::Reachable,
                latency_us: Some(us),
                message: format!("可以连接 · {}", format_latency_us(us)),
                tips: Vec::new(),
            }),
            Err(error) => results.push(ProbeResult {
                id: target.id.into(),
                name: target.name.into(),
                host: target.host.into(),
                status: ProbeStatus::Unreachable,
                latency_us: None,
                message: format!("不可以用 · {error}"),
                tips: failure_tips(target.host, Some(target.tip)),
            }),
        }
    }
    NetTestReport {
        scope: local_scope_label().into(),
        results,
        notes: vec!["仅探测 TCP 443 建连与耗时；成功不保证网页内容完整可用。".into()],
    }
}

/// 解析 `wsl.exe -l -q` 输出：去掉 BOM、CRLF，过滤空行与尾部 NUL。
pub fn parse_wsl_distros(text: &str) -> Vec<String> {
    let cleaned = text.replace('\0', "").trim_start_matches('\u{feff}').to_string();
    cleaned
        .lines()
        .map(|line| line.trim().trim_end_matches('\0').trim())
        .filter(|line| !line.is_empty())
        .map(|line| line.to_string())
        .collect()
}

/// 优先挑选默认 WSL 发行版：Ubuntu* 优先，其次任意第一个可用项。
pub fn prefer_wsl_distro(distros: &[String]) -> Option<String> {
    if distros.is_empty() {
        return None;
    }
    for preferred in ["Ubuntu", "Ubuntu-24.04", "Ubuntu-22.04", "Ubuntu-20.04", "Debian"] {
        if let Some(hit) = distros.iter().find(|d| d.eq_ignore_ascii_case(preferred)) {
            return Some(hit.clone());
        }
    }
    distros
        .iter()
        .find(|d| d.to_ascii_lowercase().contains("ubuntu"))
        .cloned()
        .or_else(|| distros.first().cloned())
}

/// 在指定 WSL 发行版内对三站做 bash `/dev/tcp` 探测。
/// 输出约定每行：`OK <host> <ms>` 或 `FAIL <host> <reason>`。
/// 注意：脚本必须经 **stdin** 交给 `bash -s`，不能走 `wsl -- bash -c <脚本>`：
/// 在 Windows 上 wsl.exe 会破坏命令行参数里的 `$h` 等 shell 变量，导致主机名为空、结果无法解析。
pub fn wsl_probe_script(timeout_ms: u64) -> String {
    // timeout 秒向上取整，至少 1 秒；只用 bash 内建 / 常见 coreutils，不依赖 curl/python。
    let secs = ((timeout_ms.max(1) + 999) / 1000).max(1);
    let hosts = TARGETS
        .iter()
        .map(|t| t.host)
        .collect::<Vec<_>>()
        .join(" ");
    // 统一 LF；避免把 CRLF 写进 stdin 造成 `exit 0\r` 之类的解析错误。
    // 耗时用微秒：EPOCHREALTIME（bash≥5）优先，退回 date +%s%N；两者都不可用时
    // 计 0，解析层据此显示「耗时不可测」而不是伪造 <1 ms。
    // 优先 `timeout` 命令；缺失时用后台任务 + sleep/kill 的 bash 内建兜底，避免强依赖 coreutils。
    format!(
        "set +e\n\
         for h in {hosts}; do\n\
         \x20 if [[ -n \"${{EPOCHREALTIME-}}\" ]]; then start=$EPOCHREALTIME; else start=$(date +%s%N 2>/dev/null || echo 0); fi\n\
         \x20 ok=0\n\
         \x20 if command -v timeout >/dev/null 2>&1; then\n\
         \x20   if timeout {secs} bash -c \"echo >/dev/tcp/${{h}}/443\" >/dev/null 2>&1; then ok=1; fi\n\
         \x20 else\n\
         \x20   bash -c \"echo >/dev/tcp/${{h}}/443\" >/dev/null 2>&1 &\n\
         \x20   pid=$!\n\
         \x20   ( sleep {secs}; kill \"$pid\" 2>/dev/null ) &\n\
         \x20   killer=$!\n\
         \x20   if wait \"$pid\" 2>/dev/null; then ok=1; fi\n\
         \x20   kill \"$killer\" 2>/dev/null\n\
         \x20   wait \"$killer\" 2>/dev/null\n\
         \x20 fi\n\
         \x20 if [ \"$ok\" = 1 ]; then\n\
         \x20   if [[ -n \"${{EPOCHREALTIME-}}\" ]]; then end=$EPOCHREALTIME; else end=$(date +%s%N 2>/dev/null || echo 0); fi\n\
         \x20   us=$(awk -v a=\"$start\" -v b=\"$end\" 'BEGIN{{ d=b-a; if (d<0) d=0; if (a ~ /\\./) printf \"%.0f\", d*1000000; else printf \"%.0f\", d/1000 }}')\n\
         \x20   echo \"OK ${{h}} ${{us}}\"\n\
         \x20 else\n\
         \x20   echo \"FAIL ${{h}} tcp_or_dns\"\n\
         \x20 fi\n\
         done\n\
         exit 0\n",
        hosts = hosts,
        secs = secs
    )
}

/// 解析 WSL 探测脚本输出为按 host 键控的结果行。
pub fn parse_wsl_probe_lines(text: &str) -> Vec<(String, Result<u64, String>)> {
    let mut out = Vec::new();
    for line in text.lines() {
        // wsl.exe 有时以 UTF-16 输出；解码后可能残留 NUL / BOM。
        let line = line.trim_matches(|c| c == '\0' || c == '\u{feff}').trim();
        let mut parts = line.split_whitespace();
        let Some(kind) = parts.next() else { continue };
        let Some(host) = parts.next() else { continue };
        // 畸形行（如参数被破坏后的 `FAIL  tcp_or_dns`）会把 reason 当成 host；
        // 主机名必须含点才算有效，避免假解析。
        if !host.contains('.') || host.starts_with('-') {
            continue;
        }
        match kind {
            "OK" => {
                // 缺字段/不可解析不得伪造成 0μs 可达：会把异常链路显示成「<1 ms」。
                let Some(ms) = parts.next().and_then(|v| v.parse::<u64>().ok()) else {
                    out.push((
                        host.to_string(),
                        Err("WSL 内 OK 响应缺少可解析的耗时字段".to_string()),
                    ));
                    continue;
                };
                out.push((host.to_string(), Ok(ms)));
            }
            "FAIL" => {
                let reason = parts.collect::<Vec<_>>().join(" ");
                let reason = if reason.is_empty() {
                    "WSL 内 TCP 连接失败".to_string()
                } else {
                    format!("WSL 内 TCP 连接失败（{reason}）")
                };
                out.push((host.to_string(), Err(reason)));
            }
            _ => {}
        }
    }
    out
}

/// 把 WSL 原始输出装配成与本机一致的报告。
pub fn assemble_wsl_report(
    distro: &str,
    script_output: &str,
    notes: Vec<String>,
) -> NetTestReport {
    let parsed = parse_wsl_probe_lines(script_output);
    let mut notes = notes;
    if parsed.is_empty() && !script_output.trim().is_empty() {
        // 有输出却解析不到：多半是编码/畸形行，把原文摘要放进备注便于排查。
        let head: String = script_output
            .trim()
            .chars()
            .filter(|c| !c.is_control() || *c == '\n')
            .take(200)
            .collect();
        notes.push(format!("WSL 原始输出未能解析：{head}"));
    }
    let mut results = Vec::with_capacity(TARGETS.len());
    for target in TARGETS {
        let hit = parsed
            .iter()
            .find(|(host, _)| host.eq_ignore_ascii_case(target.host))
            .cloned();
        match hit {
            // 脚本在 bash<5（无 EPOCHREALTIME）且 date 不支持 %s%N 时回退计 0：
            // 可达性为真，但耗时是占位值，必须显示「不可测」，不得伪造成 <1 ms。
            Some((_, Ok(0))) => results.push(ProbeResult {
                id: target.id.into(),
                name: target.name.into(),
                host: target.host.into(),
                status: ProbeStatus::Reachable,
                latency_us: None,
                message: "可以连接 · 耗时不可测（发行版缺少微秒级时间源）".into(),
                tips: Vec::new(),
            }),
            Some((_, Ok(us))) => results.push(ProbeResult {
                id: target.id.into(),
                name: target.name.into(),
                host: target.host.into(),
                status: ProbeStatus::Reachable,
                latency_us: Some(us),
                message: format!("可以连接 · {}", format_latency_us(us)),
                tips: Vec::new(),
            }),
            Some((_, Err(error))) => results.push(ProbeResult {
                id: target.id.into(),
                name: target.name.into(),
                host: target.host.into(),
                status: ProbeStatus::Unreachable,
                latency_us: None,
                message: format!("不可以用 · {error}"),
                tips: failure_tips(target.host, Some(target.tip)),
            }),
            None => results.push(ProbeResult {
                id: target.id.into(),
                name: target.name.into(),
                host: target.host.into(),
                status: ProbeStatus::Unknown,
                latency_us: None,
                message: "未在 WSL 输出中得到该站点结果".into(),
                tips: failure_tips(target.host, Some("可在 WSL 终端手动执行 bash /dev/tcp 探测确认。")),
            }),
        }
    }
    NetTestReport {
        scope: format!("WSL2:{distro}"),
        results,
        notes,
    }
}

/// 外部查询类命令（wsl -l 等）的宿主总超时。
const LIST_CMD_TIMEOUT: Duration = Duration::from_secs(15);

#[cfg(windows)]
fn run_capture_no_window(program: &std::path::Path, args: &[&str]) -> Result<String, String> {
    use std::os::windows::process::CommandExt;
    let mut command = std::process::Command::new(program);
    command
        .args(args)
        .creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    let output = crate::process::run_with_timeout(&mut command, LIST_CMD_TIMEOUT)
        .map_err(|e| e.to_string())?;
    let display = program.display().to_string();
    if let Some(note) = output.truncation_note() {
        return Err(format!("{display} {note}"));
    }
    // wsl.exe 可能以 UTF-16 输出；与 run_wsl 共用 decode_wsl_bytes，避免两套解码分叉。
    // wsl.exe 把自身错误诊断写进 stdout（UTF-16、非零退出码）：非零退出码时不得把
    // 错误文本当发行版列表解析（会出现幻影条目），统一作为失败返回。
    let text = decode_wsl_bytes(&output.stdout);
    if !output.status.success() {
        let detail = if !text.trim().is_empty() {
            text
        } else {
            decode_wsl_bytes(&output.stderr)
        };
        return Err(format!(
            "{display} 退出码 {:?}：{}",
            output.status.code(),
            detail.trim()
        ));
    }
    Ok(text)
}

#[cfg(not(windows))]
fn run_capture_no_window(program: &std::path::Path, args: &[&str]) -> Result<String, String> {
    let mut command = std::process::Command::new(program);
    command.args(args);
    let output = crate::process::run_with_timeout(&mut command, LIST_CMD_TIMEOUT)
        .map_err(|e| e.to_string())?;
    // 与 Windows 版同口径：非零退出码时输出不得当成功结果解析。
    if !output.status.success() {
        let detail = if output.stdout.is_empty() {
            String::from_utf8_lossy(&output.stderr).into_owned()
        } else {
            String::from_utf8_lossy(&output.stdout).into_owned()
        };
        return Err(format!(
            "{} 退出码 {:?}：{}",
            program.display(),
            output.status.code(),
            detail.trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// 列出可用 WSL 发行版（仅 Windows 宿主有意义）。
#[cfg(windows)]
pub fn list_wsl_distros() -> Result<Vec<String>, String> {
    let text = run_capture_no_window(&crate::process::system_tool("wsl.exe"), &["-l", "-q"])?;
    let distros = parse_wsl_distros(&text);
    if distros.is_empty() {
        Err("未检测到已安装的 WSL 发行版".into())
    } else {
        Ok(distros)
    }
}

#[cfg(not(windows))]
pub fn list_wsl_distros() -> Result<Vec<String>, String> {
    Err("当前平台不支持在宿主上枚举 WSL 发行版".into())
}

/// 在指定发行版内跑三站探测并组装报告。
/// 脚本经 stdin 交给 `bash -s`：避免 wsl.exe 破坏 `-c` 参数中的 `$h`。
/// 宿主侧总超时 ≈ `timeout_ms * 3 + 15s`（三站脚本 + 启动/通信余量）。
#[cfg(windows)]
pub fn run_wsl(distro: &str, timeout_ms: u64) -> NetTestReport {
    use std::os::windows::process::CommandExt;
    use std::process::Command;

    let script = wsl_probe_script(timeout_ms);
    let host_timeout = Duration::from_millis(timeout_ms.saturating_mul(3).saturating_add(15_000));
    let wsl = crate::process::system_tool("wsl.exe");
    let mut command = Command::new(&wsl);
    command
        .args(["-d", distro, "--", "bash", "-s"])
        .creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    let output = match crate::process::run_with_timeout_input(
        &mut command,
        Some(script.as_bytes()),
        host_timeout,
    ) {
        Ok(output) => output,
        Err(error) => {
            return wsl_run_error_report(distro, &error.to_string());
        }
    };
    // 探测输出被截断说明结果不完整：报给用户而不是静默解析缺项列表。
    if let Some(note) = output.truncation_note() {
        return wsl_run_error_report(distro, &note);
    }
    let mut out = decode_wsl_bytes(&output.stdout);
    let err = decode_wsl_bytes(&output.stderr);
    if out.trim().is_empty() && !err.trim().is_empty() {
        // 无 stdout 时把 stderr 并入，避免变成毫无信息的「未测试」。
        out = format!("{out}\n{err}");
    }
    if out.trim().is_empty() {
        return wsl_run_error_report(
            distro,
            &format!(
                "发行版 {distro} 返回空输出（退出码 {:?}）",
                output.status.code()
            ),
        );
    }
    assemble_wsl_report(
        distro,
        &out,
        vec!["在 WSL 发行版内探测 TCP 443；与 Windows 宿主网络可能不同。".into()],
    )
}

/// wsl.exe 字节流解码：BOM → UTF-16 启发式 → 严格 UTF-8 → UTF-16LE 重试 → lossy。
/// 仅靠「NUL 占比 ≥ 一半」会在 CJK 发行版名（高位字节非 0）上失效，故增加奇数位 NUL 与 BOM 证据。
/// 纯函数，非 Windows 也可编译，便于单测。
#[allow(dead_code)] // 调用方在 windows 专用路径；非 Windows 仅测试使用
fn decode_wsl_bytes(raw: &[u8]) -> String {
    if raw.is_empty() {
        return String::new();
    }
    if raw.starts_with(&[0xFF, 0xFE]) {
        let units: Vec<u16> = raw[2..]
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        return String::from_utf16_lossy(&units);
    }
    if raw.starts_with(&[0xFE, 0xFF]) {
        let units: Vec<u16> = raw[2..]
            .chunks_exact(2)
            .map(|c| u16::from_be_bytes([c[0], c[1]]))
            .collect();
        return String::from_utf16_lossy(&units);
    }
    let nulls = raw.iter().filter(|b| **b == 0).count();
    let odd_nulls = raw.iter().enumerate().filter(|(i, b)| i % 2 == 1 && **b == 0).count();
    let looks_utf16 = raw.len() % 2 == 0
        && (nulls * 2 >= raw.len().max(1) || odd_nulls * 4 >= raw.len().max(1));
    if looks_utf16 {
        let units: Vec<u16> = raw
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        return String::from_utf16_lossy(&units);
    }
    match std::str::from_utf8(raw) {
        Ok(text) => text.to_string(),
        Err(_) if raw.len() % 2 == 0 => {
            let units: Vec<u16> = raw
                .chunks_exact(2)
                .map(|c| u16::from_le_bytes([c[0], c[1]]))
                .collect();
            String::from_utf16_lossy(&units)
        }
        Err(_) => String::from_utf8_lossy(raw).into_owned(),
    }
}

#[cfg(windows)]
fn wsl_run_error_report(distro: &str, error: &str) -> NetTestReport {
    NetTestReport {
        scope: format!("WSL2:{distro}"),
        results: TARGETS
            .iter()
            .map(|t| ProbeResult {
                id: t.id.into(),
                name: t.name.into(),
                host: t.host.into(),
                // 环境不可用（无法启动发行版/输出截断/空输出）是「未得到结论」，
                // 不是站点不可达：标 Unknown 让汇总结为「未测试」，不得误报「无法连接」。
                status: ProbeStatus::Unknown,
                latency_us: None,
                message: format!("无法在 WSL 内执行探测 · {error}"),
                tips: {
                    let mut tips = failure_tips(t.host, None);
                    tips.push("确认已安装 WSL2 且发行版处于可用状态（可运行 `wsl -l -v` 查看）。".into());
                    let check = if distro.is_empty() { "Ubuntu".to_string() } else { distro.to_string() };
                    tips.push(format!("在终端执行 `wsl -d {check}` 验证发行版能否启动。"));
                    tips
                },
            })
            .collect(),
        notes: vec![error.to_string()],
    }
}

#[cfg(not(windows))]
pub fn run_wsl(_distro: &str, _timeout_ms: u64) -> NetTestReport {
    NetTestReport {
        scope: "WSL2".into(),
        results: TARGETS
            .iter()
            .map(|t| ProbeResult {
                id: t.id.into(),
                name: t.name.into(),
                host: t.host.into(),
                status: ProbeStatus::Unknown,
                latency_us: None,
                message: "当前平台不支持从宿主调用 WSL 探测".into(),
                tips: Vec::new(),
            })
            .collect(),
        notes: vec!["WSL 探测仅在 Windows 宿主上可用。".into()],
    }
}

/// 空发行版时的错误报告（未装 WSL / 列表为空）。
fn wsl_missing_report(error: String) -> NetTestReport {
    NetTestReport {
        scope: "WSL2".into(),
        results: TARGETS
            .iter()
            .map(|t| ProbeResult {
                id: t.id.into(),
                name: t.name.into(),
                host: t.host.into(),
                // 未安装 WSL 属环境不可用：整组 Unknown（未测试），不得标成站点不可达。
                status: ProbeStatus::Unknown,
                latency_us: None,
                message: format!("未选择或未找到可用 WSL 发行版 · {error}"),
                tips: {
                    let mut tips = failure_tips(t.host, None);
                    tips.push("在「应用」中启用「适用于 Linux 的 Windows 子系统」，或安装发行版。".into());
                    tips.push("命令提示：wsl --install -d Ubuntu".into());
                    tips.push("安装后在本页「发行版」下拉中刷新并选择要测试的实例。".into());
                    tips
                },
            })
            .collect(),
        notes: vec![error],
    }
}

/// 统一入口：scope 0=Windows 本机，1=WSL2。
/// `distro` 为用户选中的发行版名；为空时再按 Ubuntu* 优先自动挑选。
pub fn run(scope: i32, distro: &str, timeout_ms: u64) -> NetTestReport {
    if scope == 1 {
        let selected = distro.trim().to_string();
        let distro = if !selected.is_empty() {
            selected
        } else {
            #[cfg(windows)]
            {
                match list_wsl_distros() {
                    Ok(distros) => {
                        prefer_wsl_distro(&distros).unwrap_or_else(|| "Ubuntu".into())
                    }
                    Err(error) => return wsl_missing_report(error),
                }
            }
            #[cfg(not(windows))]
            {
                return run_wsl("", timeout_ms);
            }
        };
        return run_wsl(&distro, timeout_ms);
    }
    run_local(timeout_ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets_cover_three_sites() {
        let list = targets();
        assert_eq!(list.len(), 3);
        let ids: Vec<&str> = list.iter().map(|(id, _, _)| *id).collect();
        assert_eq!(ids, vec!["chatgpt", "google", "github"]);
        assert_eq!(find_target("google").unwrap().host, "www.google.com");
        assert!(find_target("nope").is_none());
    }

    #[test]
    fn parse_wsl_distros_handles_bom_crlf_and_nul() {
        let sample = "\u{feff}Ubuntu\r\n\0Ubuntu-22.04\0\n\ndebian\r\n";
        let rows = parse_wsl_distros(sample);
        assert_eq!(rows, vec!["Ubuntu", "Ubuntu-22.04", "debian"]);
    }

    #[test]
    fn prefer_wsl_prefers_ubuntu_family() {
        let distros = vec!["Debian".into(), "Ubuntu-22.04".into(), "Alpine".into()];
        assert_eq!(prefer_wsl_distro(&distros).as_deref(), Some("Ubuntu-22.04"));
        let only = vec!["Alpine".into()];
        assert_eq!(prefer_wsl_distro(&only).as_deref(), Some("Alpine"));
        assert!(prefer_wsl_distro(&[]).is_none());
    }

    #[test]
    fn parse_wsl_probe_lines_reads_ok_and_fail() {
        let out = "OK chatgpt.com 12\nFAIL www.google.com tcp_or_dns\nOK github.com 8\nnoise\n";
        let rows = parse_wsl_probe_lines(out);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].0, "chatgpt.com");
        assert_eq!(rows[0].1.as_ref().unwrap(), &12u64);
        assert!(rows[1].1.is_err());
        assert!(rows[1].1.as_ref().unwrap_err().contains("WSL"));
        assert_eq!(rows[2].1.as_ref().unwrap(), &8u64);
    }

    #[test]
    fn assemble_wsl_report_fills_all_targets() {
        let out = "OK chatgpt.com 10\nFAIL www.google.com tcp_or_dns\n";
        let report = assemble_wsl_report("Ubuntu", out, vec!["备注".into()]);
        assert_eq!(report.scope, "WSL2:Ubuntu");
        assert_eq!(report.results.len(), 3);
        assert_eq!(report.results[0].status, ProbeStatus::Reachable);
        assert_eq!(report.results[1].status, ProbeStatus::Unreachable);
        assert_eq!(report.results[2].status, ProbeStatus::Unknown);
        assert!(report.results[1].tips.iter().any(|t| t.contains("代理") || t.contains("ChatGPT") || t.contains("GitHub") || t.contains("浏览器") || t.contains("互联网")));
    }

    #[test]
    fn failure_tips_include_host_and_optional_extra() {
        let tips = failure_tips("github.com", Some("站点提示"));
        assert!(tips.iter().any(|t| t.contains("https://github.com")));
        assert!(tips.iter().any(|t| t == "站点提示"));
    }

    #[test]
    fn wsl_probe_script_lists_hosts_and_timeout() {
        let script = wsl_probe_script(5000);
        assert!(script.contains("chatgpt.com"));
        assert!(script.contains("www.google.com"));
        assert!(script.contains("github.com"));
        assert!(script.contains("/dev/tcp"));
        assert!(script.contains("timeout 5"));
        assert!(script.contains("command -v timeout"), "无 timeout 命令时需有 bash 内建兜底");
        assert!(script.contains("kill"), "兜底路径应能终止挂起的探测子进程");
        assert!(!script.contains('\r'), "脚本必须 LF，避免经 stdin 时出现 exit 0\\r");
        // 必须包含 bash 变量展开；若误写成无 $ 的字面量，WSL 侧会得到空主机名。
        assert!(script.contains("${h}"));
    }

    #[test]
    fn parse_wsl_probe_lines_ignores_empty_host_rows() {
        // 历史 bug：bash -c 参数被破坏后输出 `FAIL  tcp_or_dns`（无主机名），不得当作有效结果。
        let rows = parse_wsl_probe_lines("FAIL  tcp_or_dns\nFAIL\ngarbage\nOK not-a-host 1\n");
        assert!(rows.is_empty());
        let ok = parse_wsl_probe_lines("OK chatgpt.com 12\n");
        assert_eq!(ok.len(), 1);
        assert_eq!(ok[0].0, "chatgpt.com");
    }

    #[test]
    fn wsl_probe_script_converts_epochrealtime_to_microseconds() {
        let script = wsl_probe_script(1000);
        assert!(script.contains("d*1000000") || script.contains("d*1e6"), "EPOCHREALTIME 秒差必须乘到微秒");
        assert!(script.contains("a ~ /\\./"), "需按 start 是否含小数点区分时间基");
        // 复刻脚本 awk 逻辑：EPOCHREALTIME 始终含小数点（如 100.500000）；纳秒路径为整数字符串。
        let awk_us = |a: &str, b: &str| -> u64 {
            let a_n: f64 = a.parse().unwrap();
            let b_n: f64 = b.parse().unwrap();
            let d = b_n - a_n;
            let d = if d < 0.0 { 0.0 } else { d };
            if a.contains('.') {
                (d * 1_000_000.0).round() as u64
            } else {
                (d / 1000.0).round() as u64
            }
        };
        assert_eq!(awk_us("100.500000", "102.500000"), 2_000_000);
        assert_eq!(awk_us("1000000000", "3000000000"), 2_000_000);
    }

    #[test]
    fn parse_wsl_probe_lines_ok_without_latency_is_failure() {
        let missing = parse_wsl_probe_lines("OK chatgpt.com\n");
        assert_eq!(missing.len(), 1);
        assert!(missing[0].1.is_err(), "缺耗时的 OK 行不得标成可达 0μs");
        let bad = parse_wsl_probe_lines("OK chatgpt.com notanumber\n");
        assert!(bad[0].1.is_err());
        let ok = parse_wsl_probe_lines("OK chatgpt.com 2000000\n");
        assert_eq!(ok[0].1.as_ref().ok().copied(), Some(2_000_000));
    }

    #[test]
    fn decode_wsl_bytes_handles_bom_and_cjk_utf16() {
        // UTF-16LE + BOM
        let mut le = vec![0xFF, 0xFE];
        for u in "Ubuntu-22.04".encode_utf16() { le.extend_from_slice(&u.to_le_bytes()); }
        assert_eq!(decode_wsl_bytes(&le), "Ubuntu-22.04");

        // UTF-16BE + BOM
        let mut be = vec![0xFE, 0xFF];
        for u in "Debian".encode_utf16() { be.extend_from_slice(&u.to_be_bytes()); }
        assert_eq!(decode_wsl_bytes(&be), "Debian");

        // 无 BOM 的 ASCII UTF-16LE：奇数位 NUL 占比高
        let mut ascii16 = Vec::new();
        for u in "OK host 12".encode_utf16() { ascii16.extend_from_slice(&u.to_le_bytes()); }
        assert_eq!(decode_wsl_bytes(&ascii16), "OK host 12");

        // CJK 混合：高位字节非 0，靠 odd_nulls 启发式
        let mut cjk = Vec::new();
        for u in "中文终端".encode_utf16() { cjk.extend_from_slice(&u.to_le_bytes()); }
        assert_eq!(decode_wsl_bytes(&cjk), "中文终端");

        // 严格 UTF-8 保持原样
        assert_eq!(decode_wsl_bytes(b"plain utf8 text"), "plain utf8 text");

        // 非法 UTF-8 偶长度：回退 UTF-16LE lossy（不 panic）
        let _ = decode_wsl_bytes(&[0xFF, 0x00, 0xFE, 0x01]);
        // 非法 UTF-8 奇长度：lossy UTF-8（不 panic）
        let _ = decode_wsl_bytes(&[0xC0, 0x80, 0x41]);
    }

    #[test]
    fn assemble_wsl_report_notes_raw_when_unparseable() {
        let report = assemble_wsl_report("Ubuntu", "weird output\n", vec![]);
        assert!(report.notes.iter().any(|n| n.contains("未能解析")));
        assert_eq!(report.results[0].status, ProbeStatus::Unknown);
    }

    #[test]
    fn run_local_without_network_does_not_panic() {
        // 只验证结构完整；真实连通性随网络变化，不断言可达性。
        let report = run_local(100);
        assert_eq!(report.scope, local_scope_label());
        assert_eq!(report.results.len(), 3);
        for row in &report.results {
            assert!(!row.message.is_empty());
            if row.status == ProbeStatus::Unreachable {
                assert!(!row.tips.is_empty(), "失败必须给出排查提示");
            }
        }
    }

    #[test]
    fn format_latency_us_shows_sub_millisecond() {
        assert_eq!(format_latency_us(0), "<1 ms");
        assert_eq!(format_latency_us(999), "<1 ms");
        assert_eq!(format_latency_us(1500), "1 ms");
        assert_eq!(format_latency_us(250_000), "250 ms");
    }

    #[test]
    fn run_windows_scope_ignores_distro_argument() {
        // scope=0 时不读 WSL，distro 参数可任意。
        let report = run(0, "Ubuntu-22.04", 100);
        assert_eq!(report.scope, local_scope_label());
        assert_eq!(report.results.len(), 3);
    }

    #[test]
    fn wsl_missing_report_lists_actionable_tips() {
        let report = wsl_missing_report("未检测到已安装的 WSL 发行版".into());
        assert_eq!(report.scope, "WSL2");
        assert_eq!(report.results.len(), 3);
        assert!(report.results[0].tips.iter().any(|t| t.contains("wsl --install")));
        // 回归（X-03/X-07）：WSL 环境不可用是「未得到结论」，不是「站点不可达」；
        // 三站不得被标成 Unreachable（否则汇总显示「3 个无法连接」误导排查方向）。
        assert!(report.results.iter().all(|r| r.status == ProbeStatus::Unknown),
            "环境不可用必须整组标记 Unknown（未测试），不得标 Unreachable");
    }

    #[test]
    fn resolve_with_timeout_returns_error_for_invalid_host() {
        // 使用含非法字符的主机名，确保 DNS 解析必然失败，不依赖外部 DNS 行为。
        let result = resolve_with_timeout(
            "invalid\x00hostname",
            443,
            Duration::from_millis(2000),
        );
        assert!(result.is_err(), "非法主机名应返回解析错误");
        // 不做全局槽位断言：并行测试的在途解析可能瞬时占满全部槽位，任何时点的全局
        // 快照在此处都存在竞态。非法主机名走快速失败路径，worker 结束即释放槽位，
        // 不会永久占位；「恰好释放一次」由 acquire/release 的 swap 仲裁逻辑保证。
    }

    #[test]
    fn probe_host_shares_deadline_across_addresses() {
        // 用极短超时验证 deadline 逻辑存在：即使 DNS 成功，多地址 connect 也不会叠乘超时。
        let result = probe_host("127.0.0.1", Duration::from_millis(300));
        // 127.0.0.1:443 可能不通（测试环境），也可能通；只验证不 panic 且结构正确。
        match result {
            Ok(us) => {
                // 成功时耗时应 < 300ms。
                assert!(us < 300_000, "成功耗时应小于 timeout：{us}");
            }
            Err(msg) => {
                assert!(!msg.is_empty(), "失败必须给出错误信息");
            }
        }
    }
}

#[cfg(test)]
mod zero_latency_tests {
    use super::*;

    #[test]
    fn zero_latency_is_reachable_but_shown_as_unknown() {
        // 回归：bash<5（无 EPOCHREALTIME）且 date 不支持 %s%N 时脚本回退输出 0。
        // 可达性为真，但 0 是占位值：不得显示成「<1 ms」，必须标为耗时不可测。
        let report = assemble_wsl_report("Ubuntu-22.04", "OK github.com 0\n", Vec::new());
        assert_eq!(report.results.len(), 3);
        let row = report.results.iter().find(|r| r.host == "github.com").unwrap();
        assert_eq!(row.status, ProbeStatus::Reachable);
        assert_eq!(row.latency_us, None, "占位 0 不得当作真实微秒耗时");
        assert!(row.message.contains("不可测"), "{}", row.message);
    }
}
