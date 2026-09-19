//! Linux AF_VSOCK adapter for the VM guest-agent protocol.

use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::pin::Pin;
use std::task::{Context, Poll, ready};

use socket2::{Domain, SockAddr, Socket, Type};
use tokio::io::unix::AsyncFd;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

const VMADDR_CID_ANY: u32 = u32::MAX;
const LISTEN_BACKLOG: libc::c_int = 128;

/// A validated AF_VSOCK CID and port.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VsockAddress {
    cid: u32,
    port: u32,
}

impl VsockAddress {
    /// Create an address.
    ///
    /// `VMADDR_CID_ANY` is valid for listeners, but [`VsockStream::connect`]
    /// rejects it because it does not identify a concrete guest.
    pub fn new(cid: u32, port: u32) -> io::Result<Self> {
        if port == 0 {
            return Err(invalid_input("vsock port must be nonzero"));
        }
        Ok(Self { cid, port })
    }

    /// Return the AF_VSOCK context identifier.
    pub fn cid(&self) -> u32 {
        self.cid
    }

    /// Return the AF_VSOCK port.
    pub fn port(&self) -> u32 {
        self.port
    }

    fn as_sock_addr(&self) -> SockAddr {
        SockAddr::vsock(self.cid, self.port)
    }

    fn from_sock_addr(sock_addr: &SockAddr) -> io::Result<Self> {
        let (cid, port) = sock_addr
            .as_vsock_address()
            .ok_or_else(|| invalid_input("socket address is not an AF_VSOCK address"))?;
        Self::new(cid, port)
    }
}

/// An async AF_VSOCK stream that implements the generic byte-stream contract.
#[derive(Debug)]
pub struct VsockStream {
    socket: AsyncSocketStream,
    peer: VsockAddress,
}

impl VsockStream {
    /// Connect to a concrete guest CID and port.
    pub async fn connect(address: VsockAddress) -> io::Result<Self> {
        if address.cid == VMADDR_CID_ANY {
            return Err(invalid_input(
                "vsock connect address must use a concrete CID",
            ));
        }

        let socket = Socket::new(Domain::VSOCK, Type::STREAM, None)?;
        socket.set_nonblocking(true)?;
        match socket.connect(&address.as_sock_addr()) {
            Ok(()) => Self::from_connected_socket(socket, address),
            Err(error) if error.raw_os_error() == Some(libc::EINPROGRESS) => {
                let socket = AsyncSocketStream::new(socket)?;
                socket.wait_for_connect().await?;
                Ok(Self {
                    socket,
                    peer: address,
                })
            }
            Err(error) => Err(error),
        }
    }

    fn from_connected_socket(socket: Socket, peer: VsockAddress) -> io::Result<Self> {
        Ok(Self {
            socket: AsyncSocketStream::new(socket)?,
            peer,
        })
    }

    /// Return the concrete remote AF_VSOCK address.
    pub fn peer_address(&self) -> VsockAddress {
        self.peer
    }

    /// Return the local AF_VSOCK address.
    pub fn local_address(&self) -> io::Result<VsockAddress> {
        VsockAddress::from_sock_addr(&self.socket.get_ref().local_addr()?)
    }
}

impl AsyncRead for VsockStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().socket).poll_read(cx, buf)
    }
}

impl AsyncWrite for VsockStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().socket).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().socket).poll_shutdown(_cx)
    }
}

/// An async AF_VSOCK listener.
#[derive(Debug)]
pub struct VsockListener {
    listener: AsyncSocketListener,
    local: VsockAddress,
}

impl VsockListener {
    /// Bind and listen on an AF_VSOCK address.
    ///
    /// `VMADDR_CID_ANY` is accepted for guest-side listeners.
    pub fn bind(address: VsockAddress) -> io::Result<Self> {
        let socket = Socket::new(Domain::VSOCK, Type::STREAM, None)?;
        socket.set_nonblocking(true)?;
        socket.bind(&address.as_sock_addr())?;
        socket.listen(LISTEN_BACKLOG)?;
        Ok(Self {
            listener: AsyncSocketListener::new(socket)?,
            local: address,
        })
    }

