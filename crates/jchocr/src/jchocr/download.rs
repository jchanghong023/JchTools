use std::io::Read;
use std::time::Duration;
use ureq::unversioned::{
    resolver::DefaultResolver,
    transport::{
        ConnectProxyConnector, ConnectionDetails, Connector, RustlsConnector, SocksConnector,
        TcpConnector,
    },
};

use jchtools_xberg::xberg_runtime::MAX_MESSAGE_BYTES;

use super::cli::Failure;

// 为共享协议 envelope 的有界元数据预留 256 KiB，其余预算覆盖 Base64 的 4:3 扩张。
const MAX_DOWNLOADED_IMAGE_BYTES: u64 = ((MAX_MESSAGE_BYTES - 256 * 1024) / 4) * 3;

// 仅在 TCP / SOCKS / CONNECT 阶段保留可回退错误；TLS 握手也可能返回 Io
// 或 Timeout::Connect，不能从最终错误枚举或错误文案反推它属于代理连接阶段。
#[derive(Debug)]
struct ConnectionStage<C>(C);

#[derive(Debug)]
struct ProxyConnectionFailure(ureq::Error);

impl std::fmt::Display for ProxyConnectionFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("代理连接阶段失败")
    }
}

impl std::error::Error for ProxyConnectionFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.0)
    }
}

impl<C: Connector> Connector for ConnectionStage<C> {
    type Out = C::Out;

    fn connect(
        &self,
        details: &ConnectionDetails<'_>,
        chained: Option<()>,
    ) -> Result<Option<Self::Out>, ureq::Error> {
        self.0.connect(details, chained).map_err(|error| {
            if matches!(
                error,
                ureq::Error::HostNotFound
                    | ureq::Error::ConnectionFailed
                    | ureq::Error::ConnectProxyFailed(_)
                    | ureq::Error::Io(_)
                    | ureq::Error::Timeout(ureq::Timeout::Connect)
            ) {
                ureq::Error::Other(Box::new(ProxyConnectionFailure(error)))
            } else {
                error
            }
        })
    }
}

fn proxy_connection_failed(error: &ureq::Error) -> bool {
    match error {
        // 解析可能在 connector 之前失败；这两个变体不表示 TLS / 响应阶段错误。
        ureq::Error::HostNotFound | ureq::Error::ConnectionFailed => true,
        ureq::Error::Other(error) => error.is::<ProxyConnectionFailure>(),
        _ => false,
    }
}

fn http_request(
    url: &str,
    proxy: Option<&str>,
) -> Result<ureq::http::Response<ureq::Body>, ureq::Error> {
    // HTTP 错误状态仍是有效响应：统一交给下载层判断状态码，避免错误枚举携带响应体。
    let mut config = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .max_redirects(0)
        .max_redirects_will_error(false)
        .timeout_connect(Some(Duration::from_secs(30)))
        .timeout_recv_response(Some(Duration::from_secs(30)))
        .timeout_recv_body(Some(Duration::from_secs(30)));
    // ureq 3 的 config_builder 默认继承环境代理（Proxy::try_from_env）；
    // P-09 要求逐跳显式路由，无代理时必须显式清空，避免环境变量劫持"直连"。
    let proxy = proxy.and_then(|value| ureq::Proxy::new(value).ok());
    config = config.proxy(proxy);
    // 与锁定 ureq 的默认 SOCKS → CONNECT → TCP → rustls 链一致，仅在 TLS
    // 之前标记连接错误。证书校验及握手签名校验保持默认开启。
    let connection =
        ().chain(SocksConnector::default())
            .chain(ConnectProxyConnector::default())
            .chain(TcpConnector::default());
    let connector = ConnectionStage(connection).chain(RustlsConnector::default());
    ureq::Agent::with_parts(config.build(), connector, DefaultResolver::default())
        .get(url)
        .call()
}

fn image_download_over_budget(stage: &'static str) -> Failure {
    tracing::warn!(
        component = "jchocr",
        event = "image_download_rejected",
        stage,
        reason = "message_budget_exceeded",
        "图片下载超过共享协议容量预算"
    );
    Failure::new(1, "network", "图片下载超过共享协议容量预算")
}

pub(super) fn fetch_image(mut url: String) -> Result<Vec<u8>, Failure> {
    let proxy = jchtools_foundation::system_proxy::read();
    for hop in 0..3 {
        let endpoint = proxy.endpoint_for_url(&url);
        let mut attempt = 1_u8;
        let mut route = if endpoint.is_some() {
            "proxy"
        } else {
            "direct"
        };
        tracing::info!(
            component = "jchocr",
            event = "image_download_started",
            redirect_hop = hop,
            target = "image_download",
            attempt,
            route,
            "图片下载开始"
        );
        let response = match http_request(&url, endpoint.as_deref()) {
            Err(error) if endpoint.is_some() && proxy_connection_failed(&error) => {
                tracing::warn!(
                    component = "jchocr",
                    event = "image_download_direct_retry",
                    target = "image_download",
                    stage = "connect",
                    redirect_hop = hop,
                    route,
                    attempt,
                    next_attempt = 2,
                    "系统代理连接失败，直连重试一次"
                );
                attempt = 2;
                route = "direct";
                http_request(&url, None)
            }
            result => result,
        }
        .map_err(|_| Failure::new(1, "network", "图片下载失败：网络连接错误"))?;
        if matches!(response.status().as_u16(), 301 | 302 | 303 | 307 | 308) {
            if hop == 2 {
                return Err(Failure::new(
                    1,
                    "network",
                    "图片下载达到 HTTP 重定向次数上限",
                ));
            }
            let location = response
                .headers()
                .get("location")
                .and_then(|value| value.to_str().ok())
                .ok_or_else(|| Failure::new(1, "network", "图片重定向缺少 Location 响应头"))?;
            let source = url::Url::parse(&url)
                .map_err(|_| Failure::new(1, "network", "图片重定向源地址无效"))?;
            let target = source
                .join(location)
                .map_err(|_| Failure::new(1, "network", "图片重定向目标地址无效"))?;
            if !matches!(target.scheme(), "http" | "https") || target.host_str().is_none() {
                return Err(Failure::new(1, "network", "图片重定向仅接受 HTTP 或 HTTPS"));
            }
            url = target.to_string();
            continue;
        }
        if !(200..300).contains(&response.status().as_u16()) {
            tracing::error!(
                component = "jchocr",
                event = "image_download_failed",
                target = "image_download",
                stage = "response_status",
                http_status = response.status().as_u16(),
                redirect_hop = hop,
                route,
                attempt,
                "图片下载返回失败状态"
            );
            return Err(Failure::new(
                1,
                "network",
                format!("图片下载失败：HTTP {}", response.status().as_u16()),
            ));
        }
        if response
            .headers()
            .get("content-length")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.trim().parse::<u64>().ok())
            .is_some_and(|length| length > MAX_DOWNLOADED_IMAGE_BYTES)
        {
            return Err(image_download_over_budget("content_length"));
        }
        let mut bytes = Vec::new();
        response
            .into_body()
            .into_reader()
            .take(MAX_DOWNLOADED_IMAGE_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| Failure::new(1, "network", "无法读取下载的图片"))?;
        if bytes.len() as u64 > MAX_DOWNLOADED_IMAGE_BYTES {
            return Err(image_download_over_budget("body"));
        }
        tracing::info!(
            component = "jchocr",
            event = "image_download_completed",
            "图片下载完成"
        );
        return Ok(bytes);
    }
    unreachable!("最后一跳必然结束重定向循环")
}
