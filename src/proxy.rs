//! 本机代理/网络身份只读检测与命令参考。不修改系统设置、不自动执行设置命令。
//! 除环境变量与注册表外，网卡 IP/MAC 经本地 PowerShell 读取；外网 IP 在用户刷新状态页时向公共回显服务查询（只读、不上传本机数据）。
use std::collections::HashMap;

pub const ENV_PROXY_NAMES: [&str; 4] = ["HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "NO_PROXY"];
/// 代理特征端口（不含 8080/8888：本地开发服务器最常用，避免大量误报）。
pub const FEATURE_PORTS: &[u16] = &[
    7890, 7891, 1080, 1081, 20171, 20172, 10808, 10809, 2080, 33210, 41091,
];
/// 命令参考里示例代理端口的默认值；UI 可改，改后命令全文随端口替换。
pub const DEFAULT_PROXY_COMMAND_PORT: u16 = 7890;
/// 常见 VPN / 代理客户端进程名子串（不区分大小写）。
const KNOWN_CLIENTS: &[&str] = &[
    "clash", "mihomo", "v2ray", "v2rayn", "xray", "sing-box", "singbox", "shadowsocks",
    "ss-local", "nekobox", "hiddify", "wireguard", "openvpn", "surge", "shadowtls", "tun2socks",
    "easytier", "proxygen",
];
/// `naive` 作为普通子串太泛，只按显式名单匹配。
const NAIVE_EXPLICIT: &[&str] = &["naive", "naiveproxy"];
const VIRTUAL_ADAPTER_HINTS: &[&str] = &["tap", "wintun", "wireguard", "openvpn", "tun", "虚拟"];

#[derive(Debug, Clone)]
pub struct EnvVarStatus {
    pub name: String,
    pub value: Option<String>,
}
#[derive(Debug, Clone)]
pub struct SystemProxyStatus {
    pub enabled: bool,
    pub server: String,
    pub override_list: String,
    pub source: String,
}
#[derive(Debug, Clone)]
pub struct VpnProcess {
    pub name: String,
    pub pid: u32,
    pub ports: Vec<u16>,
    pub label: String,
}
#[derive(Debug, Clone)]
pub struct AdapterStatus {
    pub name: String,
    pub description: String,
    pub status: String,
    pub virtual_like: bool,
    /// 物理/虚拟网卡 MAC（PowerShell `MacAddress`；解析失败为空串）。
    pub mac: String,
    /// 该网卡上的 IPv4 地址（不含 127.0.0.1）。
    pub ipv4: Vec<String>,
}
#[derive(Debug, Clone)]
pub struct ProxySnapshot {
    pub env_vars: Vec<EnvVarStatus>,
    pub system_proxy: Option<SystemProxyStatus>,
    pub vpn_processes: Vec<VpnProcess>,
    pub adapters: Vec<AdapterStatus>,
    /// 本机对外 IPv4（优先：Up 且非虚拟网卡上的 DHCP/固定地址；其次任意 Up 网卡）。
    pub local_ip: String,
    /// 访问公网时使用的出口 IP（刷新状态页时向公共回显服务查询）。
    pub public_ip: String,
    pub public_ip_error: String,
    pub notes: Vec<String>,
}
impl Default for ProxySnapshot {
    fn default() -> Self {
        Self {
            env_vars: Vec::new(),
            system_proxy: None,
            vpn_processes: Vec::new(),
            adapters: Vec::new(),
            local_ip: String::new(),
            public_ip: String::new(),
            public_ip_error: String::new(),
            notes: Vec::new(),
        }
    }
}
#[derive(Debug, Clone)]
pub struct CommandTip {
    pub platform: &'static str,
    pub group: &'static str,
    pub title: &'static str,
    pub command: String,
    pub note: &'static str,
}

/// 把模板命令里的示例端口换成用户配置的端口。无效端口回落到默认值。
pub fn command_tips(port: u16) -> Vec<CommandTip> {
    let port = if port == 0 { DEFAULT_PROXY_COMMAND_PORT } else { port };
    let default = DEFAULT_PROXY_COMMAND_PORT.to_string();
    let configured = port.to_string();
    template_command_tips()
        .into_iter()
        .map(|mut tip| {
            if configured != default {
                tip.command = tip.command.replace(&default, &configured);
            }
            tip
        })
        .collect()
}

/// 精选命令参考（静态编译进二进制，只读展示、可复制，不自动执行）。
/// 命令正文用默认端口书写，经 `command_tips` 做端口替换后展示/复制。
fn template_command_tips() -> Vec<CommandTip> {
    vec![
        CommandTip {
            platform: "Windows",
            group: "会话环境变量",
            title: "PowerShell 设置当前会话代理",
            command: "$env:HTTP_PROXY='http://127.0.0.1:7890'\n$env:HTTPS_PROXY='http://127.0.0.1:7890'\n$env:ALL_PROXY='http://127.0.0.1:7890'\n$env:NO_PROXY='localhost;127.0.0.1'".into(),
            note: "只影响当前 PowerShell 会话；关闭窗口后失效。",
        },
        CommandTip {
            platform: "Windows",
            group: "会话环境变量",
            title: "PowerShell 清除当前会话代理",
            command: "Remove-Item Env:HTTP_PROXY,Env:HTTPS_PROXY,Env:ALL_PROXY,Env:NO_PROXY -ErrorAction SilentlyContinue".into(),
            note: "变量不存在时不会报错。",
        },
        CommandTip {
            platform: "Windows",
            group: "会话环境变量",
            title: "cmd 设置当前会话代理",
            command: "set HTTP_PROXY=http://127.0.0.1:7890\nset HTTPS_PROXY=http://127.0.0.1:7890".into(),
            note: "仅当前 cmd 窗口；新开进程不会继承。",
        },
        CommandTip {
            platform: "Windows",
            group: "会话环境变量",
            title: "setx 写入用户环境变量",
            command: "setx HTTP_PROXY http://127.0.0.1:7890\nsetx HTTPS_PROXY http://127.0.0.1:7890".into(),
            note: "setx 影响之后启动的新进程，不改变当前窗口；写入后请新开终端验证。",
        },
        CommandTip {
            platform: "Windows",
            group: "系统代理",
            title: "查看 WinHTTP 系统代理",
            command: "netsh winhttp show proxy".into(),
            note: "WinHTTP 多供系统服务与部分安装程序；浏览器通常走 WinINET（IE 设置）。",
        },
        CommandTip {
            platform: "Windows",
            group: "系统代理",
            title: "设置 WinHTTP 代理",
            command: "netsh winhttp set proxy proxy-server=\"http=127.0.0.1:7890;https=127.0.0.1:7890\" bypass-list=\"localhost;127.0.0.1\"".into(),
            note: "需要管理员权限；不会改浏览器使用的 WinINET 设置。",
        },
        CommandTip {
            platform: "Windows",
            group: "系统代理",
            title: "重置 WinHTTP 为直接连接",
            command: "netsh winhttp reset proxy".into(),
            note: "需要管理员权限；只影响 WinHTTP。",
        },
        CommandTip {
            platform: "Windows",
            group: "查看",
            title: "查看 WinINET 系统代理注册表",
            command: "Get-ItemProperty 'HKCU:\\Software\\Microsoft\\Windows\\CurrentVersion\\Internet Settings' | Select-Object ProxyEnable,ProxyServer,ProxyOverride".into(),
            note: "与本页「系统代理」卡片同源；ProxyEnable=1 表示已启用。",
        },
        CommandTip {
            platform: "Windows",
            group: "查看",
            title: "列出当前环境变量中的代理设置",
            command: "Get-ChildItem Env: | Where-Object { $_.Name -match 'proxy' }".into(),
            note: "PowerShell；大小写不敏感匹配。",
        },
        CommandTip {
            platform: "Linux",
            group: "会话环境变量",
            title: "export 当前 shell 代理",
            command: "export http_proxy=http://127.0.0.1:7890\nexport https_proxy=http://127.0.0.1:7890\nexport all_proxy=http://127.0.0.1:7890\nexport no_proxy=localhost,127.0.0.1".into(),
            note: "许多工具只认小写；大写形式也可按需再 export。",
        },
        CommandTip {
            platform: "Linux",
            group: "会话环境变量",
            title: "unset 清除当前会话代理",
            command: "unset http_proxy https_proxy all_proxy no_proxy HTTP_PROXY HTTPS_PROXY ALL_PROXY NO_PROXY".into(),
            note: "同时清大小写，避免残留半套配置。",
        },
        CommandTip {
            platform: "Linux",
            group: "会话环境变量",
            title: "写入 ~/.bashrc 长期生效",
            command: "cat >> ~/.bashrc <<'EOF'\nexport http_proxy=http://127.0.0.1:7890\nexport https_proxy=http://127.0.0.1:7890\nexport no_proxy=localhost,127.0.0.1\nEOF\nsource ~/.bashrc".into(),
            note: "只影响 bash 登录/交互 shell；其他 shell 与系统服务不读这里。",
        },
        CommandTip {
            platform: "Linux",
            group: "系统代理",
            title: "系统级环境变量（/etc/environment）",
            command: "sudo tee -a /etc/environment <<'EOF'\nhttp_proxy=http://127.0.0.1:7890\nhttps_proxy=http://127.0.0.1:7890\nno_proxy=localhost,127.0.0.1\nEOF".into(),
            note: "需要管理员；图形会话与服务是否继承取决于发行版与会话管理。",
        },
        CommandTip {
            platform: "Linux",
            group: "桌面/包管理",
            title: "GNOME 桌面代理",
            command: "gsettings set org.gnome.system.proxy mode 'manual'\ngsettings set org.gnome.system.proxy.http host '127.0.0.1'\ngsettings set org.gnome.system.proxy.http port 7890\ngsettings set org.gnome.system.proxy.https host '127.0.0.1'\ngsettings set org.gnome.system.proxy.https port 7890".into(),
            note: "影响 GNOME 应用；恢复：mode 改回 'none'。",
        },
        CommandTip {
            platform: "Linux",
            group: "桌面/包管理",
            title: "apt 指定代理",
            command: "sudo tee /etc/apt/apt.conf.d/99proxy <<'EOF'\nAcquire::http::Proxy \"http://127.0.0.1:7890\";\nAcquire::https::Proxy \"http://127.0.0.1:7890\";\nEOF".into(),
            note: "只影响 apt；用完可删除该配置文件。",
        },
        CommandTip {
            platform: "Linux",
            group: "查看",
            title: "列出当前 shell 中的代理变量",
            command: "env | grep -i proxy".into(),
            note: "只显示当前进程环境。",
        },
    ]
}

/// 从环境快照收集代理变量。Windows 按大小写不敏感合并；Unix 大小写形式各自成行。
pub fn collect_env_vars(
    vars: &HashMap<String, String>,
    case_insensitive: bool,
) -> Vec<EnvVarStatus> {
    let mut rows = Vec::new();
    if case_insensitive {
        let upper: HashMap<String, &String> = vars
            .iter()
            .map(|(k, v)| (k.to_ascii_uppercase(), v))
            .collect();
        for name in ENV_PROXY_NAMES {
            rows.push(EnvVarStatus {
                name: name.to_string(),
                value: upper.get(name).map(|s| (*s).clone()),
            });
        }
    } else {
        for name in ENV_PROXY_NAMES {
            rows.push(EnvVarStatus {
                name: name.to_string(),
                value: vars.get(name).cloned(),
            });
            let lower = name.to_ascii_lowercase();
            if let Some(value) = vars.get(&lower) {
                rows.push(EnvVarStatus {
                    name: lower,
                    value: Some(value.clone()),
                });
            }
        }
    }
    rows
}

fn process_stem(name: &str) -> String {
    let lower = name.to_ascii_lowercase();
    lower.strip_suffix(".exe").unwrap_or(&lower).to_string()
}

/// 是否命中内置常见客户端进程名。
pub fn matches_known_client(process_name: &str) -> bool {
    let stem = process_stem(process_name);
    if NAIVE_EXPLICIT.iter().any(|n| stem == *n) {
        return true;
    }
    KNOWN_CLIENTS.iter().any(|n| stem.contains(n))
}

/// 解析 `tasklist /fo csv /nh` 输出（已解码的文本）。
pub fn parse_tasklist_csv(text: &str) -> Vec<(String, u32)> {
    let mut out = Vec::new();
    let mut reader = csv::ReaderBuilder::new()
        .has_headers(false)
        .flexible(true)
        .from_reader(text.as_bytes());
    for record in reader.records() {
        let Ok(record) = record else { continue };
        let Some(name) = record.get(0) else { continue };
        let Some(pid) = record.get(1) else { continue };
        let Ok(pid) = pid.trim().parse::<u32>() else {
            continue;
        };
        out.push((name.trim().to_string(), pid));
    }
    out
}

/// 解析 `netstat -ano -p tcp` 输出中的 TCP 监听端口（按 PID 归组）。
pub fn parse_netstat_listeners(text: &str) -> HashMap<u32, Vec<u16>> {
    let mut map: HashMap<u32, Vec<u16>> = HashMap::new();
    for line in text.lines() {
        let cols: Vec<&str> = line.split_whitespace().collect();
        if cols.len() < 5 {
            continue;
        }
        if !cols[0].eq_ignore_ascii_case("TCP") {
            continue;
        }
        // 状态列前缀匹配 LISTEN，对本地化更稳健（英文 LISTENING / 部分环境 LISTEN）。
        if !cols[3].to_ascii_uppercase().starts_with("LISTEN") {
            continue;
        }
        let Some(port) = cols[1]
            .rsplit([':', ']'])
            .find(|p| !p.is_empty())
            .and_then(|p| p.parse::<u16>().ok())
        else {
            continue;
        };
        let Ok(pid) = cols[4].parse::<u32>() else {
            continue;
        };
        let ports = map.entry(pid).or_default();
        if !ports.contains(&port) {
            ports.push(port);
        }
    }
    for ports in map.values_mut() {
        ports.sort_unstable();
    }
    map
}

/// 名称/描述是否像虚拟网卡。
pub fn is_virtual_adapter(name: &str, description: &str) -> bool {
    let hay = format!("{name} {description}").to_ascii_lowercase();
    VIRTUAL_ADAPTER_HINTS.iter().any(|hint| {
        if *hint == "虚拟" {
            hay.contains(hint)
        } else {
            hay.contains(hint)
        }
    })
}

fn json_string(value: Option<&serde_json::Value>) -> String {
    match value {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(serde_json::Value::Number(n)) => n.to_string(),
        Some(other) => other.to_string().trim_matches('"').to_string(),
        None => String::new(),
    }
}

/// 解析 PowerShell `Get-NetAdapter | Select-Object ... | ConvertTo-Json -Compress` 输出。
/// 单对象与数组两种形态都要能解析。兼容旧字段（无 MacAddress / ifIndex）。
/// 返回 (网卡列表, 与列表等长的 ifIndex；未知为 0)。
pub fn parse_net_adapter_json(text: &str) -> anyhow::Result<(Vec<AdapterStatus>, Vec<u32>)> {
    let text = text.trim();
    if text.is_empty() {
        anyhow::bail!("网卡查询返回空输出");
    }
    let value: serde_json::Value = serde_json::from_str(text)
        .map_err(|e| anyhow::anyhow!("无法解析 Get-NetAdapter JSON：{e}"))?;
    let items: Vec<&serde_json::Value> = match &value {
        serde_json::Value::Array(list) => list.iter().collect(),
        single @ serde_json::Value::Object(_) => vec![single],
        _ => anyhow::bail!("Get-NetAdapter JSON 形态不受支持"),
    };
    let mut out = Vec::new();
    let mut indexes = Vec::new();
    for item in items {
        let name = item
            .get("Name")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let description = item
            .get("InterfaceDescription")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let status = json_string(item.get("Status"));
        let mac = json_string(item.get("MacAddress")).trim().to_string();
        let if_index = item
            .get("ifIndex")
            .or_else(|| item.get("IfIndex"))
            .and_then(|v| v.as_i64())
            .unwrap_or_default() as u32;
        let virtual_like = is_virtual_adapter(&name, &description);
        out.push(AdapterStatus {
            name,
            description,
            status,
            virtual_like,
            mac,
            ipv4: Vec::new(),
        });
        indexes.push(if_index);
    }
    Ok((out, indexes))
}

/// 解析 `Get-NetIPAddress -AddressFamily IPv4` 的 JSON（单对象/数组），得到 (ifIndex, IPAddress)。
pub fn parse_net_ip_address_json(text: &str) -> Vec<(u32, String)> {
    let text = text.trim();
    if text.is_empty() {
        return Vec::new();
    }
    let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
        return Vec::new();
    };
    let items: Vec<&serde_json::Value> = match &value {
        serde_json::Value::Array(list) => list.iter().collect(),
        single @ serde_json::Value::Object(_) => vec![single],
        _ => Vec::new(),
    };
    let mut out = Vec::new();
    for item in items {
        let Ok(if_index) = item
            .get("InterfaceIndex")
            .and_then(|v| v.as_i64())
            .unwrap_or_default()
            .try_into()
        else {
            continue;
        };
        let ip = json_string(item.get("IPAddress")).trim().to_string();
        if ip.is_empty() || ip == "127.0.0.1" || ip.starts_with("127.") {
            continue;
        }
        out.push((if_index, ip));
    }
    out
}

