//! The run buffer and the local trace files.
//!
//! `RunBufferProcessor` is the only span processor on the provider; no exporter is registered on
//! the provider directly. It holds every span of a run until the run's root span ends, then
//! filters, redacts, and scrubs the whole run at once, so a secret first seen late in a run is
//! still removed from spans that ended earlier. Only the finished run reaches an exporter.
use super::{names, redact::Scrubber};
use opentelemetry::{
    metrics::Counter,
    trace::{SpanId, Status, TraceId},
    Array, KeyValue, StringValue, Value,
};
use opentelemetry_proto::{
    tonic::{
        collector::trace::v1::ExportTraceServiceRequest,
        common::v1::{any_value, AnyValue, KeyValue as ProtoKeyValue},
    },
    transform::{common::tonic::ResourceAttributesWithSchema, trace::tonic as transform},
};
use opentelemetry_sdk::{
    error::OTelSdkResult,
    trace::{SpanData, SpanProcessor},
    Resource,
};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc, Arc, Mutex,
    },
    time::Duration,
};

/// The newest traces kept; older files are removed after each run.
pub(super) const KEEP: usize = 200;
pub(super) const MAX_SPANS: usize = 1000;
pub(super) const MAX_FILE: usize = crate::trace::MAX_FILE;

/// One run in progress: where it is written, its capture setting, and what it has seen.
#[derive(Debug)]
pub(super) struct RunState {
    pub(super) path: PathBuf,
    pub(super) content: bool,
    pub(super) secrets: Mutex<Vec<String>>,
    pub(super) dropped: AtomicUsize,
    spans: Mutex<Vec<SpanData>>,
}

impl RunState {
    pub(super) fn new(path: PathBuf, content: bool) -> Self {
        Self {
            path,
            content,
            secrets: Mutex::new(vec![]),
            dropped: AtomicUsize::new(0),
            spans: Mutex::new(vec![]),
        }
    }
}

/// Runs in progress by trace ID, shared by `RunScope`s and the processor.
#[derive(Clone, Debug, Default)]
pub(super) struct Runs(Arc<Mutex<HashMap<TraceId, Arc<RunState>>>>);

impl Runs {
    pub(super) fn insert(&self, trace_id: TraceId, run: Arc<RunState>) {
        if let Ok(mut runs) = self.0.lock() {
            runs.insert(trace_id, run);
        }
    }

    fn get(&self, trace_id: TraceId) -> Option<Arc<RunState>> {
        self.0.lock().ok()?.get(&trace_id).cloned()
    }

    fn remove(&self, trace_id: TraceId) -> Option<Arc<RunState>> {
        self.0.lock().ok()?.remove(&trace_id)
    }
}

/// Finished runs waiting for the writer thread; more than this are dropped and counted.
pub(super) const QUEUE: usize = 64;

enum Job {
    Run(Arc<RunState>, Vec<SpanData>, Resource),
    Flush(mpsc::SyncSender<()>),
}

/// The trace writer: a dedicated thread fed by a bounded queue, so ending a run never does file
/// IO or blocks the caller. Serialization, the write, and pruning all happen on that thread.
#[derive(Debug)]
pub(super) struct Writer {
    queue: Mutex<Option<mpsc::SyncSender<Job>>>,
    dropped_runs: AtomicUsize,
    /// `blackwall.trace.dropped_runs`, so the Usage page can report the gap.
    dropped_counter: Option<Counter<u64>>,
    #[cfg(test)]
    _idle: Mutex<Option<mpsc::Receiver<Job>>>,
}

impl Writer {
    fn start(dropped_counter: Counter<u64>) -> Self {
        let (queue, jobs) = mpsc::sync_channel::<Job>(QUEUE);
        let spawned = std::thread::Builder::new()
            .name("blackwall-traces".into())
            .spawn(move || {
                for job in jobs {
                    match job {
                        Job::Run(run, spans, resource) => {
                            if let Some(body) = finish_run(&run, spans, &resource) {
                                FileSpanExporter.export(&run.path, body);
                            }
                        }
                        Job::Flush(done) => {
                            let _ = done.send(());
                        }
                    }
                }
            });
        Self {
            queue: Mutex::new(spawned.ok().map(|_| queue)),
            dropped_runs: AtomicUsize::new(0),
            dropped_counter: Some(dropped_counter),
            #[cfg(test)]
            _idle: Mutex::new(None),
        }
    }

