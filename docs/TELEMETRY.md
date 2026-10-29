# Telemetry

Blackwall records local traces and usage metrics with the OpenTelemetry SDK
(`opentelemetry_sdk` 0.33.1), using the OpenTelemetry generative AI semantic conventions. Every
resource and instrumentation scope carries the conventions' schema URL
(`https://opentelemetry.io/schemas/1.42.0`, from `opentelemetry-semantic-conventions` 0.33.1).
Everything stays in the data folder (`~/.blackwall`, or `BLACKWALL_DATA_DIR`). Nothing is sent
anywhere unless you set an OTLP endpoint; [Export](#export) then sends the same data the files
hold.

Telemetry follows the existing traces switch: **Settings → Workflows → Runs → Record agent runs**
on the desktop (`tracesEnabled`), `bw config set traces on|off` in the CLI. It is on by default.
Turning it off stops both traces and usage metrics. Content capture (`traceContent`,
`bw config set trace-content on|off`) is off by default and only affects traces.

No `OTEL_*` environment variable changes this behaviour, including export: Blackwall gives the
SDK its sampler, span limits, resource, and export interval explicitly, does not use the SDK's
environment resource detectors, and ignores `OTEL_EXPORTER_OTLP_*`.

## Signals

| Signal | Name | Kind | When |
|---|---|---|---|
| Span | `invoke_agent blackwall` | root | Each Agent run (desktop and `bw`) |
| Span | `chat {model}` | root | Each Chat run on the desktop (streamed, web, and one-shot) |
| Span | `chat {model}` | client | Each model call |
| Span | `execute_tool {name}` | internal | Each tool call, including web tools in Chat |
| Span | `invoke_agent subagent` | internal | Each `spawn_agent` child, under that call's `execute_tool spawn_agent` span; the child's own `chat` and `execute_tool` spans sit under it |
| Metric | `gen_ai.client.token.usage` | histogram, `{token}` | Once per model call that reports usage, once each for input and output |
| Metric | `gen_ai.client.operation.duration` | histogram, `s` | Once per model call |
| Metric | `blackwall.tool.invocations` | counter, `{call}` | Once per tool call |
| Metric | `blackwall.run.outcomes` | counter, `{run}` | Once per finished, failed, or stopped run |
| Metric | `blackwall.client.output_speed` | histogram, `{token}/s` | Once per successful model call that produced output |
| Metric | `blackwall.compaction.tokens_saved` | histogram, `{token}` | Once per compaction, the estimated context it released |
| Metric | `blackwall.trace.dropped_runs` | counter, `{run}` | Once per run whose trace the writer queue dropped |

`blackwall.*` is our namespace for what the conventions do not define yet.

**Compaction.** Summarizing history is a `chat` operation, as the conventions define it, so its
calls use `gen_ai.operation.name=chat` with the run's usual `blackwall.mode`, plus
`blackwall.compaction=true` on the metrics and the span. Their tokens and durations count in
`gen_ai.client.token.usage` and `gen_ai.client.operation.duration`. Automatic compaction inside a
run adds a `chat {model}` span to that run; the summary text is never recorded, even with content
capture on. An explicit compaction request (`/compact`) is not a run: it writes no trace file and
no `blackwall.run.outcomes` point, only its model-call metrics.

**Unknown tools.** When a model calls a tool it was not offered, the call is denied and recorded
as `execute_tool unknown` with `gen_ai.tool.name=unknown` and `blackwall.tool.outcome=denied`. The
made-up name is model-written text, so it never reaches a trace or a metric.

## Attributes

**Resource:** `service.name=blackwall`, `service.version`, `blackwall.surface=desktop|cli`.

**Model-call metrics:** `gen_ai.operation.name=chat`, `gen_ai.request.model`,
`gen_ai.provider.name`, `server.address` (host only; left out when the endpoint has no host),
`blackwall.mode=chat|agent|subagent`, and `blackwall.compaction=true` on compaction calls. Token
usage adds `gen_ai.token.type=input|output`, plus `cached_input` (from the server's
`prompt_tokens_details.cached_tokens`) and `reasoning` (from
`completion_tokens_details.reasoning_tokens`) when the server reports them. These two are parts of
`input` and `output`, not extra tokens: sum only `input` and `output` for a total.

**Project and session:** model-call metrics, output speed, compaction savings, and run outcomes
also carry `blackwall.project`, the workspace folder's name (never its path, and absent for Chat,
which has no project), and `blackwall.session`, the conversation's neutral ID (absent when the ID
is not plain letters, digits, `-`, or `_`). Titles and prompts are never recorded. Duration adds `error.type` on failure, one of
`network`, `http`, `invalid_response`, `limit`, or `_OTHER`, the same in every Chat and Agent
path. `gen_ai.provider.name` is `ollama` for an
endpoint on port 11434 and `openai_compatible` otherwise.