/// 把 IPv4 列表挂到对应网卡（按 ifIndex）。
pub fn attach_ipv4_to_adapters(adapters: &mut [AdapterStatus], if_indexes: &[u32], ips: &[(u32, String)]) {
    for (adapter, if_index) in adapters.iter_mut().zip(if_indexes.iter()) {
        for (idx, ip) in ips {
            if idx == if_index && !adapter.ipv4.contains(ip) {
                adapter.ipv4.push(ip.clone());
            }
        }
        adapter.ipv4.sort();
    }
}

/// 从网卡列表挑选「当前系统对外 IPv4」优先级：Up 且非虚拟 → 有地址的任意 Up → 有地址的任意卡。
pub fn pick_local_ip(adapters: &[AdapterStatus]) -> String {
    let candidates = |filter: fn(&AdapterStatus) -> bool| -> Vec<String> {
        adapters
            .iter()
            .filter(|a| filter(a))
            .flat_map(|a| a.ipv4.iter().cloned())
            .collect()
    };
    let prefer = candidates(|a| status_is_up(&a.status) && !a.virtual_like);
    let up_any = candidates(|a| status_is_up(&a.status));
    let any = candidates(|_| true);
    prefer.into_iter().next().or_else(|| up_any.into_iter().next()).or_else(|| any.into_iter().next()).unwrap_or_default()
}

