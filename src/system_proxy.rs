//! P-09 系统代理：读取 Windows 当前用户的手动代理设置（Internet 设置的
//! `ProxyEnable` / `ProxyServer` / `ProxyOverride`），为应用自身下载
//! （`snap_ocr_assets` 的 ureq 原语）与 Git 工具的网络命令（fetch / push）提供
//! 统一代理出口；经代理连接失败后的直连回退由调用方执行。
//!
//! 只支持手动代理：PAC / WPAD 自动配置不在范围内（下载并执行代理脚本会扩大
//! P-03 联网边界）。注册表读取失败一律视为「未启用」，回落直连，不阻塞功能。
//! 本模块不联网，也不引用 `ureq`（P-03 的联网文件白名单不受影响）。

/// `ProxyOverride` 例外表的单条规则。
#[derive(Clone, PartialEq, Eq, Debug)]
enum BypassRule {
    /// `<local>`：不含点的主机名（内网裸名）不走代理。
    Local,
    /// 含 `*` 通配的模式，按大小写不敏感的通配匹配。
    Pattern(String),
    /// 精确主机名（可能带 `:端口`）。
    Exact(String),
}

/// Windows 当前用户手动系统代理的快照（P-09）。
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct SystemProxy {
    enabled: bool,
    /// 各协议代理端点，统一为 `http://host:port`（HTTP 代理，含 CONNECT）
    /// 或 `socks5://host:port`（SOCKS 条目）。
    http: Option<String>,
    https: Option<String>,
    socks: Option<String>,
    bypass: Vec<BypassRule>,
}

impl SystemProxy {
    /// 由注册表值构造（纯函数，便于测试）：`ProxyEnable` 为 0、`ProxyServer`
    /// 缺失或整体不可解析时视为未启用（直连）。
    pub fn from_registry_values(
        enable: u32,
        server: Option<&str>,
        override_list: Option<&str>,
    ) -> SystemProxy {
        let Some(server) = server.map(str::trim).filter(|value| !value.is_empty()) else {
            return SystemProxy::default();
        };
        if enable == 0 {
            return SystemProxy::default();
        }
        let mut proxy = SystemProxy {
            enabled: true,
            bypass: parse_override(override_list),
            ..SystemProxy::default()
        };
        if server.contains('=') {
            // 按协议形式：`http=h:p;https=h:p;socks=h:p`（分隔符为分号或空白）。
            for entry in server.split([';', ' ', '\t']) {
                let Some((protocol, target)) = entry.trim().split_once('=') else {
                    continue;
                };
                let Some(endpoint) = normalize_host_port(target) else {
                    continue;
                };
                match protocol.trim().to_ascii_lowercase().as_str() {
                    // registry 的 http=/https= 条目本身都是 HTTP 代理；
                    // ftp 条目本应用没有出口（P-03），忽略。
                    "http" => proxy.http = Some(format!("http://{endpoint}")),
                    "https" => proxy.https = Some(format!("http://{endpoint}")),
                    "socks" => proxy.socks = Some(format!("socks5://{endpoint}")),
                    _ => {}
                }
            }
        } else {
            let Some(endpoint) = normalize_host_port(server) else {
                return SystemProxy::default();
            };
            proxy.http = Some(format!("http://{endpoint}"));
            proxy.https = Some(format!("http://{endpoint}"));
        }
        proxy
    }

    /// 系统代理是否开启且解析到至少一个可用端点。
    pub fn is_enabled(&self) -> bool {
        self.enabled && (self.http.is_some() || self.https.is_some() || self.socks.is_some())
    }

    /// 为目标 URL 选择代理端点：未启用、URL 不可解析或命中例外表时返回
    /// `None`（直连）。https 目标优先 `https=`，其次 `socks=`，再次 `http=`；
    /// http 目标优先 `http=`，其次 `socks=`，再次 `https=`。
    pub fn endpoint_for_url(&self, url: &str) -> Option<String> {
        if !self.is_enabled() {
            return None;
        }
        let (scheme, host) = split_scheme_host(url)?;
        if self.bypass.iter().any(|rule| rule_matches(rule, &host)) {
            return None;
        }
        if scheme == "https" {
            self.https
                .clone()
                .or_else(|| self.socks.clone())
                .or_else(|| self.http.clone())
        } else {
            self.http
                .clone()
                .or_else(|| self.socks.clone())
                .or_else(|| self.https.clone())
        }
    }

