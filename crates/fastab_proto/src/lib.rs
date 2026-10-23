//! Protocol buffer definitions

pub mod fig;
pub mod fig_common;
pub mod figterm;
pub mod hooks;
pub mod local;
pub mod mux;
pub(crate) mod proto;
pub mod remote_hooks;
pub mod util;
use std::fmt::Debug;
use std::mem::size_of;
use std::num::TryFromIntError;
use std::sync::LazyLock;

use bytes::{Buf, Bytes, BytesMut};
pub use prost;
use prost::{DecodeError, EncodeError, Message};
use prost_reflect::DescriptorPool;
pub use prost_reflect::{DynamicMessage, ReflectMessage};
use serde::Serialize;
use thiserror::Error;

pub mod remote {
    pub use crate::proto::remote::*;
}

/// Maximum encoded frame, including its fixed header. Match the existing
/// desktop/PTY outbox budget and reject a size before buffering its body.
pub const MAX_FIG_MESSAGE_BYTES: usize = 4 * 1024 * 1024;
const FIG_MESSAGE_HEADER_BYTES: usize = 18;
const MAX_FIG_MESSAGE_BODY_BYTES: usize = MAX_FIG_MESSAGE_BYTES - FIG_MESSAGE_HEADER_BYTES;

// This is not used explicitly, but it must be here for the derive
// impls on the protos for dynamic message
static DESCRIPTOR_POOL: LazyLock<DescriptorPool> = LazyLock::new(|| {
    DescriptorPool::decode(include_bytes!(concat!(env!("OUT_DIR"), "/file_descriptor_set.bin")).as_ref()).unwrap()
});

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FigMessageType {
    Protobuf,
    Json,
    MessagePack,
}

impl FigMessageType {
    pub const fn header(&self) -> &'static [u8] {
        match self {
            FigMessageType::Protobuf => b"fig-pbuf",
            FigMessageType::Json => b"fig-json",
            FigMessageType::MessagePack => b"fig-mpak",
        }
    }
}

/// A fig message
///
/// The format of a fig message is:
///
///   - The header `\x1b@`
///   - The type of the message (must be 8 bytes)
///     - `fig-pbuf` - Protocol Buffer
///     - `fig-json` - Json
///     - `fig-mpak` - MessagePack
///   - The length of the remainder of the message encoded as a big endian u64
///   - The message, encoded as protobuf, json-protobuf, or messagepack-protobuf
#[derive(Debug, Clone)]
pub struct FigMessage {
    pub inner: Bytes,
    pub message_type: FigMessageType,
}

#[derive(Debug)]
pub enum FigMessageComponent {
    Header,
    BodySize,
    Body,
}

