//! The last of a local server's error output, for the owner only. It is cleaned and redacted, and
//! it is never sent to the model, saved in a conversation or traced.

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex, PoisonError},
    time::Duration,
};
use tokio::io::{AsyncRead, AsyncReadExt};

/// Bytes kept, and the most text ever shown.
const TAIL_BYTES: usize = 2048;
const TAIL_LINES: usize = 40;
/// How long a failed server's last output is waited for.
const SETTLE: Duration = Duration::from_millis(250);

/// The last [`TAIL_BYTES`] a server wrote to its error output.
#[derive(Clone, Default)]
pub(crate) struct StderrTail(Arc<Mutex<VecDeque<u8>>>);

impl StderrTail {
    /// Read `stderr` until it closes, keeping only its last bytes.
    pub(crate) fn spawn_reader(stderr: impl AsyncRead + Unpin + Send + 'static) -> Self {
        let tail = Self::default();
        let buffer = Arc::clone(&tail.0);
        tokio::spawn(async move {
            let mut stderr = stderr;
            let mut chunk = [0u8; 1024];
            while let Ok(read @ 1..) = stderr.read(&mut chunk).await {
                let mut tail = buffer.lock().unwrap_or_else(PoisonError::into_inner);
                tail.extend(&chunk[..read]);
                let excess = tail.len().saturating_sub(TAIL_BYTES);
                tail.drain(..excess);
            }
        });
        tail
    }

    /// Give a stopped server's reader a moment to take what it wrote. The reader holds the other
    /// reference to the buffer until the stream closes.
    pub(crate) async fn settle(&self) {
        let deadline = tokio::time::Instant::now() + SETTLE;
        while Arc::strong_count(&self.0) > 1 && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    pub(crate) fn text(&self, secrets: &[String]) -> String {
        let bytes: Vec<u8> = self
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .copied()
            .collect();
        sanitize(&bytes, secrets)
    }
}

/// Plain text safe to show: escape sequences and control characters other than newline and tab
/// removed, secrets redacted, then the last 40 lines and at most 2,048 bytes.
pub(crate) fn sanitize(bytes: &[u8], secrets: &[String]) -> String {
    let text = strip(&String::from_utf8_lossy(bytes));
    // Redacting the whole text first leaves no part of a secret at the cut.
    let secrets: Vec<String> = secrets
        .iter()
        .filter(|secret| !secret.is_empty())
        .cloned()
        .collect();
    let text = super::redact(text, &secrets);
    let text = text.trim_end();
    let start = text
        .rmatch_indices('\n')
        .nth(TAIL_LINES - 1)
        .map_or(0, |(index, _)| index + 1);
    let mut text = &text[start..];
    if text.len() > TAIL_BYTES {
        let mut cut = text.len() - TAIL_BYTES;
        while !text.is_char_boundary(cut) {
            cut += 1;
        }
        text = &text[cut..];
    }
    text.to_owned()
}

/// Characters that draw nothing or reorder text (bidi overrides and isolates, zero-width marks,
/// the byte order mark, tag characters and the like: general category Cf plus the default-ignorable
/// code points), which could make a card look like something it is not.
fn is_invisible_format(c: char) -> bool {
    matches!(c,
        '\u{AD}' | '\u{34F}' | '\u{600}'..='\u{605}' | '\u{61C}' | '\u{6DD}' | '\u{70F}'
        | '\u{8E2}' | '\u{115F}' | '\u{1160}' | '\u{17B4}' | '\u{17B5}'
        | '\u{180B}'..='\u{180F}' | '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}'
        | '\u{2060}'..='\u{206F}' | '\u{3164}' | '\u{FE00}'..='\u{FE0F}' | '\u{FEFF}'
        | '\u{FFA0}' | '\u{FFF0}'..='\u{FFFB}' | '\u{110BD}' | '\u{110CD}'
        | '\u{1BCA0}'..='\u{1BCA3}' | '\u{1D173}'..='\u{1D17A}' | '\u{E0000}'..='\u{E0FFF}')
}

fn strip(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\u{1b}' => match chars.next() {
                // CSI: parameters, then one final byte in @ through ~.
                Some('[') => {
                    for c in chars.by_ref() {
                        if ('@'..='~').contains(&c) {
                            break;
                        }
                    }
                }
                // OSC: up to BEL or ESC \.
                Some(']') => {
                    while let Some(c) = chars.next() {
                        if c == '\u{7}' {
                            break;
                        }
                        if c == '\u{1b}' {
                            if chars.peek() == Some(&'\\') {
                                chars.next();
                            }
                            break;
                        }
                    }
                }
                // Any other escape takes the character after it.
                _ => {}
            },
            '\n' | '\t' => out.push(c),
            c if c.is_control() || is_invisible_format(c) => {}
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_strips_escapes_controls_caps_and_redacts() {
        let raw = format!("\x1b[31mboom\x1b[0m\x07 key=s3cret\n{}", "x".repeat(5000));
        let text = sanitize(raw.as_bytes(), &["s3cret".into()]);
        assert!(
            text.len() <= 2048
                && !text.contains('\x1b')
                && !text.contains('\x07')
                && !text.contains("s3cret")
        );
    }