    /// P-09：目标 URL 是否命中例外表（系统代理未启用时恒为 false）。git 网络
    /// 命令注入代理前先做此判断：命中的目标直接按现状直连——libcurl 的
    /// `no_proxy` 无法表达 `<local>` 与 `192.168.*` 这类任意位置通配，仅靠
    /// [`Self::git_env`] 的近似翻译会丢弃这些条目，导致内网/裸主机目标仍被推入代理。
    pub fn bypassed(&self, url: &str) -> bool {
        self.is_enabled()
            && split_scheme_host(url)
                .is_some_and(|(_, host)| self.bypass.iter().any(|rule| rule_matches(rule, &host)))
    }

    /// git 网络命令的环境变量注入（P-09）：仅在系统代理开启时非空，键固定为
    /// 小写（libcurl 优先识别小写）。`no_proxy` 无法表达 `<local>` 与任意位置
    /// 通配，按近似规则翻译（`*.foo.com` → `foo.com`，`*` → `*`，无法表达的
    /// 条目丢弃），不影响例外表以外的行为。
    pub fn git_env(&self) -> Vec<(String, String)> {
        if !self.is_enabled() {
            return Vec::new();
        }
        let mut env = Vec::new();
        if let Some(http) = &self.http {
            env.push(("http_proxy".to_string(), http.clone()));
        }
        if let Some(https) = &self.https {
            env.push(("https_proxy".to_string(), https.clone()));
        }
        if let Some(socks) = &self.socks {
            env.push(("all_proxy".to_string(), socks.clone()));
        }
        let no_proxy: Vec<String> = self.bypass.iter().filter_map(translate_no_proxy).collect();
        if !no_proxy.is_empty() {
            env.push(("no_proxy".to_string(), no_proxy.join(",")));
        }
        env
    }
}

/// 直连回退时需要从子进程环境移除的变量名（P-09）：小写与历史大写形态都清掉，
/// 保证重试确实不经任何代理。
pub const GIT_PROXY_ENV_KEYS: [&str; 6] = [
    "http_proxy",
    "https_proxy",
    "all_proxy",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "ALL_PROXY",
];

/// 判断 git 输出是否像「经代理连接失败」的网络错误（P-09 直连回退的触发
/// 条件）。只匹配 libcurl / git 的连接类错误文案；认证失败、冲突、合并等
/// 非连接失败不触发，避免无意义的直连重试。
pub fn network_failure_signature(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    [
        "could not resolve host",
        "failed to connect",
        "couldn't connect",
        "connection refused",
        "connection reset",
        "connection aborted",
        "connection was closed",
        "timed out",
        "timeout was reached",
    ]
    .iter()
    .any(|signature| lower.contains(signature))
}

/// 读取当前用户的 Internet 设置并构造快照；非 Windows 平台恒为未启用
/// （P-07：仅支持 Windows，此分支只服务本地编译）。
#[cfg(windows)]
pub fn read() -> SystemProxy {
    let enable = read_registry_dword("ProxyEnable").unwrap_or(0);
    let server = read_registry_string("ProxyServer");
    let override_list = read_registry_string("ProxyOverride");
    SystemProxy::from_registry_values(enable, server.as_deref(), override_list.as_deref())
}

#[cfg(not(windows))]
pub fn read() -> SystemProxy {
    SystemProxy::default()
}

/// 从 URL 拆出 (scheme, host)：host 小写并去掉结尾 FQDN 点；带端口、IPv6
/// 字面量与 userinfo 的常见形态都可解析，解析不出 host 返回 `None`。
fn split_scheme_host(url: &str) -> Option<(String, String)> {
    let (scheme, rest) = url.split_once("://")?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    let host_port = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    let host = if let Some(literal) = host_port.strip_prefix('[') {
        literal.split(']').next().unwrap_or(literal)
    } else {
        host_port.split(':').next().unwrap_or(host_port)
    };
    let host = host.trim_matches('.').to_ascii_lowercase();
    if host.is_empty() {
        None
    } else {
        Some((scheme.to_ascii_lowercase(), host))
    }
}

/// `ProxyServer` 端点合法性检查：非空且不含路径分隔与空白即接受（端口缺省
/// 由代理 scheme 默认值决定）。
fn normalize_host_port(target: &str) -> Option<String> {
    let target = target.trim();
    if target.is_empty() || target.contains(['/', '\\', ' ', '\t']) {
        return None;
    }
    Some(target.to_string())
}

