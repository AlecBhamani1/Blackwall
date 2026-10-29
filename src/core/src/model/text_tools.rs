//! Opt-in recognition of `<tool_call>{json}</tool_call>` blocks that a model wrote as text.
//!
//! Only blocks outside code fences count, only for offered tools, and only with object
//! arguments. Everything else stays text. Calls found here always ask for approval.
use super::{ToolCall, ToolFunction};
use serde_json::Value;

const OPEN: &str = "<tool_call>";
const CLOSE: &str = "</tool_call>";
/// The same limit as structured calls.
pub const MAX_TEXT_CALLS: usize = 8;

/// A Markdown fence line: three backticks or tildes after at most three spaces.
fn is_fence(line: &str) -> bool {
    let trimmed = line.trim_start_matches(' ');
    line.len() - trimmed.len() <= 3 && (trimmed.starts_with("```") || trimmed.starts_with("~~~"))
}

/// Finds the first `<tool_call>` outside a code fence as text arrives, one line at a time.
#[derive(Debug, Default)]
pub(super) struct TagScan {
    /// Where the first line not yet scanned starts.
    line: usize,
    fenced: bool,
    found: Option<usize>,
}

impl TagScan {
    /// How much of `content` can be shown now: everything before the first block, or before
    /// a trailing partial `<tool_call>` tag.
    pub fn shown(&mut self, content: &str) -> usize {
        if let Some(found) = self.found {
            return found;
        }
        while let Some(end) = content[self.line..].find('\n') {
            let line = &content[self.line..self.line + end];
            if let Some(at) = self.open_in(line) {
                let found = self.line + at;
                self.found = Some(found);
                return found;
            }
            if is_fence(line) {
                self.fenced = !self.fenced;
            }
            self.line += end + 1;
        }
        let rest = &content[self.line..];
        if let Some(at) = self.open_in(rest) {
            let found = self.line + at;
            self.found = Some(found);
            return found;
        }
        let partial = (1..OPEN.len())
            .rev()
            .find(|&n| rest.ends_with(&OPEN[..n]))
            .unwrap_or(0);
        content.len() - partial
    }

    fn open_in(&self, line: &str) -> Option<usize> {
        if self.fenced || is_fence(line) {
            None
        } else {
            line.find(OPEN)
        }
    }
}

/// Byte ranges of every complete `<tool_call>…</tool_call>` block outside code fences.
fn blocks(content: &str) -> Vec<(usize, usize)> {
    // Each line's start and whether a tag on it is fenced off.
    let mut lines = vec![];
    let (mut offset, mut fenced) = (0, false);
    for line in content.split_inclusive('\n') {
        let fence = is_fence(line);
        lines.push((offset, fenced || fence));
        if fence {
            fenced = !fenced;
        }
        offset += line.len();
    }
    let mut found = vec![];
    let mut from = 0;
    while let Some(at) = content[from..].find(OPEN) {
        let start = from + at;
        let line = lines.partition_point(|(begin, _)| *begin <= start);
        if line.checked_sub(1).is_some_and(|line| lines[line].1) {
            from = start + OPEN.len();
            continue;
        }
        let Some(close) = content[start + OPEN.len()..].find(CLOSE) else {
            break;
        };
        let end = start + OPEN.len() + close + CLOSE.len();
        found.push((start, end));
        from = end;
    }
    found
}

fn call(block: &str, offered: &[String], index: usize) -> Option<ToolCall> {
    let inner = block.strip_prefix(OPEN)?.strip_suffix(CLOSE)?.trim();
    let value: Value = serde_json::from_str(inner).ok()?;
    let name = value.get("name")?.as_str()?;
    if !offered.iter().any(|offered| offered == name) {
        return None;
    }
    let arguments = match value.get("arguments")? {
        Value::String(text) => serde_json::from_str::<Value>(text).ok()?,
        other => other.clone(),
    };
    if !arguments.is_object() {
        return None;
    }
    Some(ToolCall {
        id: format!("text_{index}"),
        function: ToolFunction {
            name: name.to_owned(),
            arguments: arguments.to_string(),
        },
    })
}