    /// A writer whose queue is never drained, to fill it in tests.
    #[cfg(test)]
    pub(super) fn idle(capacity: usize) -> Self {
        let (queue, jobs) = mpsc::sync_channel::<Job>(capacity);
        Self {
            queue: Mutex::new(Some(queue)),
            dropped_runs: AtomicUsize::new(0),
            dropped_counter: None,
            _idle: Mutex::new(Some(jobs)),
        }
    }

    /// An idle writer that also counts drops in a metric.
    #[cfg(test)]
    pub(super) fn idle_counted(capacity: usize, counter: Counter<u64>) -> Self {
        Self {
            dropped_counter: Some(counter),
            ..Self::idle(capacity)
        }
    }

    fn sender(&self) -> Option<mpsc::SyncSender<Job>> {
        self.queue.lock().ok()?.clone()
    }

    /// Queue a finished run without blocking; a full queue drops it.
    pub(super) fn submit(&self, run: Arc<RunState>, spans: Vec<SpanData>, resource: Resource) {
        let sent = self
            .sender()
            .is_some_and(|queue| queue.try_send(Job::Run(run, spans, resource)).is_ok());
        if !sent {
            self.dropped_runs.fetch_add(1, Ordering::Relaxed);
            if let Some(counter) = &self.dropped_counter {
                counter.add(1, &[]);
            }
        }
    }

    #[cfg(test)]
    pub(super) fn dropped_runs(&self) -> usize {
        self.dropped_runs.load(Ordering::Relaxed)
    }

    /// Wait up to `timeout` for every queued run to be written.
    fn flush(&self, timeout: Duration) {
        let Some(queue) = self.sender() else {
            return;
        };
        let (done, finished) = mpsc::sync_channel(1);
        if queue.send(Job::Flush(done)).is_ok() {
            let _ = finished.recv_timeout(timeout);
        }
    }

    fn stop(&self, timeout: Duration) {
        self.flush(timeout);
        if let Ok(mut queue) = self.queue.lock() {
            queue.take();
        }
    }
}

#[derive(Debug)]
pub(super) struct RunBufferProcessor {
    runs: Runs,
    resource: Resource,
    writer: Writer,
}

impl RunBufferProcessor {
    pub(super) fn new(runs: Runs, dropped_runs: Counter<u64>) -> Self {
        Self {
            runs,
            resource: Resource::builder_empty().build(),
            writer: Writer::start(dropped_runs),
        }
    }
}

impl SpanProcessor for RunBufferProcessor {
    fn on_start(&self, _: &mut opentelemetry_sdk::trace::Span, _: &opentelemetry::Context) {}

    fn on_end(&self, span: SpanData) {
        let trace_id = span.span_context.trace_id();
        if span.parent_span_id != SpanId::INVALID {
            // Spans of unknown or finished runs are dropped.
            if let Some(run) = self.runs.get(trace_id) {
                if let Ok(mut spans) = run.spans.lock() {
                    spans.push(span);
                }
            }
            return;
        }
        let Some(run) = self.runs.remove(trace_id) else {
            return;
        };
        let mut spans = run
            .spans
            .lock()
            .map(|mut spans| std::mem::take(&mut *spans))
            .unwrap_or_default();
        spans.push(span);
        self.writer.submit(run, spans, self.resource.clone());
    }

    fn force_flush(&self) -> OTelSdkResult {
        self.writer.flush(Duration::from_secs(5));
        Ok(())
    }

    fn shutdown_with_timeout(&self, timeout: Duration) -> OTelSdkResult {
        self.writer.stop(timeout);
        Ok(())
    }

    fn set_resource(&mut self, resource: &Resource) {
        self.resource = resource.clone();
    }
}

fn scrub_value(value: &mut Value, scrubber: &Scrubber) {
    match value {
        Value::String(text) => *text = StringValue::from(scrubber.scrub(text.as_str())),
        Value::Array(Array::String(items)) => {
            for item in items.iter_mut() {
                *item = StringValue::from(scrubber.scrub(item.as_str()));
            }
        }
        _ => {}
    }
}

