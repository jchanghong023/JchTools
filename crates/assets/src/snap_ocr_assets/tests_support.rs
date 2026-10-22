use std::fs;
use std::io::{Read, Write};
use std::path::Path;

#[derive(Clone)]
pub(super) struct DiagnosticCapture(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for DiagnosticCapture {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .expect("锁定日志捕获器")
            .extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

pub(super) fn capture_diagnostics<T>(run: impl FnOnce() -> T) -> (T, String) {
    // tracing-core 的单 dispatcher 快路径用首次触达 callsite 的线程默认值
    // 缓存兴趣；并行的无捕获测试会把本线程尚未触达的 ERROR callsite 缓存为
    // never。保留一个仅注册、不安装的 dispatcher，使捕获期间始终按所有
    // 存活 subscriber 合并兴趣；不刷缓存、不改变生产过滤或线程默认值。
    static REGISTRY_ANCHOR: std::sync::LazyLock<tracing::Dispatch> =
        std::sync::LazyLock::new(|| {
            tracing::Dispatch::new(tracing::subscriber::NoSubscriber::new())
        });
    std::sync::LazyLock::force(&REGISTRY_ANCHOR);
    let bytes = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let writer = DiagnosticCapture(bytes.clone());
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_max_level(tracing::Level::INFO)
        .with_writer(move || writer.clone())
        .finish();
    let result = tracing::subscriber::with_default(subscriber, run);
    let logs =
        String::from_utf8(bytes.lock().expect("读取日志捕获器").clone()).expect("日志必须为 UTF-8");
    (result, logs)
}

pub(super) struct DiagnosticWorkspace(std::path::PathBuf);

impl DiagnosticWorkspace {
    pub(super) fn new() -> Self {
        let manifest_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let repo = manifest_dir
            .ancestors()
            .find(|path| path.join("scripts/make_tmp.py").is_file())
            .expect("定位仓库工作目录工厂");
        let root = repo.join(".tmp").join(format!(
            "asset-diagnostics-{}",
            uuid::Uuid::new_v4().simple(),
        ));
        let output = std::process::Command::new("python")
            .arg(repo.join("scripts/make_tmp.py"))
            .args(["workspace", "--destination"])
            .arg(&root)
            .current_dir(&repo)
            .output()
            .expect("通过工作目录工厂创建资产诊断测试目录");
        assert!(
            output.status.success(),
            "工作目录工厂失败：{}",
            String::from_utf8_lossy(&output.stderr)
        );
        Self(root)
    }

    pub(super) fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for DiagnosticWorkspace {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

pub(super) fn diagnostic_http_server(
    response: Vec<u8>,
) -> (std::net::SocketAddr, std::thread::JoinHandle<()>) {
    diagnostic_http_responses(vec![response])
}

pub(super) fn diagnostic_http_responses(
    responses: Vec<Vec<u8>>,
) -> (std::net::SocketAddr, std::thread::JoinHandle<()>) {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("监听本地 HTTP");
    let address = listener.local_addr().expect("获取本地 HTTP 地址");
    listener.set_nonblocking(true).expect("设置监听器非阻塞");
    let thread = std::thread::spawn(move || {
        for response in responses {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error)
                        if error.kind() == std::io::ErrorKind::WouldBlock
                            && std::time::Instant::now() < deadline =>
                    {
                        std::thread::sleep(std::time::Duration::from_millis(10));
                    }
                    Err(error) => panic!("等待诊断 HTTP 请求失败：{error}"),
                }
            };
            stream.set_nonblocking(false).expect("设置连接阻塞");
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .expect("设置读取超时");
            stream
                .set_write_timeout(Some(std::time::Duration::from_secs(5)))
                .expect("设置写入超时");
            let mut request = Vec::new();
            let mut buffer = [0_u8; 512];
            while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                let read = stream.read(&mut buffer).expect("消费真实 GET 请求");
                assert!(read > 0, "GET 请求不得在请求头完成前断开");
                request.extend_from_slice(&buffer[..read]);
                assert!(request.len() < 16 * 1024, "本地 GET 请求头必须有界");
            }
            stream.write_all(&response).expect("发送真实 HTTP 响应");
        }
    });
    (address, thread)
}