/// Tool calls written as `<tool_call>` blocks outside code fences, for offered tools only, and
/// the content left once those blocks are removed. At most [`MAX_TEXT_CALLS`] are taken; any
/// other block stays text. `None` when nothing matched.
pub fn parse_text_tool_calls(content: &str, offered: &[String]) -> Option<(Vec<ToolCall>, String)> {
    let mut calls = vec![];
    let mut remaining = String::new();
    let mut kept = 0;
    for (start, end) in blocks(content) {
        if calls.len() == MAX_TEXT_CALLS {
            break;
        }
        if let Some(call) = call(&content[start..end], offered, calls.len() + 1) {
            remaining.push_str(&content[kept..start]);
            kept = end;
            calls.push(call);
        }
    }
    if calls.is_empty() {
        return None;
    }
    remaining.push_str(&content[kept..]);
    Some((calls, remaining.trim_end().to_owned()))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn offered() -> Vec<String> {
        vec!["read_file".into(), "list_files".into()]
    }

    #[test]
    fn text_tool_call_hermes_format() {
        let content = "Let me look.\n<tool_call>\n{\"name\": \"read_file\", \"arguments\": {\"path\": \"a.txt\"}}\n</tool_call>";
        let (calls, remaining) = parse_text_tool_calls(content, &offered()).unwrap();
        assert_eq!(remaining, "Let me look.");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "text_1");
        assert_eq!(calls[0].function.name, "read_file");
        assert_eq!(calls[0].function.arguments, r#"{"path":"a.txt"}"#);
        // Arguments sent as a JSON string are accepted when they hold an object.
        let quoted =
            r#"<tool_call>{"name":"list_files","arguments":"{\"path\":\".\"}"}</tool_call>"#;
        assert_eq!(
            parse_text_tool_calls(quoted, &offered()).unwrap().0[0]
                .function
                .arguments,
            r#"{"path":"."}"#
        );
    }

    #[test]
    fn text_tool_call_for_unoffered_tool_stays_text() {
        for content in [
            r#"<tool_call>{"name":"shell","arguments":{"command":"rm -rf /"}}</tool_call>"#,
            r#"<tool_call>{"name":"read_file","arguments":["not","an","object"]}</tool_call>"#,
            r#"<tool_call>{"name":"read_file"}</tool_call>"#,
            r#"<tool_call>not json</tool_call>"#,
            r#"<tool_call>{"name":"read_file","arguments":{}}"#,
            r#"{"name":"read_file","arguments":{"path":"a"}}"#,
        ] {
            assert!(
                parse_text_tool_calls(content, &offered()).is_none(),
                "{content}"
            );
        }
        // A valid block is taken and an unoffered one beside it stays.
        let mixed = r#"<tool_call>{"name":"shell","arguments":{}}</tool_call> then <tool_call>{"name":"list_files","arguments":{}}</tool_call>"#;
        let (calls, remaining) = parse_text_tool_calls(mixed, &offered()).unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(
            remaining,
            r#"<tool_call>{"name":"shell","arguments":{}}</tool_call> then"#
        );
    }

    #[test]
    fn text_tool_call_in_fence_ignored() {
        let block = r#"<tool_call>{"name":"read_file","arguments":{"path":"a"}}</tool_call>"#;
        for content in [
            format!("Example:\n```\n{block}\n```\n"),
            format!("Example:\n  ~~~json\n{block}\n"),
            format!("```{block}\n```"),
        ] {
            assert!(
                parse_text_tool_calls(&content, &offered()).is_none(),
                "{content}"
            );
        }
        // After the fence closes, a block counts again.
        let after = format!("```\n{block}\n```\nNow:\n{block}");
        let (calls, remaining) = parse_text_tool_calls(&after, &offered()).unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(remaining, format!("```\n{block}\n```\nNow:"));
    }

    #[test]
    fn max_eight_text_tool_calls() {
        let block = r#"<tool_call>{"name":"list_files","arguments":{}}</tool_call>"#;
        let content = block.repeat(10);
        let (calls, remaining) = parse_text_tool_calls(&content, &offered()).unwrap();
        assert_eq!(calls.len(), MAX_TEXT_CALLS);
        assert_eq!(calls[7].id, "text_8");
        assert_eq!(remaining, block.repeat(2));
    }

    #[test]
    fn scan_holds_back_from_the_first_block_and_partial_tags() {
        let mut scan = TagScan::default();
        assert_eq!(scan.shown("Hello <tool_c"), 6);
        assert_eq!(scan.shown("Hello <tool_call>{"), 6);
        assert_eq!(scan.shown("Hello <tool_call>{...} more"), 6);
        let mut fenced = TagScan::default();
        let text = "```\n<tool_call>{}</tool_call>\n";
        assert_eq!(fenced.shown(text), text.len());
    }
}
