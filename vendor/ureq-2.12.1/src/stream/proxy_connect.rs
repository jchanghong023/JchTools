use std::io::{self, Write};

pub(super) fn write_proxy_connect(stream: &mut impl Write, request: &str) -> io::Result<()> {
    write!(stream, "{}", request)?;
    stream.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Error, ErrorKind};

    // 覆盖 P-09：CONNECT 写入失败必须保留 Io/BrokenPipe，不能 panic。
    #[test]
    fn proxy_connect_write_error_is_not_panic() {
        struct BrokenWriter;
        impl Write for BrokenWriter {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::Error::new(io::ErrorKind::BrokenPipe, "代理连接已断开"))
            }

            fn flush(&mut self) -> io::Result<()> {
                panic!("写失败后不应执行 flush");
            }
        }
        // 与 connect_host 的 ? 一样，通过真实的 From<io::Error> 转为 Transport。
        let error: Error = write_proxy_connect(
            &mut BrokenWriter,
            "CONNECT example.com:443 HTTP/1.1\r\n\r\n",
        )
        .unwrap_err()
        .into();
        assert_eq!(error.kind(), ErrorKind::Io);
        let source = std::error::Error::source(&error).unwrap();
        let source = source.downcast_ref::<io::Error>().unwrap();
        assert_eq!(source.kind(), io::ErrorKind::BrokenPipe);
    }

    // 真实公开 Agent 经本地 HTTP CONNECT 完成请求；TLS 仅用透传测试连接器隔离。
    #[test]
    fn proxy_connect_public_request_still_succeeds() {
        struct TestTls;
        impl crate::TlsConnector for TestTls {
            fn connect(
                &self,
                dns_name: &str,
                stream: Box<dyn crate::ReadWrite>,
            ) -> Result<Box<dyn crate::ReadWrite>, Error> {
                assert_eq!(dns_name, "example.com");
                Ok(stream)
            }
        }
        fn read_headers(socket: &mut std::net::TcpStream) -> Vec<u8> {
            use std::io::Read;
            let mut request = Vec::new();
            let mut byte = [0];
            while !request.ends_with(b"\r\n\r\n") {
                assert!(request.len() < 4096);
                socket.read_exact(&mut byte).unwrap();
                request.push(byte[0]);
            }
            request
        }
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let proxy_addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let timeout = Some(std::time::Duration::from_secs(2));
            socket.set_read_timeout(timeout).unwrap();
            socket.set_write_timeout(timeout).unwrap();
            let connect = read_headers(&mut socket);
            assert!(connect.starts_with(b"CONNECT example.com:443 HTTP/1.1\r\n"));
            socket
                .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                .unwrap();
            let request = read_headers(&mut socket);
            assert!(request.starts_with(b"GET https://example.com/image HTTP/1.1\r\n"));
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
                .unwrap();
        });
        let response = crate::AgentBuilder::new()
            .proxy(crate::Proxy::new(format!("http://{proxy_addr}")).unwrap())
            .tls_connector(std::sync::Arc::new(TestTls))
            .timeout_connect(std::time::Duration::from_secs(2))
            .timeout_read(std::time::Duration::from_secs(2))
            .build()
            .get("https://example.com/image")
            .call();
        server.join().unwrap();
        assert_eq!(response.unwrap().into_string().unwrap(), "ok");
    }
}
