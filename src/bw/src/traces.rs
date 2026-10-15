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

/// One line per span, children indented under their parent in start order.
fn tree(spans: &[SpanView]) -> String {
    fn walk(spans: &[SpanView], parent: Option<&str>, depth: usize, out: &mut String) {
        let mut children: Vec<_> = spans
            .iter()
            .filter(|span| span.parent.as_deref() == parent)
            .collect();
        children.sort_by_key(|span| span.start_ms);
        for span in children {
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
            walk(spans, Some(&span.id), depth + 1, out);
        }
    }
    let mut out = String::new();
    walk(spans, None, 0, &mut out);
    out
}
