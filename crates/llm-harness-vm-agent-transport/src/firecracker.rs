//! Firecracker host-side Unix socket transport for guest-agent vsock.
//!
//! Firecracker exposes each guest vsock device through a host Unix socket.
//! A host connection first sends `connect <guest-port>\n` and waits for
//! `OK <host-port>\n`; the same stream then transparently carries vsock data.

use std::io;
use std::path::Path;
use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::net::UnixStream;

const MAX_HANDSHAKE_RESPONSE_BYTES: usize = 32;

/// An established Firecracker vsock connection carried over a Unix socket.
#[derive(Debug)]
pub struct FirecrackerVsockStream {
    stream: UnixStream,
    local_port: u32,
}

impl FirecrackerVsockStream {
    /// Connect to a guest vsock port through a Firecracker host UDS path.
    pub async fn connect(uds_path: impl AsRef<Path>, guest_port: u32) -> io::Result<Self> {
        if guest_port == 0 {
            return Err(invalid_input("guest vsock port must be nonzero"));
        }

        let mut stream = UnixStream::connect(uds_path).await?;
        let request = format!("connect {guest_port}\n");
        stream.write_all(request.as_bytes()).await?;
        stream.flush().await?;

        let response = read_handshake_response(&mut stream).await?;
        let local_port = parse_handshake_response(&response)?;
        Ok(Self { stream, local_port })
    }

    /// Return the host-side vsock port reported by Firecracker.
    pub fn local_port(&self) -> u32 {
        self.local_port
    }
}

impl AsyncRead for FirecrackerVsockStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().stream).poll_read(cx, buf)
    }
}

impl AsyncWrite for FirecrackerVsockStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().stream).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().stream).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().stream).poll_shutdown(cx)
    }
}

async fn read_handshake_response(stream: &mut UnixStream) -> io::Result<String> {
    let mut response = Vec::with_capacity(16);
    loop {
        let mut byte = [0_u8; 1];
        let read_len = stream.read(&mut byte).await?;
        if read_len == 0 {
            return Err(invalid_input(
                "Firecracker vsock socket closed before handshake completed",
            ));
        }
        if byte[0] == b'\n' {
            break;
        }
        if response.len() >= MAX_HANDSHAKE_RESPONSE_BYTES {
            return Err(invalid_input(
                "Firecracker vsock handshake response is too long",
            ));
        }
        response.push(byte[0]);
    }

    String::from_utf8(response)
        .map_err(|_| invalid_input("Firecracker vsock handshake response is not UTF-8"))
}

fn parse_handshake_response(response: &str) -> io::Result<u32> {
    let Some((prefix, local_port)) = response.split_once(' ') else {
        return Err(invalid_input(
            "Firecracker vsock handshake response is malformed",
        ));
    };
    if prefix != "OK" {
        return Err(invalid_input(
            "Firecracker vsock handshake response is malformed",
        ));
    }
    let local_port = local_port
        .parse::<u32>()
        .map_err(|_| invalid_input("Firecracker vsock handshake response has an invalid port"))?;
    if local_port == 0 {
        return Err(invalid_input(
            "Firecracker vsock handshake response has an invalid port",
        ));
    }
    Ok(local_port)
}

fn invalid_input(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FrameStream, ProtocolPeer};
    use llm_harness_vm_agent_protocol::{AgentMessage, Frame};
    use tokio::net::UnixListener;

    #[tokio::test]
    async fn connect_rejects_guest_port_zero() {
        let root = tempfile::tempdir().expect("temp directory");
        let error = FirecrackerVsockStream::connect(root.path().join("missing.sock"), 0)
            .await
            .expect_err("port zero");
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }

    #[tokio::test]
    async fn stream_completes_handshake_and_carries_guest_agent_frames() {
        let root = tempfile::tempdir().expect("temp directory");
        let socket_path = root.path().join("firecracker.sock");
        let listener = UnixListener::bind(&socket_path).expect("bind Firecracker socket");

        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept host connection");
            let mut command = [0_u8; b"connect 6000\n".len()];
            socket
                .read_exact(&mut command)
                .await
                .expect("read connect command");
            assert_eq!(command.as_slice(), b"connect 6000\n");
            socket
                .write_all(b"OK 1073741824\n")
                .await
                .expect("write handshake response");
            socket.flush().await.expect("flush handshake response");

            let mut guest = FrameStream::with_default_limit(socket, ProtocolPeer::Guest)
                .expect("guest frame stream");
            let request = guest.recv().await.expect("receive host request");
            let request_id = request.request_id();
            assert_eq!(request.into_message(), AgentMessage::Ping);
            guest
                .send(Frame::new(ProtocolPeer::Guest, request_id, AgentMessage::Pong).unwrap())
                .await
                .expect("send pong");
        });

        let stream = FirecrackerVsockStream::connect(&socket_path, 6000)
            .await
            .expect("connect through Firecracker UDS");
        assert_eq!(stream.local_port(), 1_073_741_824);
        let mut host =
            FrameStream::with_default_limit(stream, ProtocolPeer::Host).expect("host stream");
        host.send(Frame::new(ProtocolPeer::Host, 1, AgentMessage::Ping).unwrap())
            .await
            .expect("send ping");
        let response = host.recv().await.expect("receive pong");
        assert_eq!(response.into_message(), AgentMessage::Pong);

        server.await.expect("server task");
    }

    #[tokio::test]
    async fn stream_rejects_a_malformed_handshake_response() {
        let root = tempfile::tempdir().expect("temp directory");
        let socket_path = root.path().join("firecracker.sock");
        let listener = UnixListener::bind(&socket_path).expect("bind Firecracker socket");

        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept host connection");
            let mut command = [0_u8; b"connect 6000\n".len()];
            socket
                .read_exact(&mut command)
                .await
                .expect("read connect command");
            socket
                .write_all(b"ERROR 1073741824\n")
                .await
                .expect("write handshake response");
        });

        let error = FirecrackerVsockStream::connect(&socket_path, 6000)
            .await
            .expect_err("malformed handshake");
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        server.await.expect("server task");
    }
}
