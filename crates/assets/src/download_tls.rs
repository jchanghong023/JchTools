//! 仅供可选资产下载使用的 TLS：跳过证书身份检查，保留真实握手签名验证。
//! 通过锁定 ureq 的公开 transport 扩展接入，不改变进程或系统的 TLS 默认配置。

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, ClientConnection, DigitallySignedStruct, SignatureScheme, StreamOwned};
use std::io::{Read, Write};
use std::sync::Arc;
use ureq::unversioned::resolver::DefaultResolver;
use ureq::unversioned::transport::{
    Buffers, ConnectProxyConnector, ConnectionDetails, Connector, Either, LazyBuffers, NextTimeout,
    SocksConnector, TcpConnector, Transport, TransportAdapter,
};

pub(crate) fn agent(config: ureq::config::Config) -> Result<ureq::Agent, ureq::Error> {
    // 与 ureq 的默认链顺序一致；CONNECT 递归建立代理连接时也使用此 TLS 层。
    let connector =
        ().chain(SocksConnector::default())
            .chain(ConnectProxyConnector::default())
            .chain(TcpConnector::default())
            .chain(DownloadTlsConnector::new()?);
    Ok(ureq::Agent::with_parts(
        config,
        connector,
        DefaultResolver::default(),
    ))
}

#[derive(Debug)]
struct IdentityBypassVerifier {
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for IdentityBypassVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        // P-09 仅允许跳过信任链、颁发者、域名及有效期，不放宽握手签名。
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

#[derive(Debug)]
struct DownloadTlsConnector {
    config: Arc<ClientConfig>,
}

impl DownloadTlsConnector {
    fn new() -> Result<Self, ureq::Error> {
        // 显式选取已锁定的 ring 实现，不安装或更改全局 CryptoProvider；配置错误向调用方传播。
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let config = ClientConfig::builder_with_provider(provider.clone())
            .with_protocol_versions(rustls::ALL_VERSIONS)?
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(IdentityBypassVerifier { provider }))
            .with_no_client_auth();
        Ok(Self {
            config: Arc::new(config),
        })
    }
}

impl<In: Transport> Connector<In> for DownloadTlsConnector {
    type Out = Either<In, DownloadTlsTransport>;

    fn connect(
        &self,
        details: &ConnectionDetails<'_>,
        chained: Option<In>,
    ) -> Result<Option<Self::Out>, ureq::Error> {
        let transport = chained.ok_or(ureq::Error::ConnectionFailed)?;
        if !details.needs_tls() || transport.is_tls() {
            return Ok(Some(Either::A(transport)));
        }
        let host = details
            .uri
            .host()
            .ok_or(ureq::Error::Tls("下载 TLS 缺少主机名"))?;
        // http::Uri 的 IPv6 host 含方括号，rustls 的 IP ServerName 不接受方括号。
        let host = host
            .strip_prefix('[')
            .and_then(|value| value.strip_suffix(']'))
            .unwrap_or(host);
        let name = ServerName::try_from(host)
            .map_err(|_| ureq::Error::Tls("下载 TLS 主机名无效"))?
            .to_owned();
        let mut conn = ClientConnection::new(self.config.clone(), name)?;
        let mut sock = TransportAdapter::new(transport.boxed());
        sock.set_timeout(details.timeout);
        conn.complete_io(&mut sock)?;
        Ok(Some(Either::B(DownloadTlsTransport {
            buffers: LazyBuffers::new(
                details.config.input_buffer_size(),
                details.config.output_buffer_size(),
            ),
            stream: StreamOwned { conn, sock },
        })))
    }
}

struct DownloadTlsTransport {
    buffers: LazyBuffers,
    stream: StreamOwned<ClientConnection, TransportAdapter>,
}
impl std::fmt::Debug for DownloadTlsTransport {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DownloadTlsTransport")
            .field("chained", &self.stream.sock.inner())
            .finish_non_exhaustive()
    }
}

impl Transport for DownloadTlsTransport {
    fn buffers(&mut self) -> &mut dyn Buffers {
        &mut self.buffers
    }

    fn transmit_output(&mut self, amount: usize, timeout: NextTimeout) -> Result<(), ureq::Error> {
        self.stream.get_mut().set_timeout(timeout);
        self.stream.write_all(&self.buffers.output()[..amount])?;
        Ok(())
    }

    fn await_input(&mut self, timeout: NextTimeout) -> Result<bool, ureq::Error> {
        self.stream.get_mut().set_timeout(timeout);
        let amount = self.stream.read(self.buffers.input_append_buf())?;
        self.buffers.input_appended(amount);
        Ok(amount > 0)
    }

    fn is_open(&mut self) -> bool {
        self.stream.get_mut().get_mut().is_open()
    }

    fn is_tls(&self) -> bool {
        true
    }
}