**Tool metric:** `gen_ai.tool.name` (MCP tools as `server__tool`; `unknown` for a tool that was not
offered) and
`blackwall.tool.outcome=ok|error|denied|timeout`.

**Run metric:** `blackwall.mode` and
`blackwall.run.outcome=complete|error|cancelled|iterations|denied`.

**Spans:** `gen_ai.operation.name`, `gen_ai.agent.name`, `gen_ai.agent.id` (subagents),
`gen_ai.request.model`, `gen_ai.response.finish_reasons`, `gen_ai.usage.input_tokens`,
`gen_ai.usage.output_tokens`, `gen_ai.tool.name`, `gen_ai.tool.type`,
`error.type`, `blackwall.request_id`, `blackwall.mode`, `blackwall.compaction`, and
`approval_requested` events with
`blackwall.approval.kind`. With content capture on, spans may also carry
`gen_ai.output.messages`, `gen_ai.tool.call.arguments`, `gen_ai.tool.call.result`, and
`gen_ai.tool.call.id`, which the model writes and so is treated as content.

## What is never recorded

- **Metrics** never carry prompt text, answers, reasoning, tool arguments, tool output, file paths,
  or error messages. Their attributes are limited to the lists above.
- **Traces with content capture off** keep only the metadata attributes listed above. A single
  processor drops every other key, so a prompt, system instructions, reasoning, or tool data cannot
  reach a file even if code tries to record it. A failed span's status message is only its error
  class (its `error.type`, or `_OTHER`), never the error text.
- **Traces with content capture on** record answers and tool data, redacted like audit entries
  plus secret-named JSON and YAML fields, cut to 4 KiB each. When the run ends, every value
  redacted anywhere in the run is scrubbed from all of its spans, including spans that ended
  before the secret appeared. Redaction matches common secret names and forms and can miss
  others, so turn content on only while investigating a run.

## How a run is written

The tracer provider has one span processor, `RunBufferProcessor`, and no exporter of its own. It
holds every span of a run until the run's root span ends. It then applies the content filter or
the redaction scrub to the whole run and checks the 4 MiB limit, and only then the finished run is
written. Ending a run does no file work itself: the run is queued for a dedicated writer thread,
which serializes, writes, and prunes. The queue holds 64 finished runs; if it is ever full, the
run is dropped and counted rather than blocking the agent. The queue is drained when the app or
`bw` exits. A run that would pass 4 MiB with content is written without its content
attributes and marked `blackwall.trace.content_removed`. A run is limited to 1,000 spans; spans
beyond that are counted in `blackwall.trace.dropped_spans`. Spans still open when a run is stopped
are written as `Cancelled.` with `error.type=cancelled`.

## File formats

**Traces:** `traces/<started-ms>-<request-id>.json`, one OTLP/JSON `ExportTraceServiceRequest`
per run, serialized from the official `opentelemetry-proto` messages. The newest 200 files are
kept. Files written by earlier versions use the same format and still list and open in
**Runs** and `bw traces`.

**Metrics:** `metrics/YYYY-MM-DD.jsonl` (UTC day), one OTLP/JSON `ExportMetricsServiceRequest`
per line with delta temporality, so each line holds only what happened since the previous line.
A line is appended every 60 seconds while measurements are pending, and when the app or `bw`
exits. This is the shape the OpenTelemetry Collector's `file` exporter writes and
its `otlpjsonfile` receiver reads, for example:

```yaml
receivers:
  otlpjsonfile:
    include: ["/Users/you/.blackwall/metrics/*.jsonl", "/Users/you/.blackwall/traces/*.json"]
```

## Usage page

