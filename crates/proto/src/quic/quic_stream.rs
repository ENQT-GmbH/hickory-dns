use std::convert::TryInto;

use bytes::Bytes;
use quinn::{RecvStream, SendStream, VarInt};

use crate::{
    error::{ProtoError, ProtoErrorKind},
    op::Message,
};

/// ```text
/// 5.1. Connection Establishment
///
/// DoQ connections are established as described in the QUIC transport specification [RFC9000]. During connection establishment,
/// DoQ support is indicated by selecting the ALPN token "doq" in the crypto handshake.
/// ```
pub const DOQ_ALPN: &[u8] = b"doq";

/// [DoQ Error Codes](https://www.ietf.org/archive/id/draft-ietf-dprive-dnsoquic-10.html#name-doq-error-codes), draft-ietf-dprive-dnsoquic, Feb. 28, 2022
/// ```text
///  5.3. DoQ Error Codes
///
/// The following error codes are defined for use when abruptly terminating streams, aborting reading of streams, or immediately closing connections:
///
/// DOQ_NO_ERROR (0x0):
///     No error. This is used when the connection or stream needs to be closed, but there is no error to signal.
///
/// DOQ_INTERNAL_ERROR (0x1):
///     The DoQ implementation encountered an internal error and is incapable of pursuing the transaction or the connection.
///
/// DOQ_PROTOCOL_ERROR (0x2):
///     The DoQ implementation encountered an protocol error and is forcibly aborting the connection.
///
/// DOQ_REQUEST_CANCELLED (0x3):
///     A DoQ client uses this to signal that it wants to cancel an outstanding transaction.
///
/// DOQ_EXCESSIVE_LOAD (0x4):
///     A DoQ implementation uses this to signal when closing a connection due to excessive load.
///
/// DOQ_ERROR_RESERVED (0xd098ea5e):
///     Alternative error code used for tests.
/// ```
#[repr(u32)]
pub enum DoqErrorCode {
    /// No error. This is used when the connection or stream needs to be closed, but there is no error to signal.
    NoError,
    /// The DoQ implementation encountered an internal error and is incapable of pursuing the transaction or the connection.
    InternalError,
    /// The DoQ implementation encountered an protocol error and is forcibly aborting the connection.
    ProtocolError,
    /// A DoQ client uses this to signal that it wants to cancel an outstanding transaction.
    RequestCancelled,
    /// A DoQ implementation uses this to signal when closing a connection due to excessive load.
    ExcessiveLoad,
    /// Alternative error code used for tests.
    ErrorReserved,
    /// Unknown Error code
    Unknown(u32),
}

// not using repr(u32) above because of the Unknown
const NO_ERROR: u32 = 0x0;
const INTERNAL_ERROR: u32 = 0x1;
const PROTOCOL_ERROR: u32 = 0x2;
const REQUEST_CANCELLED: u32 = 0x3;
const EXCESSIVE_LOAD: u32 = 0x4;
const ERROR_RESERVED: u32 = 0xd098ea5e;

impl From<DoqErrorCode> for VarInt {
    fn from(doq_error: DoqErrorCode) -> Self {
        use DoqErrorCode::*;

        match doq_error {
            NoError => VarInt::from_u32(NO_ERROR),
            InternalError => VarInt::from_u32(INTERNAL_ERROR),
            ProtocolError => VarInt::from_u32(PROTOCOL_ERROR),
            RequestCancelled => VarInt::from_u32(REQUEST_CANCELLED),
            ExcessiveLoad => VarInt::from_u32(EXCESSIVE_LOAD),
            ErrorReserved => VarInt::from_u32(ERROR_RESERVED),
            Unknown(code) => VarInt::from_u32(code),
        }
    }
}

impl From<VarInt> for DoqErrorCode {
    fn from(doq_error: VarInt) -> Self {
        let code: u32 = if let Ok(code) = doq_error.into_inner().try_into() {
            code
        } else {
            return Self::ProtocolError;
        };

        match code {
            NO_ERROR => Self::NoError,
            INTERNAL_ERROR => Self::InternalError,
            PROTOCOL_ERROR => Self::ProtocolError,
            REQUEST_CANCELLED => Self::RequestCancelled,
            EXCESSIVE_LOAD => Self::ExcessiveLoad,
            ERROR_RESERVED => Self::ErrorReserved,
            _ => Self::Unknown(code),
        }
    }
}

pub(crate) struct QuicStream {
    send_stream: SendStream,
    receive_stream: RecvStream,
}

impl QuicStream {
    pub(crate) fn new(send_stream: SendStream, receive_stream: RecvStream) -> Self {
        Self {
            send_stream,
            receive_stream,
        }
    }

    pub(crate) async fn send(&mut self, mut message: Message) -> Result<(), ProtoError> {
        // RFC: When sending queries over a QUIC connection, the DNS Message ID MUST be set to zero. The stream mapping for DoQ allows for
        // unambiguous correlation of queries and responses and so the Message ID field is not required.
        message.set_id(0);

        let bytes = Bytes::from(message.to_vec()?);

        // In order that multiple responses can be parsed, a 2-octet length field is used in exactly the same way as the 2-octet length
        // field defined for DNS over TCP [RFC1035]. The practical result of this is that the content of each QUIC stream is exactly
        // the same as the content of a TCP connection that would manage exactly one query.All DNS messages (queries and responses)
        // sent over DoQ connections MUST be encoded as a 2-octet length field followed by the message content as specified in [RFC1035].
        let len = bytes.len().to_ne_bytes().to_vec();
        let len = Bytes::from(len);

        self.send_stream.write_all_chunks(&mut [len, bytes]).await?;
        Ok(())
    }

    /// finishes the send stream, i.e. there will be no more data sent to the remote
    pub(crate) async fn finish(&mut self) -> Result<(), ProtoError> {
        self.send_stream.finish();

        Ok(())
    }

    pub(crate) async fn receive(&mut self) -> Result<Message, ProtoError> {
        // following above, the data should be first the length, followed by the message(s)
        let mut len = [0u8; 2];
        self.receive_stream.read_exact(&mut len).await?;
        let len = u16::from_ne_bytes(len) as usize;

        // RFC: DoQ Queries and Responses are sent on QUIC streams, which in theory can carry up to 2^62 bytes.
        //  However, DNS messages are restricted in practice to a maximum size of 65535 bytes. This maximum size
        //  is enforced by the use of a two-octet message length field in DNS over TCP [RFC1035] and DNS over TLS [RFC7858],
        //  and by the definition of the "application/dns-message" for DNS over HTTP [RFC8484]. DoQ enforces the same restriction.
        let mut bytes = vec![0; len];
        self.receive_stream.read_exact(&mut bytes[..len]);

        let message = Message::from_vec(&bytes)?;

        // assert that the message id is 0, this is a bad dns-over-quic packet if not
        if message.id() != 0 {
            return Err(ProtoErrorKind::QuicMessageIdNot0(message.id()).into());
        }

        Ok(message)
    }
}
