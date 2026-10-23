use memmem::{Searcher, TwoWaySearcher};

const INITIAL_CAPACITY: usize = 16;
// Keep ordinary input reusable; only retire a large, fully consumed paste.
const MAX_IDLE_CAPACITY: usize = 64 * 1024;

/// This is a simple, small, read buffer that always has the buffer
/// contents available as a contiguous slice.
#[derive(Debug)]
pub struct ReadBuffer {
    storage: Vec<u8>,
    /// Bytes before this offset are consumed. Advancing only moves it, so a
    /// long run of one-character events (an unbracketed paste) costs O(n)
    /// instead of shifting the whole remainder once per event.
    start: usize,
}

impl Default for ReadBuffer {
    fn default() -> Self {
        Self::new()
    }
}

impl ReadBuffer {
    pub fn new() -> Self {
        Self {
            storage: Vec::with_capacity(INITIAL_CAPACITY),
            start: 0,
        }
    }

    pub fn as_slice(&self) -> &[u8] {
        &self.storage[self.start..]
    }

    pub fn is_empty(&self) -> bool {
        self.start == self.storage.len()
    }

    pub fn len(&self) -> usize {
        self.storage.len() - self.start
    }

    pub fn capacity(&self) -> usize {
        self.storage.capacity()
    }

    /// Mark `len` bytes as consumed. The remainder stays where it is until
    /// the next append compacts it, or the buffer empties and rewinds.
    pub fn advance(&mut self, len: usize) {
        debug_assert!(len <= self.len());
        self.start += len;
        if self.start == self.storage.len() {
            self.storage.clear();
            self.start = 0;
        }
    }

    /// Append the contents of the slice to the read buffer. Consumed bytes
    /// are dropped first, so storage never holds more than the unread
    /// remainder plus the new slice.
    pub fn extend_with(&mut self, slice: &[u8]) {
        if self.start > 0 {
            self.storage.drain(..self.start);
            self.start = 0;
        }
        self.storage.extend_from_slice(slice);
    }

    /// Run after a whole parse pass, never between bytes of the same paste.
    pub fn release_large_empty_buffer(&mut self) {
        if self.is_empty() && self.storage.capacity() > MAX_IDLE_CAPACITY {
            self.storage = Vec::with_capacity(INITIAL_CAPACITY);
            self.start = 0;
        }
    }

    /// Search for `needle` starting at `offset`.  Returns its offset
    /// into the buffer if found, else None.
    pub fn find_subsequence(&self, offset: usize, needle: &[u8]) -> Option<usize> {
        let needle = TwoWaySearcher::new(needle);
        let haystack = &self.as_slice()[offset..];
        needle.search_in(haystack).map(|x| x + offset)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn consumed_bytes_are_dropped_before_the_next_append() {
        let mut buf = ReadBuffer::new();
        buf.extend_with(b"abcdef");
        buf.advance(2);
        assert_eq!(buf.as_slice(), b"cdef");
        assert_eq!(buf.len(), 4);
        assert_eq!(buf.find_subsequence(1, b"ef"), Some(2));
        buf.extend_with(b"gh");
        assert_eq!(buf.as_slice(), b"cdefgh");
        assert_eq!(buf.storage.len(), 6);
        buf.advance(6);
        assert!(buf.is_empty());
        assert!(buf.storage.is_empty());
    }
}
