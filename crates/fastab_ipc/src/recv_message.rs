use std::io;

use async_trait::async_trait;
use bytes::Buf;
use fastab_proto::prost::Message;
use fastab_proto::{FigMessage, ReflectMessage};
use tokio::io::{AsyncRead, AsyncReadExt};

use crate::BufferedReader;
use crate::error::RecvError;

#[async_trait]
pub trait RecvMessage {
    async fn recv_message<R>(&mut self) -> Result<Option<R>, RecvError>
    where
        R: Message + ReflectMessage + Default;
}

#[async_trait]
impl<T> RecvMessage for BufferedReader<T>
where
    T: AsyncRead + Unpin + Send,
{
    async fn recv_message<M>(&mut self) -> Result<Option<M>, RecvError>
    where
        M: Message + ReflectMessage + Default,
    {
        loop {
            self.buffer_high_water = self.buffer_high_water.max(self.buffer.capacity());
            // Try to parse the message until the buffer is a valid message
            let mut cursor = io::Cursor::new(&self.buffer);
            match FigMessage::parse(&mut cursor) {
                // If the parsed message is valid, return it
                Ok((len, message)) => {
                    self.buffer.advance(len);
                    self.reclaim_consumed_buffer();
                    return Ok(Some(message.decode()?));
                },
                // If the message is incomplete, read more into the buffer
                Err(fastab_proto::FigMessageParseError::Incomplete(_, _)) => {
                    // Bound each read and the accumulated encoded frame. The
                    // parser rejects an oversized declared length at the header.
                    let remaining = fastab_proto::MAX_FIG_MESSAGE_BYTES.saturating_sub(self.buffer.len());
                    let bytes = (&mut self.inner)
                        .take(remaining.min(16 * 1024) as u64)
                        .read_buf(&mut self.buffer)
                        .await?;

                    // If the buffer is empty, we've reached EOF
                    if bytes == 0 {
                        if self.buffer.is_empty() {
                            return Ok(None);
                        } else {
                            return Err(RecvError::Io(io::Error::from(io::ErrorKind::UnexpectedEof)));
                        }
                    }
                },
                // On any other error, return the error
                Err(err) => {
                    // TODO(grant): add resyncing to message boundary
                    let position = cursor.position() as usize;
                    self.buffer.advance(position);
                    self.reclaim_consumed_buffer();
                    return Err(err.into());
                },
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;
    use crate::SendMessage;

    fn large_frame() -> bytes::Bytes {
        use fastab_proto::FigProtobufEncodable;
        fastab_proto::hooks::hook_to_message(fastab_proto::hooks::new_edit_buffer_hook(
            None,
            "x".repeat(128 * 1024),
            0,
            0,
            None,
        ))
        .encode_fastab_protobuf()
        .unwrap()
    }

    #[tokio::test]
    async fn consuming_a_large_frame_releases_its_backing_even_with_a_partial_next_frame() {
        use fastab_proto::FigProtobufEncodable;
        let large = large_frame();
        let small_message = test_message_small();
        let small = small_message.encode_fastab_protobuf().unwrap();
        for suffix_len in [0, 8, small.len()] {
            let mut buffer = bytes::BytesMut::from(large.as_ref());
            buffer.extend_from_slice(&small[..suffix_len]);
            let frame_bytes = buffer.len();
            buffer.extend_from_slice(b"allocation witness");
            let witness = buffer.split_off(frame_bytes).freeze();
            assert!(!witness.is_unique());

            let mut reader = BufferedReader::new(Cursor::new(small[suffix_len..].to_vec()));
            reader.buffer = buffer;
            reader
                .recv_message::<fastab_proto::local::LocalMessage>()
                .await
                .unwrap()
                .unwrap();
            // Unlike a len/capacity assertion, uniqueness proves the original
            // allocation is no longer owned by the empty/small reader view.
            assert!(witness.is_unique());
            assert_eq!(reader.recv_message().await.unwrap(), Some(small_message.clone()));
            assert!(
                reader
                    .recv_message::<fastab_proto::local::LocalMessage>()
                    .await
                    .unwrap()
                    .is_none()
            );
        }
    }

    #[tokio::test]
    async fn fragmented_large_frame_then_small_frame_releases_the_high_water() {
        use fastab_proto::FigProtobufEncodable;
        let mut wire = large_frame().to_vec();
        let small = test_message_small();
        wire.extend_from_slice(&small.encode_fastab_protobuf().unwrap());
        let (mut sender, receiver) = tokio::io::duplex(4096);
        let producer = tokio::spawn(async move {
            use tokio::io::AsyncWriteExt;
            for chunk in wire.chunks(37) {
                sender.write_all(chunk).await.unwrap();
            }
        });
        let mut reader = BufferedReader::new(receiver);
        assert!(
            reader
                .recv_message::<fastab_proto::local::LocalMessage>()
                .await
                .unwrap()
                .is_some()
        );
        assert_eq!(reader.recv_message().await.unwrap(), Some(small));
        assert!(reader.buffer_high_water <= 64 * 1024);
        assert!(
            reader
                .recv_message::<fastab_proto::local::LocalMessage>()
                .await
                .unwrap()
                .is_none()
        );
        producer.await.unwrap();
    }

    #[tokio::test]
    async fn oversized_header_is_rejected_without_waiting_for_a_body() {
        let mut wire = b"\x1b@fig-pbuf".to_vec();
        wire.extend_from_slice(&u64::MAX.to_be_bytes());
        let mut reader = BufferedReader::new(Cursor::new(wire));
        assert!(matches!(
            reader.recv_message::<fastab_proto::local::LocalMessage>().await,
            Err(RecvError::Parse(fastab_proto::FigMessageParseError::BodyTooLarge(
                u64::MAX
            )))
        ));
    }

    fn mock(initial: Vec<u8>) -> BufferedReader<Cursor<Vec<u8>>> {
        let size = initial.len();
        let mut inner = Cursor::new(initial);
        inner.set_position(size as u64);
        BufferedReader::new(inner)
    }

    fn test_message_small() -> fastab_proto::local::LocalMessage {
        fastab_proto::hooks::hook_to_message(fastab_proto::hooks::new_hide_hook())
    }

    fn test_message_large() -> fastab_proto::local::LocalMessage {
        fastab_proto::hooks::hook_to_message(fastab_proto::hooks::new_edit_buffer_hook(
            None,
            "A".repeat(10000),
            0,
            0,
            None,
        ))
    }

    #[tokio::test]
    async fn single_message_small() {
        let mut mock = mock(vec![]);
        mock.send_message(test_message_small()).await.unwrap();
        mock.inner.set_position(0);
        assert_eq!(mock.recv_message().await.unwrap(), Some(test_message_small()));
    }

    #[tokio::test]
    async fn single_message_large() {
        let mut mock = mock(vec![]);
        mock.send_message(test_message_large()).await.unwrap();
        mock.inner.set_position(0);
        assert_eq!(mock.recv_message().await.unwrap(), Some(test_message_large()));
    }

    #[tokio::test]
    async fn mutlti_message_small() {
        let mut mock = mock(vec![]);
        for _ in 0..500 {
            mock.send_message(test_message_small()).await.unwrap();
        }
        mock.inner.set_position(0);
        for _ in 0..500 {
            assert_eq!(mock.recv_message().await.unwrap(), Some(test_message_small()));
        }
        assert_eq!(mock.read(&mut [0u8]).await.unwrap(), 0);
        assert_eq!(mock.buffer.len(), 0);
    }

    #[tokio::test]
    async fn mutlti_message_large() {
        let mut mock = mock(vec![]);
        for _ in 0..500 {
            mock.send_message(test_message_large()).await.unwrap();
        }
        mock.inner.set_position(0);
        for _ in 0..500 {
            assert_eq!(mock.recv_message().await.unwrap(), Some(test_message_large()));
        }
        assert_eq!(mock.read(&mut [0u8]).await.unwrap(), 0);
        assert_eq!(mock.buffer.len(), 0);
    }

    #[tokio::test]
    async fn invalid_header() {
        let mut mock = mock(vec![b'f', b'o', b'o']);
        mock.inner.set_position(0);
        assert!(mock.recv_message::<fastab_proto::local::LocalMessage>().await.is_err());
    }
}
