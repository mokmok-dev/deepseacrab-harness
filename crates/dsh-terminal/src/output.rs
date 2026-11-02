//! Bounded terminal output with a cursor tracking what the model has read.

use std::sync::{Mutex, MutexGuard, PoisonError};

/// Newest bytes kept for one terminal.
pub const MAX_OUTPUT_BYTES: usize = 1024 * 1024;

/// Output bytes plus the offset already handed to the model.
#[derive(Debug, Default)]
pub struct Output {
    bytes: Vec<u8>,
    cursor: usize,
}

impl Output {
    /// An empty log.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            bytes: Vec::new(),
            cursor: 0,
        }
    }

    /// Append PTY bytes, dropping the oldest bytes beyond `cap`.
    ///
    /// The cursor follows the drop so it still points at the oldest byte the
    /// model has not read.
    pub fn append(&mut self, chunk: &[u8], cap: usize) {
        self.bytes.extend_from_slice(chunk);
        let excess = self.bytes.len().saturating_sub(cap);
        self.bytes.drain(..excess);
        self.cursor = self.cursor.saturating_sub(excess);
    }

    /// Bytes not yet read, advancing the cursor to the end.
    pub fn take(&mut self) -> Vec<u8> {
        let chunk = self.bytes[self.cursor..].to_vec();
        self.cursor = self.bytes.len();
        chunk
    }

    /// Number of bytes currently retained.
    #[cfg(test)]
    #[must_use]
    pub fn buffered(&self) -> usize {
        self.bytes.len()
    }

    /// Bytes currently retained.
    #[cfg(test)]
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

/// Lock the shared output buffer, recovering a guard from a poisoned mutex.
///
/// The reader thread holds this lock only while appending plain bytes, so a
/// poisoned guard still owns a consistent buffer.
pub fn lock(output: &Mutex<Output>) -> MutexGuard<'_, Output> {
    output.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn output_is_capped_at_one_mebibyte() {
        assert_eq!(MAX_OUTPUT_BYTES, 1_048_576);
    }

    #[test]
    fn old_bytes_are_dropped_first() {
        let mut output = Output::new();
        output.append(b"abc", 8);
        assert_eq!(output.take(), b"abc");
        output.append(b"defgh", 5);
        assert_eq!(output.buffered(), 5);
        assert_eq!(output.take(), b"defgh");
    }

    #[test]
    fn trimming_moves_the_cursor_to_the_oldest_unread_byte() {
        let mut output = Output::new();
        output.append(b"abcdef", 10);
        assert_eq!(output.take(), b"abcdef");
        output.append(b"ghij", 10);
        output.append(b"klm", 10);
        assert_eq!(output.buffered(), 10);
        assert_eq!(output.take(), b"ghijklm");
        assert_eq!(output.take(), Vec::<u8>::new());
    }

    #[test]
    fn an_empty_chunk_changes_nothing() {
        let mut output = Output::new();
        output.append(b"abc", 4);
        assert_eq!(output.take(), b"abc");
        output.append(b"", 4);
        assert_eq!(output.buffered(), 3);
        assert_eq!(output.take(), Vec::<u8>::new());
    }

    #[test]
    fn a_poisoned_output_lock_is_recovered() {
        let output = Mutex::new(Output::new());
        let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            output.lock().expect("lock").append(b"x", 4);
            panic!("poison the mutex");
        }));
        assert!(poisoned.is_err());
        assert_eq!(lock(&output).buffered(), 1);
    }

    proptest! {
        #[test]
        fn appending_keeps_the_newest_bytes(
            chunks in prop::collection::vec(prop::collection::vec(any::<u8>(), 0..64), 0..16),
            cap in 0_usize..64,
        ) {
            let mut output = Output::new();
            let mut all: Vec<u8> = Vec::new();
            for chunk in chunks {
                output.append(&chunk, cap);
                all.extend_from_slice(&chunk);
                prop_assert!(output.buffered() <= cap);
                prop_assert_eq!(output.bytes(), &all[all.len() - output.buffered()..]);
            }
            let taken = output.take();
            prop_assert_eq!(&taken[..], &all[all.len().saturating_sub(cap)..]);
            prop_assert_eq!(output.take().len(), 0);
        }

        #[test]
        fn reads_return_only_unread_bytes(
            chunks in prop::collection::vec(prop::collection::vec(any::<u8>(), 0..32), 0..12),
            cap in 0_usize..32,
        ) {
            let mut output = Output::new();
            let mut all: Vec<u8> = Vec::new();
            let mut unread = 0_usize;
            for chunk in chunks {
                let before = output.buffered();
                output.append(&chunk, cap);
                all.extend_from_slice(&chunk);
                let length = (before + chunk.len()).min(cap);
                unread = (unread + chunk.len()).min(length);
                prop_assert_eq!(output.buffered(), length);
                prop_assert!(unread <= length);
                let taken = output.take();
                prop_assert_eq!(taken.len(), unread);
                unread = 0;
            }
        }
    }
}
