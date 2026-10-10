//! AH-13：在真实 TCP 连接上观察读端关闭，不读取或复制 Hyper 的管线请求字节。

use std::{
    io,
    net::SocketAddr,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};

use axum::{
    extract::connect_info::Connected,
    serve::{IncomingStream, Listener},
};
use tokio::{
    io::{AsyncRead, AsyncWrite, Interest, ReadBuf},
    net::{
        tcp::{OwnedReadHalf, OwnedWriteHalf},
        TcpListener,
    },
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

#[derive(Clone, Debug)]
pub(super) struct ConnectionClosed(pub(super) CancellationToken);

impl Connected<IncomingStream<'_, ObservedListener>> for ConnectionClosed {
    fn connect_info(stream: IncomingStream<'_, ObservedListener>) -> Self {
        Self(stream.io().closed.clone())
    }
}

pub(super) struct ObservedListener(pub(super) TcpListener);

impl Listener for ObservedListener {
    type Io = ObservedIo;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        // 复用 Axum 的接受错误处理，不另建 HTTP 服务或网络重试约定。
        let (socket, addr) = Listener::accept(&mut self.0).await;
        let (read, write) = socket.into_split();
        let read = Arc::new(read);
        let closed = CancellationToken::new();
        let observer = tokio::spawn(observe_read_closed(read.clone(), closed.clone()));
        (
            ObservedIo {
                read,
                write,
                closed,
                observer,
            },
            addr,
        )
    }

    fn local_addr(&self) -> io::Result<Self::Addr> {
        self.0.local_addr()
    }
}

pub(super) struct ObservedIo {
    read: Arc<OwnedReadHalf>,
    write: OwnedWriteHalf,
    closed: CancellationToken,
    observer: JoinHandle<()>,
}

impl Drop for ObservedIo {
    fn drop(&mut self) {
        self.closed.cancel();
        self.observer.abort();
    }
}

impl AsyncRead for ObservedIo {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if buf.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }
        let socket = self.read.as_ref().as_ref();
        loop {
            match socket.poll_read_ready(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Err(error)) => {
                    self.closed.cancel();
                    return Poll::Ready(Err(error));
                }
                Poll::Ready(Ok(())) => {}
            }
            match socket.try_read(buf.initialize_unfilled()) {
                Ok(size) => {
                    if size == 0 {
                        self.closed.cancel();
                    }
                    buf.advance(size);
                    return Poll::Ready(Ok(()));
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) => {
                    self.closed.cancel();
                    return Poll::Ready(Err(error));
                }
            }
        }
    }
}

impl AsyncWrite for ObservedIo {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().write).poll_write(cx, buf)
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().write).poll_write_vectored(cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        self.write.is_write_vectored()
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().write).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().write).poll_shutdown(cx)
    }
}

#[cfg(windows)]
async fn observe_read_closed(read: Arc<OwnedReadHalf>, closed: CancellationToken) {
    // Mio 1.2.4 保留独立的 AFD DISCONNECT 兴趣，但 Windows 的读关闭通知仍可
    // 延后到已缓冲数据被消费；Hyper 等待首响应时不会继续消费后续管线请求。
    // 改查该 socket 的 TCP 状态，不注册 WSAEventSelect、不改 Mio 的兴趣或非阻塞
    // 模式，也不执行 recv/peek。CLOSE_WAIT 表示内核已收到对端 FIN，即使请求仍未读。
    // SIO_TCP_INFO v0：https://learn.microsoft.com/windows/win32/winsock/sio-tcp-info
    let socket = read.as_ref().as_ref();
    let mut sample = tokio::time::interval(Duration::from_millis(100));
    sample.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut reported_error = false;
    let mut observe_readiness = true;
    loop {
        tokio::select! {
            biased;
            () = closed.cancelled() => return,
            _ = sample.tick() => observe_readiness = true,
            readiness = socket.ready(Interest::READABLE), if observe_readiness => {
                if readiness.is_ok_and(tokio::io::Ready::is_read_closed) {
                    closed.cancel();
                    return;
                }
                // 仍接受 Mio 的真实关闭位（包括 RST），但普通 readable 不是断连。
                // 未读字节保持 ready 时，每个采样周期最多处理一次，不能忙等。
                observe_readiness = false;
                continue;
            }
        }
        match windows_read_closed(socket) {
            Ok(true) => {
                closed.cancel();
                return;
            }
            Ok(false) => {}
            Err(error) => {
                // 查询失败不证明断连，不能把不支持的 IOCTL 等错误变成请求取消。
                // 每连接只记录一次；正常 Hyper I/O 的 EOF/错误仍由 ObservedIo 处理。
                if !reported_error {
                    tracing::warn!(%error, "模型服务 HTTP TCP 关闭状态查询失败");
                    reported_error = true;
                }
            }
        }
    }
}