fn status_is_up(status: &str) -> bool {
    let lower = status.to_ascii_lowercase();
    lower.trim() == "up"
}

/// 校验并截取公共回显服务返回的纯 IP 文本（防止 HTML/异常页进入界面）。
pub fn normalize_public_ip(raw: &str) -> Result<String, String> {
    let trimmed = raw.trim().trim_matches(|c: char| c == '"' || c == '\'');
    // 允许一行内出现多个候选，取第一个看起来像 IPv4/IPv6 的。
    for token in trimmed.split_whitespace() {
        let token = token.trim().trim_end_matches('.');
        if is_plausible_ip(token) {
            return Ok(token.to_string());
        }
    }
    if is_plausible_ip(trimmed) {
        return Ok(trimmed.to_string());
    }
    Err("回显内容不是合法 IP".into())
}

fn is_plausible_ip(token: &str) -> bool {
    if token.contains(':') {
        // IPv6：至少两组冒号分隔的十六进制段
        let parts: Vec<&str> = token.split(':').filter(|p| !p.is_empty()).collect();
        return parts.len() >= 2
            && parts.iter().all(|p| p.chars().all(|c| c.is_ascii_hexdigit()) && p.len() <= 4);
    }
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() != 4 {
        return false;
    }
    parts.iter().all(|p| {
        !p.is_empty()
            && p.len() <= 3
            && p.chars().all(|c| c.is_ascii_digit())
            && p.parse::<u8>().is_ok()
    })
}

