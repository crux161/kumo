#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

//! `wc` core: line/word/byte counting kept as a host-tested pure function so the bin is only
//! I/O glue. A line is a `\n` byte; a word is a maximal run of non-whitespace. The streaming
//! [`Counter`] tracks word continuity across feeds, so a file read in chunks counts the same
//! as one read whole.

/// Line, word, and byte tally for a byte stream.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Counts {
    pub lines: usize,
    pub words: usize,
    pub bytes: usize,
}

/// Incremental `wc` counter: [`feed`](Counter::feed) byte chunks, then read [`counts`](Counter::counts).
/// `in_word` carries across feeds so a word split by a chunk boundary is counted once.
#[derive(Clone, Copy, Debug, Default)]
pub struct Counter {
    lines: usize,
    words: usize,
    bytes: usize,
    in_word: bool,
}

impl Counter {
    /// A zeroed counter.
    pub fn new() -> Self {
        Self::default()
    }

    /// Fold one chunk of bytes into the running tally.
    pub fn feed(&mut self, chunk: &[u8]) {
        self.bytes += chunk.len();
        for &b in chunk {
            if b == b'\n' {
                self.lines += 1;
            }
            if b.is_ascii_whitespace() {
                self.in_word = false;
            } else if !self.in_word {
                self.in_word = true;
                self.words += 1;
            }
        }
    }

    /// The tally so far.
    pub fn counts(&self) -> Counts {
        Counts {
            lines: self.lines,
            words: self.words,
            bytes: self.bytes,
        }
    }
}

/// Count a whole slice in one shot (the non-streaming convenience over [`Counter`]).
pub fn count(input: &[u8]) -> Counts {
    let mut counter = Counter::new();
    counter.feed(input);
    counter.counts()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_empty_input() {
        assert_eq!(count(b""), Counts::default());
    }

    #[test]
    fn counts_lines_words_and_bytes() {
        assert_eq!(
            count(b"hello world\nfoo\n"),
            Counts {
                lines: 2,
                words: 3,
                bytes: 16,
            }
        );
    }

    #[test]
    fn counts_without_trailing_newline_have_zero_lines() {
        assert_eq!(
            count(b"a b c"),
            Counts {
                lines: 0,
                words: 3,
                bytes: 5,
            }
        );
    }

    #[test]
    fn counts_collapse_whitespace_runs() {
        assert_eq!(
            count(b"  a   b  \n"),
            Counts {
                lines: 1,
                words: 2,
                bytes: 10,
            }
        );
    }

    #[test]
    fn counter_keeps_word_continuity_across_feeds() {
        // "hello world\n" split mid-word must still count two words, not three.
        let mut counter = Counter::new();
        counter.feed(b"hel");
        counter.feed(b"lo wo");
        counter.feed(b"rld\n");
        assert_eq!(
            counter.counts(),
            Counts {
                lines: 1,
                words: 2,
                bytes: 12,
            }
        );
    }
}
