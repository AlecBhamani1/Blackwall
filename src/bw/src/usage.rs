//! `bw usage`: the Usage page's numbers as text or JSON, read from the local metrics files.
use blackwall_core::usage::{self, Day, GroupRow, UsageSummary};
use std::path::Path;

const USAGE: &str = "Use bw usage [--days 1|7|30|90] [--json].";

pub fn run(directory: &Path, args: &[String], telemetry_enabled: bool) -> Result<(), String> {
    let mut days = 7;
    let mut json = false;
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--json" => json = true,
            "--days" => {
                days = match args.next().map(String::as_str) {
                    Some("1") => 1,
                    Some("7") => 7,
                    Some("30") => 30,
                    Some("90") => 90,
                    _ => return Err(USAGE.into()),
                }
            }
            _ => return Err(USAGE.into()),
        }
    }
    let (from, to) = Day::range(days, Day::today());
    let mut summary = usage::summary(directory, from, to);
    summary.telemetry_enabled = telemetry_enabled;
    if json {
        println!(
            "{}",
            serde_json::to_string(&summary).map_err(|error| error.to_string())?
        );
    } else {
        print!("{}", text(&summary, days));
    }
    Ok(())
}

/// A compact count: 950, 12.4k, 4.82M.
pub fn compact(value: u64) -> String {
    match value {
        0..=999 => value.to_string(),
        1_000..=999_999 => trim(value as f64 / 1e3, "k"),
        _ => trim(value as f64 / 1e6, "M"),
    }
}

fn trim(value: f64, unit: &str) -> String {
    let digits = if value >= 100.0 {
        0
    } else if value >= 10.0 {
        1
    } else {
        2
    };
    let text = format!("{value:.digits$}");
    let text = if text.contains('.') {
        text.trim_end_matches('0').trim_end_matches('.')
    } else {
        &text
    };
    format!("{text}{unit}")
}

fn percent(share: f64) -> String {
    format!("{:.1}%", share * 100.0)
}

fn count(value: u64, word: &str) -> String {
    format!("{value} {word}{}", if value == 1 { "" } else { "s" })
}

fn group_line(row: &GroupRow, fallback: &str) -> String {
    let name = row.name.as_deref().unwrap_or(fallback);
    let runs = row
        .runs
        .map(|runs| format!(", {}", count(runs, "run")))
        .unwrap_or_default();
    format!(
        "  {name:<24} {:>8}  {:>6}{runs}, {}, {}\n",
        compact(row.total),
        percent(row.share),
        count(row.sessions, "session"),
        count(row.model_calls, "call")
    )
}

pub fn text(summary: &UsageSummary, days: u16) -> String {
    let mut out = String::new();
    let span = if days == 1 {
        "today".to_owned()
    } else {
        format!("last {days} days")
    };
    out += &format!("Usage, {span} ({} to {}, UTC)\n", summary.from, summary.to);
    if !summary.telemetry_enabled {
        out += "Telemetry is off, so nothing new is recorded. Turn it on with bw config set traces on.\n";
    }
    let totals = &summary.totals;
    if totals.total == 0 && totals.runs == 0 && totals.model_calls == 0 {
        out += "No usage yet.\n";
        for warning in &summary.warnings {
            out += &format!("Warning: {warning}\n");
        }
        return out;
    }
    let tokens = &totals.tokens;
    out += &format!(
        "Tokens recorded: {} ({} in, {} cached, {} out, {} reasoning)\n",
        compact(totals.total),
        compact(tokens.input),
        compact(tokens.cached_input),
        compact(tokens.output),
        compact(tokens.reasoning)
    );
    out += &format!(
        "Runs: {} · sessions: {} · model calls: {}\n",
        totals.runs, totals.sessions, totals.model_calls
    );
    if let Some(rate) = totals.run_success {
        let others: Vec<_> = totals
            .runs_by_outcome
            .iter()
            .filter(|(outcome, _)| *outcome != "complete")
            .map(|(outcome, count)| format!("{count} {outcome}"))
            .collect();
        let detail = if others.is_empty() {
            String::new()
        } else {
            format!(" ({})", others.join(", "))
        };
        out += &format!("Run success: {}{detail}\n", percent(rate));
    }
    if let Some(speed) = totals.median_output_speed {
        let model = totals
            .top_model
            .as_deref()
            .map(|model| format!(", busiest model {model}"))
            .unwrap_or_default();
        out += &format!("Output speed: {speed:.0} tok/s median{model}\n");
    }
    out += &format!(
        "Tokens saved: {} by {} compaction{}\n",
        compact(totals.tokens_saved),
        totals.compactions,
        if totals.compactions == 1 { "" } else { "s" }
    );
    let mut missing = vec![];
    if let Some(first) = &summary.gaps.first_day {
        missing.push(format!(
            "nothing before {first}, when usage tracking started"
        ));
    }
    if summary.gaps.dropped_runs > 0 {
        missing.push(format!(
            "{} run trace{} dropped (writer queue full)",
            summary.gaps.dropped_runs,
            if summary.gaps.dropped_runs == 1 {
                ""
            } else {
                "s"
            }
        ));
    }
    if !missing.is_empty() {
        out += &format!("Missing from this picture: {}\n", missing.join(" · "));
    }
    out += "\nProjects:\n";
    for row in &summary.per_project {
        out += &group_line(row, "(no project)");
    }
    out += "\nModels:\n";
    for row in &summary.per_model {
        out += &group_line(row, "unknown");
    }
    out += "\nModes:\n";
    for row in &summary.per_mode {
        out += &format!(
            "  {:<24} {:>8}  {}, {}\n",
            row.mode,
            compact(row.total),
            count(row.runs, "run"),
            count(row.model_calls, "call")
        );
    }
    if !summary.tools.is_empty() {
        out += "\nTop tools:\n";
        for tool in &summary.tools {
            let rate = |count: u64| percent(count as f64 / tool.calls.max(1) as f64);
            out += &format!(
                "  {:<24} {:>6} calls  {} errors  {} denied\n",
                tool.name,
                tool.calls,
                rate(tool.errors),
                rate(tool.denied)
            );
        }
    }
    for warning in &summary.warnings {
        out += &format!("Warning: {warning}\n");
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn compact_numbers() {
        assert_eq!(compact(950), "950");
        assert_eq!(compact(12_400), "12.4k");
        assert_eq!(compact(612_000), "612k");
        assert_eq!(compact(4_820_000), "4.82M");
        assert_eq!(compact(1_000_000), "1M");
    }

    #[test]
    fn empty_range_says_no_usage_yet() {
        let directory =
            std::env::temp_dir().join(format!("bw-usage-missing-{}", std::process::id()));
        let mut summary = usage::summary(&directory, Day(20_000), Day(20_006));
        assert!(text(&summary, 7).contains("No usage yet."));
        summary.telemetry_enabled = false;
        assert!(text(&summary, 7).contains("Telemetry is off"));
    }
}
