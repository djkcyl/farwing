use std::fmt::Debug;
use std::io::{self, Write};
use std::net::{IpAddr, Shutdown, SocketAddr};
use std::pin::Pin;
use std::sync::{Arc, Mutex as StdMutex};
use std::task::{Context, Poll, Wake, Waker};

use async_trait::async_trait;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{verify_tls12_signature, verify_tls13_signature, CryptoProvider};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, ReadBuf, ReadHalf, WriteHalf};
use tokio::net::{TcpSocket, TcpStream};
use tokio::sync::{Mutex, Notify};
use tokio::task::JoinHandle;
use tokio_rustls::TlsConnector;
use tokio_util::sync::CancellationToken;
use util::Conn;

trait AsyncTurnStream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> AsyncTurnStream for T {}
type BoxedTurnStream = Box<dyn AsyncTurnStream>;

/// Attempt the actual nonblocking syscall before waiting on Tokio readiness.
/// A not-yet-delivered readiness event/cooperative budget is not evidence that
/// the socket send buffer is full.
struct NonblockingTcp {
    stream: TcpStream,
    direct: Arc<std::net::TcpStream>,
}

impl AsyncRead for NonblockingTcp {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        output: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_read(cx, output)
    }
}

impl AsyncWrite for NonblockingTcp {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        input: &[u8],
    ) -> Poll<io::Result<usize>> {
        match (&*self.direct).write(input) {
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                Pin::new(&mut self.stream).poll_write(cx, input)
            }
            result => Poll::Ready(result),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_shutdown(cx)
    }
}

// AsyncStunTCPSocket constructor, used for TURN/TCP and TURN/TLS.
const MAX_FRAME_SIZE: usize = 65_556;

struct WriterWake(Notify);

impl Wake for WriterWake {
    fn wake(self: Arc<Self>) {
        self.0.notify_one();
    }
    fn wake_by_ref(self: &Arc<Self>) {
        self.0.notify_one();
    }
}

struct StreamWriter {
    stream: Option<WriteHalf<BoxedTurnStream>>,
    packet: Vec<u8>,
    offset: usize,
    flushing: bool,
}

impl StreamWriter {
    fn busy(&self) -> bool {
        !self.packet.is_empty() || self.flushing
    }

    /// Send until complete or would-block, preserving the remainder.
    /// A pending poll never causes this method to wait while holding a caller.
    fn drain(&mut self, cx: &mut Context<'_>) -> io::Result<bool> {
        let stream = self.stream.as_mut().ok_or_else(stream_closed)?;
        while self.offset < self.packet.len() {
            match Pin::new(&mut *stream).poll_write(cx, &self.packet[self.offset..]) {
                Poll::Ready(Ok(0)) => return Err(io::ErrorKind::WriteZero.into()),
                Poll::Ready(Ok(n)) => self.offset += n,
                Poll::Ready(Err(error)) => return Err(error),
                Poll::Pending => return Ok(false),
            }
        }
        if !self.packet.is_empty() || self.flushing {
            self.flushing = true;
            match Pin::new(stream).poll_flush(cx) {
                Poll::Ready(Ok(())) => {
                    self.packet.clear();
                    self.offset = 0;
                    self.flushing = false;
                }
                Poll::Ready(Err(error)) => return Err(error),
                Poll::Pending => return Ok(false),
            }
        }
        Ok(true)
    }
}

fn stream_closed() -> io::Error {
    io::Error::new(io::ErrorKind::BrokenPipe, "TURN stream closed")
}

// STUN's length excludes 20 bytes; ChannelData's excludes 4.
// Reserved non-STUN prefixes are still framed here, then discarded by TurnPort.
fn frame_lengths(header: &[u8]) -> (usize, usize) {
    let length = usize::from(u16::from_be_bytes([header[2], header[3]]));
    if u16::from_be_bytes([header[0], header[1]]) <= 0x3fff {
        (20 + length, 20 + length)
    } else {
        let frame_len = 4 + length;
        (frame_len, (frame_len + 3) & !3)
    }
}

