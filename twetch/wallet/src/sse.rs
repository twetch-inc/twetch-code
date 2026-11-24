//! Finds the cost in a streamed answer without delaying it.

use serde::Deserialize;

/// A data line longer than this cannot be the usage chunk; its bytes are not kept.
const MAX_LINE: usize = 1 << 20;

#[derive(Deserialize)]
struct Usage {
    cost_sats: Option<u64>,
}

#[derive(Deserialize)]
struct Chunk {
    request_id: Option<String>,
    usage: Option<Usage>,
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub struct Cost {
    pub request_id: Option<String>,
    pub sats: u64,
}

#[derive(Default)]
pub struct Scanner {
    line: Vec<u8>,
    skipping: bool,
}

impl Scanner {
    /// Feed the next bytes as they arrived. Returns the cost once its line is whole.
    pub fn push(&mut self, mut bytes: &[u8]) -> Option<Cost> {
        let mut found = None;
        while let Some(end) = memchr::memchr(b'\n', bytes) {
            let (head, rest) = bytes.split_at(end);
            bytes = &rest[1..];
            let cost = if self.skipping {
                self.skipping = false;
                None
            } else if self.line.is_empty() {
                parse(head)
            } else {
                self.line.extend_from_slice(head);
                parse(&self.line)
            };
            self.line.clear();
            found = cost.or(found);
        }
        if !self.skipping {
            if self.line.len() + bytes.len() > MAX_LINE {
                self.skipping = true;
                self.line.clear();
            } else {
                self.line.extend_from_slice(bytes);
            }
        }
        found
    }

    /// The stream ended: a last line may have no newline.
    pub fn finish(&mut self) -> Option<Cost> {
        parse(&std::mem::take(&mut self.line))
    }
}

fn parse(line: &[u8]) -> Option<Cost> {
    let data = line.strip_prefix(b"data:")?;
    memchr::memmem::find(data, b"\"cost_sats\"")?;
    let chunk: Chunk = serde_json::from_slice(data.trim_ascii()).ok()?;
    Some(Cost { request_id: chunk.request_id, sats: chunk.usage?.cost_sats? })
}

#[cfg(test)]
mod tests {
    use super::*;

    const STREAM: &[u8] = b"data: {\"id\":\"c\",\"choices\":[{\"delta\":{\"content\":\"\\\"cost_sats\\\": 9\"}}]}\r\n\r\ndata: {\"request_id\":\"r1\",\"choices\":[],\"usage\":{\"prompt_tokens\":3,\"cost_sats\":1234,\"cost_usd\":0.1,\"balance_sats\":-5}}\n\ndata: [DONE]\n\n";

    #[test]
    fn cost_survives_every_chunk_boundary() {
        let want = Some(Cost { request_id: Some("r1".into()), sats: 1234 });
        for cut in 0..=STREAM.len() {
            let mut s = Scanner::default();
            let first = s.push(&STREAM[..cut]);
            let second = s.push(&STREAM[cut..]);
            assert_eq!(second.or(first), want, "cut at {cut}");
        }
        let mut s = Scanner::default();
        let mut found = None;
        for b in STREAM {
            found = s.push(std::slice::from_ref(b)).or(found);
        }
        assert_eq!(found, want, "one byte at a time");
    }

    #[test]
    fn model_text_is_not_a_cost() {
        let mut s = Scanner::default();
        assert_eq!(s.push(b"data: {\"choices\":[{\"delta\":{\"content\":\"{\\\"usage\\\":{\\\"cost_sats\\\":1}}\"}}]}\n\n"), None);
        assert_eq!(s.push(b"data: \"cost_sats\" not json\n"), None);
        assert_eq!(s.push(b": comment \"cost_sats\"\n"), None);
    }

    #[test]
    fn a_last_line_without_newline_counts() {
        let mut s = Scanner::default();
        assert_eq!(s.push(b"data: {\"usage\":{\"cost_sats\":7}}"), None);
        assert_eq!(s.finish(), Some(Cost { request_id: None, sats: 7 }));
    }

    #[test]
    fn a_huge_line_is_skipped_and_the_next_is_read() {
        let mut s = Scanner::default();
        for _ in 0..3 {
            assert_eq!(s.push(&vec![b'x'; MAX_LINE / 2 + 1]), None);
        }
        assert!(s.line.len() <= MAX_LINE);
        assert_eq!(s.push(b"\ndata: {\"usage\":{\"cost_sats\":8}}\n"), Some(Cost { request_id: None, sats: 8 }));
    }
}