// 合成证书仅用于 127.0.0.1，2020–2120 年有效；私钥不是生产凭据，不写系统证书库。
// ureq 3 不再转发 rustls，测试 TLS 服务端经 dev-dependency 直接使用 rustls
//（与 ureq 依赖的 rustls 0.23 同一副本，feature 并集启用 ring 提供者）。
pub(super) fn diagnostic_tls_server(
    proxy: bool,
    connections: usize,
    response: String,
) -> (std::net::SocketAddr, std::thread::JoinHandle<Vec<bool>>) {
    use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
    use rustls::{ServerConfig, ServerConnection, StreamOwned};
    let config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(
            vec![CertificateDer::from(
                include_bytes!("../../tests/fixtures/download-server.der").to_vec(),
            )],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(
                include_bytes!("../../tests/fixtures/download-server-key.der").to_vec(),
            )),
        )
        .expect("加载合成 TLS 服务端证书");
    let config = std::sync::Arc::new(config);
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let address = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();
    let server = std::thread::spawn(move || {
        fn read_headers(reader: &mut impl Read) -> std::io::Result<Vec<u8>> {
            let mut headers = Vec::new();
            let mut buffer = [0_u8; 512];
            while !headers.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                let read = reader.read(&mut buffer)?;
                if read == 0 || headers.len() > 16 * 1024 {
                    return Err(std::io::ErrorKind::UnexpectedEof.into());
                }
                headers.extend_from_slice(&buffer[..read]);
            }
            Ok(headers)
        }
        let mut results = Vec::new();
        for _ in 0..connections {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            let mut socket = loop {
                match listener.accept() {
                    Ok((socket, _)) => break socket,
                    Err(error)
                        if error.kind() == std::io::ErrorKind::WouldBlock
                            && std::time::Instant::now() < deadline =>
                    {
                        std::thread::sleep(std::time::Duration::from_millis(10));
                    }
                    Err(error) => panic!("等待本地 TLS 连接失败：{error}"),
                }
            };
            socket.set_nonblocking(false).unwrap();
            socket
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            socket
                .set_write_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            if proxy {
                assert!(read_headers(&mut socket).unwrap().starts_with(b"CONNECT "));
                socket
                    .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
                    .unwrap();
            }
            let connection = ServerConnection::new(std::sync::Arc::clone(&config)).unwrap();
            let mut stream = StreamOwned::new(connection, socket);
            match read_headers(&mut stream) {
                Ok(headers) => {
                    assert!(headers.starts_with(b"GET "), "必须收到真实 GET 请求");
                    stream.write_all(response.as_bytes()).unwrap();
                    stream.conn.send_close_notify();
                    stream.flush().unwrap();
                    results.push(true);
                }
                Err(_) => results.push(false),
            }
        }
        results
    });
    (address, server)
}

#[derive(Debug)]
struct ReviewF29WrongTranscriptKey(std::sync::Arc<dyn rustls::sign::SigningKey>);

impl rustls::sign::SigningKey for ReviewF29WrongTranscriptKey {
    fn choose_scheme(
        &self,
        offered: &[rustls::SignatureScheme],
    ) -> Option<Box<dyn rustls::sign::Signer>> {
        self.0.choose_scheme(offered).map(|signer| {
            Box::new(ReviewF29WrongTranscriptSigner(signer)) as Box<dyn rustls::sign::Signer>
        })
    }

    fn algorithm(&self) -> rustls::SignatureAlgorithm {
        self.0.algorithm()
    }
}

#[derive(Debug)]
struct ReviewF29WrongTranscriptSigner(Box<dyn rustls::sign::Signer>);

impl rustls::sign::Signer for ReviewF29WrongTranscriptSigner {
    fn sign(&self, message: &[u8]) -> Result<Vec<u8>, rustls::Error> {
        // 真实私钥签署错误的 transcript：签名编码有效，但绝非当前握手的签名。
        let mut wrong_message = message.to_vec();
        wrong_message.push(0);
        self.0.sign(&wrong_message)
    }

    fn scheme(&self) -> rustls::SignatureScheme {
        self.0.scheme()
    }
}

