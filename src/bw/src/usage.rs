//! `bw usage`: the Usage page's numbers as text or JSON, read from the local metrics files.
use blackwall_core::usage::{self, Day, GroupRow, UsageSummary};
use std::path::Path;

const USAGE: &str =
    "Use bw usage [--days N] [--utc] [--json], with N from 1 to 400 (such as 1, 7, 30, or 90).";

pub fn run(directory: &Path, args: &[String], telemetry_enabled: bool) -> Result<(), String> {
    let (days, utc, json) = parse(args)?;
    let offset = if utc { Some(0) } else { local_offset() };
    let offset_minutes = offset.unwrap_or(0);
    let (from, to) = Day::range(days, Day::today(offset_minutes));
    let mut summary = usage::summary(directory, from, to, offset_minutes);
    summary.telemetry_enabled = telemetry_enabled;
    if json {
        println!(
            "{}",
            serde_json::to_string(&summary).map_err(|error| error.to_string())?
        );
    } else {
        print!("{}", text(&summary, days, offset_minutes));
    }
    Ok(())
}

/// `--days N` (1–400, default 7), `--utc`, and `--json`.
fn parse(args: &[String]) -> Result<(u16, bool, bool), String> {
    let (mut days, mut utc, mut json) = (7, false, false);
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--json" => json = true,
            "--utc" => utc = true,
            "--days" => {
                days = args
                    .next()
                    .and_then(|value| value.parse::<u16>().ok())
                    .filter(|value| (1..=400).contains(value))
                    .ok_or(USAGE)?;
            }
            _ => return Err(USAGE.into()),
        }
    }
    Ok((days, utc, json))
}

/// This computer's UTC offset in minutes, from `date +%z` (`-0700`). `None` where that is not
/// available, such as Windows, and days are then counted in UTC.
fn local_offset() -> Option<i32> {
    if cfg!(windows) {
        return None;
    }
    let output = std::process::Command::new("date")
        .arg("+%z")
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    parse_offset(String::from_utf8_lossy(&output.stdout).trim())
}

/// `+0530` or `-0700` as minutes east of UTC.
fn parse_offset(text: &str) -> Option<i32> {
    let (sign, digits) = match text.as_bytes().first()? {
        b'+' => (1, &text[1..]),
        b'-' => (-1, &text[1..]),
        _ => return None,
    };
    if digits.len() != 4 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let hours: i32 = digits[..2].parse().ok()?;
    let minutes: i32 = digits[2..].parse().ok()?;
    (hours <= 14 && minutes < 60).then_some(sign * (hours * 60 + minutes))
}

fn zone(offset_minutes: i32) -> String {
    if offset_minutes == 0 {
        return "UTC".into();
    }
    let sign = if offset_minutes < 0 { '-' } else { '+' };
    let minutes = offset_minutes.unsigned_abs();
    format!("UTC{sign}{:02}:{:02}", minutes / 60, minutes % 60)
}

/// A compact count: 950, 12.4k, 4.82M.
/// A Unix time in milliseconds as local `YYYY-MM-DD HH:MM`.
fn moment(unix_ms: u64, offset_minutes: i32) -> String {
    let secs = (unix_ms / 1000) as i64 + i64::from(offset_minutes) * 60;
    let day = Day(u32::try_from(secs.div_euclid(86_400)).unwrap_or_default());
    let minutes = secs.rem_euclid(86_400) / 60;
    format!("{day} {:02}:{:02}", minutes / 60, minutes % 60)
}

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

