//! Usage metrics as OTLP/JSON lines: one `ExportMetricsServiceRequest` per line in
//! `metrics/YYYY-MM-DD.jsonl` (UTC), the shape the OpenTelemetry Collector's file exporter writes
//! and its `otlpjsonfile` receiver reads. Delta temporality, so each line holds only new data.
use super::{export::Exporter, Retention};
use opentelemetry_proto::tonic::{
    collector::metrics::v1::ExportMetricsServiceRequest, metrics::v1::metric::Data,
};
use opentelemetry_sdk::{
    error::OTelSdkResult,
    metrics::{data::ResourceMetrics, exporter::PushMetricExporter, Temporality},
};
use std::{
    io::Write,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

pub const METRICS_DIRECTORY: &str = "metrics";

/// UTC days since the Unix epoch for a time.
pub(crate) fn day_of(time: SystemTime) -> i64 {
    time.duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| (elapsed.as_secs() / 86_400) as i64)
}

/// The civil date for a day number (Howard Hinnant's `civil_from_days`).
pub(crate) fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

/// The day number for a civil date (Howard Hinnant's `days_from_civil`).
pub(crate) fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let yoe = year.rem_euclid(400);
    let month = i64::from(month);
    let doy = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + i64::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// `YYYY-MM-DD.jsonl` for a day number.
pub(crate) fn file_name(day: i64) -> String {
    let (year, month, day) = civil_from_days(day);
    format!("{year:04}-{month:02}-{day:02}.jsonl")
}

/// The day a metrics file holds, or `None` for any other name.
pub(crate) fn parse_name(name: &str) -> Option<i64> {
    let stem = name.strip_suffix(".jsonl")?;
    let bytes = stem.as_bytes();
    if bytes.len() != 10 || bytes[4] != b'-' || bytes[7] != b'-' {
        return None;
    }
    let number = |range: std::ops::Range<usize>| -> Option<u32> {
        let part = stem.get(range)?;
        part.bytes()
            .all(|b| b.is_ascii_digit())
            .then(|| part.parse().ok())?
    };
    let (year, month, day) = (number(0..4)?, number(5..7)?, number(8..10)?);
    ((1..=12).contains(&month) && (1..=31).contains(&day))
        .then(|| days_from_civil(i64::from(year), month, day))
}

/// Delete metrics files from before the retention window. `today` is a UTC day number.
pub(crate) fn prune(data_dir: &Path, retention: Retention, today: i64) {
    let Some(days) = retention.days() else {
        return;
    };
    let directory = data_dir.join(METRICS_DIRECTORY);
    if std::fs::symlink_metadata(&directory).is_ok_and(|m| m.file_type().is_symlink()) {
        return;
    }
    let Ok(entries) = std::fs::read_dir(&directory) else {
        return;
    };
    let oldest = today - i64::from(days);
    for entry in entries.filter_map(Result::ok) {
        let name = entry.file_name().to_string_lossy().into_owned();
        let old = parse_name(&name).is_some_and(|day| day < oldest);
        if old && entry.file_type().is_ok_and(|kind| kind.is_file()) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

#[derive(Debug)]
pub(crate) struct FileMetricExporter {
    directory: PathBuf,
    exporter: Exporter,
}

impl FileMetricExporter {
    pub(super) fn new(data_dir: &Path, exporter: Exporter) -> Self {
        Self {
            directory: data_dir.join(METRICS_DIRECTORY),
            exporter,
        }
    }

    /// Append one request line, and hand the same request to OTLP export. Metrics failures never
    /// affect a run.
    fn append(&self, mut request: ExportMetricsServiceRequest) {
        for resource in &mut request.resource_metrics {
            for scope in &mut resource.scope_metrics {
                scope.metrics.retain(|metric| match &metric.data {
                    Some(Data::Histogram(data)) => !data.data_points.is_empty(),
                    Some(Data::Sum(data)) => !data.data_points.is_empty(),
                    Some(Data::Gauge(data)) => !data.data_points.is_empty(),
                    Some(_) => true,
                    None => false,
                });
            }
            resource
                .scope_metrics
                .retain(|scope| !scope.metrics.is_empty());
        }
        request
            .resource_metrics
            .retain(|resource| !resource.scope_metrics.is_empty());
        if request.resource_metrics.is_empty() {
            return;
        }
        self.exporter.submit_metrics(&request);
        let Ok(mut line) = serde_json::to_vec(&request) else {
            return;
        };
        line.push(b'\n');
        if std::fs::create_dir_all(&self.directory).is_err()
            || std::fs::symlink_metadata(&self.directory).is_ok_and(|m| m.file_type().is_symlink())
        {
            return;
        }
        let path = self.directory.join(file_name(day_of(SystemTime::now())));
        let mut options = std::fs::OpenOptions::new();
        options.create(true).append(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        if let Ok(mut file) = options.open(path) {
            // One write per line keeps a crash from interleaving two requests.
            let _ = file.write_all(&line).and_then(|()| file.flush());
        }
    }
}

impl PushMetricExporter for FileMetricExporter {
    async fn export(&self, metrics: &ResourceMetrics) -> OTelSdkResult {
        self.append(ExportMetricsServiceRequest::from(metrics));
        Ok(())
    }

    fn force_flush(&self) -> OTelSdkResult {
        Ok(())
    }

    fn shutdown_with_timeout(&self, _: Duration) -> OTelSdkResult {
        Ok(())
    }

    fn temporality(&self) -> Temporality {
        Temporality::Delta
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn civil_dates_round_trip() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(file_name(days_from_civil(2026, 10, 28)), "2026-10-28.jsonl");
        assert_eq!(civil_from_days(days_from_civil(2000, 2, 29)), (2000, 2, 29));
        for day in [-1, 59, 365, 11_016, 20_000, 30_000] {
            let (year, month, date) = civil_from_days(day);
            assert_eq!(days_from_civil(year, month, date), day);
        }
        assert_eq!(
            parse_name("2026-10-28.jsonl"),
            Some(days_from_civil(2026, 10, 28))
        );
        for name in [
            "2026-13-01.jsonl",
            "notes.jsonl",
            "2026-10-28.json",
            "+026-10-28.jsonl",
        ] {
            assert_eq!(parse_name(name), None, "{name}");
        }
    }
}
