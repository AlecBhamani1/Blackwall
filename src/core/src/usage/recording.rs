//! When recording (the "Record runs" switch) was turned on or off, so the Usage page can list the
//! periods it could not count. One JSON line per change in `metrics/recording.jsonl`:
//! `{"unixMs":1793208600000,"recording":false,"surface":"desktop"}`. The desktop app and `bw`
//! have separate switches, so each surface's periods are worked out on their own. The file holds
//! no prompts, paths, or names.
use super::METRICS_DIRECTORY;
use crate::telemetry::Surface;
use serde::{Deserialize, Serialize};
use std::{
    io::{BufRead, Write},
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

pub const RECORDING_FILE: &str = "recording.jsonl";
/// The most off periods listed for one range; the newest are kept.
const MAX_PERIODS: usize = 20;

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct Marker {
    unix_ms: u64,
    recording: bool,
    #[serde(default = "desktop")]
    surface: String,
}

fn desktop() -> String {
    Surface::Desktop.as_str().into()
}

/// A period with recording off. `from_unix_ms` is `None` when it began before the first marker,
/// and `to_unix_ms` is `None` while recording is still off.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OffPeriod {
    /// `desktop` or `cli`.
    pub surface: String,
    pub from_unix_ms: Option<u64>,
    pub to_unix_ms: Option<u64>,
}

/// Note that recording was turned on or off now. Call it only when the setting changes. Failures
/// are ignored: the marker only explains gaps.
pub fn mark_recording(data_dir: &Path, recording: bool, surface: Surface) {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as u64);
    mark_at(data_dir, recording, surface, now);
}

pub(crate) fn mark_at(data_dir: &Path, recording: bool, surface: Surface, unix_ms: u64) {
    let directory = data_dir.join(METRICS_DIRECTORY);
    if std::fs::create_dir_all(&directory).is_err()
        || std::fs::symlink_metadata(&directory).is_ok_and(|m| m.file_type().is_symlink())
    {
        return;
    }
    let Ok(mut line) = serde_json::to_vec(&Marker {
        unix_ms,
        recording,
        surface: surface.as_str().into(),
    }) else {
        return;
    };
    line.push(b'\n');
    let path = directory.join(RECORDING_FILE);
    if std::fs::symlink_metadata(&path).is_ok_and(|m| !m.is_file()) {
        return;
    }
    let mut options = std::fs::OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    if let Ok(mut file) = options.open(path) {
        let _ = file.write_all(&line);
    }
}

/// The off periods that overlap `from_ms..to_ms`, oldest first. Unreadable lines are skipped.
pub(crate) fn off_periods(data_dir: &Path, from_ms: u64, to_ms: u64) -> Vec<OffPeriod> {
    let path = data_dir.join(METRICS_DIRECTORY).join(RECORDING_FILE);
    if std::fs::symlink_metadata(&path).map_or(true, |m| !m.is_file()) {
        return vec![];
    }
    let Ok(file) = std::fs::File::open(path) else {
        return vec![];
    };
    let mut markers: Vec<Marker> = std::io::BufReader::new(file)
        .lines()
        .map_while(Result::ok)
        .filter_map(|line| serde_json::from_str(&line).ok())
        .collect();
    markers.sort_by_key(|marker| marker.unix_ms);
    let mut surfaces: Vec<String> = markers.iter().map(|m| m.surface.clone()).collect();
    surfaces.sort();
    surfaces.dedup();
    let mut periods: Vec<OffPeriod> = surfaces
        .iter()
        .flat_map(|surface| {
            periods_of(
                markers.iter().filter(|marker| &marker.surface == surface),
                surface,
            )
        })
        .collect();
    periods.retain(|period| {
        period.from_unix_ms.unwrap_or(0) < to_ms && period.to_unix_ms.unwrap_or(u64::MAX) > from_ms
    });
    periods.sort_by_key(|period| (period.from_unix_ms, period.to_unix_ms));
    let extra = periods.len().saturating_sub(MAX_PERIODS);
    periods.drain(..extra);
    periods
}

/// One surface's off periods from its markers, oldest first.
fn periods_of<'a>(markers: impl Iterator<Item = &'a Marker>, surface: &str) -> Vec<OffPeriod> {
    let period = |from_unix_ms, to_unix_ms| OffPeriod {
        surface: surface.into(),
        from_unix_ms,
        to_unix_ms,
    };
    let mut periods = vec![];
    let mut previous: Option<bool> = None;
    let mut start = None;
    for marker in markers {
        match (previous, marker.recording) {
            // A repeated marker changes nothing.
            (Some(false), false) | (Some(true), true) => {}
            (_, false) => start = Some(marker.unix_ms),
            (Some(false), true) => periods.push(period(start.take(), Some(marker.unix_ms))),
            // Only changes are marked, so a first "on" means recording was off before it.
            (None, true) => periods.push(period(None, Some(marker.unix_ms))),
        }
        previous = Some(marker.recording);
    }
    if previous == Some(false) {
        periods.push(period(start, None));
    }
    periods
}