    /// Accept the next AF_VSOCK connection.
    pub async fn accept(&self) -> io::Result<VsockStream> {
        let (socket, peer_addr) = self.listener.accept().await?;
        let peer = VsockAddress::from_sock_addr(&peer_addr)?;
        VsockStream::from_connected_socket(socket, peer)
    }

    /// Return the configured local AF_VSOCK address.
    pub fn local_address(&self) -> VsockAddress {
        self.local
    }
}

#[derive(Debug)]
struct AsyncSocketStream {
    socket: AsyncFd<Socket>,
}

impl AsyncSocketStream {
    fn new(socket: Socket) -> io::Result<Self> {
        socket.set_nonblocking(true)?;
        Ok(Self {
            socket: AsyncFd::new(socket)?,
        })
    }

    fn get_ref(&self) -> &Socket {
        self.socket.get_ref()
    }

    async fn wait_for_connect(&self) -> io::Result<()> {
        let guard = self.socket.writable().await?;
        if let Some(error) = guard.get_inner().take_error()? {
            return Err(error);
        }
        Ok(())
    }
}

impl AsyncRead for AsyncSocketStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if buf.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }

        let stream = self.get_mut();
        loop {
            let mut guard = ready!(stream.socket.poll_read_ready(cx))?;
            match guard.try_io(|socket| {
                retry_interrupted(|| socket.get_ref().read(buf.initialize_unfilled()))
            }) {
                Ok(result) => {
                    let read_len = result?;
                    buf.advance(read_len);
                    return Poll::Ready(Ok(()));
                }
                Err(_would_block) => continue,
            }
        }
    }
}

impl AsyncWrite for AsyncSocketStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }

        let stream = self.get_mut();
        loop {
            let mut guard = ready!(stream.socket.poll_write_ready(cx))?;
            match guard.try_io(|socket| retry_interrupted(|| socket.get_ref().write(buf))) {
                Ok(result) => return Poll::Ready(result),
                Err(_would_block) => continue,
            }
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let socket = self.get_ref();
        Poll::Ready(retry_interrupted(|| socket.shutdown(Shutdown::Write)))
    }
}

#[derive(Debug)]
struct AsyncSocketListener {
    listener: AsyncFd<Socket>,
}

impl AsyncSocketListener {
    fn new(listener: Socket) -> io::Result<Self> {
        listener.set_nonblocking(true)?;
        Ok(Self {
            listener: AsyncFd::new(listener)?,
        })
    }

    async fn accept(&self) -> io::Result<(Socket, SockAddr)> {
        loop {
            let mut guard = self.listener.readable().await?;
            match guard.try_io(|listener| retry_interrupted(|| listener.get_ref().accept())) {
                Ok(result) => return result,
                Err(_would_block) => continue,
            }
        }
    }
}

fn retry_interrupted<T>(mut operation: impl FnMut() -> io::Result<T>) -> io::Result<T> {
    loop {
        match operation() {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            result => return result,
        }
    }
}