/// 回落解析 `netsh interface show interface`（OEM 解码后的文本）。
pub fn parse_netsh_interfaces(text: &str) -> Vec<AdapterStatus> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('-') || line.starts_with('=') {
            continue;
        }
        // 英文表头 / 中文表头都跳过：管理员状态、状态、类型、接口名 四列。
        let lower = line.to_ascii_lowercase();
        if lower.contains("admin state")
            || lower.contains("interface name")
            || line.contains("管理员状态")
            || (line.contains("状态") && line.contains("接口"))
        {
            continue;
        }
        let cols: Vec<&str> = line.split_whitespace().collect();
        if cols.len() < 4 {
            continue;
        }
        let name = cols[3..].join(" ");
        let status = cols[1].to_string();
        let virtual_like = is_virtual_adapter(&name, "");
        out.push(AdapterStatus {
            name,
            description: String::new(),
            status,
            virtual_like,
            mac: String::new(),
            ipv4: Vec::new(),
        });
    }
    out
}

/// 用进程列表 + netstat 监听端口组装 VPN/代理条目。
pub fn build_vpn_processes(
    processes: &[(String, u32)],
    listeners: &HashMap<u32, Vec<u16>>,
) -> Vec<VpnProcess> {
    let mut out = Vec::new();
    let mut matched_pids = std::collections::HashSet::new();
    for (name, pid) in processes {
        if matches_known_client(name) {
            matched_pids.insert(*pid);
            let mut ports = listeners.get(pid).cloned().unwrap_or_default();
            ports.sort_unstable();
            out.push(VpnProcess {
                name: name.clone(),
                pid: *pid,
                ports,
                label: "按进程名匹配".into(),
            });
        }
    }
    // 未命中名单时：若某进程在代理特征端口监听，也列入（端口启发式）。
    for (name, pid) in processes {
        if matched_pids.contains(pid) {
            continue;
        }
        let Some(ports) = listeners.get(pid) else {
            continue;
        };
        let hits: Vec<u16> = ports
            .iter()
            .copied()
            .filter(|p| FEATURE_PORTS.contains(p))
            .collect();
        if hits.is_empty() {
            continue;
        }
        matched_pids.insert(*pid);
        out.push(VpnProcess {
            name: name.clone(),
            pid: *pid,
            ports: hits,
            label: "按端口启发式".into(),
        });
    }
    out.sort_by(|a, b| a.name.to_ascii_lowercase().cmp(&b.name.to_ascii_lowercase()));
    out
}