pub(super) fn review_f29_tls_server(
    version: &'static rustls::SupportedProtocolVersion,
    wrong_signature: bool,
    response: String,
) -> (std::net::SocketAddr, std::thread::JoinHandle<bool>) {
    use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
    use rustls::sign::{CertifiedKey, SingleCertAndKey};
    use std::sync::Arc;

    let key = rustls::crypto::ring::sign::any_supported_type(&PrivateKeyDer::Pkcs8(
        PrivatePkcs8KeyDer::from(
            include_bytes!("../../tests/fixtures/download-server-key.der").to_vec(),
        ),
    ))
    .expect("加载本地 TLS 测试私钥");
    let key: Arc<dyn rustls::sign::SigningKey> = if wrong_signature {
        Arc::new(ReviewF29WrongTranscriptKey(key))
    } else {
        key
    };
    let certified = CertifiedKey::new(
        vec![CertificateDer::from(
            include_bytes!("../../tests/fixtures/download-server.der").to_vec(),
        )],
        key,
    );
    let config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_protocol_versions(&[version])
    .expect("锁定测试 TLS 版本")
    .with_no_client_auth()
    .with_cert_resolver(Arc::new(SingleCertAndKey::from(certified)));
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let address = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();
    let server = std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let socket = loop {
            match listener.accept() {
                Ok((socket, _)) => break socket,
                Err(error)
                    if error.kind() == std::io::ErrorKind::WouldBlock
                        && std::time::Instant::now() < deadline =>
                {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                Err(error) => panic!("等待 F29 本地 TLS 连接失败：{error}"),
            }
        };
        socket.set_nonblocking(false).unwrap();
        socket
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        socket
            .set_write_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        let connection = rustls::ServerConnection::new(Arc::new(config)).unwrap();
        let mut stream = rustls::StreamOwned::new(connection, socket);
        let mut headers = Vec::new();
        let mut buffer = [0_u8; 512];
        while !headers.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
            match stream.read(&mut buffer) {
                Ok(0) | Err(_) => return false,
                Ok(read) => headers.extend_from_slice(&buffer[..read]),
            }
            assert!(headers.len() < 16 * 1024, "测试 GET 请求头必须有界");
        }
        assert!(headers.starts_with(b"GET "), "必须消费真实下载 GET");
        stream.write_all(response.as_bytes()).unwrap();
        stream.conn.send_close_notify();
        stream.flush().unwrap();
        true
    });
    (address, server)
}

// ── 测试环境守卫：snap 侧覆盖变量与 markdown_assets 共用同一把进程环境锁 ──

/// 测试共享进程环境变量（cargo test 并行线程），组件根相关用例必须串行；
/// 锁实例与 markdown_assets 的测试共用（见 foundation::test_support），避免
/// 两模块同时改写各自资产根覆盖变量相互覆盖。
pub(super) struct ComponentGuard {
    pub(super) root: tempfile::TempDir,
    /// 进入用例前的覆盖变量旧值：drop 时恢复而不是无条件删除——GUI 测试
    /// 装配设置的隔离根必须在本用例结束后仍然生效（C'-1，监督线程常驻，
    /// 任意时刻可能读资产根与派生管道名）。
    previous: Option<std::ffi::OsString>,
    /// bundled worker 覆盖旧值（S7-02 用例设置）：drop 时同样恢复，避免
    /// 泄漏到同进程其他用例的 bundled_worker() 读取。
    previous_worker: Option<std::ffi::OsString>,
    _lock: std::sync::MutexGuard<'static, ()>,
}

pub(super) fn redirect_component_env() -> ComponentGuard {
    let lock = jchtools_foundation::test_support::env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let previous = std::env::var_os("JCHTOOLS_SNAP_OCR_TEST_ASSET_ROOT");
    let previous_worker = std::env::var_os("JCHTOOLS_TEST_BUNDLED_WORKER");
    let root = tempfile::tempdir().expect("创建资产根目录");
    std::env::set_var("JCHTOOLS_SNAP_OCR_TEST_ASSET_ROOT", root.path());
    ComponentGuard {
        root,
        previous,
        previous_worker,
        _lock: lock,
    }
}

impl Drop for ComponentGuard {
    fn drop(&mut self) {
        match self.previous.clone() {
            Some(value) => {
                std::env::set_var("JCHTOOLS_SNAP_OCR_TEST_ASSET_ROOT", value);
            }
            None => std::env::remove_var("JCHTOOLS_SNAP_OCR_TEST_ASSET_ROOT"),
        }
        match self.previous_worker.clone() {
            Some(value) => {
                std::env::set_var("JCHTOOLS_TEST_BUNDLED_WORKER", value);
            }
            None => std::env::remove_var("JCHTOOLS_TEST_BUNDLED_WORKER"),
        }
    }
}

pub(super) fn sha256_bytes(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}