/// 解析 `ProxyOverride` 例外表；`<-loopback>` 是「不例外回环」的否定标记，
/// 与默认行为一致，直接忽略。精确条目经 [`override_host`] 做方括号与端口
/// 规范化后存储，与 URL 侧 [`split_scheme_host`] 输出的裸主机口径统一。
fn parse_override(override_list: Option<&str>) -> Vec<BypassRule> {
    let Some(list) = override_list else {
        return Vec::new();
    };
    list.split([';', ' ', '\t'])
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .filter(|entry| *entry != "<-loopback>")
        .map(|entry| {
            let lowered = entry.to_ascii_lowercase();
            if lowered == "<local>" {
                BypassRule::Local
            } else if lowered.contains('*') {
                BypassRule::Pattern(lowered)
            } else {
                BypassRule::Exact(override_host(&lowered).to_string())
            }
        })
        .collect()
}

/// 例外条目到 URL 侧主机比对口径的规范化：`[::1]` / `[::1]:8080` 去方括号得
/// 裸 IPv6 主机 `::1`（方括号内按 IPv6 字面量处理，不再剥端口段）；无方括号
/// 时仅当形如 `主机:数字端口` 才剥端口，含多个冒号的裸 IPv6 与其他形态原样
/// 返回（保守不命中）。括号不成对的畸形条目原样返回。
fn override_host(entry: &str) -> &str {
    if let Some(rest) = entry.strip_prefix('[') {
        if let Some((host, _tail)) = rest.split_once(']') {
            return host;
        }
        return entry;
    }
    match entry.split_once(':') {
        Some((host, port))
            if !host.is_empty()
                && !host.contains(':')
                && !port.is_empty()
                && port.chars().all(|char| char.is_ascii_digit()) =>
        {
            host
        }
        _ => entry,
    }
}

/// 单条例外规则对目标主机是否命中（host 已小写、不含端口）。`*.suffix` 是
/// 最常见形态，按 WinInet 语义要求点边界（`xcorp.example` 不命中
/// `*.corp.example`）；`*` 在其他位置的条目退化为通用通配匹配。
fn rule_matches(rule: &BypassRule, host: &str) -> bool {
    match rule {
        BypassRule::Local => !host.contains('.'),
        BypassRule::Pattern(pattern) => {
            if let Some(suffix) = pattern.strip_prefix("*.") {
                let suffix = suffix.trim_end_matches('.');
                host == suffix || host.ends_with(&format!(".{suffix}"))
            } else {
                wildcard_match(pattern, host)
            }
        }
        BypassRule::Exact(entry) => {
            // 条目在 parse_override 已规范化；此处再过一遍 [`override_host`]
            // 保证任何来源构造的规则都与 URL 侧裸主机口径一致（含方括号 IPv6）。
            let host_part = override_host(entry);
            host_part == host || host_part.split(':').next().unwrap_or(host_part) == host
        }
    }
}

/// 通配匹配（`*` 匹配任意序列，含空序列；其余字符精确相等；大小写不敏感）。
fn wildcard_match(pattern: &str, text: &str) -> bool {
    let pattern: Vec<char> = pattern.to_ascii_lowercase().chars().collect();
    let text: Vec<char> = text.to_ascii_lowercase().chars().collect();
    let (mut p, mut t) = (0_usize, 0_usize);
    let (mut star, mut mark) = (usize::MAX, 0_usize);
    while t < text.len() {
        if p < pattern.len() && (pattern[p] == '*' || pattern[p] == text[t]) {
            if pattern[p] == '*' {
                star = p;
                mark = t;
                p += 1;
            } else {
                p += 1;
                t += 1;
            }
        } else if star != usize::MAX {
            mark += 1;
            t = mark;
            p = star + 1;
        } else {
            return false;
        }
    }
    while p < pattern.len() && pattern[p] == '*' {
        p += 1;
    }
    p == pattern.len()
}

/// 例外条目到 libcurl `no_proxy` 条目的近似翻译；无法表达的返回 `None`。
/// 条目先经 [`override_host`] 规范化：方括号 IPv6 去括号（`no_proxy` 不带
/// 括号口径），`主机:端口` 剥端口。
fn translate_no_proxy(rule: &BypassRule) -> Option<String> {
    match rule {
        BypassRule::Local => None,
        BypassRule::Pattern(pattern) => {
            if pattern == "*" {
                Some((*pattern).clone())
            } else {
                pattern.strip_prefix("*.").map(str::to_string)
            }
        }
        BypassRule::Exact(entry) => Some(override_host(entry).to_string()),
    }
}

