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

- No prompt text and no tool input, unless you ask with
  `OTEL_LOG_USER_PROMPTS` or `OTEL_LOG_TOOL_DETAILS`. Tool input is the whole
  input: `bash` commands, `write` content, `edit` strings, file paths. Only
  turn these on while debugging.
- No model output, and no provider error bodies: an API failure reports its
  status code, because the body is often the request echoed back.
- No environment variables.
- No user or organisation identity. Maki has no idea who you are and does not
  invent an id either. If you want team labels, add them yourself through
  `OTEL_RESOURCE_ATTRIBUTES`.

## Quick start

Run a collector on the usual ports, then:

```
export MAKI_ENABLE_TELEMETRY=1
export OTEL_METRICS_EXPORTER=otlp
export OTEL_LOGS_EXPORTER=otlp
export OTEL_EXPORTER_OTLP_PROTOCOL=grpc
export OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:4317
maki
```

For HTTP instead of gRPC:

```
export OTEL_EXPORTER_OTLP_PROTOCOL=http/protobuf
export OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:4318
```

Maki appends `/v1/metrics` and `/v1/logs` to that endpoint, as the OTLP spec
says to. A per-signal endpoint is used exactly as written.

No collector yet? Set the exporter to `console` and everything is written to
the maki log file as OTLP/JSON instead:

```
MAKI_ENABLE_TELEMETRY=1 OTEL_METRICS_EXPORTER=console maki
```

## How it works

```
call sites --try_send--> bounded queues --> background task --> collector
   emit()                 events                aggregate
   (one atomic load        measurements         batch
    when disabled)                              retry
```

A call site does one relaxed atomic load, and when telemetry is off that is
the whole cost. When it is on, the value goes into a bounded channel with
`try_send`; if the channel is full the value is dropped and counted, and the
count is logged once per export interval. Exports run on a background task, so
a slow collector cannot stall a turn, and a failed export ends up as a line in
the log file.

## Configuration

Every setting has an environment variable and a matching key in the
`telemetry` table of `init.lua`. **The environment variable wins.**

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

| Env var | Lua key | Default |
| --- | --- | --- |
| `MAKI_ENABLE_TELEMETRY` | `enabled` | off |
| `OTEL_SDK_DISABLED` | - | `false` |
| `OTEL_METRICS_EXPORTER` | `metrics_exporter` | `none` |
| `OTEL_LOGS_EXPORTER` | `logs_exporter` | `none` |
| `OTEL_EXPORTER_OTLP_PROTOCOL` | `protocol` | none, required for `otlp` |
| `OTEL_EXPORTER_OTLP_ENDPOINT` | `endpoint` | none, required for `otlp` |
| `OTEL_EXPORTER_OTLP_HEADERS` | `headers` | empty |
| `OTEL_EXPORTER_OTLP_TIMEOUT` | `timeout_ms` | 10000 |
| `OTEL_EXPORTER_OTLP_COMPRESSION` | `compression` | `none` |
| `OTEL_EXPORTER_OTLP_METRICS_*` | `metrics_*` | inherit the generic value |
| `OTEL_EXPORTER_OTLP_LOGS_*` | `logs_*` | inherit the generic value |
| `OTEL_METRIC_EXPORT_INTERVAL` | `metrics_interval_ms` | 60000 |
| `OTEL_METRIC_EXPORT_TIMEOUT` | `metrics_export_timeout_ms` | 30000 |
| `OTEL_LOGS_EXPORT_INTERVAL` | `logs_interval_ms` | 5000 |
| `OTEL_BLRP_SCHEDULE_DELAY` | `logs_interval_ms` | 5000 |
| `OTEL_BLRP_MAX_QUEUE_SIZE` | `logs_max_queue_size` | 2048 |
| `OTEL_BLRP_MAX_EXPORT_BATCH_SIZE` | `logs_max_export_batch_size` | 512 |
| `OTEL_BLRP_EXPORT_TIMEOUT` | `logs_export_timeout_ms` | 30000 |
| `OTEL_EXPORTER_OTLP_METRICS_TEMPORALITY_PREFERENCE` | `metrics_temporality` | `delta` |
| `OTEL_SERVICE_NAME` | `service_name` | `maki` |
| `OTEL_RESOURCE_ATTRIBUTES` | `resource_attributes` | empty |
| `OTEL_METRICS_INCLUDE_SESSION_ID` | `metrics_include_session_id` | `true` |
| `OTEL_METRICS_INCLUDE_VERSION` | `metrics_include_version` | `false` |
| `OTEL_LOG_USER_PROMPTS` | `log_user_prompts` | `false` |
| `OTEL_LOG_TOOL_DETAILS` | `log_tool_details` | `false` |
| `MAKI_OTEL_CONTENT_MAX_LENGTH` | `content_max_length` | 10240 |