#[derive(Debug)]
struct RelayCertificateVerifier {
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for RelayCertificateVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(
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
        verify_tls13_signature(
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

pub(crate) struct TurnStreamConn {
    reader: Mutex<Option<ReadHalf<BoxedTurnStream>>>,
    writer: Arc<StdMutex<StreamWriter>>,
    writer_wake: Arc<WriterWake>,
    writer_task: StdMutex<Option<JoinHandle<()>>>,
    shutdown_socket: StdMutex<Option<Arc<std::net::TcpStream>>>,
    close_serial: Mutex<()>,
    stopped: CancellationToken,
    write_error: Arc<StdMutex<Option<String>>>,
    local_addr: SocketAddr,
    remote_addr: SocketAddr,
}

impl Drop for TurnStreamConn {
    fn drop(&mut self) {
        self.stopped.cancel();
        if let Some(socket) = self
            .shutdown_socket
            .get_mut()
            .expect("TURN socket mutex poisoned")
            .take()
        {
            let _ = socket.shutdown(Shutdown::Both);
        }
        if let Some(task) = self
            .writer_task
            .get_mut()
            .expect("TURN writer task mutex poisoned")
            .take()
        {
            task.abort();
        }
    }
}

impl TurnStreamConn {
    pub(crate) async fn connect(
        remote_addr: SocketAddr,
        local_ip: Option<IpAddr>,
        use_tls: bool,
        insecure_skip_verify: bool,
    ) -> Result<Arc<dyn Conn + Send + Sync>, util::Error> {
        let socket = if remote_addr.is_ipv4() {
            TcpSocket::new_v4()?
        } else {
            TcpSocket::new_v6()?
        };
        crate::socket_options::configure_media_buffers(&socket2::SockRef::from(&socket));
        if let Some(local_ip) = local_ip {
            socket.bind(SocketAddr::new(local_ip, 0))?;
        }
        let tcp = socket.connect(remote_addr).await?;
        tcp.set_nodelay(true)?;
        let local_addr = tcp.local_addr()?;
        // A shutdown handle wakes both halves even when TLS/socket writes are
        // backpressured. It is released along with both halves on explicit close.
        let tcp = tcp.into_std()?;
        let shutdown_socket = Arc::new(tcp.try_clone()?);
        let tcp = NonblockingTcp {
            stream: TcpStream::from_std(tcp)?,
            direct: shutdown_socket.clone(),
        };
        let stream: BoxedTurnStream = if use_tls {
            let provider = Arc::new(rustls::crypto::ring::default_provider());
            let mut config = rustls::ClientConfig::builder_with_provider(Arc::clone(&provider))
                .with_safe_default_protocol_versions()
                .map_err(io::Error::other)?
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(RelayCertificateVerifier { provider }))
                .with_no_client_auth();
            if !insecure_skip_verify {
                log::warn!(
                    "TURNS endpoint is an IP address with a relay-domain certificate; skipping the certificate name check"
                );
            }
            config.alpn_protocols.clear();
            let server_name = ServerName::IpAddress(match remote_addr.ip() {
                IpAddr::V4(ip) => ip.into(),
                IpAddr::V6(ip) => ip.into(),
            });
            let tls = TlsConnector::from(Arc::new(config))
                .connect(server_name, tcp)
                .await
                .map_err(io::Error::other)?;
            Box::new(tls)
        } else {
            Box::new(tcp)
        };
        let (reader, writer) = tokio::io::split(stream);
        let writer = Arc::new(StdMutex::new(StreamWriter {
            stream: Some(writer),
            packet: Vec::new(),
            offset: 0,
            flushing: false,
        }));
        let writer_wake = Arc::new(WriterWake(Notify::new()));
        let stopped = CancellationToken::new();
        let write_error = Arc::new(StdMutex::new(None));
        let task_writer = writer.clone();
        let task_wake = writer_wake.clone();
        let task_stop = stopped.clone();
        let task_error = write_error.clone();
        let task_socket = shutdown_socket.clone();
        let writer_task = tokio::spawn(async move {
            let waker = Waker::from(task_wake.clone());
            loop {
                tokio::select! {
                    biased;
                    _ = task_stop.cancelled() => break,
                    _ = task_wake.0.notified() => {},
                }
                let result = {
                    let mut state = task_writer.lock().expect("TURN writer mutex poisoned");
                    if !state.busy() {
                        continue;
                    }
                    state.drain(&mut Context::from_waker(&waker))
                };
                if let Err(error) = result {
                    *task_error.lock().expect("TURN write error mutex poisoned") =
                        Some(error.to_string());
                    task_stop.cancel();
                    let _ = task_socket.shutdown(Shutdown::Both);
                    break;
                }
            }
        });
        Ok(Arc::new(Self {
            reader: Mutex::new(Some(reader)),
            writer,
            writer_wake,
            writer_task: StdMutex::new(Some(writer_task)),
            shutdown_socket: StdMutex::new(Some(shutdown_socket)),
            close_serial: Mutex::new(()),
            stopped,
            write_error,
            local_addr,
            remote_addr,
        }))
    }

    async fn read_frame(&self, output: &mut [u8]) -> Result<usize, util::Error> {
        tokio::select! {
            biased;
            _ = self.stopped.cancelled() => {
                let message = self.write_error.lock().expect("TURN write error mutex poisoned")
                    .clone().unwrap_or_else(|| "TURN stream closed".to_owned());
                Err(io::Error::new(io::ErrorKind::ConnectionAborted, message).into())
            }
            result = async {
                let mut slot = self.reader.lock().await;
                let reader = slot.as_mut().ok_or_else(stream_closed)?;
                let mut header = [0u8; 4];
                reader.read_exact(&mut header).await?;
                let (frame_len, wire_len) = frame_lengths(&header);
                if frame_len > output.len() || wire_len > MAX_FRAME_SIZE {
                    return Err(io::Error::new(io::ErrorKind::InvalidData, "TURN stream frame exceeds receive buffer").into());
                }
                output[..4].copy_from_slice(&header);
                reader.read_exact(&mut output[4..frame_len]).await?;
                let mut padding = [0; 3];
                reader.read_exact(&mut padding[..wire_len - frame_len]).await?;
                Ok(frame_len)
            } => result,
        }
    }

    async fn write_frame(&self, frame: &[u8]) -> Result<usize, util::Error> {
        if self.stopped.is_cancelled() {
            return Err(stream_closed().into());
        }
        if !(4..=MAX_FRAME_SIZE).contains(&frame.len()) {
            return Err(
                io::Error::new(io::ErrorKind::InvalidInput, "invalid TURN frame size").into(),
            );
        }
        let mut state = self.writer.lock().expect("TURN writer mutex poisoned");
        if self.stopped.is_cancelled() {
            return Err(stream_closed().into());
        }
        // Intentionally drops a new datagram while the previous one is
        // partially written. No packet queue, no blocked ICE/control caller.
        if state.busy() {
            return Ok(frame.len());
        }
        let (frame_len, wire_len) = frame_lengths(frame);
        // Our ChannelData encoder includes optional padding, whereas the
        // receiver expects the unpadded message. Normalize that API boundary;
        // accept only the complete message or its exact wire-aligned form.
        if (frame.len() != frame_len && frame.len() != wire_len) || wire_len > MAX_FRAME_SIZE {
            return Err(
                io::Error::new(io::ErrorKind::InvalidInput, "TURN frame length mismatch").into(),
            );
        }
        state.packet.extend_from_slice(&frame[..frame_len]);
        state.packet.resize(wire_len, 0);
        let waker = Waker::from(self.writer_wake.clone());
        match state.drain(&mut Context::from_waker(&waker)) {
            Ok(true) => Ok(frame.len()),
            Ok(false) if state.offset == 0 => {
                // Empty socket send (would-block): abandon this packet.
                state.packet.clear();
                Ok(0)
            }
            Ok(false) => {
                log::debug!("TURN stream retained partial write: accepted={} wire_len={} tls_flush_pending={}", state.offset, wire_len, state.flushing);
                Ok(frame.len())
            }
            Err(error) => {
                *self
                    .write_error
                    .lock()
                    .expect("TURN write error mutex poisoned") = Some(error.to_string());
                self.stopped.cancel();
                if let Some(socket) = self
                    .shutdown_socket
                    .lock()
                    .expect("TURN socket mutex poisoned")
                    .as_ref()
                {
                    let _ = socket.shutdown(Shutdown::Both);
                }
                Err(error.into())
            }
        }
    }
}

#[async_trait]
impl Conn for TurnStreamConn {
    async fn connect(&self, _addr: SocketAddr) -> Result<(), util::Error> {
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "TURN stream already connected",
        )
        .into())
    }

