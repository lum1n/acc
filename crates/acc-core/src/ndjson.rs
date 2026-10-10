//! Newline-delimited JSON framing.
//!
//! Records are split on LF only and a trailing CR is stripped. This matches
//! what pi's RPC docs require (Unicode line separators inside strings are not
//! record boundaries) and is valid for Claude stream-json, Codex app-server
//! and ACP too.

/// Splits an incoming byte stream into complete lines.
#[derive(Debug)]
pub struct LineFramer {
    buf: Vec<u8>,
    max_line: usize,
    discarding: bool,
}

/// A framed line, or a notice that an oversized line was dropped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    Line(String),
    /// A line exceeded the limit and was skipped up to its newline.
    Oversized,
    /// A line was not valid UTF-8.
    InvalidUtf8,
}

impl LineFramer {
    /// `max_line` bounds memory per line; a hostile or broken stream can't grow it.
    pub fn new(max_line: usize) -> Self {
        Self {
            buf: Vec::new(),
            max_line,
            discarding: false,
        }
    }

    pub fn feed(&mut self, mut bytes: &[u8]) -> Vec<Frame> {
        let mut out = Vec::new();
        while let Some(pos) = bytes.iter().position(|&b| b == b'\n') {
            let (head, tail) = bytes.split_at(pos);
            bytes = &tail[1..];
            if self.discarding {
                self.discarding = false;
                self.buf.clear();
                continue;
            }
            if self.buf.len() + head.len() > self.max_line {
                self.buf.clear();
                out.push(Frame::Oversized);
                continue;
            }
            self.buf.extend_from_slice(head);
            if self.buf.last() == Some(&b'\r') {
                self.buf.pop();
            }
            let line = std::mem::take(&mut self.buf);
            if line.is_empty() {
                continue;
            }
            out.push(match String::from_utf8(line) {
                Ok(s) => Frame::Line(s),
                Err(_) => Frame::InvalidUtf8,
            });
        }
        if !self.discarding {
            if self.buf.len() + bytes.len() > self.max_line {
                self.buf.clear();
                self.discarding = true;
                out.push(Frame::Oversized);
            } else {
                self.buf.extend_from_slice(bytes);
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(frames: Vec<Frame>) -> Vec<String> {
        frames
            .into_iter()
            .filter_map(|f| if let Frame::Line(l) = f { Some(l) } else { None })
            .collect()
    }

    #[test]
    fn splits_across_chunks_and_strips_cr() {
        let mut f = LineFramer::new(1024);
        assert!(f.feed(b"{\"a\":").is_empty());
        assert_eq!(lines(f.feed(b"1}\r\n{\"b\":2}\n\n")), ["{\"a\":1}", "{\"b\":2}"]);
    }

    #[test]
    fn unicode_line_separator_is_not_a_boundary() {
        let mut f = LineFramer::new(1024);
        let input = "{\"t\":\"a\u{2028}b\"}\n";
        assert_eq!(lines(f.feed(input.as_bytes())), [input.trim_end()]);
    }

    #[test]
    fn oversized_line_is_skipped_once() {
        let mut f = LineFramer::new(8);
        let frames = f.feed(b"0123456789");
        assert_eq!(frames, [Frame::Oversized]);
        assert!(f.feed(b"abc").is_empty());
        assert_eq!(f.feed(b"\nok\n"), [Frame::Line("ok".into())]);
    }
}
