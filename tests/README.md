# punkfs test layout

The suite is split by what each test drives and what it asserts:

| crate | what runs | what it asserts |
|---|---|---|
| `tests/smoke` | in-process, public API, no FUSE/no network/no subprocess | one behaviour per test (one function or function-branch per assertion) |
| `tests/functional` | the client `Store`/`MetadataCache` against a live `punkfs-server` process (the out-of-process mock system); no FUSE mount | a whole operation and its outcome (list -> reconcile -> read; a change on one node visible in the merged view) |
| `tests/integration` | a real FUSE mount over a real backing directory | the fixture acts like our mocks (POSIX behaviour through real syscalls); includes the **known-gap** conformance tests that pin the endgame contract and may fail today |
| `tests/blackbox` | the real `punkfs` node binaries started autonomously (as `scripts/playground.sh` does) | end-to-end scenarios (a folder built from several remote folders; a change on one is visible in the others). May all fail — we build the implementation toward them |

Internal-only tests that need `pub(crate)` access stay in `src/` `mod tests` (attr
translation, catalog merge internals, backing security, telemetry sink) and are
used sparingly.

## Running

```sh
cargo test --test smoke
cargo test --test functional
cargo test --test integration   # requires /dev/fuse + fusermount3
cargo test --test blackbox      # requires /dev/fuse + fusermount3
```

`tests/functional` and `tests/integration` share the harness in `tests/common/`
(`TestServer`, `TestCluster`, `Mount`, `MultiMount`).