    async fn recv(&self, buf: &mut [u8]) -> Result<usize, util::Error> {
        self.read_frame(buf).await
    }

    async fn recv_from(&self, buf: &mut [u8]) -> Result<(usize, SocketAddr), util::Error> {
        Ok((self.read_frame(buf).await?, self.remote_addr))
    }

    async fn send(&self, buf: &[u8]) -> Result<usize, util::Error> {
        self.write_frame(buf).await
    }

    async fn send_to(&self, buf: &[u8], target: SocketAddr) -> Result<usize, util::Error> {
        if target != self.remote_addr {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "TURN stream target differs from connected server",
            )
            .into());
        }
        self.write_frame(buf).await
    }

    fn local_addr(&self) -> Result<SocketAddr, util::Error> {
        Ok(self.local_addr)
    }

    fn remote_addr(&self) -> Option<SocketAddr> {
        Some(self.remote_addr)
    }

    async fn close(&self) -> Result<(), util::Error> {
        let _close = self.close_serial.lock().await;
        self.stopped.cancel();
        let socket = self
            .shutdown_socket
            .lock()
            .expect("TURN socket mutex poisoned")
            .take();
        if let Some(socket) = socket {
            let _ = socket.shutdown(Shutdown::Both);
        }
        let task = self
            .writer_task
            .lock()
            .expect("TURN writer task mutex poisoned")
            .take();
        if let Some(task) = task {
            if let Err(error) = task.await {
                log::error!("TURN writer failed during shutdown: {error}");
            }
            log::debug!("TURN partial-write worker joined");
        }
        self.reader.lock().await.take();
        let mut state = self.writer.lock().expect("TURN writer mutex poisoned");
        state.stream.take();
        state.packet.clear();
        state.offset = 0;
        state.flushing = false;
        Ok(())
    }

    fn as_any(&self) -> &(dyn std::any::Any + Send + Sync) {
        self
    }
}