#[cfg(windows)]
fn decode_oem(bytes: &[u8]) -> String {
    if bytes.is_empty() {
        return String::new();
    }
    use windows_sys::Win32::Globalization::{MultiByteToWideChar, CP_OEMCP};
    let wide_len = unsafe {
        MultiByteToWideChar(
            CP_OEMCP,
            0,
            bytes.as_ptr(),
            bytes.len() as i32,
            std::ptr::null_mut(),
            0,
        )
    };
    if wide_len <= 0 {
        return String::from_utf8_lossy(bytes).into_owned();
    }
    let mut wide = vec![0u16; wide_len as usize];
    let written = unsafe {
        MultiByteToWideChar(
            CP_OEMCP,
            0,
            bytes.as_ptr(),
            bytes.len() as i32,
            wide.as_mut_ptr(),
            wide_len,
        )
    };
    if written <= 0 {
        return String::from_utf8_lossy(bytes).into_owned();
    }
    String::from_utf16_lossy(&wide[..written as usize])
}

#[cfg(not(windows))]
fn decode_oem(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// 子进程约定：不闪控制台窗口；OEM 代码页解码（PowerShell 强制 UTF-8）。
#[cfg(windows)]
fn run_capture_oem(program: &str, args: &[&str]) -> anyhow::Result<String> {
    use std::os::windows::process::CommandExt;
    let output = std::process::Command::new(program)
        .args(args)
        .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| anyhow::anyhow!("无法启动 {program}：{e}"))?;
    if !output.status.success() && output.stdout.is_empty() {
        let err = decode_oem(&output.stderr);
        anyhow::bail!("{program} 退出码 {:?}：{}", output.status.code(), err.trim());
    }
    Ok(decode_oem(&output.stdout))
}

#[cfg(not(windows))]
fn run_capture_oem(program: &str, args: &[&str]) -> anyhow::Result<String> {
    let output = std::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| anyhow::anyhow!("无法启动 {program}：{e}"))?;
    if !output.status.success() && output.stdout.is_empty() {
        anyhow::bail!("{program} 退出码 {:?}", output.status.code());
    }
    Ok(decode_oem(&output.stdout))
}

/// PowerShell 统一强制 UTF-8 输出。
#[cfg(windows)]
fn run_powershell_utf8(script: &str) -> anyhow::Result<String> {
    use std::os::windows::process::CommandExt;
    let command = format!(
        "[Console]::OutputEncoding=[Text.Encoding]::UTF8; {script}"
    );
    let output = std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            &command,
        ])
        .creation_flags(0x0800_0000)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| anyhow::anyhow!("无法启动 PowerShell：{e}"))?;
    if !output.status.success() && output.stdout.is_empty() {
        let err = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("PowerShell 退出码 {:?}：{}", output.status.code(), err.trim());
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(windows)]
fn read_registry_string(name: &str) -> Option<String> {
    use windows_sys::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_SZ};
    let path: Vec<u16> = "Software\\Microsoft\\Windows\\CurrentVersion\\Internet Settings"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let value_name: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
    let mut buffer = vec![0u16; 1024];
    let mut size = (buffer.len() * 2) as u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            path.as_ptr(),
            value_name.as_ptr(),
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            buffer.as_mut_ptr() as *mut core::ffi::c_void,
            &mut size,
        )
    };
    if status != 0 || size < 2 {
        return None;
    }
    let units = (size as usize / 2).saturating_sub(1);
    Some(String::from_utf16_lossy(&buffer[..units.min(buffer.len())]))
}

#[cfg(windows)]
fn read_registry_dword(name: &str) -> Option<u32> {
    use windows_sys::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD};
    let path: Vec<u16> = "Software\\Microsoft\\Windows\\CurrentVersion\\Internet Settings"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let value_name: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
    let mut value: u32 = 0;
    let mut size = std::mem::size_of::<u32>() as u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            path.as_ptr(),
            value_name.as_ptr(),
            RRF_RT_REG_DWORD,
            std::ptr::null_mut(),
            &mut value as *mut u32 as *mut core::ffi::c_void,
            &mut size,
        )
    };
    (status == 0).then_some(value)
}

