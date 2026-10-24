+++
title = "Telemetry"
weight = 12
[extra]
group = "Reference"
+++

# Telemetry

Maki can export OpenTelemetry metrics and events to a collector you run. It is
off by default, and once enabled it only ever sends data to the endpoint you
configure.

The format matches Claude Code's telemetry, down to the environment variable
names, so a dashboard you already built mostly works.

## What gets exported

Two signals:

- **Metrics**: counters for sessions, tokens, cost, lines changed, permission
  decisions, commits, pull requests, and time spent working.
- **Events**: one OTLP log record per prompt, API call, API error, tool result
  and permission decision.

## What does not

- No prompt text and no tool input, unless you ask with `log_user_prompts` or
  `log_tool_details`. Tool input is the whole
  input: `bash` commands, `write` content, `edit` strings, file paths. Only
  turn these on while debugging.
- No model output, and no provider error bodies: an API failure reports its
  status code, because the body is often the request echoed back.
- No environment variables.
- No user or organisation identity. Maki has no idea who you are and does not
  invent an id either. If you want team labels, add them yourself through
  `resource_attributes`.

## Quick start

Run a collector on the usual ports, then in `init.lua`:

```lua
maki.setup({
    telemetry = {
        enabled = true,
        metrics_exporter = "otlp",
        logs_exporter = "otlp",
        protocol = "grpc",
        endpoint = "http://localhost:4317",
    },
})
```

For HTTP instead of gRPC, set `protocol = "http/protobuf"` and
`endpoint = "http://localhost:4318"`. Without `protocol` and `endpoint`, the
SDK sends OTLP/JSON over HTTP to `http://localhost:4318`.

For HTTP, maki appends `/v1/metrics` and `/v1/logs` to `endpoint`, as the OTLP
spec says to. A per-signal endpoint is used exactly as written.

No collector yet? Set `metrics_exporter` and `logs_exporter` to `"console"`
and everything is written to the maki log file as OTLP/JSON instead.

## How it works

```
emit() --+--> counters, summed in place --> metrics reader thread --> collector
         |
         +--> bounded event queue -------> event processor thread --> collector

(one atomic load when disabled)
```

Maki exports through the official OpenTelemetry Rust SDK. A call site does one
relaxed atomic load, and when telemetry is off that is the whole cost. When it
is on, a metric is added to its counter right away and an event goes into a
bounded queue. If the queue is full the event is dropped. Exports run in the
background on the SDK's own threads, so a slow collector cannot stall a turn.

A failed export is retried up to three times with backoff, and the collector's
`Retry-After` is honored. `timeout_ms` bounds a whole export with its retries.
An export that still fails ends up as a line in the log file. On exit, maki
waits at most two seconds for the last export. A shutdown that runs out of
time logs `otel shutdown timed out` and drops what was still being sent.

## Configuration

