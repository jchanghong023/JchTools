use std::fmt;
use std::iter::once;
use std::net::{SocketAddr, TcpStream};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::{io, thread};

use socks::{Socks4Stream, Socks5Stream, ToTargetAddr};

use crate::Error;
use crate::proxy::{Proxy, ProxyProtocol};
use crate::util::UriExt;

use super::ResolvedSocketAddrs;
use super::chain::Either;

use super::tcp::TcpTransport;
use super::{ConnectionDetails, Connector, LazyBuffers, NextTimeout, Transport};

/// Connector for SOCKS proxies.
///
/// Requires the **socks-proxy** feature.
///
/// The connector looks at the proxy settings in [`proxy`](crate::config::ConfigBuilder::proxy) to
/// determine whether to attempt a proxy connection or not.
#[derive(Default)]
pub struct SocksConnector(());

impl<In: Transport> Connector<In> for SocksConnector {
    type Out = Either<In, TcpTransport>;

    fn connect(
        &self,
        details: &ConnectionDetails,
        chained: Option<In>,
    ) -> Result<Option<Self::Out>, Error> {
        let proxy = match details.config.proxy() {
            Some(v) if v.protocol().is_socks() => v,
            // If there is no proxy configured, or it isn't a SOCKS proxy, use whatever is chained.
            _ => {
                trace!("SOCKS not configured");
                return Ok(chained.map(Either::A));
            }
        };

        if chained.is_some() {
            trace!("Skip");
            return Ok(chained.map(Either::A));
        }

        let proxy_addrs = details
            .resolver
            .resolve(proxy.uri(), details.config, details.timeout)?;

        // Check if this host is not supposed to be proxied.
        let is_no_proxy = details
            .config
            .proxy()
            .map(|p| p.is_no_proxy(details.uri))
            .unwrap_or(false);

        if is_no_proxy {
            return Ok(None);
        }

        let stream = if proxy.resolve_target() {
            // The target is already resolved by run().
            let resolved = details.addrs.iter().cloned();

            try_connect(&proxy_addrs, resolved, proxy, details.timeout)?
        } else {
            // Do not to resolve the target locally, instead pass (host, port)
            // to the proxy and let it resolve.
            // 本地修补（P-09）：socks crate 的 (&str, u16) 目标不识别 IPv6
            // 方括号，带方括号的主机会按域名编码（ATYP=3），代理无法解析
            // "[::1]" 这样的目标。剥除方括号后由 Ipv6Addr 解析命中 IP 目标
            // （ATYP=4）；域名与 IPv4 目标不受影响。
            let iter = once(details.uri.host_port())
                .map(|(host, port)| (socks_target_host(host), port));
            try_connect(&proxy_addrs, iter, proxy, details.timeout)?
        };

        if details.config.no_delay() {
            stream.set_nodelay(true)?;
        }

        let buffers = LazyBuffers::new(
            details.config.input_buffer_size(),
            details.config.output_buffer_size(),
        );
        let transport = TcpTransport::new(stream, buffers);

        Ok(Some(Either::B(transport)))
    }
}

fn try_connect<'a, T: ToTargetAddr + fmt::Debug + Send + 'a + Clone>(
    proxy_addrs: &ResolvedSocketAddrs,
    target_addrs: impl Iterator<Item = T>,
    proxy: &Proxy,
    timeout: NextTimeout,
) -> Result<TcpStream, Error> {
    for target_addr in target_addrs {
        for proxy_addr in proxy_addrs {
            trace!(
                "Try connect {} {} -> {:?}",
                proxy.protocol(),
                proxy_addr,
                target_addr
            );

            match try_connect_single(*proxy_addr, target_addr.clone(), proxy, timeout) {
                Ok(v) => {
                    debug!(
                        "{} connected {} -> {:?}",
                        proxy.protocol(),
                        proxy_addr,
                        target_addr
                    );
                    return Ok(v);
                }
                // Intercept ConnectionRefused to try next addrs
                Err(Error::Io(e)) if e.kind() == io::ErrorKind::ConnectionRefused => {
                    trace!(
                        "{} -> {:?} proxy connection refused",
                        proxy_addr, target_addr
                    );
                    continue;
                }
                // Other errors bail
                Err(e) => return Err(e),
            }
        }
    }

    debug!("Proxy failed to to connect to any resolved address");
    Err(Error::Io(io::Error::new(
        io::ErrorKind::ConnectionRefused,
        "Connection refused",
    )))
}

fn try_connect_single<'a, T: ToTargetAddr + Send + 'a>(
    proxy_addr: SocketAddr,
    target_addr: T,
    proxy: &Proxy,
    timeout: NextTimeout,
) -> Result<TcpStream, Error> {
    // The async behavior is only used if we want to time cap connecting.
    let use_sync = timeout.after.is_not_happening();

    if use_sync {
        connect_proxy(proxy, proxy_addr, target_addr)
    } else {
        let (tx, rx) = mpsc::sync_channel(1);
        let proxy = proxy.clone();

        thread::scope(move |s| {
            s.spawn(move || tx.send(connect_proxy(&proxy, proxy_addr, target_addr)));

            match rx.recv_timeout(*timeout.after) {
                Ok(v) => v,
                Err(RecvTimeoutError::Timeout) => Err(Error::Timeout(timeout.reason)),
                Err(RecvTimeoutError::Disconnected) => unreachable!("mpsc sender gone"),
            }
        })
    }
}