    #[test]
    fn sanitize_keeps_text_and_drops_sequences_and_controls() {
        let raw = "\x1b]0;title\x07\x1b[1;31mred\x1b[0m\tok\r\n\u{85}\u{9b}line\x00two\n";
        assert_eq!(sanitize(raw.as_bytes(), &[]), "red\tok\nlinetwo");
        let raw = "a \x1b]8;;http://x\x1b\\link\x1b]8;;\x1b\\ b";
        assert_eq!(sanitize(raw.as_bytes(), &[]), "a link b");
    }

    #[test]
    fn sanitize_removes_invisible_formatting_characters() {
        let raw = "ok \u{202E}gnp.exe\u{202C} \u{2066}a\u{2069}\u{200B}b\u{200D}c\u{200F}\u{FEFF}d\u{AD}e\u{2060}f\u{E0041}g\u{FE0F}h";
        assert_eq!(sanitize(raw.as_bytes(), &[]), "ok gnp.exe abcdefgh");
        // Ordinary non-ASCII text stays.
        assert_eq!(
            sanitize("caf\u{e9} \u{4e2d}\u{6587}".as_bytes(), &[]),
            "caf\u{e9} \u{4e2d}\u{6587}"
        );
    }

    #[test]
    fn sanitize_keeps_the_last_forty_lines_and_redacts_across_the_cut() {
        let raw: String = (0..100).map(|n| format!("line {n}\n")).collect();
        let text = sanitize(raw.as_bytes(), &[]);
        assert_eq!(text.lines().count(), 40);
        assert!(text.starts_with("line 60") && text.ends_with("line 99"));
        // A grown replacement still fits.
        let raw = "k".repeat(2048);
        assert!(sanitize(raw.as_bytes(), &["k".into()]).len() <= 2048);
    }

    #[tokio::test]
    async fn the_tail_keeps_only_the_last_bytes_of_a_long_stream() {
        let raw = format!("{}end", "x".repeat(10_000));
        let tail = StderrTail::spawn_reader(std::io::Cursor::new(raw.into_bytes()));
        tail.settle().await;
        let text = tail.text(&[]);
        assert!(text.len() == 2048 && text.ends_with("xxend"));
    }

    #[test]
    fn sanitize_fuzz_is_panic_free_and_bounded() {
        let mut state = 0x9e37_79b9_7f4a_7c15_u64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let pieces: [&[u8]; 12] = [
            b"\x1b[",
            b"\x1b]",
            b"\x07",
            b"\x1b\\",
            b"\n",
            b"\xc2\x85",
            b"\xff\xfe",
            b"s3c",
            b"secret",
            b"\xe2\x82",
            "\u{202E}".as_bytes(),
            "\u{200B}".as_bytes(),
        ];
        let secrets = ["secret".to_owned(), "s3c".to_owned(), "\u{20ac}".to_owned()];
        for _ in 0..2000 {
            let mut input = vec![];
            for _ in 0..(next() % 400) {
                match next() % 3 {
                    0 => input.push(next() as u8),
                    _ => input.extend_from_slice(pieces[(next() % 12) as usize]),
                }
            }
            let text = sanitize(&input, &secrets);
            assert!(text.len() <= TAIL_BYTES && text.lines().count() <= TAIL_LINES);
            assert!(!text.contains('\x1b') && !text.contains("secret"));
            assert!(!text.chars().any(is_invisible_format));
        }
    }
}
