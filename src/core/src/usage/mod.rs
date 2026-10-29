//! The Usage page and `bw usage`: totals aggregated from the local metrics files
//! (`metrics/YYYY-MM-DD.jsonl`, OTLP/JSON lines written by [`crate::telemetry`]).
//!
//! Everything here reads files the owner already has; nothing is sent anywhere. Unreadable lines,
//! such as one truncated by a crash, are skipped with one warning per file, so a damaged file
//! never hides the rest of the range.
mod recording;

pub use recording::{mark_recording, OffPeriod, RECORDING_FILE};

use crate::telemetry::{
    file_metrics::{civil_from_days, day_of, file_name, parse_name},
    names, METRICS_DIRECTORY,
};
use opentelemetry_proto::tonic::{
    collector::metrics::v1::ExportMetricsServiceRequest,
    common::v1::{any_value, AnyValue, KeyValue},
    metrics::v1::{metric, number_data_point, HistogramDataPoint, NumberDataPoint},
};
use serde::Serialize;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

/// The Usage page refreshes every 2 minutes; data older than two refreshes is stale.
pub const REFRESH_SECS: u64 = 120;
pub const STALE_AFTER_SECS: u64 = 2 * REFRESH_SECS;

/// A day number: days since the Unix epoch. Metrics files are named by UTC day; the Usage page
/// counts local days, the UTC day numbering shifted by the viewer's UTC offset.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct Day(pub u32);

/// UTC offsets run from -12:00 to +14:00.
pub fn clamp_offset(minutes: i32) -> i32 {
    minutes.clamp(-12 * 60, 14 * 60)
}

/// The local day number of a Unix time in seconds, at `offset_minutes` east of UTC.
fn local_day(unix_secs: i64, offset_minutes: i32) -> i64 {
    (unix_secs + i64::from(clamp_offset(offset_minutes)) * 60).div_euclid(86_400)
}

impl Day {
    /// Today in the zone `offset_minutes` east of UTC.
    pub fn today(offset_minutes: i32) -> Self {
        Self::local(SystemTime::now(), offset_minutes)
    }

    /// The UTC day of a time.
    pub fn of(time: SystemTime) -> Self {
        Self(u32::try_from(day_of(time)).unwrap_or_default())
    }

    /// The local day of a time, `offset_minutes` east of UTC.
    pub fn local(time: SystemTime, offset_minutes: i32) -> Self {
        let secs = time
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_secs() as i64);
        Self(u32::try_from(local_day(secs, offset_minutes)).unwrap_or_default())
    }

    /// The `days` days ending with `to`, at least one.
    pub fn range(days: u16, to: Self) -> (Self, Self) {
        let back = u32::from(days.max(1)) - 1;
        (Self(to.0.saturating_sub(back)), to)
    }

    /// The metrics file name for this day.
    pub fn file_name(self) -> String {
        file_name(i64::from(self.0))
    }
}

impl std::fmt::Display for Day {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (year, month, day) = civil_from_days(i64::from(self.0));
        write!(f, "{year:04}-{month:02}-{day:02}")
    }
}

/// Token counts. `cached_input` is part of `input` and `reasoning` part of `output`, and both
/// are known only when the server reports them.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Tokens {
    pub input: u64,
    pub cached_input: u64,
    pub output: u64,
    pub reasoning: u64,
}

impl Tokens {
    pub fn total(&self) -> u64 {
        self.input + self.output
    }

