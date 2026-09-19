//! Async byte-stream transport for the VM guest-agent protocol.
//!
//! This crate is intentionally independent of vsock. The same bounded frame
//! stream can be tested over an in-memory duplex stream and later adapted to
//! AF_VSOCK, Unix sockets, or another byte-oriented transport.

#[cfg(target_os = "linux")]
pub mod vsock;

use std::collections::VecDeque;

use llm_harness_vm_agent_protocol::{
    DEFAULT_MAX_PAYLOAD_BYTES, Frame, FrameDecoder, ProtocolError, ProtocolPeer,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Errors produced while moving protocol frames over a byte stream.
#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    /// The underlying stream failed.
    #[error("guest agent transport io error: {0}")]
    Io(#[from] std::io::Error),
    /// A frame violated the guest-agent wire protocol.
    #[error("guest agent transport protocol error: {0}")]
    Protocol(#[from] ProtocolError),
    /// The remote peer closed the stream before another frame arrived.
    #[error("guest agent transport stream closed")]
    Closed,
    /// The remote peer closed the stream while a frame was incomplete.
    #[error("guest agent transport closed during a partial frame")]
    ClosedDuringPartialFrame,
}

/// A bounded, directional frame stream over any async duplex byte stream.
pub struct FrameStream<S> {
    stream: S,
    decoder: FrameDecoder,
    pending: VecDeque<Frame>,
    max_payload_bytes: usize,
    local_peer: ProtocolPeer,
}

impl<S> FrameStream<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    /// Create a stream for the local protocol peer.
    ///
    /// Outbound frames must originate from `peer`; inbound frames must
    /// originate from the opposite peer.
    pub fn new(
        stream: S,
        local_peer: ProtocolPeer,
        max_payload_bytes: usize,
    ) -> Result<Self, TransportError> {
        let remote_peer = match local_peer {
            ProtocolPeer::Host => ProtocolPeer::Guest,
            ProtocolPeer::Guest => ProtocolPeer::Host,
        };
        let decoder = FrameDecoder::new(remote_peer, max_payload_bytes)?;
        Ok(Self {
            stream,
            decoder,
            pending: VecDeque::new(),
            max_payload_bytes,
            local_peer,
        })
    }

    /// Create a stream using the protocol's default payload limit.
    pub fn with_default_limit(stream: S, peer: ProtocolPeer) -> Result<Self, TransportError> {
        Self::new(stream, peer, DEFAULT_MAX_PAYLOAD_BYTES)
    }

    /// Send one validated frame and flush the stream.
    pub async fn send(&mut self, frame: Frame) -> Result<(), TransportError> {
        if frame.peer() != self.local_peer {
            return Err(TransportError::Protocol(ProtocolError::InvalidMessage(
                "frame originates from the wrong protocol peer",
            )));
        }
        let bytes = frame.encode(self.max_payload_bytes)?;
        self.stream.write_all(&bytes).await?;
        self.stream.flush().await?;
        Ok(())
    }

    /// Receive the next inbound frame, buffering partial and coalesced frames.
    pub async fn recv(&mut self) -> Result<Frame, TransportError> {
        if let Some(frame) = self.pending.pop_front() {
            return Ok(frame);
        }

        let mut chunk = [0u8; 8192];
        loop {
            let read_len = self.stream.read(&mut chunk).await?;
            if read_len == 0 {
                if self.decoder.has_partial_frame() {
                    return Err(TransportError::ClosedDuringPartialFrame);
                }
                return Err(TransportError::Closed);
            }

            let mut decoded = Vec::new();
            self.decoder.decode(&chunk[..read_len], &mut decoded)?;
            if decoded.is_empty() {
                continue;
            }
            self.pending.extend(decoded);
            return Ok(self
                .pending
                .pop_front()
                .expect("decoded frames are inserted before being returned"));
        }
    }

    /// Shut down the write half of the underlying stream.
    pub async fn close(&mut self) -> Result<(), TransportError> {
        self.stream.shutdown().await?;
        Ok(())
    }

    /// Return the local peer whose outbound frames this stream accepts.
    pub fn peer(&self) -> ProtocolPeer {
        self.local_peer
    }

    /// Return the configured maximum frame payload size.
    pub fn max_payload_bytes(&self) -> usize {
        self.max_payload_bytes
    }

    /// Borrow the underlying stream.
    pub fn get_ref(&self) -> &S {
        &self.stream
    }

    /// Mutably borrow the underlying stream.
    pub fn get_mut(&mut self) -> &mut S {
        &mut self.stream
    }

    /// Consume the frame stream and return the underlying stream.
    ///
    /// Buffered frames are intentionally discarded. Use `recv` to drain the
    /// stream before releasing the underlying transport.
    pub fn into_inner(self) -> S {
        self.stream
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use llm_harness_vm_agent_protocol::{AgentMessage, FRAME_HEADER_LEN};
    use tokio::io::duplex;

    fn host_frame(request_id: u64, message: AgentMessage) -> Frame {
        Frame::new(ProtocolPeer::Host, request_id, message).expect("valid host frame")
    }

    fn guest_frame(request_id: u64, message: AgentMessage) -> Frame {
        Frame::new(ProtocolPeer::Guest, request_id, message).expect("valid guest frame")
    }

    #[tokio::test]
    async fn frame_stream_round_trips_host_and_guest_directions() {
        let (host_io, guest_io) = duplex(64);
        let mut host_stream =
            FrameStream::with_default_limit(host_io, ProtocolPeer::Host).expect("host stream");
        let mut guest_stream =
            FrameStream::with_default_limit(guest_io, ProtocolPeer::Guest).expect("guest stream");

        let request = host_frame(1, AgentMessage::Ping);
        host_stream.send(request.clone()).await.expect("send host");
        let received = guest_stream.recv().await.expect("recv host");
        assert_eq!(received, request);

        let response = guest_frame(1, AgentMessage::Pong);
        guest_stream
            .send(response.clone())
            .await
            .expect("send guest");
        let received = host_stream.recv().await.expect("recv guest");
        assert_eq!(received, response);
    }

    #[tokio::test]
    async fn frame_stream_buffers_partial_and_coalesced_frames() {
        let (host_io, guest_io) = duplex(128);
        let mut host_stream =
            FrameStream::with_default_limit(host_io, ProtocolPeer::Host).expect("host stream");
        let mut guest_stream =
            FrameStream::with_default_limit(guest_io, ProtocolPeer::Guest).expect("guest stream");

        let first = host_frame(1, AgentMessage::Ping);
        let second = host_frame(2, AgentMessage::Status);
        let mut wire = first.encode(1024).expect("first bytes");
        wire.extend(second.encode(1024).expect("second bytes"));
        host_stream
            .get_mut()
            .write_all(&wire)
            .await
            .expect("write wire");

        assert_eq!(guest_stream.recv().await.expect("first frame"), first);
        assert_eq!(guest_stream.recv().await.expect("second frame"), second);
    }

    #[tokio::test]
    async fn frame_stream_rejects_wrong_peer_before_returning_frame() {
        let (host_io, guest_io) = duplex(64);
        let mut host_stream =
            FrameStream::with_default_limit(host_io, ProtocolPeer::Host).expect("host stream");
        let mut guest_stream =
            FrameStream::with_default_limit(guest_io, ProtocolPeer::Guest).expect("guest stream");

        let request = host_frame(1, AgentMessage::Ping);
        let error = guest_stream
            .send(request.clone())
            .await
            .expect_err("wrong peer");
        assert!(matches!(
            error,
            TransportError::Protocol(ProtocolError::InvalidMessage(_))
        ));

        let bytes = request.encode(64).expect("host bytes");
        guest_stream
            .get_mut()
            .write_all(&bytes)
            .await
            .expect("write guest");
        let error = host_stream.recv().await.expect_err("wrong peer");
        assert!(matches!(
            error,
            TransportError::Protocol(ProtocolError::InvalidMessage(_))
        ));
    }

    #[tokio::test]
    async fn frame_stream_reports_clean_close_after_draining_pending_frames() {
        let (host_io, guest_io) = duplex(128);
        let mut host_stream =
            FrameStream::with_default_limit(host_io, ProtocolPeer::Host).expect("host stream");
        let mut guest_stream =
            FrameStream::with_default_limit(guest_io, ProtocolPeer::Guest).expect("guest stream");

        let first = host_frame(1, AgentMessage::Ping);
        let second = host_frame(2, AgentMessage::Status);
        let mut wire = first.encode(1024).expect("first bytes");
        wire.extend(second.encode(1024).expect("second bytes"));
        host_stream
            .get_mut()
            .write_all(&wire)
            .await
            .expect("write wire");
        host_stream.close().await.expect("close host");
        drop(host_stream);

        assert_eq!(guest_stream.recv().await.expect("first frame"), first);
        assert_eq!(guest_stream.recv().await.expect("second frame"), second);
        assert!(matches!(
            guest_stream.recv().await,
            Err(TransportError::Closed)
        ));
    }

    #[tokio::test]
    async fn frame_stream_reports_close_during_partial_frame() {
        let (host_io, guest_io) = duplex(128);
        let mut host_stream =
            FrameStream::with_default_limit(host_io, ProtocolPeer::Host).expect("host stream");
        let mut guest_stream =
            FrameStream::with_default_limit(guest_io, ProtocolPeer::Guest).expect("guest stream");

        let request = host_frame(1, AgentMessage::Ping);
        let mut wire = request.encode(1024).expect("host bytes");
        wire.truncate(FRAME_HEADER_LEN / 2);
        host_stream
            .get_mut()
            .write_all(&wire)
            .await
            .expect("write partial header");
        host_stream.close().await.expect("close host");
        drop(host_stream);

        assert!(matches!(
            guest_stream.recv().await,
            Err(TransportError::ClosedDuringPartialFrame)
        ));
    }

    #[tokio::test]
    async fn frame_stream_rejects_invalid_limit() {
        let (stream, _peer) = duplex(8);
        assert!(FrameStream::new(stream, ProtocolPeer::Host, 0).is_err());
    }

    #[tokio::test]
    async fn frame_stream_into_inner_returns_underlying_stream() {
        let (host_io, guest_io) = duplex(64);
        let host_stream =
            FrameStream::with_default_limit(host_io, ProtocolPeer::Host).expect("host stream");
        let guest_stream =
            FrameStream::with_default_limit(guest_io, ProtocolPeer::Guest).expect("guest stream");

        let mut host_io = host_stream.into_inner();
        let mut guest_io = guest_stream.into_inner();
        host_io
            .write_all(b"released")
            .await
            .expect("write through released stream");
        let mut bytes = [0u8; 8];
        guest_io.read_exact(&mut bytes).await.expect("read bytes");
        assert_eq!(&bytes, b"released");
    }
}