pub fn text(summary: &UsageSummary, days: u16, offset_minutes: i32) -> String {
    let mut out = String::new();
    let span = if days == 1 {
        "today".to_owned()
    } else {
        format!("last {days} days")
    };
    out += &format!(
        "Usage, {span} ({} to {}, {})\n",
        summary.from,
        summary.to,
        zone(offset_minutes)
    );
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
            .map(|(outcome, count)| match outcome.as_str() {
                "error" => format!("{count} error{}", if *count == 1 { "" } else { "s" }),
                "iterations" => format!("{count} hit the step limit"),
                _ => format!("{count} {outcome}"),
            })
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
    for period in &summary.gaps.off_periods {
        let who = if period.surface == "cli" {
            "bw"
        } else {
            "desktop"
        };
        missing.push(match (period.from_unix_ms, period.to_unix_ms) {
            (Some(from), Some(to)) => format!(
                "{who} recording off {} to {}",
                moment(from, offset_minutes),
                moment(to, offset_minutes)
            ),
            (Some(from), None) => {
                format!("{who} recording off since {}", moment(from, offset_minutes))
            }
            (None, Some(to)) => format!("{who} recording off until {}", moment(to, offset_minutes)),
            (None, None) => format!("{who} recording off"),
        });
    }
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
    fn off_periods_print_in_local_time() {
        // 2026-10-28 17:30 UTC is 10:30 at UTC-7.
        let ms = 1_793_208_600_000;
        assert_eq!(moment(ms, -420), "2026-10-28 10:30");
        assert_eq!(moment(ms, 0), "2026-10-28 17:30");
        let mut summary = blackwall_core::usage::summary(
            std::path::Path::new("/nonexistent-blackwall-usage"),
            Day(20_000),
            Day(20_000),
            0,
        );
        summary.totals.total = 1;
        summary.gaps.off_periods = vec![blackwall_core::usage::OffPeriod {
            surface: "cli".into(),
            from_unix_ms: Some(ms),
            to_unix_ms: None,
        }];
        assert!(text(&summary, 1, 0).contains("bw recording off since 2026-10-28 17:30"));
    }

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
        let mut summary = usage::summary(&directory, Day(20_000), Day(20_006), 0);
        assert!(text(&summary, 7, 0).contains("No usage yet."));
        assert!(text(&summary, 7, -420).contains("2024-10-04 to 2024-10-10, UTC-07:00)"));
        summary.telemetry_enabled = false;
        assert!(text(&summary, 7, 0).contains("Telemetry is off"));
    }

    #[test]
    fn run_outcomes_use_plain_language_and_pluralize() {
        let mut summary = usage::summary(
            std::path::Path::new("/nonexistent-blackwall-usage"),
            Day(20_000),
            Day(20_000),
            0,
        );
        summary.totals.total = 1;
        summary.totals.run_success = Some(0.6);
        summary.totals.runs_by_outcome.insert("error".into(), 1);
        summary
            .totals
            .runs_by_outcome
            .insert("iterations".into(), 1);
        summary.totals.runs_by_outcome.insert("denied".into(), 2);
        assert!(text(&summary, 1, 0)
            .contains("Run success: 60.0% (2 denied, 1 error, 1 hit the step limit)"));
    }

    #[test]
    fn days_accept_any_count_from_1_to_400() {
        let args = |list: &[&str]| list.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>();
        assert_eq!(parse(&args(&[])).unwrap(), (7, false, false));
        assert_eq!(parse(&args(&["--days", "14"])).unwrap().0, 14);
        assert_eq!(parse(&args(&["--days", "1"])).unwrap().0, 1);
        assert_eq!(
            parse(&args(&["--days", "400", "--utc", "--json"])).unwrap(),
            (400, true, true)
        );
        for bad in [
            &["--days", "0"][..],
            &["--days", "401"],
            &["--days"],
            &["--days", "x"],
            &["7"],
        ] {
            assert!(parse(&args(bad)).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn offsets_parse_from_date_output() {
        assert_eq!(parse_offset("-0700"), Some(-420));
        assert_eq!(parse_offset("+0530"), Some(330));
        assert_eq!(parse_offset("+0000"), Some(0));
        for bad in ["", "0700", "-07:00", "+9999", "PDT"] {
            assert_eq!(parse_offset(bad), None, "{bad}");
        }
        assert_eq!(zone(330), "UTC+05:30");
        assert_eq!(zone(0), "UTC");
    }
}
