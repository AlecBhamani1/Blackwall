//! Separates `<think>…</think>` reasoning from streamed answer text.

const OPEN: &str = "<think>";
const CLOSE: &str = "</think>";

/// Splits streamed content into answer and reasoning. A tag split across chunks is held back
/// until it is complete, and reasoning left open at the end never becomes answer text.
#[derive(Debug, Default)]
pub struct ThinkFilter {
    inside: bool,
    pending: String,
    /// Drop the blank lines a model writes between `</think>` and its answer.
    after_close: bool,
}

impl ThinkFilter {
    /// Returns the answer text and the reasoning text that are now certain.
    pub fn push(&mut self, chunk: &str) -> (String, String) {
        self.pending.push_str(chunk);
        let (mut answer, mut reasoning) = (String::new(), String::new());
        loop {
            let tag = if self.inside { CLOSE } else { OPEN };
            let (end, found) = match self.pending.find(tag) {
                Some(at) => (at, true),
                None => (self.pending.len() - partial_tag(&self.pending, tag), false),
            };
            let text: String = self.pending.drain(..end).collect();
            self.emit(&text, &mut answer, &mut reasoning);
            if !found {
                return (answer, reasoning);
            }
            self.pending.drain(..tag.len());
            self.inside = !self.inside;
            self.after_close = !self.inside;
        }
    }

    /// Ends the stream. A partial tag is text; reasoning left open stays reasoning.
    pub fn finish(mut self) -> (String, String) {
        let (mut answer, mut reasoning) = (String::new(), String::new());
        let text = std::mem::take(&mut self.pending);
        self.emit(&text, &mut answer, &mut reasoning);
        (answer, reasoning)
    }

    fn emit(&mut self, text: &str, answer: &mut String, reasoning: &mut String) {
        if self.inside {
            reasoning.push_str(text);
            return;
        }
        let text = if self.after_close {
            text.trim_start()
        } else {
            text
        };
        if !text.is_empty() {
            self.after_close = false;
            answer.push_str(text);
        }
    }
}

/// The length of the longest end of `text` that could begin `tag`. Tags are ASCII, so the cut
/// is always on a character boundary.
fn partial_tag(text: &str, tag: &str) -> usize {
    (1..tag.len())
        .rev()
        .find(|&n| text.ends_with(&tag[..n]))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(chunks: &[&str]) -> (String, String) {
        let mut filter = ThinkFilter::default();
        let (mut answer, mut reasoning) = (String::new(), String::new());
        for chunk in chunks {
            let (a, r) = filter.push(chunk);
            answer.push_str(&a);
            reasoning.push_str(&r);
        }
        let (a, r) = filter.finish();
        (answer + &a, reasoning + &r)
    }

    #[test]
    fn think_tag_split_across_chunks() {
        let (answer, reasoning) = run(&["<th", "ink>plan 🦀 ", "steps</thi", "nk>\n\nThe answer."]);
        assert_eq!(answer, "The answer.");
        assert_eq!(reasoning, "plan 🦀 steps");
        // Every byte boundary, so no split of either tag leaks.
        let text = "Before <think>secret</think> after";
        for cut in 1..text.len() {
            let (answer, reasoning) = run(&[&text[..cut], &text[cut..]]);
            assert_eq!(answer, "Before after", "cut at {cut}");
            assert_eq!(reasoning, "secret");
        }
    }

    #[test]
    fn unclosed_think_is_dropped() {
        let (answer, reasoning) = run(&["Hello <think>never", " closed"]);
        assert_eq!(answer, "Hello ");
        assert_eq!(reasoning, "never closed");
        // A partial tag at the end is ordinary text.
        assert_eq!(run(&["a < b, <thi"]).0, "a < b, <thi");
    }

    #[test]
    fn text_without_tags_streams_unchanged() {
        let mut filter = ThinkFilter::default();
        assert_eq!(filter.push("plain"), ("plain".into(), String::new()));
        assert_eq!(filter.push(" text <"), (" text ".into(), String::new()));
        assert_eq!(filter.push("b>"), ("<b>".into(), String::new()));
    }
}