#[cfg(windows)]
fn read_system_proxy() -> Option<SystemProxyStatus> {
    let enabled = read_registry_dword("ProxyEnable").unwrap_or(0) != 0;
    let server = read_registry_string("ProxyServer").unwrap_or_default();
    let override_list = read_registry_string("ProxyOverride").unwrap_or_default();
    Some(SystemProxyStatus {
        enabled,
        server,
        override_list,
        source: r"HKCU\Software\Microsoft\Windows\CurrentVersion\Internet Settings".into(),
    })
}

#[cfg(not(windows))]
fn read_system_proxy() -> Option<SystemProxyStatus> {
    None
}

fn current_env_map() -> HashMap<String, String> {
    std::env::vars().collect()
}

/// 一站式只读检测。单项失败写入 notes，整体不 panic、不中断其它检测。
pub fn detect() -> ProxySnapshot {
    detect_with_env(&current_env_map())
}

/// 可注入环境快照的检测入口（测试用）。
pub fn detect_with_env(vars: &HashMap<String, String>) -> ProxySnapshot {
    let mut snap = ProxySnapshot::default();
    #[cfg(windows)]
    {
        snap.env_vars = collect_env_vars(vars, true);
    }
    #[cfg(not(windows))]
    {
        snap.env_vars = collect_env_vars(vars, false);
    }

    match read_system_proxy() {
        Some(status) => snap.system_proxy = Some(status),
        None => {
            #[cfg(not(windows))]
            snap.notes.push("当前平台仅完整支持 Windows 检测（系统代理 / 进程 / 网卡）".into());
        }
    }

    #[cfg(windows)]
    {
        let tasklist = run_capture_oem("tasklist", &["/fo", "csv", "/nh"]);
        let netstat = run_capture_oem("netstat", &["-ano", "-p", "tcp"]);
        let processes = match &tasklist {
            Ok(text) => parse_tasklist_csv(text),
            Err(e) => {
                snap.notes.push(format!("进程列表检测失败：{e}"));
                Vec::new()
            }
        };
        let listeners = match &netstat {
            Ok(text) => parse_netstat_listeners(text),
            Err(e) => {
                snap.notes.push(format!("监听端口检测失败：{e}"));
                HashMap::new()
            }
        };
        snap.vpn_processes = build_vpn_processes(&processes, &listeners);

        // 网卡：Name/描述/状态/MAC/ifIndex（与旧字段兼容；netsh 回落无 MAC/IP）。
        let mut if_indexes: Vec<u32> = Vec::new();
        let ps = run_powershell_utf8(
            "Get-NetAdapter | Select-Object Name,InterfaceDescription,Status,MacAddress,ifIndex | ConvertTo-Json -Compress",
        );
        match &ps {
            Ok(text) => match parse_net_adapter_json(text) {
                Ok((adapters, indexes)) => {
                    snap.adapters = adapters;
                    if_indexes = indexes;
                }
                Err(e) => {
                    if let Ok(netsh) = run_capture_oem("netsh", &["interface", "show", "interface"])
                    {
                        snap.adapters = parse_netsh_interfaces(&netsh);
                        if snap.adapters.is_empty() {
                            snap.notes.push(format!("网卡 JSON 解析失败且 netsh 回落无结果：{e}"));
                        }
                    } else {
                        snap.notes.push(format!("网卡检测失败：{e}"));
                    }
                }
            },
            Err(e) => {
                if let Ok(netsh) = run_capture_oem("netsh", &["interface", "show", "interface"]) {
                    snap.adapters = parse_netsh_interfaces(&netsh);
                    if snap.adapters.is_empty() {
                        snap.notes.push(format!("Get-NetAdapter 失败且 netsh 回落无结果：{e}"));
                    }
                } else {
                    snap.notes.push(format!("网卡检测失败：{e}"));
                }
            }
        }
        if !snap.adapters.is_empty() {
            let ip_json = run_powershell_utf8(
                "Get-NetIPAddress -AddressFamily IPv4 -ErrorAction SilentlyContinue | Select-Object InterfaceIndex,IPAddress | ConvertTo-Json -Compress",
            );
            match &ip_json {
                Ok(text) => {
                    let ips = parse_net_ip_address_json(text);
                    if if_indexes.len() == snap.adapters.len() {
                        attach_ipv4_to_adapters(&mut snap.adapters, &if_indexes, &ips);
                    } else {
                        snap.notes.push(
                            "网卡 ifIndex 与 IP 列表长度不一致，已跳过按索引挂载；请手动核对各卡 IPv4。"
                                .into(),
                        );
                    }
                }
                Err(e) => snap.notes.push(format!("IPv4 地址读取失败：{e}")),
            }
            snap.local_ip = pick_local_ip(&snap.adapters);
            // 外网 IP：向只读公共回显服务查询（不携带本机标识；失败只记 public_ip_error）。
            match fetch_public_ip() {
                Ok(ip) => snap.public_ip = ip,
                Err(err) => snap.public_ip_error = err,
            }
        }
    }
    snap
}

