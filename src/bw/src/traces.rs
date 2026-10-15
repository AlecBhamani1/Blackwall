//! Local agent-run traces: list recent runs and show one as a tree. These never call a model.
use blackwall_core::trace::{self, SpanView};
use std::path::Path;

const USAGE: &str = "Use bw traces [list] or bw traces show <id>.";

pub fn run(directory: &Path, args: &[String]) -> Result<(), String> {
    match args.first().map(String::as_str).unwrap_or("list") {
        "list" if args.len() <= 1 => {
            let traces = trace::list(directory, 20);
            if traces.is_empty() {
                println!(
                    "No traces yet. Agent runs record one when traces are on (bw config set traces on)."
                );
            }
            for summary in traces {
                println!(
                    "{}  {}  {} model call{}, {} tool call{}, {} in / {} out tokens{}{}",
                    summary.id,
                    seconds(summary.duration_ms),
                    summary.model_calls,
                    plural(summary.model_calls),
                    summary.tool_calls,
                    plural(summary.tool_calls),
                    summary.input_tokens,
                    summary.output_tokens,
                    summary
                        .model
                        .map(|model| format!("  {model}"))
                        .unwrap_or_default(),
                    summary
                        .error
                        .map(|error| format!("  failed: {error}"))
                        .unwrap_or_default(),
                );
            }
            Ok(())
        }
        "show" if args.len() == 2 => {
            let spans = trace::read(directory, &args[1])?;
            print!("{}", tree(&spans));
            Ok(())
        }
        _ => Err(USAGE.into()),
    }
}

fn plural(count: usize) -> &'static str {
    if count == 1 {
        ""
    } else {
        "s"
    }
}

fn seconds(ms: u64) -> String {
    format!("{}.{}s", ms / 1000, (ms % 1000) / 100)
}

/// One line per span, children indented under their parent in start order. Spans whose parent
/// is missing are listed at the top level, and each ID's children are listed once.
fn tree(spans: &[SpanView]) -> String {
    fn walk(
        spans: &[SpanView],
        parent: Option<&str>,
        depth: usize,
        placed: &mut [bool],
        expanded: &mut std::collections::HashSet<String>,
        out: &mut String,
    ) {
        let mut children: Vec<_> = (0..spans.len())
            .filter(|&index| !placed[index] && spans[index].parent.as_deref() == parent)
            .collect();
        children.sort_by_key(|&index| spans[index].start_ms);
        for index in children {
            if placed[index] {
                continue;
            }
            placed[index] = true;
            let span = &spans[index];
            let mut details = vec![seconds(span.duration_ms)];
            let number = |key: &str| span.attributes.get(key).and_then(|v| v.as_u64());
            if let (Some(input), Some(output)) = (
                number("gen_ai.usage.input_tokens"),
                number("gen_ai.usage.output_tokens"),
            ) {
                details.push(format!("{input} in / {output} out tokens"));
            }
            details.extend(span.events.iter().map(|event| event.replace('_', " ")));
            let status = match &span.error {
                Some(error) => format!("  ✗ {error}"),
                None => "  ✓".into(),
            };
            out.push_str(&format!(
                "{}{}  {}{status}\n",
                "  ".repeat(depth),
                span.name,
                details.join(", ")
            ));
            for key in [
                "gen_ai.tool.call.arguments",
                "gen_ai.tool.call.result",
                "gen_ai.output.messages",
            ] {
                if let Some(text) = span.attributes.get(key).and_then(|v| v.as_str()) {
                    out.push_str(&format!(
                        "{}  {}: {}\n",
                        "  ".repeat(depth),
                        key.rsplit('.').next().unwrap_or(key),
                        text.replace('\n', " ")
                    ));
                }
            }
            if expanded.insert(span.id.clone()) {
                walk(spans, Some(&span.id), depth + 1, placed, expanded, out);
            }
        }
    }
    let mut out = String::new();
    let mut placed = vec![false; spans.len()];
    let mut expanded = std::collections::HashSet::new();
    walk(spans, None, 0, &mut placed, &mut expanded, &mut out);
    let mut orphans: Vec<_> = (0..spans.len()).filter(|&index| !placed[index]).collect();
    orphans.sort_by_key(|&index| spans[index].start_ms);
    for index in orphans {
        if !placed[index] {
            walk(
                spans,
                spans[index].parent.as_deref(),
                0,
                &mut placed,
                &mut expanded,
                &mut out,
            );
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn span(id: &str, parent: Option<&str>, start_ms: u64) -> SpanView {
        SpanView {
            id: id.into(),
            parent: parent.map(Into::into),
            name: format!("span {id}"),
            start_ms,
            duration_ms: 0,
            error: None,
            attributes: Default::default(),
            events: vec![],
        }
    }

    #[test]
    fn tree_keeps_orphans_and_stops_on_cycles() {
        let lines: Vec<String> = tree(&[
            span("root", None, 0),
            span("b", Some("root"), 2),
            span("a", Some("root"), 1),
            span("orphan", Some("missing"), 3),
            span("x", Some("y"), 4),
            span("y", Some("x"), 5),
            span("root", Some("root"), 6),
        ])
        .lines()
        .map(|line| line.split("  0.0s").next().unwrap().to_owned())
        .collect();
        assert_eq!(
            lines,
            [
                "span root",
                "  span a",
                "  span b",
                "  span root",
                "span orphan",
                "span x",
                "  span y",
            ]
        );
    }
}
