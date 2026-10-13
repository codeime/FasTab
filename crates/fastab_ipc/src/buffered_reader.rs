use std::io::{self, IoSlice};
use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::BytesMut;
use pin_project_lite::pin_project;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

pin_project! {
    /// A light wrapper around a `BufReader<UnixStream>`
    #[derive(Debug)]
    pub struct BufferedReader<T> {
        #[pin]
        pub(crate) inner: T,
        pub(crate) buffer: BytesMut,
        // `advance` can hide an entire allocation behind a zero-capacity tail.
        // Track the allocation before consumption, not just the visible view.
        pub(crate) buffer_high_water: usize,
    }
}

impl<T> BufferedReader<T> {
    pub fn new(inner: T) -> Self {
        Self {
            inner,
            buffer: BytesMut::new(),
            buffer_high_water: 0,
        }
    }

    pub fn buffer(&self) -> &[u8] {
        self.buffer.as_ref()
    }

    /// Converts into inner `BufStream<UnixStream>`
    pub fn into_inner(self) -> T {
        self.inner
    }

    pub(crate) fn reclaim_consumed_buffer(&mut self) {
        const RETAIN_LIMIT: usize = 64 * 1024;
        const SMALL_TAIL: usize = 16 * 1024;
        if self.buffer_high_water > RETAIN_LIMIT && self.buffer.len() <= SMALL_TAIL {
            // A small unread suffix may contain the next frame's header/body.
            // Copy that suffix before dropping the original backing allocation.
            self.buffer = BytesMut::from(self.buffer.as_ref());
            self.buffer_high_water = self.buffer.capacity();
        }
    }
}

impl<T> AsyncWrite for BufferedReader<T>
where
    T: AsyncWrite,
{
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        self.project().inner.poll_write(cx, buf)
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        self.project().inner.poll_write_vectored(cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.project().inner.poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.project().inner.poll_shutdown(cx)
    }
}

impl<T> AsyncRead for BufferedReader<T>
where
    T: AsyncRead,
{
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        self.project().inner.poll_read(cx, buf)
    }
}
