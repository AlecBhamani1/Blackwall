#![allow(clippy::unwrap_used)]
use super::*;
use crate::{
    protocol::TokenUsage,
    telemetry::{Mode, RunOutcome, Telemetry, TelemetryOptions, ToolOutcome},
};

fn folder() -> PathBuf {
    let directory = std::env::temp_dir().join(format!("blackwall-usage-{}", rand::random::<u64>()));
    std::fs::create_dir_all(&directory).unwrap();
    directory
}

fn usage(input: u64, output: u64) -> TokenUsage {
    TokenUsage {
        prompt_tokens: input,
        completion_tokens: output,
        total_tokens: input + output,
        ..Default::default()
    }
}

/// Set every data point's timestamps to `nanos`.
fn retime(value: &mut serde_json::Value, nanos: u64) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, item) in map.iter_mut() {
                if key == "timeUnixNano" || key == "startTimeUnixNano" {
                    *item = serde_json::Value::String(nanos.to_string());
                } else {
                    retime(item, nanos);
                }
            }
        }
        serde_json::Value::Array(items) => items.iter_mut().for_each(|item| retime(item, nanos)),
        _ => {}
    }
}

/// Record real metrics with the SDK, as if at `unix_secs`, filed under that UTC day like the
/// exporter does.
fn record_at(directory: &Path, unix_secs: u64, work: impl FnOnce(&Telemetry)) {
    let scratch = folder();
    let telemetry = Telemetry::start(
        &scratch,
        TelemetryOptions {
            traces: true,
            ..Default::default()
        },
    );
    work(&telemetry);
    telemetry.shutdown();
    let metrics = directory.join(METRICS_DIRECTORY);
    std::fs::create_dir_all(&metrics).unwrap();
    let day = Day((unix_secs / 86_400) as u32);
    for entry in std::fs::read_dir(scratch.join(METRICS_DIRECTORY)).unwrap() {
        let body = std::fs::read_to_string(entry.unwrap().path()).unwrap();
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(metrics.join(day.file_name()))
            .unwrap();
        for line in body.lines() {
            let mut request: serde_json::Value = serde_json::from_str(line).unwrap();
            retime(&mut request, unix_secs * 1_000_000_000);
            std::io::Write::write_all(&mut file, format!("{request}\n").as_bytes()).unwrap();
        }
    }
    std::fs::remove_dir_all(scratch).unwrap();
}

/// [`record_at`] at noon UTC on `day`.
fn record(directory: &Path, day: Day, work: impl FnOnce(&Telemetry)) {
    record_at(directory, u64::from(day.0) * 86_400 + 43_200, work);
}

fn agent_call(
    telemetry: &Telemetry,
    project: &str,
    session: &str,
    model: &str,
    tokens: (u64, u64),
) {
    let scope = telemetry
        .run("r", Mode::Agent)
        .labelled(Some(Path::new(&format!("/work/{project}"))), Some(session));
    scope.record_model_call(
        model,
        "127.0.0.1:11434",
        Some(&usage(tokens.0, tokens.1)),
        1.0,
        None,
    );
    scope.finish(RunOutcome::Complete);
}

const D: u32 = 20_000;