All durations are in milliseconds and floored at 100ms, so a zero cannot make
the export loop busy-spin. Exporter values are `otlp`, `console`, `none`, or
a comma-separated mix; a repeat is ignored. Protocols are `grpc`,
`http/protobuf` and `http/json`. Headers parse as `k=v,k2=v2` with
percent-decoded values.

Setting `OTEL_SDK_DISABLED=true` turns telemetry off no matter what anything
else says, so you can disable it across a whole team without editing anyone's
`init.lua`.

Per-signal settings override the generic one, but only within the same source:
the environment always beats `init.lua`, so `OTEL_EXPORTER_OTLP_ENDPOINT`
overrides a `metrics_endpoint` written in Lua. Headers merge instead: a
per-signal header replaces the generic one with the same key and the rest
stay. That is a deliberate departure from the spec, where per-signal headers
replace the generic list outright.

`OTEL_SERVICE_NAME` wins over a `service.name` in `OTEL_RESOURCE_ATTRIBUTES`.
If neither is set, the service is `maki`.

The types and descriptions of the Lua keys are in the generated
[Configuration](/docs/configuration/#telemetry) reference.

## Resource

Every export carries `service.name`, `service.version`,
`telemetry.sdk.{name,language,version}`, `os.type` and `host.arch`, plus
anything you add through `OTEL_RESOURCE_ATTRIBUTES`. Your attributes win if a
key collides.

## Standard attributes

Every metric and event carries `terminal.type`. Events also carry `session.id`,
`app.version`, `event.name` and `event.sequence`, a counter that orders events
emitted in the same nanosecond. The time of an event is on the record itself,
as `timeUnixNano`.

Metrics get `session.id` and `app.version` only when you ask for them, because
both multiply metric cardinality. `session.id` is on by default, `app.version`
is not.

## Metrics

All of them are monotonic sums with delta temporality by default.

| Metric | Unit | Attributes |
| --- | --- | --- |
| `maki.session.count` | | `start_type` = `fresh`, `resume`, `continue` |
| `maki.token.usage` | tokens | `type` = `input`, `output`, `cacheRead`, `cacheCreation`; `model`; `provider` |
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

All events are OTLP log records at severity INFO. The payload is in attributes;
the body is empty.

| Event | Attributes |
| --- | --- |
| `maki.user_prompt` | `prompt_length`, and `prompt` only with `OTEL_LOG_USER_PROMPTS` |
| `maki.api_request` | `model`, `provider`, `input_tokens`, `output_tokens`, `cache_read_tokens`, `cache_creation_tokens`, `cost_usd`, `duration_ms`, `stop_reason` |
| `maki.api_error` | `model`, `provider`, `error`, `status_code`, `attempt`, `duration_ms` |
| `maki.tool_result` | `tool_name`, `tool_source`, `success`, `duration_ms`, `error_type`, and `tool_input` only with `OTEL_LOG_TOOL_DETAILS` |
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
`OTEL_METRICS_EXPORTER=console`, do something, quit, and look for
`otel console export` in the log file.

Against a real collector, `maki.session.count` should appear within one metrics
interval (60 seconds by default). Shorten it while testing:

```
export OTEL_METRIC_EXPORT_INTERVAL=5000
```

## Troubleshooting

Telemetry problems never show up in the UI. They all go to the log file, so
start there.

**Nothing arrives.** Check that `MAKI_ENABLE_TELEMETRY` is set and that an
exporter is not `none`. Maki logs `telemetry enabled` at startup when it is
actually on.

**`telemetry disabled` in the log.** A setting failed to parse. The message
names the key, the value it got, and what it expected.

**gRPC fails immediately.** Maki speaks cleartext h2c with prior knowledge,
which is what collectors expect on port 4317. If yours does not, switch to
`OTEL_EXPORTER_OTLP_PROTOCOL=http/protobuf` and port 4318.

**`otel queue full` warnings.** Events are produced faster than the collector
accepts them. Raise `OTEL_BLRP_MAX_QUEUE_SIZE`, or shorten
`OTEL_LOGS_EXPORT_INTERVAL` so batches go out more often.

**Exports look truncated.** `MAKI_OTEL_CONTENT_MAX_LENGTH` caps prompt and tool
input text at 10 KB by default.

Related pages: [Configuration](/docs/configuration/#telemetry),
[Token Economy](/docs/token-economy/).