#[derive(Debug, Error)]
pub enum FigMessageParseError {
    /// The missing component and the needed bytes
    #[error("incomplete message, missing {0:?}")]
    Incomplete(FigMessageComponent, usize),
    #[error("invalid message header {0} (raw type {1})")]
    InvalidHeader(String, String),
    #[error("invalid message type")]
    InvalidMessageType([u8; 8]),
    #[error("message body is too large: {0} bytes")]
    BodyTooLarge(u64),
    #[error("failed to convert int: {0}")]
    TryFromInt(#[from] TryFromIntError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Error)]
pub enum FigMessageDecodeError {
    #[error("name is not a valid protobuf: {0}")]
    NameNotValid(String),
    #[error(transparent)]
    ProstDecode(#[from] DecodeError),
    #[error(transparent)]
    JsonDecode(#[from] serde_json::Error),
    #[error(transparent)]
    RmpDecode(#[from] rmp_serde::decode::Error),
}

#[derive(Debug, Error)]
pub enum FigMessageEncodeError {
    #[error("message body is too large: {0} bytes")]
    BodyTooLarge(usize),
    #[error(transparent)]
    IntError(#[from] std::num::TryFromIntError),
    #[error(transparent)]
    ProstEncode(#[from] EncodeError),
    #[error(transparent)]
    JsonEncode(#[from] serde_json::Error),
    #[error(transparent)]
    RmpEncode(#[from] rmp_serde::encode::Error),
    #[error(transparent)]
    IoError(#[from] std::io::Error),
}

impl FigMessage {
    pub fn json(json: impl Serialize) -> Result<Bytes, FigMessageEncodeError> {
        FigMessage::encode(FigMessageType::Json, serde_json::to_vec(&json)?.into())
    }

    pub fn message_pack(message_pack: impl Serialize) -> Result<Bytes, FigMessageEncodeError> {
        FigMessage::encode(FigMessageType::MessagePack, rmp_serde::to_vec(&message_pack)?.into())
    }

    pub fn encode_buf(&self, dst: &mut BytesMut) -> Result<(), FigMessageEncodeError> {
        let body = &self.inner;
        Self::encode_header(dst, self.message_type, body.len())?;
        dst.extend_from_slice(body);
        Ok(())
    }

    /// Write the frame header for a `body_len`-byte body and reserve room for
    /// that body. An oversized body is rejected before `dst` is touched.
    fn encode_header(
        dst: &mut BytesMut,
        message_type: FigMessageType,
        body_len: usize,
    ) -> Result<(), FigMessageEncodeError> {
        if body_len > MAX_FIG_MESSAGE_BODY_BYTES {
            return Err(FigMessageEncodeError::BodyTooLarge(body_len));
        }
        let message_len: u64 = body_len.try_into()?;
        let message_len_be = message_len.to_be_bytes();

        dst.reserve(b"\x1b@".len() + message_type.header().len() + message_len_be.len() + body_len);
        dst.extend_from_slice(b"\x1b@");
        dst.extend_from_slice(message_type.header());
        dst.extend_from_slice(&message_len_be);
        Ok(())
    }

    pub fn to_encoded(&self) -> Result<Bytes, FigMessageEncodeError> {
        let mut inner: BytesMut = BytesMut::new();
        self.encode_buf(&mut inner)?;
        Ok(inner.freeze())
    }

    pub fn encode(message_type: FigMessageType, body: Bytes) -> Result<Bytes, FigMessageEncodeError> {
        let msg = Self {
            inner: body,
            message_type,
        };

        let mut inner: BytesMut = BytesMut::new();
        msg.encode_buf(&mut inner)?;

        Ok(inner.freeze())
    }

    pub fn parse(src: &mut impl bytes::Buf) -> Result<(usize, FigMessage), FigMessageParseError> {
        if src.remaining() < 10 {
            return Err(FigMessageParseError::Incomplete(
                FigMessageComponent::Header,
                10 - src.remaining(),
            ));
        }

        let mut header = [0; 2];
        src.copy_to_slice(&mut header);
        if header[0] != b'\x1b' || header[1] != b'@' {
            let mut message_type_buf = [0; 8];
            src.copy_to_slice(&mut message_type_buf);
            return Err(FigMessageParseError::InvalidHeader(
                hex::encode(header),
                hex::encode(message_type_buf),
            ));
        }

        let mut message_type_buf = [0; 8];
        src.copy_to_slice(&mut message_type_buf);
        let message_type = match &message_type_buf {
            b"fig-pbuf" => FigMessageType::Protobuf,
            b"fig-json" => FigMessageType::Json,
            b"fig-mpak" => FigMessageType::MessagePack,
            _ => return Err(FigMessageParseError::InvalidMessageType(message_type_buf)),
        };

        if src.remaining() < size_of::<u64>() {
            return Err(FigMessageParseError::Incomplete(
                FigMessageComponent::BodySize,
                size_of::<u64>() - src.remaining(),
            ));
        }

        let body_len = src.get_u64();
        if body_len > MAX_FIG_MESSAGE_BODY_BYTES as u64 {
            return Err(FigMessageParseError::BodyTooLarge(body_len));
        }
        let len: usize = body_len.try_into()?;

        if src.remaining() < len {
            return Err(FigMessageParseError::Incomplete(
                FigMessageComponent::Body,
                len - src.remaining(),
            ));
        }

        let mut inner = vec![0; len];
        src.copy_to_slice(&mut inner);

        let message_len = 10 + size_of::<u64>() + len;

        Ok((
            message_len,
            FigMessage {
                inner: Bytes::from(inner),
                message_type,
            },
        ))
    }

    pub fn decode<T>(self) -> Result<T, FigMessageDecodeError>
    where
        T: Message + ReflectMessage + Default,
    {
        match self.message_type {
            FigMessageType::Protobuf => Ok(T::decode(self.inner)?),
            FigMessageType::Json => Ok(DynamicMessage::deserialize(
                T::default().descriptor(),
                &mut serde_json::Deserializer::from_slice(self.inner.as_ref()),
            )?
            .transcode_to()?),
            FigMessageType::MessagePack => Ok(DynamicMessage::deserialize(
                T::default().descriptor(),
                &mut rmp_serde::Deserializer::from_read_ref(self.inner.as_ref()),
            )?
            .transcode_to()?),
        }
    }
}

impl std::ops::Deref for FigMessage {
    type Target = Bytes;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

/// A trait for types that can be converted to a FigProtobuf
pub trait FigProtobufEncodable: Debug + Send + Sync {
    /// Encodes a protobuf message into a fig message
    fn encode_fastab_protobuf(&self) -> Result<Bytes, FigMessageEncodeError>;
}

impl<T: Message + Debug> FigProtobufEncodable for T {
    fn encode_fastab_protobuf(&self) -> Result<Bytes, FigMessageEncodeError> {
        // Every IPC frame goes through here: encode the body directly behind
        // its header instead of into a separate buffer that is then copied.
        let mut dst = BytesMut::new();
        FigMessage::encode_header(&mut dst, FigMessageType::Protobuf, self.encoded_len())?;
        self.encode(&mut dst)?;
        Ok(dst.freeze())
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn frame_budget_is_checked_before_waiting_for_the_body() {
        for size in [MAX_FIG_MESSAGE_BODY_BYTES as u64 + 1, u64::MAX] {
            let mut header = b"\x1b@fig-pbuf".to_vec();
            header.extend_from_slice(&size.to_be_bytes());
            assert!(matches!(
                FigMessage::parse(&mut header.as_slice()),
                Err(FigMessageParseError::BodyTooLarge(actual)) if actual == size
            ));
        }
    }

    #[test]
    fn largest_frame_round_trips_and_larger_encoding_does_not_touch_destination() {
        let body = Bytes::from(vec![b'x'; MAX_FIG_MESSAGE_BODY_BYTES]);
        let encoded = FigMessage::encode(FigMessageType::Protobuf, body.clone()).unwrap();
        assert_eq!(encoded.len(), MAX_FIG_MESSAGE_BYTES);
        let (length, decoded) = FigMessage::parse(&mut encoded.as_ref()).unwrap();
        assert_eq!(length, MAX_FIG_MESSAGE_BYTES);
        assert_eq!(decoded.inner, body);

        let oversized = FigMessage {
            inner: Bytes::from(vec![0; MAX_FIG_MESSAGE_BODY_BYTES + 1]),
            message_type: FigMessageType::Protobuf,
        };
        let mut destination = BytesMut::from(&b"prior frame"[..]);
        assert!(matches!(
            oversized.encode_buf(&mut destination),
            Err(FigMessageEncodeError::BodyTooLarge(_))
        ));
        assert_eq!(&destination[..], b"prior frame");
    }

    fn test_message() -> local::LocalMessage {
        let ctx = local::ShellContext {
            pid: Some(123),
            ttys: Some("/dev/pty123".into()),
            process_name: Some("/bin/bash".into()),
            current_working_directory: Some("/home/user".into()),
            session_id: None,
            terminal: None,
            hostname: None,
            shell_path: Some("/bin/bash".into()),
            wsl_distro: None,
            environment_variables: vec![],
            qterm_version: None,
            preexec: Some(false),
            osc_lock: Some(true),
            alias: Some("alias abc='abc d'\n".into()),
        };
        let hook = hooks::new_edit_buffer_hook(Some(ctx), "test", 2, 3, None);
        hooks::hook_to_message(hook)
    }

    #[test]
    fn test_to_fig_pbuf() {
        let message = test_message();
        assert_eq!(&message.encode_fastab_protobuf().unwrap()[..10], b"\x1b@fig-pbuf");
    }

    #[test]
    fn single_buffer_protobuf_frame_matches_the_framed_body() {
        let message = test_message();
        let expected = FigMessage::encode(FigMessageType::Protobuf, message.encode_to_vec().into()).unwrap();
        assert_eq!(message.encode_fastab_protobuf().unwrap(), expected);
    }

    #[test]
    fn json_round_trip() {
        let message = test_message();
        let json = serde_json::to_vec(&message.transcode_to_dynamic()).unwrap();

        let msg = FigMessage {
            inner: Bytes::from(json),
            message_type: FigMessageType::Json,
        };

        assert_eq!(&msg.to_encoded().unwrap()[..10], b"\x1b@fig-json");

        let decoded_message: local::LocalMessage = msg.decode().unwrap();

        assert_eq!(message, decoded_message);
    }

    #[test]
    fn json_decode() {
        let msg = FigMessage {
            inner: Bytes::from(
                serde_json::to_vec(&json!({
                    "hook": {
                        "caretPosition": {
                          "x": 123.0,
                          "y": 456,
                          "width": 34.0,
                          "height": 61
                        }
                    }
                }))
                .unwrap(),
            ),
            message_type: FigMessageType::Json,
        };

        let decoded_message: local::LocalMessage = msg.decode().unwrap();

        let hook = match decoded_message.r#type.unwrap() {
            local::local_message::Type::Hook(hook) => hook,
            local::local_message::Type::Command(_) => panic!(),
        };

        let caret_position = match hook.hook.unwrap() {
            local::hook::Hook::CaretPosition(caret_position) => caret_position,
            _ => panic!(),
        };

        assert_eq!(caret_position.x, 123.0);
        assert_eq!(caret_position.y, 456.0);
        assert_eq!(caret_position.width, 34.0);
        assert_eq!(caret_position.height, 61.0);
    }

    #[test]
    fn rmp_round_trip() {
        let message = test_message();
        let mpack = rmp_serde::to_vec(&message.transcode_to_dynamic()).unwrap();

        let msg = FigMessage {
            inner: Bytes::from(mpack),
            message_type: FigMessageType::MessagePack,
        };

        assert_eq!(&msg.to_encoded().unwrap()[..10], b"\x1b@fig-mpak");

        let decoded_message: local::LocalMessage = msg.decode().unwrap();

        assert_eq!(message, decoded_message);
    }
}
