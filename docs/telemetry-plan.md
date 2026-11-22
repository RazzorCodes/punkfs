# Local-first telemetry plan

**Status:** the local logger and request correlation are implemented for the
supported request paths. The metadata cache emits diagnostic events; broader
application instrumentation and OTel export are not yet implemented. See
`docs/telemetry.md` for configuration and operational limits.

## Goal and boundaries

Provide structured local diagnostic logging now, with an event and context
model that can later be exported to OTel without changing operation call sites.
The logger must not require a network connection or remote collector. Diagnostic
logs are not the post-MVP audit system, and this plan does not implement audit,
metrics, distributed tracing export, or an OTel collector.

## Event contract

- The severity order, highest first, is `fatal`, `critical`, `error`, `warning`,
  `info`, `debug`, `trace`, following OTel ordering. The default threshold is
  `info`: it records the first five levels and excludes `debug` and `trace`.
  Thresholds are inclusive.
- Every event includes a boolean `console` flag. After severity filtering, a
  true flag copies its human-readable message to the console in addition to
  the local destination. If the primary destination is already the same console,
  do not print it twice. A false flag does not suppress a console that is the
  configured primary destination. The flag does not imply process termination.
- Record an explicit severity value, timestamp, message, component, operation
  when relevant, correlation ID when in an operation, and structured context
  such as node ID, outcome, error category and elapsed time when available.
  Record process/service instance identity to distinguish separate processes.
- Do not log file contents, credentials or secrets. Whether paths are emitted
  should be configurable; avoid using paths, GUIDs or correlation IDs as
  metric labels. Lifecycle events outside a request need not invent an
  operation correlation ID.
- Fatal and critical describe severity; neither automatically exits or panics.
  Callers decide how to respond to a failure.

The proposed local destination is newline-delimited JSON, with a configured
file path, bounded rotation/retention and a bounded asynchronous writer. File
logging is the default destination; select stderr explicitly where a file is
not practical. Console mirroring uses stderr. Define file size, retention,
queue capacity, startup failure and shutdown/flush policies as configuration;
do not claim durable or lossless logging. Never write logs under a punkfs
mount in a way that could recurse into the filesystem.

## Context and operation lifetime

An **operation** is one incoming request, not an entire application command:
for example, a FUSE callback starts one operation, while `cat` may trigger many
callbacks and therefore many IDs. A direct gRPC request without an upstream
context starts its own operation. Background work has its own root context
unless it is causally initiated by an existing operation.

1. At FUSE ingress, generate a new UUID v4 correlation ID before delegated
   work begins. At gRPC ingress, accept a valid incoming ID or generate one
   if absent. Use a dedicated lowercase `x-punkfs-correlation-id` ASCII
   metadata field containing a canonical lowercase UUID v4 string.
2. Reject malformed, duplicate or oversized supplied IDs with gRPC
   `INVALID_ARGUMENT`; never silently treat them as valid or log their raw
   value. Missing metadata is valid for direct clients. Check for duplicate
   metadata explicitly. This is a proposed protocol behavior to implement and
   test, not a claim about the current server.
3. Keep the same ID across all suboperations, gRPC calls, retries and fan-out
   for the lifetime of that ingress operation. A remote server retains it and
   passes it on for any further calls. Give child spans their own span IDs;
   do not replace the operation correlation ID with a span ID.
4. Complete the context when the request has finished, including error and
   cancellation paths. Instrument background work explicitly if it outlives
   the request, recording the causal correlation ID without pretending the
   original request is still active.
5. Correlation is diagnostic metadata, not a file GUID, authorization token,
   or idempotency key. Do not rely on its uniqueness or trust it for security.

Use an explicit clonable operation context at boundaries rather than relying
on thread-local state. This is important because FUSE callbacks are
synchronous, the store uses `Runtime::block_on`, and work may be spawned or
cross runtime threads. If `tracing` spans are used, instrument futures rather
than holding an entered-span guard across `await`. Fan-out clones the same
context; separate concurrent ingress operations must never share it.

## Implementation design

1. Add `src/telemetry/` with typed `Severity`, `OperationContext`, structured
   event and configuration types, a logger interface, filtering and local
   sinks. Keep the application-facing API independent of collector details.