fn connect_proxy<'a, T: ToTargetAddr + 'a>(
    proxy: &Proxy,
    proxy_addr: SocketAddr,
    target_addr: T,
) -> Result<TcpStream, Error> {
    let stream = match proxy.protocol() {
        ProxyProtocol::Socks4 | ProxyProtocol::Socks4A => {
            if proxy.username().is_some() {
                debug!("SOCKS4 does not support username/password");
            }

            Socks4Stream::connect(proxy_addr, target_addr, "")?.into_inner()
        }

        ProxyProtocol::Socks5 | ProxyProtocol::Socks5h => {
            if let Some(username) = proxy.username() {
                // Connect with authentication.
                let password = proxy.password().unwrap_or("");

                Socks5Stream::connect_with_password(proxy_addr, target_addr, username, password)?
            } else {
                Socks5Stream::connect(proxy_addr, target_addr)?
            }
            .into_inner()
        }

        _ => unreachable!(), // HTTP(s) proxies.
    };

    Ok(stream)
}

impl fmt::Debug for SocksConnector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SocksConnector").finish()
    }
}

/// 本地修补（P-09）：URL 的 IPv6 host 带方括号，仅在交给 socks crate 的
/// 远端解析路径（socks5h / socks4a）时剥除，使 IPv6 字面量目标按 IP
/// 地址类型编码；socks5 本地解析路径已在 run() 中解析为 SocketAddr。
fn socks_target_host(host: &str) -> &str {
    host.strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
        .unwrap_or(host)
}

#[cfg(test)]
mod tests {
    use super::socks_target_host;

    // 覆盖 P-09：远端解析路径只剥成对的 IPv6 方括号，域名与 IPv4 原样。
    #[test]
    fn target_host_strips_ipv6_brackets_only() {
        assert_eq!(socks_target_host("[::1]"), "::1");
        assert_eq!(socks_target_host("[2001:db8::1]"), "2001:db8::1");
        assert_eq!(socks_target_host("example.com"), "example.com");
        assert_eq!(socks_target_host("127.0.0.1"), "127.0.0.1");
    }

    // 覆盖 P-09：剥除后的 IPv6 字面量命中 socks 的 IP 目标（ATYP=4）；
    // 未剥除的带括号主机按域名目标编码（ATYP=3），作为当前缺陷对照。
    #[test]
    fn bare_ipv6_target_maps_to_ip_target_addr() {
        use socks::TargetAddr;
        use socks::ToTargetAddr;
        use std::net::SocketAddr;

        let addr = socks_target_host("[::1]").to_target_addr().unwrap();
        assert!(matches!(addr, TargetAddr::Ip(SocketAddr::V6(_))));

        let addr = ("[::1]", 8080u16).to_target_addr().unwrap();
        assert!(matches!(
            addr,
            TargetAddr::Domain(domain, 8080) if domain == "[::1]"
        ));
    }

    // 覆盖 P-09：经 socks5h 远端解析代理请求 IPv6 字面量目标时，CONNECT
    // 命令必须使用 IPv6 地址类型（ATYP=4 + 16 字节地址），不能把带方括号
    // 的主机当域名发送。真实公开 Agent + 本地回环伪 SOCKS5 服务器。
    #[test]
    fn socks5h_ipv6_target_uses_ip_address() {
        use std::io::{Read, Write};
        use std::net::Ipv6Addr;
        use std::net::TcpListener;
        use std::thread;
        use std::time::Duration;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let proxy_addr = listener.local_addr().unwrap();

        let server = thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let timeout = Some(Duration::from_secs(5));
            socket.set_read_timeout(timeout).unwrap();
            socket.set_write_timeout(timeout).unwrap();

            let mut greeting = [0; 3];
            socket.read_exact(&mut greeting).unwrap();
            assert_eq!(greeting, [5, 1, 0]);
            socket.write_all(&[5, 0]).unwrap();

            let mut command = [0; 4];
            socket.read_exact(&mut command).unwrap();
            assert_eq!(command, [5, 1, 0, 4]);

            let mut target = [0; 18];
            socket.read_exact(&mut target).unwrap();
            assert_eq!(&target[..16], &Ipv6Addr::LOCALHOST.octets());
            assert_eq!(&target[16..], &8080u16.to_be_bytes());

            socket
                .write_all(&[5, 0, 0, 1, 127, 0, 0, 1, 0, 0])
                .unwrap();

            let mut request = Vec::new();
            let mut byte = [0];
            while !request.ends_with(b"\r\n\r\n") {
                assert!(request.len() < 4096);
                socket.read_exact(&mut byte).unwrap();
                request.push(byte[0]);
            }
            assert!(request.starts_with(b"GET /image HTTP/1.1\r\n"));

            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
                .unwrap();
        });

        let proxy = crate::Proxy::new(format!("socks5h://{proxy_addr}")).unwrap();

        let agent: crate::Agent = crate::Agent::config_builder()
            .proxy(Some(proxy))
            .timeout_connect(Some(Duration::from_secs(5)))
            .build()
            .into();

        let result = agent.get("http://[::1]:8080/image").call();

        server.join().unwrap();
        assert_eq!(
            result.unwrap().body_mut().read_to_string().unwrap(),
            "ok"
        );
    }
}
