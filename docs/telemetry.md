# Local telemetry

The telemetry module supplies a structured local logger and per-operation
correlation. The metadata cache emits events, but broader filesystem and gRPC
instrumentation and an OTel exporter are not yet implemented. The design and
future acceptance criteria are in `telemetry-plan.md`.

## Configuration

The server and mount examples initialize a logger at startup. The default
threshold is `info`; the inclusive order is `fatal`, `critical`, `error`,
`warning`, `info`, `debug`, `trace`.

| Environment variable | Meaning |
|---|---|
| `PUNKFS_LOG_LEVEL` | One of the seven levels above; `warn` is also accepted as an alias for `warning`. |
| `PUNKFS_LOG_DESTINATION` | `file` (default) or `stderr`. |
| `PUNKFS_LOG_FILE` | Explicit file path when using the file destination. |
| `XDG_STATE_HOME` / `HOME` | Used for the default file location. |

The default path is `$XDG_STATE_HOME/punkfs/<service>-<pid>.jsonl`, or
`$HOME/.local/state/punkfs/<service>-<pid>.jsonl`. Each process writes its
own file, avoiding races between server processes. The default queue has
1,024 records; files rotate at 10 MiB and keep three previous files per
process. `telemetry::Config` permits callers to configure these values when
embedding the library. Use a separate explicitly configured file for every
process: concurrent writers and rotation of a shared file are unsupported.

Rotation bounds each process's files, **not the total across process
restarts**: operators must remove old PID-named files. An invalid configuration
or inability to open the configured destination fails startup; select `stderr`
explicitly if a file is unavailable. Do not put the log destination inside a
punkfs mount.

Events are filtered before routing. A true per-event `console` flag mirrors
the message to stderr if the primary destination is a file; it does not bypass
the threshold or duplicate an event whose primary destination is stderr.
The bounded queue drops events when full or oversized. `Logger::stats()`
reports dropped records and sink failures; telemetry failures do not affect
filesystem operation results. `flush` and guard shutdown are bounded and
best-effort, not a durability guarantee. Forced termination cannot guarantee
flush, and the current executables do not install graceful signal handlers.

The metadata cache emits an `info` initialization event and recovery events,
`warning` events for unreachable nodes and conflicts, and an `error` when no
node answers a listing. Reconciliation summaries and cache mutations use
`debug`; individual upserts use `trace`. Events include counts and relevant
node IDs, but not paths, GUIDs or file contents. Request-driven cache events
retain the originating operation ID. A direct cache caller can use the
`*_in` methods with an existing `OperationContext`; the ordinary methods create
a new context. A cache can use the global logger or an explicitly supplied
logger via `MetadataCache::with_logger`.

## Correlation

Each implemented FUSE callback creates a new UUID v4 operation ID. Its scoped store
passes that ID in the `x-punkfs-correlation-id` gRPC metadata header through
all store RPCs, including availability probes and multi-node listings. An
incoming gRPC request validates and preserves the ID, or generates one when
absent; a successful response returns it in the same metadata header. Invalid
or duplicate IDs receive `INVALID_ARGUMENT`. Direct store callers can pass an
`OperationContext` to the `*_in` methods; their ordinary methods create a
fresh context. A scoped FUSE decision can retrieve its operation context via
`StoreApi::operation_context()` for future diagnostic events, and can pass it
to `Logger::emit_in`. FUSE operations handled by fuser's default trait methods
are not yet wrapped in a correlation context.

An ID represents one FUSE or gRPC ingress operation, not a whole shell
command, and is diagnostic context rather than a security or idempotency key.
There is no OTel span/trace propagation yet. `fatal` and `critical` remain
separate values in OTel's FATAL severity range.