/// 查询访问公网时使用的出口 IP。优先 api.ipify.org，失败回落 ifconfig.me。
/// 单独函数便于单元测试 normalize；真实网络仅在 UI 刷新状态页时触发。
fn fetch_public_ip() -> Result<String, String> {
    const ENDPOINTS: &[&str] = &["https://api.ipify.org", "https://ifconfig.me/ip"];
    let mut last = String::from("未配置网络或查询被拒绝");
    for url in ENDPOINTS {
        match run_powershell_utf8(&format!(
            "$ProgressPreference='SilentlyContinue'; try {{ (Invoke-WebRequest -UseBasicParsing -Uri '{url}' -TimeoutSec 5).Content }} catch {{ '' }}"
        )) {
            Ok(text) => match normalize_public_ip(&text) {
                Ok(ip) => return Ok(ip),
                Err(e) => {
                    if !text.trim().is_empty() {
                        last = format!("{url}：{e}");
                    }
                }
            },
            Err(e) => last = format!("{url}：{e}"),
        }
    }
    Err(last)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn env_vars_merge_case_on_windows_style() {
        let vars = map(&[
            ("HTTP_PROXY", "http://a:1"),
            ("https_proxy", "http://b:2"),
            ("ALL_PROXY", "http://c:3"),
        ]);
        let rows = collect_env_vars(&vars, true);
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[0].name, "HTTP_PROXY");
        assert_eq!(rows[0].value.as_deref(), Some("http://a:1"));
        assert_eq!(rows[1].value.as_deref(), Some("http://b:2"), "HTTPS_PROXY 应合并小写");
        assert_eq!(rows[2].value.as_deref(), Some("http://c:3"));
        assert!(rows[3].value.is_none());
    }

    #[test]
    fn env_vars_split_case_on_unix_style() {
        let vars = map(&[
            ("HTTP_PROXY", "http://up:1"),
            ("http_proxy", "http://low:1"),
            ("ALL_PROXY", "http://all:1"),
        ]);
        let rows = collect_env_vars(&vars, false);
        assert_eq!(rows[0].name, "HTTP_PROXY");
        assert_eq!(rows[0].value.as_deref(), Some("http://up:1"));
        assert_eq!(rows[1].name, "http_proxy");
        assert_eq!(rows[1].value.as_deref(), Some("http://low:1"));
        assert_eq!(rows[2].name, "HTTPS_PROXY");
        assert!(rows[2].value.is_none());
        assert_eq!(rows[3].name, "ALL_PROXY");
        assert_eq!(rows[3].value.as_deref(), Some("http://all:1"));
    }

    #[test]
    fn known_client_match_and_naive_explicit() {
        assert!(matches_known_client("clash-verge.exe"));
        assert!(matches_known_client("v2rayN.exe"));
        assert!(matches_known_client("NaiveProxy.exe"));
        assert!(matches_known_client("naive.exe"));
        assert!(!matches_known_client("node.exe"), "普通本地开发服务器不得误报");
        assert!(!matches_known_client("nautilus.exe"), "naive 不得作为普通子串命中");
        assert!(!matches_known_client("MyNaiveApp.exe"));
    }

    #[test]
    fn parse_tasklist_csv_with_quoted_and_cjk() {
        let sample = r#""System Idle Process","0","Services","0","8 K"
"clash-verge.exe","1234","Console","1","50,000 K"
"中文进程,带逗号.exe","2048","Console","2","1,024 K"
"#;
        let rows = parse_tasklist_csv(sample);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[1].0, "clash-verge.exe");
        assert_eq!(rows[1].1, 1234);
        assert_eq!(rows[2].0, "中文进程,带逗号.exe");
        assert_eq!(rows[2].1, 2048);
    }

    #[test]
    fn parse_netstat_listeners_cjk_header_and_ipv6() {
        let sample = "\
  协议  本地地址          外部地址        状态           PID
  TCP    0.0.0.0:7890       0.0.0.0:0              LISTENING       1234
  TCP    127.0.0.1:10808    127.0.0.1:0            LISTENING       1234
  TCP    [::]:443           [::]:0                 LISTENING       999
  TCP    192.168.1.2:50000  1.2.3.4:443            ESTABLISHED     1234
  UDP    0.0.0.0:51820      *:*                                    555
";
        let map = parse_netstat_listeners(sample);
        assert_eq!(map.get(&1234).cloned().unwrap_or_default(), vec![7890, 10808]);
        assert_eq!(map.get(&999).cloned().unwrap_or_default(), vec![443]);
        assert!(!map.contains_key(&555), "只处理 TCP");
    }

    #[test]
    fn build_vpn_processes_by_name_and_port_heuristic() {
        let processes = vec![
            ("clash.exe".to_string(), 10u32),
            ("node.exe".to_string(), 20u32),
            ("some-daemon.exe".to_string(), 30u32),
        ];
        let mut listeners = HashMap::new();
        listeners.insert(10u32, vec![7890u16]);
        listeners.insert(20u32, vec![3000u16]);
        listeners.insert(30u32, vec![10808u16]);
        let rows = build_vpn_processes(&processes, &listeners);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].name, "clash.exe");
        assert_eq!(rows[0].label, "按进程名匹配");
        assert_eq!(rows[0].ports, vec![7890]);
        assert_eq!(rows[1].name, "some-daemon.exe");
        assert_eq!(rows[1].label, "按端口启发式");
        assert_eq!(rows[1].ports, vec![10808]);
    }

    #[test]
    fn parse_net_adapter_json_single_and_array() {
        let single = r#"{"Name":"以太网","InterfaceDescription":"Intel I219","Status":"Up","MacAddress":"AA-BB-CC-DD-EE-FF","ifIndex":12}"#;
        let (rows, indexes) = parse_net_adapter_json(single).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "以太网");
        assert!(!rows[0].virtual_like);
        assert_eq!(rows[0].mac, "AA-BB-CC-DD-EE-FF");
        assert_eq!(indexes, vec![12]);

        let array = r#"[{"Name":"Wintun","InterfaceDescription":"WireGuard Tunnel","Status":"Up","MacAddress":"00-11-22-33-44-55","ifIndex":5},{"Name":"WLAN","InterfaceDescription":"Realtek WiFi","Status":"Disconnected","MacAddress":"66-77-88-99-AA-BB","ifIndex":9}]"#;
        let (rows, indexes) = parse_net_adapter_json(array).unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows[0].virtual_like);
        assert_eq!(rows[1].status, "Disconnected");
        assert_eq!(indexes, vec![5, 9]);
    }

    #[test]
    fn parse_net_ip_address_json_skips_loopback() {
        let sample = r#"[
  {"InterfaceIndex":12,"IPAddress":"192.168.1.20"},
  {"InterfaceIndex":12,"IPAddress":"127.0.0.1"},
  {"InterfaceIndex":9,"IPAddress":"10.0.0.5"}
]"#;
        let ips = parse_net_ip_address_json(sample);
        assert_eq!(ips, vec![(12, "192.168.1.20".into()), (9, "10.0.0.5".into())]);
    }

    #[test]
    fn pick_local_ip_prefers_up_non_virtual() {
        let adapters = vec![
            AdapterStatus {
                name: "Wintun".into(),
                description: "WireGuard".into(),
                status: "Up".into(),
                virtual_like: true,
                mac: "00-00-00-00-00-01".into(),
                ipv4: vec!["10.0.0.2".into()],
            },
            AdapterStatus {
                name: "以太网".into(),
                description: "Intel".into(),
                status: "Up".into(),
                virtual_like: false,
                mac: "AA-BB-CC".into(),
                ipv4: vec!["192.168.1.20".into()],
            },
            AdapterStatus {
                name: "WLAN".into(),
                description: "Realtek".into(),
                status: "Disconnected".into(),
                virtual_like: false,
                mac: "DD-EE-FF".into(),
                ipv4: vec!["192.168.1.30".into()],
            },
        ];
        assert_eq!(pick_local_ip(&adapters), "192.168.1.20");
    }

    #[test]
    fn attach_ipv4_matches_by_if_index() {
        let mut adapters = parse_net_adapter_json(
            r#"[{"Name":"A","InterfaceDescription":"x","Status":"Up","ifIndex":3,"MacAddress":"11"},{"Name":"B","InterfaceDescription":"y","Status":"Up","ifIndex":4,"MacAddress":"22"}]"#,
        )
        .unwrap()
        .0;
        attach_ipv4_to_adapters(
            &mut adapters,
            &[3, 4],
            &[(4, "10.0.0.4".into()), (3, "192.168.0.3".into())],
        );
        assert_eq!(adapters[0].ipv4, vec!["192.168.0.3".to_string()]);
        assert_eq!(adapters[1].ipv4, vec!["10.0.0.4".to_string()]);
    }

    #[test]
    fn normalize_public_ip_accepts_v4_v6_and_rejects_html() {
        assert_eq!(normalize_public_ip("203.0.113.9\n").unwrap(), "203.0.113.9");
        assert_eq!(normalize_public_ip("  2001:db8::1  ").unwrap(), "2001:db8::1");
        assert!(normalize_public_ip("<html>error</html>").is_err());
        assert!(normalize_public_ip("").is_err());
    }

    #[test]
    fn command_tips_cover_both_platforms_without_blanks() {
        let tips = command_tips(DEFAULT_PROXY_COMMAND_PORT);
        assert!(tips.iter().any(|t| t.platform == "Windows"));
        assert!(tips.iter().any(|t| t.platform == "Linux"));
        for tip in &tips {
            assert!(!tip.title.is_empty(), "标题不得为空");
            assert!(!tip.command.is_empty(), "命令不得为空");
            assert!(!tip.group.is_empty());
        }
    }

    #[test]
    fn command_tips_substitute_custom_port() {
        let tips = command_tips(10809);
        let set_ps = tips
            .iter()
            .find(|t| t.title == "PowerShell 设置当前会话代理")
            .expect("应有 PowerShell 设置命令");
        assert!(set_ps.command.contains("http://127.0.0.1:10809"));
        assert!(!set_ps.command.contains(":7890"));
        let gsettings = tips
            .iter()
            .find(|t| t.title == "GNOME 桌面代理")
            .expect("应有 GNOME 命令");
        assert!(gsettings.command.contains("port 10809"));
        // 清除类命令不应被误替换。
        let clear = tips
            .iter()
            .find(|t| t.title == "unset 清除当前会话代理")
            .expect("应有 unset 命令");
        assert!(!clear.command.contains("10809"));
    }

    #[test]
    fn non_windows_detect_does_not_panic() {
        let snap = detect();
        assert_eq!(snap.env_vars.len(), 4);
        #[cfg(not(windows))]
        {
            assert!(snap.system_proxy.is_none());
            assert!(snap.vpn_processes.is_empty());
            assert!(snap.adapters.is_empty());
            assert!(!snap.notes.is_empty());
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_registry_system_proxy_readable() {
        let status = read_system_proxy().expect("HKCU Internet Settings 应可只读访问");
        // 只验证结构，不断言本机实际是否启用代理。
        assert!(!status.source.is_empty());
    }
}