#[cfg(windows)]
fn read_registry_dword(name: &str) -> Option<u32> {
    use windows_sys::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD};
    let path = wide_null("Software\\Microsoft\\Windows\\CurrentVersion\\Internet Settings");
    let name = wide_null(name);
    let mut value: u32 = 0;
    let mut size = u32::try_from(std::mem::size_of::<u32>()).unwrap_or(u32::MAX);
    // SAFETY: path/name 都是以 NUL 结尾的 UTF-16 缓冲区；value/size 是配套的
    // DWORD 输出缓冲区（RRF_RT_REG_DWORD 要求大小恰为 4），调用期间指针有效。
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            path.as_ptr(),
            name.as_ptr(),
            RRF_RT_REG_DWORD,
            std::ptr::null_mut(),
            (&raw mut value).cast::<core::ffi::c_void>(),
            &raw mut size,
        )
    };
    (status == 0).then_some(value)
}

#[cfg(windows)]
fn read_registry_string(name: &str) -> Option<String> {
    use windows_sys::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_SZ};
    let path = wide_null("Software\\Microsoft\\Windows\\CurrentVersion\\Internet Settings");
    let name = wide_null(name);
    let mut size: u32 = 0;
    // SAFETY: path/name 是以 NUL 结尾的 UTF-16 缓冲区；size 为字节尺寸输出，
    // 只探测所需长度，不写数据缓冲区。
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            path.as_ptr(),
            name.as_ptr(),
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &raw mut size,
        )
    };
    if status != 0 {
        return None;
    }
    let mut buffer = vec![0_u16; (size as usize / 2).max(1)];
    // SAFETY: buffer 按探测到的尺寸分配，足以容纳 REG_SZ 输出；size 与缓冲区
    // 配套（字节数），调用期间指针均有效。
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            path.as_ptr(),
            name.as_ptr(),
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            buffer.as_mut_ptr().cast::<core::ffi::c_void>(),
            &raw mut size,
        )
    };
    if status != 0 {
        return None;
    }
    let len = buffer
        .iter()
        .position(|&unit| unit == 0)
        .unwrap_or(buffer.len());
    Some(String::from_utf16_lossy(&buffer[..len]))
}

