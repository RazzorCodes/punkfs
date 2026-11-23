# Agent instructions

## Test placement

- Prefer tests in the blackbox, functional, integration, and smoke suites over
  tests embedded in implementation files.
- Cover each behavior in blackbox, integration, and at least one of functional
  or smoke where those layers can exercise it. Do not count a test that only
  exercises a reference filesystem as punkfs coverage.
- Put a test in the same file as implementation code only when the relevant
  method or behavior cannot be tested adequately through those suite layers;
  explain the limitation when it is not obvious. Keep desired-behavior tests
  honest, including visible failures and explicit environment-dependent skips.

## Logging and operation correlation

These are requirements for the telemetry system and future implementation
work, not claims that the current code already provides them.

- Implement and use structured logging for meaningful events in new or
  changed functionality. Log operation outcomes and important lifecycle,
  availability, reconciliation and failure events with appropriate context;
  do not silently swallow errors. Do not log file contents or secrets.
- Support these severity levels in **descending order of importance**:
  `fatal`, `critical`, `error`, `warning`, `info`, `debug`, `trace`, following
  OpenTelemetry's severity ordering. The default threshold is `info`: it
  includes `fatal` through `info` and excludes `debug` and `trace`. Do not
  collapse `fatal` and `critical` into an indistinguishable severity in the
  recorded event; OpenTelemetry has one FATAL range, so preserve both with
  distinct severity numbers and text.
- Every log event supports a `console` flag which, when set, also prints the
  message to the console in addition to the configured local log destination.
  Apply the configured severity threshold first; `console` does not bypass it.
  The flag is not an instruction to terminate the process. Avoid duplicate
  console output when the primary destination is already the console.
- Every request operation must carry a correlation ID for its full lifetime,
  starting when it enters a punkfs process and ending when it completes.
  A FUSE callback is one ingress operation; a standalone incoming gRPC request
  is another. Carry the same ID through all child work, retries and fan-out,
  across synchronous and asynchronous boundaries, outbound gRPC calls, and
  into remote processes. Remote work must retain a valid incoming ID; create
  one at ingress when none is available, and validate malformed IDs according
  to the telemetry plan. Concurrent operations must not leak IDs into one
  another. Use the context-aware logger API for request-scoped events so the
  correlation ID is recorded. Correlation IDs are diagnostic context, not
  authorization, file identity, or idempotency keys.
- Make the first implementation local, while keeping event data and context
  suitable for a later OpenTelemetry integration. Do not make operation
  success depend on a remote telemetry service. Logging is diagnostic and is
  not a substitute for the post-MVP audit system.
- Add focused tests for new logging behavior, including severity filtering,
  console routing, and end-to-end correlation propagation when those features
  are implemented. Follow the project's existing tests and avoid claiming
  unsupported telemetry behavior is already available. See
  `.docs/telemetry-plan.md` for the implementation plan and acceptance tests.
- All agent-generated or agent-required docs exist in the gitignored .docs folder. Do not create another folder to host docs.
