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

/// Record real metrics with the SDK, then file them under `day` in `directory`.
fn record(directory: &Path, day: Day, work: impl FnOnce(&Telemetry)) {
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
    for entry in std::fs::read_dir(scratch.join(METRICS_DIRECTORY)).unwrap() {
        let body = std::fs::read(entry.unwrap().path()).unwrap();
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(metrics.join(day.file_name()))
            .unwrap();
        std::io::Write::write_all(&mut file, &body).unwrap();
    }
    std::fs::remove_dir_all(scratch).unwrap();
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
    let summary = summary(&directory, Day(D), Day(D + 2));
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
    let one = super::summary(&directory, Day(D + 1), Day(D + 2));
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
    let summary = summary(&directory, Day(D), Day(D));
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
    let summary = summary(&directory, Day(D), Day(D));
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
    let summary = summary(&directory, Day(D - 3), Day(D + 3));
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
    let summary = summary(&directory, Day(D), Day(D + 6));
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
    let summary = summary(&directory, Day(D), Day(D));
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
    // A dropped-run point, as the trace writer records it: a one-run file, renamed.
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
    let summary = summary(&directory, Day(D), Day(D + 4));
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
        super::summary(&directory, Day(D + 2), Day(D + 4))
            .gaps
            .first_day,
        None
    );
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn usage_marks_stale_data() {
    let directory = folder();
    record(&directory, Day::today(), |t| {
        agent_call(t, "api", "s1", "m", (10, 1))
    });
    let now = SystemTime::now();
    let fresh = summary_at(&directory, Day::today(), Day::today(), now);
    assert!(fresh.newest_unix_ms.is_some());
    assert!(!fresh.stale);
    let later = summary_at(
        &directory,
        Day::today(),
        Day::today(),
        now + Duration::from_secs(STALE_AFTER_SECS + 5),
    );
    assert!(later.stale);
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
    let report = export(&directory, Day(D), Day(D + 1), &dest).unwrap();
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
        export(&directory, Day(D), Day(D + 1), &dest),
        Err(UsageError::Exists)
    ));
    assert!(matches!(
        export(&directory, Day(D), Day(D), &dest.join("missing")),
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
        let report = export(&directory, Day(D + 2), Day(D + 2), &dest).unwrap();
        assert_eq!(report.metrics_files, 0);
        std::fs::remove_dir_all(outside).unwrap();
    }
    std::fs::remove_dir_all(directory).unwrap();
    std::fs::remove_dir_all(dest).unwrap();
}