#[cfg(windows)]
fn wide_null(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::SystemProxy;

    // 覆盖 P-09：整体 ProxyServer 同时作为 http 与 https 出口；例外表命中则直连。
    #[test]
    fn single_server_applies_to_all_protocols_and_respects_bypass() {
        let proxy =
            SystemProxy::from_registry_values(1, Some("127.0.0.1:7890"), Some("localhost;<local>"));
        assert_eq!(
            proxy.endpoint_for_url("https://github.com/owner/repo/releases/tag/v1"),
            Some("http://127.0.0.1:7890".to_string())
        );
        assert_eq!(
            proxy.endpoint_for_url("http://example.com/a"),
            Some("http://127.0.0.1:7890".to_string())
        );
        // <local>：无点主机直连；localhost 精确例外直连。
        assert_eq!(proxy.endpoint_for_url("http://intranet/wiki"), None);
        assert_eq!(proxy.endpoint_for_url("https://LOCALHOST:8443/x"), None);
    }

    // 覆盖 P-09：按协议条目各自成端点，socks 条目映射为 socks5://；
    // https 目标缺 https= 条目时按 https → socks → http 顺序回退。
    #[test]
    fn per_protocol_entries_select_endpoint_by_target_scheme() {
        let proxy = SystemProxy::from_registry_values(
            1,
            Some("http=10.0.0.1:80;https=10.0.0.2:443;socks=[::1]:1080"),
            None,
        );
        assert_eq!(
            proxy.endpoint_for_url("https://github.com/a"),
            Some("http://10.0.0.2:443".to_string())
        );
        assert_eq!(
            proxy.endpoint_for_url("http://example.com/a"),
            Some("http://10.0.0.1:80".to_string())
        );

        let socks_only =
            SystemProxy::from_registry_values(1, Some("http=h:1;socks=127.0.0.1:1080"), None);
        assert_eq!(
            socks_only.endpoint_for_url("https://github.com/a"),
            Some("socks5://127.0.0.1:1080".to_string())
        );
    }

    // 覆盖 P-09：ProxyEnable=0、ProxyServer 缺失或不可解析时一律直连，
    // 不因注册表异常阻塞功能。
    #[test]
    fn disabled_or_unparseable_registry_falls_back_to_direct() {
        for (enable, server) in [
            (0, Some("127.0.0.1:7890")),
            (1, None),
            (1, Some("  ")),
            (1, Some("bad value/with path")),
        ] {
            let proxy = SystemProxy::from_registry_values(enable, server, None);
            assert!(
                !proxy.is_enabled(),
                "应视为未启用：enable={enable} server={server:?}"
            );
            assert_eq!(proxy.endpoint_for_url("https://github.com/a"), None);
            assert!(proxy.git_env().is_empty());
        }
    }

    // 覆盖 P-09：例外表通配（*.suffix、前缀段）与带端口精确条目的匹配口径。
    #[test]
    fn bypass_wildcards_match_windows_style_patterns() {
        let proxy = SystemProxy::from_registry_values(
            1,
            Some("127.0.0.1:7890"),
            Some("*.corp.example;192.168.*;proxyhost:443"),
        );
        assert_eq!(
            proxy.endpoint_for_url("https://git.CORP.example/repo.git"),
            None
        );
        assert_eq!(
            proxy.endpoint_for_url("https://a.b.corp.example/repo.git"),
            None
        );
        assert_eq!(proxy.endpoint_for_url("http://192.168.1.23/git"), None);
        // *.corp.example 需按点边界匹配：xcorp.example 不命中，仍走代理。
        assert_eq!(
            proxy.endpoint_for_url("https://xcorp.example/repo.git"),
            Some("http://127.0.0.1:7890".to_string())
        );
        assert_eq!(
            proxy.endpoint_for_url("https://proxyhost/repo.git"),
            None,
            "精确条目带端口时按主机名命中"
        );
        assert_eq!(
            proxy.endpoint_for_url("https://github.com/a"),
            Some("http://127.0.0.1:7890".to_string())
        );
    }

    // 覆盖 P-09（回归：`<local>`、`192.168.*` 这类 no_proxy 表达不了的例外
    // 规则经 bypassed() 前置判断命中——git 网络命令因此直连而不被注入代理；
    // 修复前这些条目在 git_env 的 no_proxy 近似翻译中被丢弃，内网目标仍被推入代理）
    #[test]
    fn bypassed_matches_untranslatable_override_rules() {
        let proxy = SystemProxy::from_registry_values(
            1,
            Some("127.0.0.1:7890"),
            Some("<local>;192.168.*;*.corp.example;github.com"),
        );
        assert!(
            proxy.bypassed("http://intranet/wiki"),
            "<local>：无点主机命中"
        );
        assert!(
            proxy.bypassed("http://192.168.1.23/git"),
            "192.168.* 前缀通配命中"
        );
        assert!(
            proxy.bypassed("https://git.corp.example/repo.git"),
            "*.suffix 命中"
        );
        assert!(proxy.bypassed("https://github.com/a"), "精确条目命中");
        assert!(!proxy.bypassed("https://example.com/a"), "例外之外不命中");
        // 未启用时恒不例外（调用方据此走注入/回退路径）。
        let disabled =
            SystemProxy::from_registry_values(0, Some("127.0.0.1:7890"), Some("<local>"));
        assert!(!disabled.bypassed("http://intranet/wiki"));
        assert!(
            !proxy.bypassed("not-a-url"),
            "解析不出 host 的目标保守处理为不例外"
        );
    }

    // 覆盖 P-09：注册表代理到 libcurl 环境变量的映射与 no_proxy 近似翻译。
    #[test]
    fn git_env_maps_registry_to_curl_variables() {
        let proxy = SystemProxy::from_registry_values(
            1,
            Some("http=10.0.0.1:80;https=10.0.0.2:443;socks=127.0.0.1:1080"),
            Some("*.corp.example;<local>;192.168.*"),
        );
        let env = proxy.git_env();
        assert!(env.contains(&("http_proxy".to_string(), "http://10.0.0.1:80".to_string())));
        assert!(env.contains(&("https_proxy".to_string(), "http://10.0.0.2:443".to_string())));
        assert!(env.contains(&(
            "all_proxy".to_string(),
            "socks5://127.0.0.1:1080".to_string()
        )));
        // <local> 与任意位置通配（192.168.*）无法表达，被丢弃；*.corp.example 翻译为后缀条目。
        let no_proxy = env
            .iter()
            .find(|(key, _)| key == "no_proxy")
            .map(|(_, value)| value.clone())
            .unwrap_or_default();
        assert_eq!(no_proxy, "corp.example");
    }

    // 覆盖 P-09：git 直连回退只由连接类失败触发，认证与推送冲突不得触发。
    #[test]
    fn network_signature_matches_connect_failures_only() {
        assert!(super::network_failure_signature(
            "fatal: unable to access 'https://github.com/a/b/': Failed to connect to github.com port 443: Timed out"
        ));
        assert!(super::network_failure_signature(
            "error: Couldn't connect to proxy"
        ));
        assert!(super::network_failure_signature(
            "fatal: unable to access 'https://github.com/': Could not resolve host: github.com"
        ));
        assert!(!super::network_failure_signature(
            "remote: HTTP Basic: Access denied\nfatal: Authentication failed for 'https://github.com/a/b/'"
        ));
        assert!(!super::network_failure_signature(
            " ! [rejected]        main -> main (non-fast-forward)"
        ));
        assert!(!super::network_failure_signature(""));
    }

    // 覆盖 P-09（回归：例外表带方括号的 IPv6 条目与 URL 侧解析出的裸 IPv6
    // 主机统一口径——修复前 `[::1]` / `[::1]:8080` 条目经 `split(':')` 被切成
    // `[`，永不命中，`::1` 目标仍被推入代理；no_proxy 翻译同样产出损坏条目）。
    #[test]
    fn ipv6_bracket_bypass_entries_match_bare_url_host() {
        // 例外表只写方括号形态（WinInet 的 IPv6 常见写法）；裸写 `::1` 本就
        // 按整串比较命中，不在此重复覆盖。
        let proxy =
            SystemProxy::from_registry_values(1, Some("127.0.0.1:7890"), Some("[::1];[::1]:8080"));
        // URL 侧 `split_scheme_host` 把 `[::1]:8080` 剥成裸主机 `::1`：例外条目
        // 无论带端口、带方括号还是裸写都必须按主机命中。
        assert_eq!(proxy.endpoint_for_url("http://[::1]:9999/x"), None);
        assert_eq!(proxy.endpoint_for_url("https://[::1]/a"), None);
        assert_eq!(proxy.endpoint_for_url("https://[::1]:8080/git"), None);
        // 同主机 IPv6 目标之外的地址仍走代理，不因方括号条目扩大例外范围。
        assert_eq!(
            proxy.endpoint_for_url("https://[::2]/a"),
            Some("http://127.0.0.1:7890".to_string())
        );
        assert!(proxy.bypassed("http://[::1]/wiki"));
    }

    // 覆盖 P-09：方括号 IPv6 例外条目翻译为 no_proxy 时去方括号（libcurl
    // `no_proxy` 不带括号口径）；普通 host:port 条目仍剥端口。
    #[test]
    fn no_proxy_translation_unwraps_ipv6_brackets() {
        let proxy = SystemProxy::from_registry_values(
            1,
            Some("127.0.0.1:7890"),
            Some("[::1]:8080;proxyhost:443;*.corp.example"),
        );
        let env = proxy.git_env();
        let no_proxy = env
            .iter()
            .find(|(key, _)| key == "no_proxy")
            .map(|(_, value)| value.clone())
            .unwrap_or_default();
        assert_eq!(no_proxy, "::1,proxyhost,corp.example");
    }

    // 覆盖 P-09：URL 主机解析覆盖 userinfo、端口、IPv6 与尾点 FQDN 形态。
    #[test]
    fn url_host_extraction_handles_common_authority_forms() {
        let proxy = SystemProxy::from_registry_values(1, Some("p:1"), Some("github.com"));
        for url in [
            "https://user:pass@GitHub.com.:443/owner/repo",
            "https://github.com/owner/repo?x=1#frag",
            "HTTP://github.com/a",
        ] {
            assert_eq!(
                proxy.endpoint_for_url(url),
                None,
                "应命中 github.com 例外：{url}"
            );
        }
        assert_eq!(proxy.endpoint_for_url("not-a-url"), None);
    }
}