`core::usage::summary(data_dir, from, to, offset_minutes)` reads `metrics/` for a range of the
viewer's local days and returns the totals the desktop **Usage** page and `bw usage` show: tokens per day and by type, rows per
project and per model (tokens, runs for projects, sessions, model calls, median output speed, p90
duration, and the busiest sessions), the Chat/Agent/Subagent split, the top 10 tools with error and
denied counts, run success, compaction savings, and gaps. Medians and p90s are read from histogram
buckets, so they are estimates within a bucket. Lines that cannot be parsed are skipped with one
warning per file, and histogram points whose bucket bounds differ from the rest are left out of
medians and p90s with one warning. `core::usage::export` copies into a new folder every
`metrics/` file whose UTC day overlaps the range and every trace started within it; a failed
export removes that folder. See [Usage](RUNTIME_GUIDE.md#usage).

**Local days.** Metrics files are named by the UTC day the line was exported, but the page and
`bw usage` count the viewer's local days: the desktop passes its current UTC offset, and `bw usage`
reads it from `date +%z` (UTC on Windows, or with `--utc`). Each data point is counted on the local
day of its timestamp, which is its export time, so the files from the day before to the day after
the range are read. A line covers up to the 60 seconds before it was exported, so use in the last
minute before local midnight can count on the next day. Points are bucketed with the offset in use
when the page is read, so after a daylight-saving change older days shift by that hour.

**The gaps line.** It reports history before the first metrics point, runs whose trace the writer
queue dropped, whether recording is off now, and the periods in the range when recording was off.
Turning recording on or off (the desktop's **Record agent runs**, or `bw config set traces`)
appends one line to `metrics/recording.jsonl`, such as
`{"unixMs":1793208600000,"recording":false,"surface":"desktop"}`; the desktop and `bw` have
separate switches, so their periods are listed separately. Changes made before this file existed
are unknown, and days with no data and no off period are shown as "no data", not as off. Only the
first 1 MiB of the file is read, and only `desktop` and `cli` markers count.

## Retention

Trace files: the newest 200. Metrics files: 400 days by default; set 30 to 3,650 days or forever
in **Settings → Security → Telemetry** (`usageRetention` in the preferences) or with
`bw config set usage-retention-days N|forever`. Files older than the window are deleted when
telemetry starts, so a shorter window takes effect at the next start. `metrics/recording.jsonl` is
not a metrics file and is kept.

## Export

Optional, and off until you set an endpoint. Blackwall then sends traces and usage metrics to any
OTLP/HTTP receiver: an OpenTelemetry Collector, Grafana Alloy, Jaeger, Langfuse, or a vendor
endpoint.

**Settings:** **Settings → Security → Telemetry** on the desktop (saved as `telemetryExport` in
the preferences), or `bw config set` for the CLI (saved in the CLI settings; `bw` uses the
desktop's until you set one of its own):

| Setting | Desktop | CLI | Default |
|---|---|---|---|
| Endpoint | Endpoint | `otlp-endpoint <url>` / `off` | off |
| Protocol | Protocol | `otlp-protocol json\|protobuf` | `http/protobuf` |
| Signals | Send traces / metrics | `otlp-signals traces,metrics` | both |
| Project names | Include project names and session IDs | `otlp-include-project on\|off` | off |
| Headers | Headers (value typed once) | `bw telemetry header <name>` (value on stdin), `--delete` | none |
| Test | Send test span | `bw telemetry test` | |

**Endpoint.** The base URL, as `OTEL_EXPORTER_OTLP_ENDPOINT` defines it: traces go to
`{endpoint}/v1/traces` and metrics to `{endpoint}/v1/metrics`. A URL ending in `/v1/traces` or
`/v1/metrics` is trimmed back to its base. `https` is required; plain `http` is accepted only for
the literal hosts `127.0.0.1`, `::1`, and `localhost`. A user name, password, query, or fragment is
refused, so no secret is ever saved with the URL.

**Headers.** Use headers for credentials such as `authorization` or `x-api-key`. Settings keep
only each header's name and the endpoint origin (`scheme://host:port`) it was entered for; the value
is stored in the credential store (service `com.blackwall.telemetry`, account
`{origin}|{name}`) and is never shown again, logged, or put in an environment variable. A value is
only ever read for its own origin, so it cannot follow a changed endpoint: after you point export at
a different origin, its headers show as "saved for <old origin>" and export pauses until you enter
them again for the new endpoint or remove them. Save the endpoint before adding a header.
`bw telemetry header` reads the value from piped standard input only, so it never lands in shell
history:

```sh
op read "op://Private/Langfuse/otlp-auth" | bw telemetry header authorization
bw telemetry header authorization --delete
```

A header whose value is missing, or that was saved for another origin, turns export off and shows
as the last error (`missing_header`, `header_other_origin`). Saving or removing a header applies at
once. Settings files never hold a value: a saved `telemetryExport` with a `headerValues` (or any
value- or secret-named) field is refused, while other unknown fields from newer versions are
ignored.

**What is sent.** Exactly what the local files hold. The run buffer finishes a run (content
filter, redaction, the whole-run secret scrub, the 4 MiB rule) once, then the writer thread hands
the same `ExportTraceServiceRequest` to the trace file and to OTLP; each metrics line is the same
`ExportMetricsServiceRequest` the file gets. So content attributes leave only when content capture
is on, and secrets found late in a run are scrubbed from every exported span. Exported metrics drop
`blackwall.project` and `blackwall.session` (and merge the series that differed only by them)
unless **Include project names** is on; the local files always keep them.

**Content.** With content capture on, export sends prompts, outputs, and tool data. Turning export
on (or moving it to a new endpoint) while **Include content** is on asks first on the desktop, and
`bw config set` prints a warning.

**How it is sent.** The messages are the official `opentelemetry-proto` types, encoded as OTLP
JSON or protobuf and POSTed by Blackwall's own small exporters on its reqwest client
(`opentelemetry-otlp` is not used). `HTTP_PROXY`, `HTTPS_PROXY`, and `ALL_PROXY` are ignored, as
for model and web requests. Redirects are never followed: a 3xx is an error. Each request
has a 10 s limit. A dedicated thread sends, fed by a queue of at most 2,048 spans and 64 metric
batches; when it is full, new data is dropped and counted, never waited for. Failed sends are not
retried. Export never slows, blocks, or fails a run, and failures are shown only in the export
status. When the app or `bw` exits, it waits up to 3 s for queued data.

**Export status.** The Usage page's **Export** card and the Telemetry settings show the endpoint,
the last successful send, the last error as its class and HTTP status (`connect`, `timeout`,
`redirect`, `http_status`, `missing_header`, …; response bodies are never read or shown), and the
spans and metric batches sent and dropped since the app started. **Send test span** sends one span
named `blackwall test span` with the saved settings (save changes first) and reports the HTTP
status.

### Examples

**OpenTelemetry Collector** (the `otlp` receiver on HTTP, printing what arrives):

```yaml
# otel.yaml
receivers:
  otlp:
    protocols:
      http:
        endpoint: 0.0.0.0:4318 # inside the container; the port is published on 127.0.0.1 only
exporters:
  debug:
    verbosity: detailed
service:
  pipelines:
    traces: { receivers: [otlp], exporters: [debug] }
    metrics: { receivers: [otlp], exporters: [debug] }
```

```sh
docker run --rm -p 127.0.0.1:4318:4318 -v "$PWD/otel.yaml:/etc/otelcol/config.yaml:ro" \
  otel/opentelemetry-collector:0.116.1
bw config set otlp-endpoint http://127.0.0.1:4318
bw telemetry test
```

For a collector on another machine, put it behind https, or forward it to loopback with
`ssh -N -L 4318:127.0.0.1:4318 <host>`. (The `0.116.0` image tag ships a binary that does not
start; use `0.116.1` or later.)

**Grafana Alloy** (receive OTLP/HTTP, forward to Tempo and Prometheus or Grafana Cloud):

```alloy
otelcol.receiver.otlp "blackwall" {
  http { endpoint = "127.0.0.1:4318" }
  output {
    traces  = [otelcol.exporter.otlp.tempo.input]
    metrics = [otelcol.exporter.prometheus.local.input]
  }
}
otelcol.exporter.otlp "tempo" {
  client { endpoint = "tempo:4317" }
}
otelcol.exporter.prometheus "local" {
  forward_to = [prometheus.remote_write.local.receiver]
}
prometheus.remote_write "local" {
  endpoint { url = "http://prometheus:9090/api/v1/write" }
}
```

```sh
bw config set otlp-endpoint http://127.0.0.1:4318
```

**Jaeger all-in-one** (traces only; Jaeger has no metrics endpoint):

```sh
docker run --rm -p 127.0.0.1:16686:16686 -p 127.0.0.1:4318:4318 jaegertracing/all-in-one:1.64.0
bw config set otlp-endpoint http://127.0.0.1:4318
bw config set otlp-signals traces
```

Open `http://127.0.0.1:16686` and pick the `blackwall` service.

**Langfuse** (traces only). The OTLP endpoint is `https://cloud.langfuse.com/api/public/otel`
(EU) or `https://us.cloud.langfuse.com/api/public/otel` (US), or `<your host>/api/public/otel` when
self-hosted. Authenticate with HTTP Basic auth of your public and secret key: keep
`Basic <base64 of public-key:secret-key>` in your password manager and pipe it in.

```sh
bw config set otlp-endpoint https://cloud.langfuse.com/api/public/otel
bw config set otlp-signals traces
op read "op://Private/Langfuse/otlp-authorization" | bw telemetry header authorization  # after the endpoint
bw telemetry test
```

Langfuse shows prompts and answers only for runs recorded with content capture on.