fn invalid_input(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FrameStream, ProtocolPeer};
    use llm_harness_vm_agent_protocol::{AgentMessage, Frame};
    use socket2::SockAddr;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn host_frame(request_id: u64, message: AgentMessage) -> Frame {
        Frame::new(ProtocolPeer::Host, request_id, message).expect("valid host frame")
    }

    fn guest_frame(request_id: u64, message: AgentMessage) -> Frame {
        Frame::new(ProtocolPeer::Guest, request_id, message).expect("valid guest frame")
    }

    fn unix_pair() -> io::Result<(VsockStream, VsockStream)> {
        let (host_socket, guest_socket) =
            Socket::pair(Domain::UNIX, Type::STREAM, None).expect("unix socket pair");
        let address = VsockAddress::new(3, 6000).expect("valid test address");
        let host =
            VsockStream::from_connected_socket(host_socket, address).expect("host async socket");
        let guest =
            VsockStream::from_connected_socket(guest_socket, address).expect("guest async socket");
        Ok((host, guest))
    }

    #[test]
    fn vsock_address_validates_port_and_maps_wire_address() {
        let error = VsockAddress::new(3, 0).expect_err("port zero");
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);

        let address = VsockAddress::new(3, 6000).expect("valid address");
        assert_eq!(address.cid(), 3);
        assert_eq!(address.port(), 6000);
        assert_eq!(address.as_sock_addr().as_vsock_address(), Some((3, 6000)));
    }

    #[tokio::test]
    async fn vsock_connect_rejects_cid_any() {
        let address = VsockAddress::new(VMADDR_CID_ANY, 6000).expect("listener address");
        let error = VsockStream::connect(address).await.expect_err("any cid");
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }

    #[tokio::test]
    async fn vsock_stream_supports_guest_agent_round_trip() {
        let (host, guest) = unix_pair().expect("socket pair");
        let mut host_stream =
            FrameStream::with_default_limit(host, ProtocolPeer::Host).expect("host frame stream");
        let mut guest_stream = FrameStream::with_default_limit(guest, ProtocolPeer::Guest)
            .expect("guest frame stream");

        let request = host_frame(1, AgentMessage::Ping);
        host_stream.send(request.clone()).await.expect("send host");
        assert_eq!(guest_stream.recv().await.expect("recv host"), request);

        let response = guest_frame(1, AgentMessage::Pong);
        guest_stream
            .send(response.clone())
            .await
            .expect("send guest");
        assert_eq!(host_stream.recv().await.expect("recv guest"), response);
    }

    #[tokio::test]
    async fn vsock_stream_shutdown_reports_clean_close() {
        let (host, guest) = unix_pair().expect("socket pair");
        let mut host_stream =
            FrameStream::with_default_limit(host, ProtocolPeer::Host).expect("host frame stream");
        let mut guest_stream = FrameStream::with_default_limit(guest, ProtocolPeer::Guest)
            .expect("guest frame stream");

        host_stream.close().await.expect("shutdown host");
        assert!(matches!(
            guest_stream.recv().await,
            Err(crate::TransportError::Closed)
        ));
    }

    #[tokio::test]
    async fn async_socket_listener_accepts_and_streams() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("guest-agent.sock");
        let unix_addr = SockAddr::unix(&path).expect("unix socket address");
        let listener_socket =
            Socket::new(Domain::UNIX, Type::STREAM, None).expect("unix listener socket");
        listener_socket
            .bind(&unix_addr)
            .expect("bind unix listener");
        listener_socket.listen(1).expect("listen unix");
        let listener = AsyncSocketListener::new(listener_socket).expect("async listener");

        let client_socket =
            Socket::new(Domain::UNIX, Type::STREAM, None).expect("unix client socket");
        client_socket
            .set_nonblocking(true)
            .expect("nonblocking client");
        match client_socket.connect(&unix_addr) {
            Ok(()) => {}
            Err(error) if error.raw_os_error() == Some(libc::EINPROGRESS) => {}
            Err(error) => panic!("unexpected unix connect error: {error}"),
        }
        let client = AsyncSocketStream::new(client_socket).expect("async client");
        client.wait_for_connect().await.expect("unix connect");

        let (server_socket, _peer) = listener.accept().await.expect("accept unix");
        let server = AsyncSocketStream::new(server_socket).expect("async server");
        let (mut client, mut server) = (client, server);

        client.write_all(b"request").await.expect("write request");
        let mut bytes = [0u8; 7];
        server.read_exact(&mut bytes).await.expect("read request");
        assert_eq!(&bytes, b"request");

        server.write_all(b"response").await.expect("write response");
        let mut bytes = [0u8; 8];
        client.read_exact(&mut bytes).await.expect("read response");
        assert_eq!(&bytes, b"response");
    }
}