Settings come from two kinds of environment variables and the `telemetry`
table in `init.lua`. The generated
[Configuration](/docs/configuration/#telemetry) reference lists every key with
its variable and default.

Maki reads its own switches: `MAKI_ENABLE_TELEMETRY`, `OTEL_SDK_DISABLED`,
`OTEL_METRICS_EXPORTER`, `OTEL_LOGS_EXPORTER`, the four content flags such as
`OTEL_LOG_USER_PROMPTS`, and `MAKI_OTEL_CONTENT_MAX_LENGTH`. For these the
environment variable wins over `init.lua`. `OTEL_SDK_DISABLED=true` turns
telemetry off no matter what anything else says, so you can disable it across
a whole team without editing anyone's `init.lua`.

Every other `OTEL_*` variable, like `OTEL_EXPORTER_OTLP_ENDPOINT`,
`OTEL_EXPORTER_OTLP_HEADERS`, `OTEL_SERVICE_NAME` or
`OTEL_BLRP_SCHEDULE_DELAY`, is read by the OpenTelemetry SDK itself. The
[SDK environment variables](https://opentelemetry.io/docs/specs/otel/configuration/sdk-environment-variables/)
and [OTLP exporter](https://opentelemetry.io/docs/specs/otel/protocol/exporter/)
specs describe them, and the
[`opentelemetry-otlp`](https://docs.rs/opentelemetry-otlp/0.33.1/opentelemetry_otlp/)
and [`opentelemetry_sdk`](https://docs.rs/opentelemetry_sdk/0.33.1/opentelemetry_sdk/)
docs say which ones the Rust SDK supports. Maki passes each `init.lua` key that
is set to the SDK in code, and code wins over the variable, even a generic
`init.lua` key over a per-signal variable: `endpoint` in `init.lua` replaces
`OTEL_EXPORTER_OTLP_METRICS_ENDPOINT`, while a per-signal protocol variable
still applies, so set both in the same place. Headers are the
exception: the SDK adds the headers from the environment on top of the ones
from `init.lua`, and an environment header replaces an `init.lua` header of the
same name.

```lua
maki.setup({
    telemetry = {
        enabled = true,
        metrics_exporter = "otlp",
        logs_exporter = "otlp",
        protocol = "grpc",
        endpoint = "http://localhost:4317",
        headers = { ["x-api-key"] = "secret" },
        resource_attributes = { team = "core", env = "dev" },
    },
})
```

Exporter values are `otlp`, `console`, `none`, or a comma-separated mix, and a
repeat is ignored. Protocols are `grpc`, `http/protobuf` and `http/json`.
Per-signal keys like `metrics_endpoint` or `metrics_headers` replace the
generic one, as the per-signal variables do. Durations are in milliseconds.
Intervals must be above zero, because at zero the SDK's export threads would
spin for the whole session. The SDK allocates the event queue up front, so
`logs_max_queue_size` and `OTEL_BLRP_MAX_QUEUE_SIZE` accept at most 65536.
`compression = "none"` leaves compression to `OTEL_EXPORTER_OTLP_COMPRESSION`,
which is off by default.

The HTTP exporter percent-decodes header values, so write a literal `%` as
`%25`. To replace the SDK's `User-Agent` header, spell the key exactly
`User-Agent`.

An endpoint needs a scheme. For gRPC, `http://` means cleartext h2c, which is
what collectors expect on port 4317, and `unix://` is a local socket. With
either protocol, `https://` means TLS checked against the operating system's
trust store. The SDK takes a gRPC endpoint without a scheme as `https://`,
unless `OTEL_EXPORTER_OTLP_INSECURE=true`.

Over gRPC, a set `SSL_CERT_FILE` replaces the operating system's trust store.
HTTP uses the platform verifier, which does the same on Linux but ignores
`SSL_CERT_FILE` on macOS and Windows. The Rust
SDK does not support `OTEL_EXPORTER_OTLP_CERTIFICATE`.

`service_name` wins over a `service.name` key in `resource_attributes`, and
both win over `OTEL_SERVICE_NAME` and `OTEL_RESOURCE_ATTRIBUTES`. If none of
them names the service, it is `maki`.

## Changes from earlier versions

Maki used to ship its own OTLP exporter. Moving to the official SDK changed
these defaults and settings:

- `init.lua` now wins over the standard `OTEL_*` variables. Maki's own
  switches still win over `init.lua`.
- `protocol` and `endpoint` are optional, with the SDK defaults above.
- Metrics use cumulative temporality unless you set
  `metrics_temporality = "delta"`.
- Events are flushed every second instead of every five seconds.
- `OTEL_LOGS_EXPORT_INTERVAL` is gone. Use `OTEL_BLRP_SCHEDULE_DELAY` or
  `logs_interval_ms`.
- `metrics_export_timeout_ms` and `logs_export_timeout_ms` are gone, so
  remove them from your config. `OTEL_METRIC_EXPORT_TIMEOUT` and
  `OTEL_BLRP_EXPORT_TIMEOUT` have no effect.
- `timeout_ms` bounds a whole export with its retries and defaults to 10
  seconds. It used to bound one request, with 30 seconds for the whole export.
- Per-signal headers replace the generic `headers` rather than merging
  with them.
- `protocol` and `compression` values are case-sensitive: `grpc` works,
  `GRPC` turns telemetry off.
- Maki no longer reads `SSL_CERT_FILE`. Trust comes from the TLS setup of
  each SDK transport.
- An endpoint without a scheme is no longer taken as `http://`.
- Durations are no longer floored at 100ms.
- Most bad values in SDK variables are ignored rather than turning telemetry
  off. Only the enum-like ones, such as protocol or compression, log a
  warning. An invalid endpoint still turns it off.
- `OTEL_RESOURCE_ATTRIBUTES` is no longer percent-decoded, and a
  `service.name` in it beats `OTEL_SERVICE_NAME`, as in the Rust SDK. Maki's
  `service.version`, `os.type` and `host.arch` now win over the same keys in
  it.
- `telemetry.sdk.name` is `opentelemetry` instead of `maki-otel`, and
  `telemetry.sdk.version` is the SDK's version instead of maki's.

## Resource

Every export carries `service.name`, `service.version`,
`telemetry.sdk.{name,language,version}`, `os.type` and `host.arch`, plus
anything you add through `resource_attributes` or `OTEL_RESOURCE_ATTRIBUTES`.
On a key collision, `resource_attributes` wins over maki's own keys, which win
over `OTEL_RESOURCE_ATTRIBUTES`. The `telemetry.sdk` keys describe the
OpenTelemetry SDK, so the name is `opentelemetry` and the version is the SDK's.

## Standard attributes

Every metric and event carries `terminal.type`. Events also carry `session.id`,
`app.version`, `event.name` and `event.sequence`, a counter that orders events
emitted in the same nanosecond. The time of an event is on the record itself,
as `timeUnixNano`.

Metrics get `session.id` and `app.version` only when you ask for them, because
both multiply metric cardinality. `session.id` is on by default, `app.version`
is not.

## Metrics

All of them are monotonic sums with cumulative temporality by default. The SDK
keeps at most 2000 attribute combinations per metric for the life of the
process, or per export with delta temporality, and folds the rest into a
single point marked `otel.metric.overflow=true`.

| Metric | Unit | Attributes |
| --- | --- | --- |
| `maki.session.count` | | `start_type` = `fresh`, `resume`, `continue` |
| `maki.token.usage` | tokens | `type` = `input`, `output`, `cacheRead`, `cacheCreation`, plus `model` and `provider` |
| `maki.cost.usage` | USD | `model`, `provider` |
| `maki.lines_of_code.count` | | `type` = `added`, `removed` |
| `maki.tool.decision` | | `tool_name`, `decision` = `accept`/`reject`, `source` |
| `maki.commit.count` | | |
| `maki.pull_request.count` | | |
| `maki.active_time.total` | s | `type` = `cli` |

`maki.cost.usage` is an estimate from the model's price table. A model with no
published price contributes nothing.

Claude Code counts decisions only for edit tools. Maki's permission model
covers every tool, so `maki.tool.decision` carries a `tool_name` and a
`source` saying where the decision came from: `rule`, `yolo`, `user_once`,
`user_session`, `user_always`, or `user_abort` when the prompt never got an
answer.

`maki.active_time.total` measures how long the agent was working, from the
moment a prompt is accepted until the run ends, whether it succeeded or not.
There is no keyboard-idle tracking yet, so no `type=user`.

Subagents are excluded from this metric and from `maki.user_prompt`. They run
inside their parent's time window and nobody typed their prompt, so counting
them would inflate busy time past wall clock and count prompts that were never
written.

## Events

All events are OTLP log records at severity INFO. The payload is in
attributes, and the body is empty.

| Event | Attributes |
| --- | --- |
| `maki.user_prompt` | `prompt_length`, and `prompt` only with `log_user_prompts` |
| `maki.api_request` | `model`, `provider`, `input_tokens`, `output_tokens`, `cache_read_tokens`, `cache_creation_tokens`, `cost_usd`, `duration_ms`, `stop_reason` |
| `maki.api_error` | `model`, `provider`, `error`, `status_code`, `attempt`, `duration_ms` |
| `maki.tool_result` | `tool_name`, `tool_source`, `success`, `duration_ms`, `error_type`, and `tool_input` only with `log_tool_details` |
| `maki.tool_decision` | `tool_name`, `decision`, `source` |

`duration_ms` on `maki.tool_result` is wall clock time: a tool that sat behind
a permission prompt includes the wait for your answer.

`error_type` is a coarse bucket (`timeout`, `not_found`, `permission_denied`,
`invalid_input`, `cancelled`, `error`) rather than the raw message, so it
stays useful as a group-by.

`error` follows the same idea. A provider's error body often just echoes your
request back, so an HTTP failure reports `API error (429)` plus the status
code. Errors raised by maki itself, like a stream timeout, are reported
verbatim.

There is no `request_id`: maki's providers do not expose response headers yet.

## Verifying

The cheapest check is the console exporter. Run maki with
`metrics_exporter = "console"`, do something, quit, and look for
`otel console export` in the log file.

Against a real collector, `maki.session.count` should appear within one metrics
interval (60 seconds by default). Shorten it while testing with
`metrics_interval_ms = 5000`.

## Troubleshooting

Telemetry problems never show up in the UI. They all go to the log file, so
start there.

**Nothing arrives.** Check that `enabled` is set and that an exporter is not
`none`. Maki logs `telemetry enabled` at startup when it is actually on.

**`telemetry disabled` in the log.** A setting failed to parse or the SDK
could not build an exporter. The message names the key or the setting, the
value it got, and what it expected. An endpoint that is not a valid URL ends up
here too, except a per-signal HTTP endpoint without a scheme: that one builds,
and every export then fails.

**`PeriodicReader.ExportError` or `BatchLogProcessor.ExportError`.** An export
failed after its retries. The line before it, from `opentelemetry-otlp`, says
whether the retries ran out (`Export.Failed.Exhausted`), the export deadline
passed (`Export.Failed.DeadlineExceeded`), or the collector rejected the
request outright (`Export.Failed.NonRetryable`), as it does for bad
credentials or a wrong path. Over HTTP, the cause of a `network error`, such as
a refused connection or an untrusted certificate, and the collector's response
body are only logged at debug level, so run with `MAKI_LOG=debug` to see them.

**gRPC fails immediately.** With an `http://` endpoint, maki speaks cleartext
h2c with prior knowledge, which is what collectors expect on port 4317. A
collector behind TLS needs `https://`. If yours supports neither, switch to
`protocol = "http/protobuf"` and port 4318.

**`BatchLogProcessor.LogDroppingStarted` in the log.** Events are produced
faster than the collector accepts them, and the SDK logs this at the first
drop. The total count is logged at shutdown. Raise `logs_max_queue_size`, or
shorten `logs_interval_ms` so batches go out more often.

**Exports look truncated.** `content_max_length` caps prompt and tool input
text at 10 KB by default.

Related pages: [Configuration](/docs/configuration/#telemetry),
[Token Economy](/docs/token-economy/).