#[test]
fn usage_sums_tokens_per_day_and_model() {
    let directory = folder();
    record(&directory, Day(D), |t| {
        agent_call(t, "api", "s1", "qwen", (100, 10));
        agent_call(t, "api", "s1", "llama", (50, 5));
    });
    record(&directory, Day(D + 2), |t| {
        agent_call(t, "api", "s2", "qwen", (200, 20));
    });
    let summary = summary(&directory, Day(D), Day(D + 2), 0);
    assert_eq!(summary.per_day.len(), 3);
    assert_eq!(summary.per_day[0].tokens.input, 150);
    assert_eq!(summary.per_day[0].tokens.output, 15);
    assert!(summary.per_day[0].recorded);
    assert!(!summary.per_day[1].recorded);
    assert_eq!(summary.per_day[1].tokens.total(), 0);
    assert_eq!(summary.per_day[2].tokens.input, 200);
    assert_eq!(summary.totals.tokens.total(), 385);
    assert_eq!(summary.totals.model_calls, 3);
    assert_eq!(summary.totals.runs, 3);
    assert_eq!(summary.totals.sessions, 2);
    let qwen = summary
        .per_model
        .iter()
        .find(|row| row.name.as_deref() == Some("qwen"))
        .unwrap();
    assert_eq!((qwen.tokens.input, qwen.tokens.output), (300, 30));
    assert_eq!(qwen.model_calls, 2);
    assert_eq!(qwen.sessions, 2);
    // Rows are sorted by tokens, largest first.
    assert_eq!(summary.per_model[0].name.as_deref(), Some("qwen"));
    assert_eq!(summary.from, "2024-10-04");
    // Days outside the range are not counted.
    let one = super::summary(&directory, Day(D + 1), Day(D + 2), 0);
    assert_eq!(one.totals.tokens.total(), 220);
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn usage_median_tokens_per_second() {
    let directory = folder();
    record(&directory, Day(D), |t| {
        let scope = t.run("r", Mode::Agent);
        // 10, 30, and 50 output tokens per second.
        for output in [10, 30, 50] {
            scope.record_model_call("m", "127.0.0.1:11434", Some(&usage(1, output)), 1.0, None);
        }
        scope.finish(RunOutcome::Complete);
    });
    let summary = summary(&directory, Day(D), Day(D), 0);
    let median = summary.totals.median_output_speed.unwrap();
    // Read from histogram buckets: the median lies in the 16-32 tok/s bucket.
    assert!((16.0..=32.0).contains(&median), "{median}");
    assert!(summary.per_model[0].median_output_speed.is_some());
    assert!(summary.per_model[0].p90_seconds.unwrap() > 0.0);
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn usage_success_rate_from_run_outcomes() {
    let directory = folder();
    record(&directory, Day(D), |t| {
        for outcome in [
            RunOutcome::Complete,
            RunOutcome::Complete,
            RunOutcome::Complete,
            RunOutcome::Error,
            RunOutcome::Iterations,
        ] {
            t.run("r", Mode::Agent).finish(outcome);
        }
        t.run("c", Mode::Chat).finish(RunOutcome::Complete);
        let scope = t.run("r", Mode::Agent);
        scope.record_tool("shell", ToolOutcome::Ok);
        scope.record_tool("shell", ToolOutcome::Denied);
        scope.record_tool("shell", ToolOutcome::Error);
        scope.record_tool("read_file", ToolOutcome::Ok);
        scope.finish(RunOutcome::Cancelled);
    });
    let summary = summary(&directory, Day(D), Day(D), 0);
    assert_eq!(summary.totals.runs, 7);
    assert_eq!(summary.totals.completed_runs, 4);
    assert_eq!(summary.totals.runs_by_outcome["iterations"], 1);
    let rate = summary.totals.run_success.unwrap();
    assert!((rate - 4.0 / 7.0).abs() < 1e-9, "{rate}");
    let agent = summary
        .per_mode
        .iter()
        .find(|row| row.mode == "agent")
        .unwrap();
    assert_eq!(agent.runs, 6);
    let shell = &summary.tools[0];
    assert_eq!(
        (shell.name.as_str(), shell.calls, shell.errors, shell.denied),
        ("shell", 3, 1, 1)
    );
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn usage_skips_corrupt_lines_and_missing_days() {
    let directory = folder();
    record(&directory, Day(D), |t| {
        agent_call(t, "api", "s1", "m", (40, 4))
    });
    let file = directory.join(METRICS_DIRECTORY).join(Day(D).file_name());
    let mut body = std::fs::read_to_string(&file).unwrap();
    // A corrupt line, then a line truncated by a crash.
    body.insert_str(0, "{not json\n");
    let truncated = body.lines().last().unwrap()[..20].to_owned();
    body.push_str(&truncated);
    std::fs::write(&file, body).unwrap();
    std::fs::write(
        directory
            .join(METRICS_DIRECTORY)
            .join(Day(D + 1).file_name()),
        [0xff, 0xfe, b'\n'],
    )
    .unwrap();
    let summary = summary(&directory, Day(D - 3), Day(D + 3), 0);
    assert_eq!(summary.totals.tokens.total(), 44);
    assert_eq!(summary.per_day.len(), 7);
    // One warning per affected file, naming the file, not every line.
    assert_eq!(summary.warnings.len(), 2, "{:?}", summary.warnings);
    assert!(summary.warnings[0].contains(&Day(D).file_name()));
    assert!(summary.warnings[0].contains("2 unreadable lines"));
    // Days before the first metrics file are history from before tracking started.
    assert_eq!(
        summary.gaps.first_day.as_deref(),
        Some(Day(D).to_string().as_str())
    );
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn usage_empty_folder_is_zero_not_error() {
    let directory = folder();
    let summary = summary(&directory, Day(D), Day(D + 6), 0);
    assert_eq!(summary.totals.tokens.total(), 0);
    assert_eq!(summary.totals.runs, 0);
    assert_eq!(summary.totals.run_success, None);
    assert_eq!(summary.per_day.len(), 7);
    assert!(summary.per_day.iter().all(|day| !day.recorded));
    assert!(summary.warnings.is_empty());
    assert_eq!(summary.files, 0);
    // With no data at all there is no gap to report.
    assert_eq!(summary.gaps.first_day, None);
    assert!(summary.telemetry_enabled);
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn usage_per_project_and_session_drilldown() {
    let directory = folder();
    record(&directory, Day(D), |t| {
        agent_call(t, "api", "s1", "qwen", (100, 10));
        agent_call(t, "api", "s1", "qwen", (100, 10));
        agent_call(t, "api", "s2", "llama", (30, 3));
        agent_call(t, "web", "s3", "qwen", (20, 2));
        let chat = t.run("c", Mode::Chat).labelled(None, Some("s4"));
        chat.record_model_call("qwen", "127.0.0.1:11434", Some(&usage(9, 1)), 1.0, None);
        chat.finish(RunOutcome::Complete);
    });
    let summary = summary(&directory, Day(D), Day(D), 0);
    let names: Vec<_> = summary
        .per_project
        .iter()
        .map(|row| row.name.as_deref())
        .collect();
    assert_eq!(names, [Some("api"), Some("web"), None]);
    let api = &summary.per_project[0];
    assert_eq!(api.tokens.total(), 253);
    assert_eq!((api.runs, api.sessions, api.model_calls), (Some(3), 2, 3));
    assert!((api.share - 253.0 / 285.0).abs() < 1e-9);
    assert_eq!(api.modes[0].mode, "agent");
    let s1 = &api.session_rows[0];
    assert_eq!((s1.id.as_str(), s1.model_calls, s1.total), ("s1", 2, 220));
    assert_eq!(s1.models, ["qwen"]);
    assert_eq!(api.session_rows[1].models, ["llama"]);
    // Model rows carry sessions too, for the Models view.
    let qwen = summary
        .per_model
        .iter()
        .find(|row| row.name.as_deref() == Some("qwen"))
        .unwrap();
    assert_eq!(qwen.sessions, 3);
    assert_eq!(qwen.runs, None);
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn usage_gaps_lists_off_periods_and_drops() {
    let directory = folder();
    record(&directory, Day(D + 2), |t| {
        agent_call(t, "api", "s1", "m", (10, 1));
        let scope = t.run("r", Mode::Agent).labelled(None, None);
        scope.record_compaction_saved(700);
        scope.finish(RunOutcome::Complete);
    });
    // A dropped-run point, as the trace writer records it: a one-run file, renamed. That the
    // writer really records this metric is pinned by
    // `telemetry::tests::a_dropped_run_is_counted_in_metrics`.
    record(&directory, Day(D + 3), |t| {
        t.run("r", Mode::Agent).finish(RunOutcome::Complete)
    });
    let file = directory
        .join(METRICS_DIRECTORY)
        .join(Day(D + 3).file_name());
    let body = std::fs::read_to_string(&file).unwrap().replace(
        crate::telemetry::names::RUN_OUTCOMES,
        crate::telemetry::names::DROPPED_RUNS,
    );
    std::fs::write(&file, body).unwrap();
    let summary = summary(&directory, Day(D), Day(D + 4), 0);
    assert_eq!(
        summary.gaps.first_day.as_deref(),
        Some(Day(D + 2).to_string().as_str())
    );
    assert_eq!(summary.gaps.dropped_runs, 1);
    assert_eq!(summary.totals.tokens_saved, 700);
    assert_eq!(summary.totals.compactions, 1);
    // Days with no file after tracking started are shown as unrecorded, not as zero usage.
    assert!(!summary.per_day[4].recorded);
    // A range that starts on or after the first day has no history gap.
    assert_eq!(
        super::summary(&directory, Day(D + 2), Day(D + 4), 0)
            .gaps
            .first_day,
        None
    );
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn usage_marks_stale_data() {
    let directory = folder();
    let now = SystemTime::now();
    let secs = now
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    record_at(&directory, secs, |t| {
        agent_call(t, "api", "s1", "m", (10, 1))
    });
    let today = Day::today(0);
    let fresh = summary_at(&directory, today, today, 0, now);
    assert!(fresh.newest_unix_ms.is_some());
    assert!(!fresh.stale);
    let later = summary_at(
        &directory,
        today,
        today,
        0,
        now + Duration::from_secs(STALE_AFTER_SECS + 5),
    );
    assert!(later.stale);
    std::fs::remove_dir_all(directory).unwrap();
}

/// Unix seconds for a civil UTC time.
fn utc(year: i64, month: u32, day: u32, hour: u64, minute: u64) -> u64 {
    let days = crate::telemetry::file_metrics::days_from_civil(year, month, day);
    days as u64 * 86_400 + hour * 3_600 + minute * 60
}

/// Pacific Daylight Time, 7 hours west of UTC.
const PDT: i32 = -7 * 60;

#[test]
fn today_is_the_whole_local_day_for_a_pacific_user_in_the_evening() {
    let directory = folder();
    // 2026-10-27 23:00 PDT: yesterday, local, though it is 2026-10-28 in UTC.
    record_at(&directory, utc(2026, 10, 28, 6, 0), |t| {
        agent_call(t, "api", "s1", "m", (1, 0))
    });
    // 2026-10-28 08:00 PDT, in the 2026-10-28 UTC file.
    record_at(&directory, utc(2026, 10, 28, 15, 0), |t| {
        agent_call(t, "api", "s1", "m", (10, 0))
    });
    // 2026-10-28 19:30 PDT, already in the 2026-10-29 UTC file.
    record_at(&directory, utc(2026, 10, 29, 2, 30), |t| {
        agent_call(t, "api", "s1", "m", (100, 0))
    });
    // At 20:00 PDT on 2026-10-28 it is 03:00 UTC the next day.
    let now = SystemTime::UNIX_EPOCH + Duration::from_secs(utc(2026, 10, 29, 3, 0));
    let today = Day::local(now, PDT);
    assert_eq!(today.to_string(), "2026-10-28");
    assert_eq!(Day::of(now).to_string(), "2026-10-29");
    let (from, to) = Day::range(1, today);
    let summary = summary_at(&directory, from, to, PDT, now);
    assert_eq!(summary.totals.tokens.input, 110);
    assert_eq!(summary.from, "2026-10-28");
    // In UTC the same instant's Today holds only the evening call.
    let utc_today = Day::of(now);
    let utc_summary = summary_at(&directory, utc_today, utc_today, 0, now);
    assert_eq!(utc_summary.totals.tokens.input, 100);
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn per_day_edges_sit_at_local_midnight() {
    let directory = folder();
    // 23:59 and 00:01 PDT, both in the same UTC file (06:59 and 07:01 UTC).
    record_at(&directory, utc(2026, 10, 28, 6, 59), |t| {
        agent_call(t, "api", "s1", "m", (5, 0))
    });
    record_at(&directory, utc(2026, 10, 28, 7, 1), |t| {
        agent_call(t, "api", "s1", "m", (7, 0))
    });
    let day = |text: &str| {
        Day(crate::telemetry::file_metrics::parse_name(&format!("{text}.jsonl")).unwrap() as u32)
    };
    let summary = summary(&directory, day("2026-10-27"), day("2026-10-28"), PDT);
    assert_eq!(summary.per_day[0].day, "2026-10-27");
    assert_eq!(summary.per_day[0].tokens.input, 5);
    assert_eq!(summary.per_day[1].tokens.input, 7);
    assert!(summary.per_day.iter().all(|day| day.recorded));
    // Offsets are clamped to real zones.
    assert_eq!(clamp_offset(-100_000), -720);
    assert_eq!(clamp_offset(100_000), 840);
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn mismatched_histogram_bounds_warn_once() {
    let directory = folder();
    record(&directory, Day(D), |t| {
        agent_call(t, "api", "s1", "m", (10, 10))
    });
    let file = directory.join(METRICS_DIRECTORY).join(Day(D).file_name());
    let line = std::fs::read_to_string(&file).unwrap();
    let mut request: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
    // A second copy of every histogram point, with different bounds.
    for metric in request["resourceMetrics"][0]["scopeMetrics"][0]["metrics"]
        .as_array_mut()
        .unwrap()
    {
        if let Some(points) = metric["histogram"]["dataPoints"].as_array_mut() {
            for point in points.iter_mut() {
                point["explicitBounds"] = serde_json::json!([1.0, 2.0]);
                point["bucketCounts"] = serde_json::json!(["0", "0", "1"]);
            }
        }
    }
    std::fs::write(&file, format!("{}{request}\n{request}\n", line)).unwrap();
    let summary = summary(&directory, Day(D), Day(D), 0);
    let warnings: Vec<_> = summary
        .warnings
        .iter()
        .filter(|warning| warning.contains("bucket bounds"))
        .collect();
    assert_eq!(warnings.len(), 1, "{:?}", summary.warnings);
    // Totals still count every point; only the quantiles leave the odd ones out.
    assert_eq!(summary.totals.tokens.input, 30);
    assert!(summary.totals.median_output_speed.is_some());
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn days_parse_and_format() {
    assert_eq!(Day(0).to_string(), "1970-01-01");
    assert_eq!(Day(D).file_name(), "2024-10-04.jsonl");
    assert_eq!(Day::range(7, Day(D)), (Day(D - 6), Day(D)));
    assert_eq!(Day::range(0, Day(D)), (Day(D), Day(D)));
}

#[test]
fn export_copies_only_range_files() {
    let directory = folder();
    let metrics = directory.join(METRICS_DIRECTORY);
    let traces = directory.join(crate::trace::TRACE_DIRECTORY);
    std::fs::create_dir_all(&metrics).unwrap();
    std::fs::create_dir_all(&traces).unwrap();
    for day in [D - 1, D, D + 1, D + 2] {
        std::fs::write(metrics.join(Day(day).file_name()), "{}\n").unwrap();
    }
    std::fs::write(metrics.join("notes.txt"), "x").unwrap();
    let ms = |day: u32| u64::from(day) * 86_400_000 + 1_000;
    for day in [D - 1, D, D + 1] {
        std::fs::write(traces.join(format!("{:013}-run.json", ms(day))), "{}").unwrap();
    }
    std::fs::write(traces.join("other.json"), "{}").unwrap();
    let dest = folder();
    let report = export(&directory, Day(D), Day(D + 1), 0, &dest).unwrap();
    assert_eq!((report.metrics_files, report.trace_files), (2, 2));
    assert!(report.folder.starts_with(&dest));
    let mut copied: Vec<_> = std::fs::read_dir(report.folder.join(METRICS_DIRECTORY))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    copied.sort();
    assert_eq!(copied, [Day(D).file_name(), Day(D + 1).file_name()]);
    assert_eq!(
        std::fs::read_dir(report.folder.join(crate::trace::TRACE_DIRECTORY))
            .unwrap()
            .count(),
        2
    );
    // Exporting the same range again never overwrites the earlier export.
    assert!(matches!(
        export(&directory, Day(D), Day(D + 1), 0, &dest),
        Err(UsageError::Exists)
    ));
    assert!(matches!(
        export(&directory, Day(D), Day(D), 0, &dest.join("missing")),
        Err(UsageError::Destination)
    ));
    #[cfg(unix)]
    {
        // A symlinked metrics file is not followed.
        let outside = folder();
        std::fs::write(outside.join("secret"), "x").unwrap();
        std::fs::remove_file(metrics.join(Day(D + 2).file_name())).unwrap();
        std::os::unix::fs::symlink(outside.join("secret"), metrics.join(Day(D + 2).file_name()))
            .unwrap();
        let report = export(&directory, Day(D + 2), Day(D + 2), 0, &dest).unwrap();
        assert_eq!(report.metrics_files, 0);
        std::fs::remove_dir_all(outside).unwrap();
    }
    std::fs::remove_dir_all(directory).unwrap();
    std::fs::remove_dir_all(dest).unwrap();
}

#[cfg(unix)]
#[test]
fn a_failed_export_removes_its_folder_so_a_retry_works() {
    use std::os::unix::fs::PermissionsExt;
    let directory = folder();
    let metrics = directory.join(METRICS_DIRECTORY);
    std::fs::create_dir_all(&metrics).unwrap();
    let file = metrics.join(Day(D).file_name());
    std::fs::write(&file, "{}\n").unwrap();
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o000)).unwrap();
    let dest = folder();
    let failed = export(&directory, Day(D), Day(D), 0, &dest);
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
    if std::fs::read(&file).is_ok() && failed.is_ok() {
        // Running as a user who can read any file (root): nothing to fail.
        std::fs::remove_dir_all(directory).unwrap();
        std::fs::remove_dir_all(dest).unwrap();
        return;
    }
    assert!(matches!(failed, Err(UsageError::Write)));
    assert_eq!(
        std::fs::read_dir(&dest).unwrap().count(),
        0,
        "no half-written folder"
    );
    let report = export(&directory, Day(D), Day(D), 0, &dest).unwrap();
    assert_eq!(report.metrics_files, 1);
    std::fs::remove_dir_all(directory).unwrap();
    std::fs::remove_dir_all(dest).unwrap();
}

#[test]
fn export_uses_local_days() {
    let directory = folder();
    let metrics = directory.join(METRICS_DIRECTORY);
    let traces = directory.join(crate::trace::TRACE_DIRECTORY);
    std::fs::create_dir_all(&metrics).unwrap();
    std::fs::create_dir_all(&traces).unwrap();
    for day in [D - 1, D, D + 1, D + 2] {
        std::fs::write(metrics.join(Day(day).file_name()), "{}\n").unwrap();
    }
    // Local day D at UTC-7 runs from 07:00 UTC on D to 07:00 UTC on D + 1.
    let ms = |day: u32, hour: u64| (u64::from(day) * 86_400 + hour * 3_600) * 1_000;
    for (day, hour) in [(D, 6), (D, 8), (D + 1, 6), (D + 1, 8)] {
        std::fs::write(
            traces.join(format!("{:013}-run{hour}.json", ms(day, hour))),
            "{}",
        )
        .unwrap();
    }
    let dest = folder();
    let report = export(&directory, Day(D), Day(D), PDT, &dest).unwrap();
    // The UTC files for D and D + 1 overlap local day D; only traces inside it are copied.
    assert_eq!((report.metrics_files, report.trace_files), (2, 2));
    std::fs::remove_dir_all(directory).unwrap();
    std::fs::remove_dir_all(dest).unwrap();
}

#[test]
fn recording_off_periods_listed_in_gaps() {
    use crate::telemetry::Surface::{Cli as CLI, Desktop as DESKTOP};
    let directory = folder();
    let day_ms = |day: u32, hour: u64| u64::from(day) * 86_400_000 + hour * 3_600_000;
    // Off on day D+1 at 10:00, back on at 15:00; off again on D+3 and still off. The repeated
    // "off" and an unreadable line change nothing.
    recording::mark_at(&directory, false, DESKTOP, day_ms(D + 1, 10));
    recording::mark_at(&directory, false, DESKTOP, day_ms(D + 1, 11));
    recording::mark_at(&directory, true, DESKTOP, day_ms(D + 1, 15));
    recording::mark_at(&directory, false, DESKTOP, day_ms(D + 3, 9));
    let markers = directory.join(METRICS_DIRECTORY).join(RECORDING_FILE);
    let mut body = std::fs::read_to_string(&markers).unwrap();
    body.push_str("not json\n");
    // A marker from an unknown surface is ignored.
    body.push_str(&format!(
        "{{\"unixMs\":{},\"recording\":false,\"surface\":\"<b>x</b>\"}}\n",
        day_ms(D + 2, 1)
    ));
    std::fs::write(&markers, body).unwrap();
    let summary = summary(&directory, Day(D), Day(D + 4), 0);
    assert_eq!(
        summary.gaps.off_periods,
        vec![
            OffPeriod {
                surface: "desktop".into(),
                from_unix_ms: Some(day_ms(D + 1, 10)),
                to_unix_ms: Some(day_ms(D + 1, 15)),
            },
            OffPeriod {
                surface: "desktop".into(),
                from_unix_ms: Some(day_ms(D + 3, 9)),
                to_unix_ms: None,
            },
        ]
    );
    // Only periods overlapping the range are listed; the marker file is not a metrics file.
    let late = super::summary(&directory, Day(D + 2), Day(D + 2), 0);
    assert!(late.gaps.off_periods.is_empty());
    assert_eq!(late.files, 0);
    // Local days: at UTC-7, D+1 ends at 07:00 UTC on D+2, after the first period.
    let pacific = super::summary(&directory, Day(D + 1), Day(D + 1), PDT);
    assert_eq!(pacific.gaps.off_periods.len(), 1);
    // A first "on" means recording was off before it, since only changes are marked.
    let other = folder();
    recording::mark_at(&other, true, CLI, day_ms(D + 1, 8));
    assert_eq!(
        super::summary(&other, Day(D), Day(D + 1), 0)
            .gaps
            .off_periods,
        vec![OffPeriod {
            surface: "cli".into(),
            from_unix_ms: None,
            to_unix_ms: Some(day_ms(D + 1, 8)),
        }]
    );
    // Each surface has its own switch: bw turning on does not end the desktop's off period.
    recording::mark_at(&directory, true, CLI, day_ms(D + 3, 12));
    let both = super::summary(&directory, Day(D + 3), Day(D + 3), 0);
    assert_eq!(both.gaps.off_periods.len(), 2);
    assert_eq!(both.gaps.off_periods[1].to_unix_ms, None);
    // Only the first 1 MiB is read: markers past it are ignored.
    let big = folder();
    recording::mark_at(&big, false, DESKTOP, day_ms(D + 1, 1));
    let path = big.join(METRICS_DIRECTORY).join(RECORDING_FILE);
    let mut text = std::fs::read_to_string(&path).unwrap();
    text.push_str(&format!("{}\n", " ".repeat(1024 * 1024)));
    text.push_str(&format!(
        "{{\"unixMs\":{},\"recording\":true,\"surface\":\"desktop\"}}\n",
        day_ms(D + 1, 2)
    ));
    std::fs::write(&path, text).unwrap();
    let capped = super::summary(&big, Day(D), Day(D + 1), 0);
    assert_eq!(capped.gaps.off_periods.len(), 1);
    assert_eq!(capped.gaps.off_periods[0].to_unix_ms, None);
    std::fs::remove_dir_all(big).unwrap();
    let json = serde_json::to_value(&summary.gaps).unwrap();
    assert_eq!(json["offPeriods"][1]["toUnixMs"], serde_json::Value::Null);
    std::fs::remove_dir_all(directory).unwrap();
    std::fs::remove_dir_all(other).unwrap();
}