    fn add(&mut self, kind: &str, count: u64) {
        match kind {
            "input" => self.input += count,
            "cached_input" => self.cached_input += count,
            "output" => self.output += count,
            "reasoning" => self.reasoning += count,
            _ => {}
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Totals {
    pub tokens: Tokens,
    pub total: u64,
    pub model_calls: u64,
    pub failed_calls: u64,
    pub runs: u64,
    pub completed_runs: u64,
    pub runs_by_outcome: std::collections::BTreeMap<String, u64>,
    /// Completed runs as a share of all runs; `None` with no runs.
    pub run_success: Option<f64>,
    pub sessions: u64,
    /// Median output tokens per second, estimated from histogram buckets.
    pub median_output_speed: Option<f64>,
    /// The model with the most tokens in the range.
    pub top_model: Option<String>,
    /// Estimated context tokens removed by compaction.
    pub tokens_saved: u64,
    pub compactions: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DayTokens {
    pub day: String,
    pub tokens: Tokens,
    /// Whether a metrics file exists for this day.
    pub recorded: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModeShare {
    pub mode: String,
    pub total: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionRow {
    /// The neutral conversation ID; never a title or prompt.
    pub id: String,
    pub models: Vec<String>,
    pub model_calls: u64,
    pub total: u64,
}

/// One project or model in "Where it went".
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GroupRow {
    /// The project's folder name or the model; `None` is Chat with no project.
    pub name: Option<String>,
    pub tokens: Tokens,
    pub total: u64,
    /// Share of the range's tokens, 0–1.
    pub share: f64,
    /// Runs, for projects only: a run can use more than one model.
    pub runs: Option<u64>,
    pub sessions: u64,
    pub model_calls: u64,
    pub failed_calls: u64,
    pub median_output_speed: Option<f64>,
    pub p90_seconds: Option<f64>,
    pub modes: Vec<ModeShare>,
    /// The busiest sessions, at most [`MAX_SESSION_ROWS`].
    pub session_rows: Vec<SessionRow>,
}

pub const MAX_SESSION_ROWS: usize = 20;
pub const MAX_TOOL_ROWS: usize = 10;
/// One OTLP/JSON request line. Corrupt local files must not cause an unbounded allocation.
const MAX_METRICS_LINE: usize = 4 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModeRow {
    pub mode: String,
    pub total: u64,
    pub model_calls: u64,
    pub runs: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolRow {
    pub name: String,
    pub calls: u64,
    pub errors: u64,
    pub denied: u64,
    pub timeouts: u64,
}

/// What the range cannot show.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Gaps {
    /// The first day with any metrics, when the range starts before it.
    pub first_day: Option<String>,
    /// Runs whose trace was dropped by a full writer queue.
    pub dropped_runs: u64,
    /// Periods in the range with recording turned off, from `metrics/recording.jsonl`. Changes
    /// made before recording markers existed are not known.
    pub off_periods: Vec<OffPeriod>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageSummary {
    pub from: String,
    pub to: String,
    pub totals: Totals,
    pub per_day: Vec<DayTokens>,
    pub per_project: Vec<GroupRow>,
    pub per_model: Vec<GroupRow>,
    pub per_mode: Vec<ModeRow>,
    pub tools: Vec<ToolRow>,
    pub gaps: Gaps,
    pub warnings: Vec<String>,
    /// Set by the adapter from the owner's settings.
    pub telemetry_enabled: bool,
    /// Metrics files read for the range.
    pub files: u32,
    /// When the newest data point in the range was recorded.
    pub newest_unix_ms: Option<u64>,
    /// The newest data point is older than [`STALE_AFTER_SECS`].
    pub stale: bool,
}

/// Aggregate usage for the local days `from` to `to`, inclusive, in the zone `offset_minutes`
/// east of UTC. Blocking.
pub fn summary(data_dir: &Path, from: Day, to: Day, offset_minutes: i32) -> UsageSummary {
    summary_at(data_dir, from, to, offset_minutes, SystemTime::now())
}

/// [`summary`] as of `now`, for the stale check.
///
/// Files are named by the UTC day they were written, so a local day can span two of them; files
/// from the day before `from` to the day after `to` are read, and each point is counted on the
/// local day of its timestamp.
pub fn summary_at(
    data_dir: &Path,
    from: Day,
    to: Day,
    offset_minutes: i32,
    now: SystemTime,
) -> UsageSummary {
    let to = to.max(from);
    let mut usage = Aggregate::new(from, to, offset_minutes);
    let mut warnings = vec![];
    let files = metric_files(data_dir);
    let window = Day(from.0.saturating_sub(1))..=Day(to.0.saturating_add(1));
    // Data before the files read can exist only if an older file does.
    let older = files.first().is_some_and(|(day, _)| day < window.start());
    let mut read = 0;
    for (day, path) in files.iter().filter(|(day, _)| window.contains(day)) {
        read += 1;
        let name = day.file_name();
        let Ok(file) = std::fs::File::open(path) else {
            warnings.push(format!("{name} could not be read."));
            continue;
        };
        let bad = read_lines(std::io::BufReader::new(file), |request| {
            usage.add_request(request);
        });
        if bad > 0 {
            let plural = if bad == 1 { "" } else { "s" };
            warnings.push(format!("{name}: {bad} unreadable line{plural} skipped."));
        }
    }
    let newest_unix_ms = (usage.newest_nanos > 0).then_some(usage.newest_nanos / 1_000_000);
    let stale = newest_unix_ms.is_some_and(|newest| {
        let age = now
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .saturating_sub(Duration::from_millis(newest));
        age > Duration::from_secs(STALE_AFTER_SECS)
    });
    let first = (!older).then_some(usage.earliest).flatten();
    if usage.mismatched_bounds {
        warnings.push(
            "Some histogram points used different bucket bounds and were left out of medians and \
             p90s."
                .into(),
        );
    }
    let mut summary = usage.finish();
    summary.gaps.first_day = first
        .filter(|first| *first > from)
        .map(|day| day.to_string());
    let offset_ms = i64::from(clamp_offset(offset_minutes)) * 60_000;
    let edge = |day: u32| (i64::from(day) * 86_400_000 - offset_ms).max(0) as u64;
    summary.gaps.off_periods =
        recording::off_periods(data_dir, edge(from.0), edge(to.0.saturating_add(1)));
    summary.warnings = warnings;
    summary.files = read;
    summary.newest_unix_ms = newest_unix_ms;
    summary.stale = stale;
    summary
}

/// Every metrics file by day, oldest first. A symlinked folder or file is not followed.
fn metric_files(data_dir: &Path) -> Vec<(Day, PathBuf)> {
    let directory = data_dir.join(METRICS_DIRECTORY);
    if std::fs::symlink_metadata(&directory).map_or(true, |meta| !meta.is_dir()) {
        return vec![];
    }
    let Ok(entries) = std::fs::read_dir(&directory) else {
        return vec![];
    };
    let mut files: Vec<_> = entries
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
        .filter_map(|entry| {
            let day = parse_name(&entry.file_name().to_string_lossy())?;
            Some((Day(u32::try_from(day).ok()?), entry.path()))
        })
        .collect();
    files.sort();
    files
}

/// Call `each` for every parseable line; returns how many lines could not be read. A line over
/// [`MAX_METRICS_LINE`] is counted as bad and skipped without being held in memory.
fn read_lines(
    mut reader: impl std::io::BufRead,
    mut each: impl FnMut(ExportMetricsServiceRequest),
) -> usize {
    use std::io::{BufRead, Read};
    let mut bad = 0;
    let mut line = vec![];
    loop {
        line.clear();
        match (&mut reader)
            .take(MAX_METRICS_LINE as u64 + 1)
            .read_until(b'\n', &mut line)
        {
            Ok(0) => return bad,
            Ok(_) => {}
            Err(_) => return bad + 1,
        }
        if line.len() > MAX_METRICS_LINE {
            bad += 1;
            if !line.ends_with(b"\n") && skip_line(&mut reader).is_err() {
                return bad;
            }
            continue;
        }
        if line.trim_ascii().is_empty() {
            continue;
        }
        match serde_json::from_slice::<ExportMetricsServiceRequest>(&line) {
            Ok(request) => each(request),
            Err(_) => bad += 1,
        }
    }
}

/// Discard the rest of the current line.
fn skip_line(reader: &mut impl std::io::BufRead) -> std::io::Result<()> {
    loop {
        let buffer = reader.fill_buf()?;
        if buffer.is_empty() {
            return Ok(());
        }
        match buffer.iter().position(|byte| *byte == b'\n') {
            Some(at) => {
                reader.consume(at + 1);
                return Ok(());
            }
            None => {
                let read = buffer.len();
                reader.consume(read);
            }
        }
    }
}

/// Histogram buckets merged across data points with the same bounds.
#[derive(Default)]
struct Buckets {
    bounds: Vec<f64>,
    counts: Vec<u64>,
}

impl Buckets {
    /// Merge a point's buckets. Returns false when its bounds differ from the ones already
    /// merged, or its counts do not match its bounds; such a point is left out.
    fn add(&mut self, point: &HistogramDataPoint) -> bool {
        if point.bucket_counts.len() != point.explicit_bounds.len() + 1 {
            return point.bucket_counts.is_empty();
        }
        if self.counts.is_empty() {
            self.bounds.clone_from(&point.explicit_bounds);
            self.counts.clone_from(&point.bucket_counts);
        } else if self.bounds == point.explicit_bounds {
            for (total, count) in self.counts.iter_mut().zip(&point.bucket_counts) {
                *total += count;
            }
        } else {
            return false;
        }
        true
    }

    /// The `q` quantile, interpolated inside its bucket as Prometheus does. The open last
    /// bucket reports its lower bound.
    fn quantile(&self, q: f64) -> Option<f64> {
        let total: u64 = self.counts.iter().sum();
        if total == 0 {
            return None;
        }
        let rank = q * total as f64;
        let mut seen = 0.0;
        for (index, &count) in self.counts.iter().enumerate() {
            let count = count as f64;
            if count > 0.0 && seen + count >= rank {
                let lower = if index == 0 {
                    0.0
                } else {
                    self.bounds[index - 1]
                };
                let Some(&upper) = self.bounds.get(index) else {
                    return Some(lower);
                };
                return Some(lower + (upper - lower) * ((rank - seen) / count));
            }
            seen += count;
        }
        self.bounds.last().copied()
    }
}

#[derive(Default)]
struct SessionTally {
    models: std::collections::BTreeSet<String>,
    calls: u64,
    total: u64,
}

#[derive(Default)]
struct Group {
    tokens: Tokens,
    runs: u64,
    calls: u64,
    failed: u64,
    sessions: BTreeMap<String, SessionTally>,
    modes: BTreeMap<String, u64>,
    speed: Buckets,
    duration: Buckets,
}

impl Group {
    fn session(&mut self, session: Option<&str>) -> Option<&mut SessionTally> {
        Some(self.sessions.entry(session?.to_owned()).or_default())
    }

    fn row(self, name: Option<String>, all: u64, runs: bool) -> GroupRow {
        let total = self.tokens.total();
        let mut modes: Vec<_> = self
            .modes
            .into_iter()
            .map(|(mode, total)| ModeShare { mode, total })
            .collect();
        modes.sort_by(|a, b| b.total.cmp(&a.total).then_with(|| a.mode.cmp(&b.mode)));
        let sessions = self.sessions.len() as u64;
        let mut session_rows: Vec<_> = self
            .sessions
            .into_iter()
            .map(|(id, tally)| SessionRow {
                id,
                models: tally.models.into_iter().collect(),
                model_calls: tally.calls,
                total: tally.total,
            })
            .collect();
        session_rows.sort_by(|a, b| b.total.cmp(&a.total).then_with(|| a.id.cmp(&b.id)));
        session_rows.truncate(MAX_SESSION_ROWS);
        GroupRow {
            name,
            tokens: self.tokens,
            total,
            share: if all == 0 {
                0.0
            } else {
                total as f64 / all as f64
            },
            runs: runs.then_some(self.runs),
            sessions,
            model_calls: self.calls,
            failed_calls: self.failed,
            median_output_speed: self.speed.quantile(0.5),
            p90_seconds: self.duration.quantile(0.9),
            modes,
            session_rows,
        }
    }
}

#[derive(Default)]
struct ModeTally {
    total: u64,
    calls: u64,
    runs: u64,
}

struct Aggregate {
    from: Day,
    offset_minutes: i32,
    /// The local day of the earliest point seen in any file read.
    earliest: Option<Day>,
    mismatched_bounds: bool,
    per_day: Vec<Tokens>,
    recorded: Vec<bool>,
    totals: Totals,
    sessions: std::collections::BTreeSet<String>,
    speed: Buckets,
    projects: BTreeMap<Option<String>, Group>,
    models: BTreeMap<String, Group>,
    modes: BTreeMap<String, ModeTally>,
    tools: BTreeMap<String, ToolRow>,
    dropped_runs: u64,
    newest_nanos: u64,
}

fn text(value: Option<&AnyValue>) -> Option<&str> {
    match value?.value.as_ref()? {
        any_value::Value::StringValue(text) => Some(text),
        _ => None,
    }
}

fn attribute<'a>(attributes: &'a [KeyValue], key: &str) -> Option<&'a str> {
    attributes
        .iter()
        .find(|pair| pair.key == key)
        .and_then(|pair| text(pair.value.as_ref()))
}

fn number(point: &NumberDataPoint) -> u64 {
    match point.value {
        Some(number_data_point::Value::AsInt(value)) => u64::try_from(value).unwrap_or_default(),
        Some(number_data_point::Value::AsDouble(value)) if value > 0.0 => value.round() as u64,
        _ => 0,
    }
}

impl Aggregate {
    fn new(from: Day, to: Day, offset_minutes: i32) -> Self {
        let days = (to.0 - from.0 + 1) as usize;
        Self {
            from,
            offset_minutes,
            earliest: None,
            mismatched_bounds: false,
            per_day: vec![Tokens::default(); days],
            recorded: vec![false; days],
            totals: Totals::default(),
            sessions: Default::default(),
            speed: Buckets::default(),
            projects: BTreeMap::new(),
            models: BTreeMap::new(),
            modes: BTreeMap::new(),
            tools: BTreeMap::new(),
            dropped_runs: 0,
            newest_nanos: 0,
        }
    }

    /// The range index of the local day a point was recorded on, or `None` outside the range.
    fn index(&mut self, time_unix_nano: u64) -> Option<usize> {
        let day = local_day((time_unix_nano / 1_000_000_000) as i64, self.offset_minutes);
        let day = Day(u32::try_from(day).ok()?);
        self.earliest = Some(self.earliest.map_or(day, |earliest| earliest.min(day)));
        let index = usize::try_from(day.0.checked_sub(self.from.0)?).ok()?;
        (index < self.per_day.len()).then_some(index)
    }

    fn add_request(&mut self, request: ExportMetricsServiceRequest) {
        for metric in request
            .resource_metrics
            .into_iter()
            .flat_map(|resource| resource.scope_metrics)
            .flat_map(|scope| scope.metrics)
        {
            match metric.data {
                Some(metric::Data::Histogram(data)) => {
                    for point in &data.data_points {
                        let Some(index) = self.index(point.time_unix_nano) else {
                            continue;
                        };
                        self.recorded[index] = true;
                        self.newest_nanos = self.newest_nanos.max(point.time_unix_nano);
                        self.add_histogram(index, &metric.name, point);
                    }
                }
                Some(metric::Data::Sum(data)) => {
                    for point in &data.data_points {
                        let Some(index) = self.index(point.time_unix_nano) else {
                            continue;
                        };
                        self.recorded[index] = true;
                        self.newest_nanos = self.newest_nanos.max(point.time_unix_nano);
                        self.add_sum(&metric.name, point);
                    }
                }
                _ => {}
            }
        }
    }

    fn add_histogram(&mut self, day: usize, name: &str, point: &HistogramDataPoint) {
        let attributes = &point.attributes;
        let get = |key| attribute(attributes, key);
        let (model, mode, project, session) = (
            get(names::GEN_AI_REQUEST_MODEL).unwrap_or("unknown"),
            get(names::MODE).unwrap_or("agent"),
            get(names::PROJECT).map(str::to_owned),
            get(names::SESSION),
        );
        if let Some(session) = session {
            self.sessions.insert(session.to_owned());
        }
        let sum = point
            .sum
            .filter(|sum| *sum > 0.0)
            .map_or(0, |sum| sum.round() as u64);
        match name {
            names::GEN_AI_CLIENT_TOKEN_USAGE => {
                let kind = get(names::GEN_AI_TOKEN_TYPE).unwrap_or_default();
                let counted = matches!(kind, "input" | "output");
                self.totals.tokens.add(kind, sum);
                self.per_day[day].add(kind, sum);
                for group in [
                    self.projects.entry(project).or_default(),
                    self.models.entry(model.to_owned()).or_default(),
                ] {
                    group.tokens.add(kind, sum);
                    if counted {
                        *group.modes.entry(mode.to_owned()).or_default() += sum;
                        if let Some(tally) = group.session(session) {
                            tally.total += sum;
                            tally.models.insert(model.to_owned());
                        }
                    }
                }
                if counted {
                    self.modes.entry(mode.to_owned()).or_default().total += sum;
                }
            }
            names::GEN_AI_CLIENT_OPERATION_DURATION => {
                let failed = if get(names::ERROR_TYPE).is_some() {
                    point.count
                } else {
                    0
                };
                self.totals.model_calls += point.count;
                self.totals.failed_calls += failed;
                self.modes.entry(mode.to_owned()).or_default().calls += point.count;
                let mut merged = true;
                for group in [
                    self.projects.entry(project).or_default(),
                    self.models.entry(model.to_owned()).or_default(),
                ] {
                    group.calls += point.count;
                    group.failed += failed;
                    merged &= group.duration.add(point);
                    if let Some(tally) = group.session(session) {
                        tally.calls += point.count;
                        tally.models.insert(model.to_owned());
                    }
                }
                self.mismatched_bounds |= !merged;
            }
            names::OUTPUT_SPEED => {
                let merged = [
                    self.speed.add(point),
                    self.projects.entry(project).or_default().speed.add(point),
                    self.models
                        .entry(model.to_owned())
                        .or_default()
                        .speed
                        .add(point),
                ];
                self.mismatched_bounds |= merged.contains(&false);
            }
            names::COMPACTION_SAVED => {
                self.totals.tokens_saved += sum;
                self.totals.compactions += point.count;
            }
            _ => {}
        }
    }

    fn add_sum(&mut self, name: &str, point: &NumberDataPoint) {
        let attributes = &point.attributes;
        let get = |key| attribute(attributes, key);
        let count = number(point);
        match name {
            names::RUN_OUTCOMES => {
                let outcome = get(names::RUN_OUTCOME).unwrap_or("error");
                let mode = get(names::MODE).unwrap_or("agent");
                self.totals.runs += count;
                if outcome == "complete" {
                    self.totals.completed_runs += count;
                }
                *self
                    .totals
                    .runs_by_outcome
                    .entry(outcome.to_owned())
                    .or_default() += count;
                self.modes.entry(mode.to_owned()).or_default().runs += count;
                let group = self
                    .projects
                    .entry(get(names::PROJECT).map(str::to_owned))
                    .or_default();
                group.runs += count;
                if let Some(session) = get(names::SESSION) {
                    group.session(Some(session));
                    self.sessions.insert(session.to_owned());
                }
            }
            names::TOOL_INVOCATIONS => {
                let tool = get(names::GEN_AI_TOOL_NAME).unwrap_or("unknown");
                let row = self
                    .tools
                    .entry(tool.to_owned())
                    .or_insert_with(|| ToolRow {
                        name: tool.to_owned(),
                        calls: 0,
                        errors: 0,
                        denied: 0,
                        timeouts: 0,
                    });
                row.calls += count;
                match get(names::TOOL_OUTCOME) {
                    Some("error") => row.errors += count,
                    Some("denied") => row.denied += count,
                    Some("timeout") => row.timeouts += count,
                    _ => {}
                }
            }
            names::DROPPED_RUNS => self.dropped_runs += count,
            _ => {}
        }
    }

    fn finish(self) -> UsageSummary {
        let mut totals = self.totals;
        totals.total = totals.tokens.total();
        totals.sessions = self.sessions.len() as u64;
        totals.run_success =
            (totals.runs > 0).then(|| totals.completed_runs as f64 / totals.runs as f64);
        totals.median_output_speed = self.speed.quantile(0.5);
        let all = totals.total;
        let by_total = |a: &GroupRow, b: &GroupRow| {
            b.total
                .cmp(&a.total)
                .then_with(|| a.name.is_none().cmp(&b.name.is_none()))
                .then_with(|| a.name.cmp(&b.name))
        };
        let mut per_project: Vec<_> = self
            .projects
            .into_iter()
            .map(|(name, group)| group.row(name, all, true))
            .collect();
        per_project.sort_by(by_total);
        let mut per_model: Vec<_> = self
            .models
            .into_iter()
            .map(|(name, group)| group.row(Some(name), all, false))
            .collect();
        per_model.sort_by(by_total);
        totals.top_model = per_model
            .first()
            .filter(|row| row.total > 0)
            .and_then(|row| row.name.clone());
        let mut per_mode: Vec<_> = self
            .modes
            .into_iter()
            .map(|(mode, tally)| ModeRow {
                mode,
                total: tally.total,
                model_calls: tally.calls,
                runs: tally.runs,
            })
            .collect();
        per_mode.sort_by(|a, b| b.total.cmp(&a.total).then_with(|| a.mode.cmp(&b.mode)));
        let mut tools: Vec<_> = self.tools.into_values().collect();
        tools.sort_by(|a, b| b.calls.cmp(&a.calls).then_with(|| a.name.cmp(&b.name)));
        tools.truncate(MAX_TOOL_ROWS);
        let from = self.from;
        let to = Day(from.0 + self.per_day.len() as u32 - 1);
        UsageSummary {
            from: from.to_string(),
            to: to.to_string(),
            totals,
            per_day: self
                .per_day
                .into_iter()
                .zip(self.recorded)
                .enumerate()
                .map(|(offset, (tokens, recorded))| DayTokens {
                    day: Day(from.0 + offset as u32).to_string(),
                    tokens,
                    recorded,
                })
                .collect(),
            per_project,
            per_model,
            per_mode,
            tools,
            gaps: Gaps {
                first_day: None,
                dropped_runs: self.dropped_runs,
                off_periods: vec![],
            },
            warnings: vec![],
            telemetry_enabled: true,
            files: 0,
            newest_unix_ms: None,
            stale: false,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum UsageError {
    #[error("Choose an existing folder to export to.")]
    Destination,
    #[error("A folder or file with this export's name already exists there.")]
    Exists,
    #[error("The export could not be written.")]
    Write,
}

impl UsageError {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Destination => "destination",
            Self::Exists => "exists",
            Self::Write => "write",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportReport {
    pub folder: PathBuf,
    pub metrics_files: usize,
    pub trace_files: usize,
}

/// Copy the files for the local days `from` to `to` (`offset_minutes` east of UTC) into a new
/// folder inside `dest`, as plain OTLP JSON any collector can import: every metrics file whose UTC
/// day overlaps the range, and every trace started within it. Symlinks are not followed. On any
/// error the new folder is removed, so a retry starts clean.
pub fn export(
    data_dir: &Path,
    from: Day,
    to: Day,
    offset_minutes: i32,
    dest: &Path,
) -> Result<ExportReport, UsageError> {
    if !std::fs::metadata(dest).is_ok_and(|meta| meta.is_dir()) {
        return Err(UsageError::Destination);
    }
    let folder = dest.join(format!("blackwall-telemetry-{from}-to-{to}"));
    match std::fs::create_dir(&folder) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            return Err(UsageError::Exists)
        }
        Err(_) => return Err(UsageError::Write),
    }
    let result = copy_range(data_dir, from, to, offset_minutes, &folder);
    if result.is_err() {
        let _ = std::fs::remove_dir_all(&folder);
    }
    result
}

fn copy_range(
    data_dir: &Path,
    from: Day,
    to: Day,
    offset_minutes: i32,
    folder: &Path,
) -> Result<ExportReport, UsageError> {
    let offset = i64::from(clamp_offset(offset_minutes)) * 60;
    // The UTC days the range's first and last instants fall on.
    let first_utc = (i64::from(from.0) * 86_400 - offset).div_euclid(86_400);
    let last_utc = ((i64::from(to.0) + 1) * 86_400 - offset - 1).div_euclid(86_400);
    let copy = |source: &Path, folder: &Path| -> Result<bool, UsageError> {
        if !std::fs::symlink_metadata(source).is_ok_and(|meta| meta.is_file()) {
            return Ok(false);
        }
        let name = source.file_name().ok_or(UsageError::Write)?;
        std::fs::create_dir_all(folder).map_err(|_| UsageError::Write)?;
        std::fs::copy(source, folder.join(name)).map_err(|_| UsageError::Write)?;
        Ok(true)
    };
    let mut report = ExportReport {
        folder: folder.to_path_buf(),
        metrics_files: 0,
        trace_files: 0,
    };
    for (_, path) in metric_files(data_dir)
        .into_iter()
        .filter(|(day, _)| (first_utc..=last_utc).contains(&i64::from(day.0)))
    {
        report.metrics_files += usize::from(copy(&path, &folder.join(METRICS_DIRECTORY))?);
    }
    let traces = data_dir.join(crate::trace::TRACE_DIRECTORY);
    let entries = std::fs::symlink_metadata(&traces)
        .is_ok_and(|meta| meta.is_dir())
        .then(|| std::fs::read_dir(&traces).ok())
        .flatten();
    for entry in entries.into_iter().flatten().filter_map(Result::ok) {
        let name = entry.file_name().to_string_lossy().into_owned();
        // Trace files are named `<started-ms>-<request-id>.json`.
        let day = name
            .strip_suffix(".json")
            .and_then(|stem| stem.split_once('-'))
            .and_then(|(millis, _)| millis.parse::<i64>().ok())
            .map(|millis| local_day(millis / 1000, offset_minutes));
        if day.is_some_and(|day| (i64::from(from.0)..=i64::from(to.0)).contains(&day)) {
            report.trace_files += usize::from(copy(
                &entry.path(),
                &folder.join(crate::trace::TRACE_DIRECTORY),
            )?);
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests;
