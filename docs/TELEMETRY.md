# Telemetry

Blackwall records local traces and usage metrics with the OpenTelemetry SDK
(`opentelemetry_sdk` 0.33.1), using the OpenTelemetry generative AI semantic conventions. Every
resource and instrumentation scope carries the conventions' schema URL
(`https://opentelemetry.io/schemas/1.42.0`, from `opentelemetry-semantic-conventions` 0.33.1).
Everything stays in the data folder (`~/.blackwall`, or `BLACKWALL_DATA_DIR`); nothing is sent
anywhere.

Telemetry follows the existing traces switch: **Settings → Workflows → Runs → Record agent runs**
on the desktop (`tracesEnabled`), `bw config set traces on|off` in the CLI. It is on by default.
Turning it off stops both traces and usage metrics. Content capture (`traceContent`,
`bw config set trace-content on|off`) is off by default and only affects traces.

No `OTEL_*` environment variable changes this behaviour. Blackwall gives the SDK its sampler, span
limits, resource, and export interval explicitly and does not use the SDK's environment resource
detectors.

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

`blackwall.*` is our namespace for what the conventions do not define yet.

## Attributes

**Resource:** `service.name=blackwall`, `service.version`, `blackwall.surface=desktop|cli`.

**Model-call metrics:** `gen_ai.operation.name=chat`, `gen_ai.request.model`,
`gen_ai.provider.name`, `server.address` (host only), `blackwall.mode=chat|agent|subagent`.
Token usage adds `gen_ai.token.type=input|output`. Duration adds `error.type` on failure, an error
class such as `network`, `http`, or `invalid_response`. `gen_ai.provider.name` is `ollama` for an
endpoint on port 11434 and `openai_compatible` otherwise.

**Tool metric:** `gen_ai.tool.name` (MCP tools as `server__tool`) and
`blackwall.tool.outcome=ok|error|denied|timeout`.

**Run metric:** `blackwall.mode` and
`blackwall.run.outcome=complete|error|cancelled|iterations|denied`.

**Spans:** `gen_ai.operation.name`, `gen_ai.agent.name`, `gen_ai.agent.id` (subagents),
`gen_ai.request.model`, `gen_ai.response.finish_reasons`, `gen_ai.usage.input_tokens`,
`gen_ai.usage.output_tokens`, `gen_ai.tool.name`, `gen_ai.tool.call.id`, `gen_ai.tool.type`,
`error.type`, `blackwall.request_id`, `blackwall.mode`, and `approval_requested` events with
`blackwall.approval.kind`. With content capture on, spans may also carry
`gen_ai.output.messages`, `gen_ai.tool.call.arguments`, and `gen_ai.tool.call.result`.

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
the redaction scrub to the whole run and checks the 4 MiB limit, and only then hands the finished
run to the file exporter. A run that would pass 4 MiB with content is written without its content
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
A line is appended when a run finishes, every 60 seconds while measurements are pending, and when
the app or `bw` exits. This is the shape the OpenTelemetry Collector's `file` exporter writes and
its `otlpjsonfile` receiver reads, for example:

```yaml
receivers:
  otlpjsonfile:
    include: ["/Users/you/.blackwall/metrics/*.jsonl", "/Users/you/.blackwall/traces/*.json"]
```

## Retention

Trace files: the newest 200. Metrics files: 400 days by default. Files older than the retention
window are deleted when telemetry starts. In core, `Retention::Days(n)` (clamped to 30–3,650) or
`Retention::Forever` sets the window; the Settings and `bw config` controls arrive with the Usage
page.