#[cfg(windows)]
fn windows_read_closed(socket: &tokio::net::TcpStream) -> io::Result<bool> {
    use std::{mem, os::windows::io::AsRawSocket, ptr};
    use windows_sys::Win32::Networking::WinSock::{
        TCP_INFO_v0, WSAGetLastError, WSAIoctl, SIO_TCP_INFO, SOCKET_ERROR, TCPSTATE_CLOSED,
        TCPSTATE_CLOSE_WAIT, TCPSTATE_CLOSING, TCPSTATE_LAST_ACK, TCPSTATE_TIME_WAIT,
    };

    // Windows SDK 的 DWORD 为 4 字节，TCP_INFO_v0 的 repr(C) 布局为 88 字节；
    // 在编译期核对当前绑定，既不截断长度，也不在每次采样时计算或校验大小。
    const VERSION_SIZE: u32 = {
        assert!(mem::size_of::<u32>() == 4);
        4
    };
    const TCP_INFO_V0_SIZE: u32 = {
        assert!(mem::size_of::<TCP_INFO_v0>() == 88);
        88
    };

    // RawSocket 是 u64，而 Winsock SOCKET 是 usize；32 位目标也必须无损转换。
    let raw_socket =
        usize::try_from(socket.as_raw_socket()).map_err(|_| io::ErrorKind::InvalidInput)?;
    let version = 0_u32;
    let mut info = mem::MaybeUninit::<TCP_INFO_v0>::uninit();
    let mut returned = 0_u32;
    // SAFETY: 调用者的 Arc<OwnedReadHalf> 在同步查询期间拥有 socket。输入是
    // 已初始化的 DWORD，输出是正确对齐的 TCP_INFO_v0 栈存储，长度已在编译期核对；
    // returned 指向可写的 DWORD。空 overlapped 和完成回调确保缓冲区不会被异步保留。
    // 该 IOCTL 仅查询本地 TCP 状态，不读取请求字节。
    let result = unsafe {
        WSAIoctl(
            raw_socket,
            SIO_TCP_INFO,
            (&raw const version).cast(),
            VERSION_SIZE,
            (&raw mut info).cast(),
            TCP_INFO_V0_SIZE,
            &raw mut returned,
            ptr::null_mut(),
            None,
        )
    };
    if result == SOCKET_ERROR {
        // SAFETY: 紧随本线程失败的 Winsock 调用取错，不访问外部内存。
        let error = unsafe { WSAGetLastError() };
        return Err(io::Error::from_raw_os_error(error));
    }
    if returned < TCP_INFO_V0_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Windows TCP 状态查询返回不完整",
        ));
    }
    // SAFETY: 同步调用成功且 returned 表明已写满 TCP_INFO_v0；当前 SDK 绑定的
    // 所有成员均为整数（包括 TCPSTATE/BOOLEAN），不存在无效位模式。
    let state = unsafe { info.assume_init() }.State;
    Ok(matches!(
        state,
        TCPSTATE_CLOSED
            | TCPSTATE_CLOSE_WAIT
            | TCPSTATE_CLOSING
            | TCPSTATE_LAST_ACK
            | TCPSTATE_TIME_WAIT
    ))
}

#[cfg(not(windows))]
async fn observe_read_closed(read: Arc<OwnedReadHalf>, closed: CancellationToken) {
    let socket = read.as_ref().as_ref();
    let mut byte = [0_u8; 1];
    loop {
        match socket.ready(Interest::READABLE).await {
            // 其他平台复用原有 readiness/peek 路径；不能仅因 readable 就取消。
            Ok(ready) if ready.is_read_closed() => break,
            Ok(ready) if ready.is_readable() => {}
            Ok(_) => {
                tokio::time::sleep(Duration::from_millis(10)).await;
                continue;
            }
            Err(_) => break,
        }
        // peek 只验证 EOF/真实错误；WouldBlock 的假就绪由 Tokio 清除再等待。
        // 非空 peek 不代表连接仍存活：未消费的管线数据可以和 FIN 同时存在。
        match socket.peek(&mut byte).await {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                // peek 不消费 readiness；避免 stale readable 忙等，同时继续观察
                // 独立的 read_closed 位。Hyper 是唯一消费请求字节的所有者。
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }
    }
    closed.cancel();
}