2. Use `tracing` and `tracing-subscriber` if helpful for spans and dispatch,
   with a severity representation that preserves all seven levels. The
   built-in `tracing` levels alone cannot distinguish `fatal` and `critical`
   from each other and from `error`. Ensure framework filtering does not drop
   events before their actual severity is evaluated. Console routing must read
   the explicit event flag, not infer it from severity.
3. Initialize the logger in `punkfs-server` and the mount entry points. The
   library must not install a global subscriber as a side effect; callers and
   tests must be able to inject or initialize telemetry without duplicate
   global initialization. Keep any writer/flush guard alive until shutdown.
4. Introduce context at every FUSE adapter and gRPC server ingress; pass it
   explicitly through decision functions/store operations and each Tonic
   `Request` metadata map. Do not add correlation fields to every protobuf
   payload. Cover all RPCs, including retry/reachability probes and fan-out.
   Direct store/library entry points without a FUSE request need a documented
   root-context entry point rather than implicitly reusing another request.
5. Add events to new or modified behavior as required by `AGENTS.md`. The
   metadata cache is instrumented; other components remain future work.

### Failure isolation

Keep file I/O and any future exporter off request paths with bounded queues.
A full queue or failed sink must not block indefinitely or make a filesystem
operation succeed or fail differently. Specify and expose dropped-event and
sink-failure counters through a non-recursive diagnostic mechanism. Startup
must report an unusable configured destination clearly; whether it fails
startup or falls back to stderr must be an explicit configuration choice.
Shutdown performs a bounded flush and reports incomplete flushing where
possible. Never assume a log write has audit-grade durability.

## OTel extension point

The local event schema keeps correlation, severity text and structured fields
independent of the sink. Later, add an optional adapter/exporter (preferably
behind a feature) and a Collector for backend routing. The application must
continue to work without either. An OTel trace ID/span ID is separate from the
punkfs correlation ID; retain the latter as an attribute. Future W3C
`traceparent` propagation can coexist with the punkfs metadata header, without
changing correlation IDs.

Use OTel's severity ordering everywhere. The proposed punkfs-to-OTel mapping
preserves the requested seven distinct levels:

| punkfs severity | OTel severity number | OTel category |
|---|---:|---|
| `fatal` | 24 | FATAL |
| `critical` | 21 | FATAL |
| `error` | 17 | ERROR |
| `warning` | 13 | WARN |
| `info` | 9 | INFO |
| `debug` | 5 | DEBUG |
| `trace` | 1 | TRACE |

OTel has no separate CRITICAL category: the two FATAL-range values are a
punkfs convention. Preserve `fatal` and `critical` in severity text as well
as their distinct numbers. Backends that group the whole FATAL range into
one category can still collapse the display; use the text or number when the
distinction matters.

The eventual Collector-to-Grafana or Kusto/Azure ingestion configuration is a
separate integration choice. The local implementation makes no promises about
backend availability or delivery guarantees.

## Acceptance tests and rollout

1. Exhaustively test every threshold against all seven levels: `debug`
   includes `debug` and more severe levels but excludes `trace`, while a
   `trace` threshold includes all levels. Verify fatal and critical stay
   distinct.
2. Test console true/false, filtered events, stderr as primary destination,
   and no duplicate console messages. Test structured JSON fields and that
   message data is not confused with log metadata.
3. Test absent, valid, malformed, duplicate and oversized gRPC correlation
   headers; verify separate incoming operations get separate IDs.
4. Test a real FUSE ingress to store to gRPC server path: one ID appears in
   client and remote events, including failure, retry and multiple-node
   fan-out. Test standalone gRPC ingress and a subsequent outbound hop.
5. Run concurrent requests and async/thread handoffs; assert no correlation
   leakage. Test cancellation/error completion and context lifetime.
6. Exercise bounded queue saturation, sink failure, retention/rotation,
   shutdown flush and telemetry-disabled/no-collector operation without
   changing filesystem outcomes. Use deterministic test sinks where possible.
7. When an OTel adapter is added, verify correlation attributes and severity
   mapping independently of any real remote service.

Deliver in slices: local schema/sinks and tests; ingress/context propagation;
operation instrumentation by future implementers; optional OTel adapter only
when an actual integration is needed. No live collector is required for MVP.