/// The run's filtered, scrubbed spans as one OTLP JSON body within the 4 MiB limit, or `None`.
///
/// Content capture off: only metadata keys survive and failure messages carry only the error
/// class. Content capture on: every string is scrubbed of the run's secrets, and content keys are
/// dropped from the whole run if it would otherwise pass the size limit.
pub(super) fn finish_run(
    run: &RunState,
    mut spans: Vec<SpanData>,
    resource: &Resource,
) -> Option<Vec<u8>> {
    // The root first, then in start order, as the original writer stored them.
    spans.sort_by_key(|span| (span.parent_span_id != SpanId::INVALID, span.start_time));
    let secrets = run.secrets.lock().map(|s| s.clone()).unwrap_or_default();
    let scrubber = Scrubber::new(secrets);
    for span in &mut spans {
        if run.content {
            for pair in &mut span.attributes {
                scrub_value(&mut pair.value, &scrubber);
            }
            for event in &mut span.events.events {
                for pair in &mut event.attributes {
                    scrub_value(&mut pair.value, &scrubber);
                }
            }
            if let Status::Error { description } = &mut span.status {
                *description = scrubber.scrub(description).into();
            }
        } else {
            let allowed = |pair: &KeyValue| names::METADATA_KEYS.contains(&pair.key.as_str());
            span.attributes.retain(allowed);
            for event in &mut span.events.events {
                event.attributes.retain(allowed);
            }
            if let Status::Error { description } = &mut span.status {
                let kind = span
                    .attributes
                    .iter()
                    .find(|pair| pair.key.as_str() == names::ERROR_TYPE)
                    .map_or_else(
                        || "_OTHER".to_owned(),
                        |pair| pair.value.as_str().into_owned(),
                    );
                *description = kind.into();
            }
        }
    }
    let mut resource_spans = transform::group_spans_by_resource_and_scope(
        spans,
        &ResourceAttributesWithSchema::from(resource),
    );
    let dropped = run.dropped.load(Ordering::Relaxed);
    let mut extra = vec![];
    if dropped > 0 {
        extra.push(attribute(
            names::DROPPED_SPANS,
            any_value::Value::IntValue(dropped as i64),
        ));
    }
    for resource in &mut resource_spans {
        if let Some(resource) = &mut resource.resource {
            resource.attributes.extend(extra.iter().cloned());
        }
    }
    let mut request = ExportTraceServiceRequest { resource_spans };
    let mut body = serde_json::to_vec(&request).ok()?;
    if body.len() > MAX_FILE {
        let content = |pair: &ProtoKeyValue| names::CONTENT_KEYS.contains(&pair.key.as_str());
        for resource in &mut request.resource_spans {
            if let Some(resource) = &mut resource.resource {
                resource.attributes.push(attribute(
                    names::CONTENT_REMOVED,
                    any_value::Value::BoolValue(true),
                ));
            }
            for span in resource
                .scope_spans
                .iter_mut()
                .flat_map(|scope| scope.spans.iter_mut())
            {
                span.attributes.retain(|pair| !content(pair));
                for event in &mut span.events {
                    event.attributes.retain(|pair| !content(pair));
                }
            }
        }
        body = serde_json::to_vec(&request).ok()?;
    }
    (body.len() <= MAX_FILE).then_some(body)
}

fn attribute(key: &str, value: any_value::Value) -> ProtoKeyValue {
    ProtoKeyValue {
        key: key.into(),
        value: Some(AnyValue { value: Some(value) }),
        ..Default::default()
    }
}

/// Writes one finished run to `traces/<started>-<request>.json` and keeps the newest 200.
#[derive(Debug)]
pub(super) struct FileSpanExporter;

impl FileSpanExporter {
    /// Trace failures never affect the run.
    pub(super) fn export(&self, path: &Path, mut body: Vec<u8>) {
        let Some(directory) = path.parent() else {
            return;
        };
        body.push(b'\n');
        if std::fs::create_dir_all(directory).is_err()
            || std::fs::symlink_metadata(directory).is_ok_and(|m| m.file_type().is_symlink())
        {
            return;
        }
        let _ = crate::policy::replace_owner_file(directory, path, &body);
        let mut files = crate::trace::list_files(directory);
        if files.len() > KEEP {
            files.sort();
            for old in &files[..files.len() - KEEP] {
                let _ = std::fs::remove_file(directory.join(old));
            }
        }
    }
}
