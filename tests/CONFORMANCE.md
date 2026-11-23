# punkfs conformance gaps

The integration suite is written against the contract a mounted filesystem
must satisfy, not against current behaviour. A failing test therefore means
"punkfs does not yet behave like a filesystem here", and each failure prints
the syscall, the contract, and the observed error.

Run it with:

```sh
cargo test --test integration
```

A full run on the symlink implementation reported **109 passing, 14 failing**
in integration, plus one blackbox failure for remote truncate. Counts may
change as contract tests are added. Each failing test names a real gap.

## 1. `setattr` is refused, which breaks truncation and metadata (9 tests)

`src/filesystem/meta.rs::setattr_decision` returns `ENOSYS` unconditionally.
The backend `SetAttr` rpc exists and is tested at the store level, so this is
a deliberate one-function decision, not a missing capability.

Consequences, each with a failing test:

| syscall | test |
|---|---|
| `open(O_WRONLY\|O_TRUNC)` | `write_conformance::truncating_open_of_an_existing_file_succeeds` |
| `ftruncate(2)` | `write_conformance::ftruncate_shrinks_the_file` |
| `truncate(2)` | `write_conformance::truncate_by_path_succeeds` |
| `truncate -s` | `cli_conformance::truncate_command_shrinks_the_file` |
| `echo > file` | `cli_conformance::shell_redirection_truncates_an_existing_file` |
| `cp` onto a file | `cli_conformance::cp_over_an_existing_file_succeeds` |
| `chmod(2)` | `metadata_conformance::chmod_changes_the_mode_on_the_backing_file` |
| `chmod` CLI | `cli_conformance::chmod_updates_the_backing_mode` |
| `utimensat(2)` | `metadata_conformance::utime_sets_the_backing_mtime` |

This is the highest-impact gap: `>` redirection onto an existing file and
`cp` are the two most common ways a shell writes data.

Fix: apply `mode`/`size`/times through the existing store `setattr`, and keep
refusing only `uid`/`gid`. That is a small change in `setattr_decision`.

## 2. Open handles break when their path changes (2 tests)

Content rpcs are addressed by path (`ReadFile{path, offset, length}`), so an
open descriptor loses its target when the file is unlinked:

- `handle_conformance::unlink_keeps_open_descriptors_readable`
- `handle_conformance::unlink_keeps_open_descriptors_writable`

POSIX requires an unlinked file to remain readable and writable through
already-open fds until the last one closes. This needs handle identity on the
wire (server-side `open`/`release` with a handle id, or uuid-addressed
content rpcs).

Related, and currently *passing* by accident: `reading_a_file_removed_mid_stream_reports_cleanly`
accepts either the correct behaviour or a clean error.

## 3. Symlinks

Existing local and remote symlinks are listed, reported by `lstat`, and
resolved by the kernel using the stored target. Combined nodes can create
symlinks locally; client-only mounts still refuse creation. The conformance
tests cover relative and absolute targets, links crossing backend ownership,
dangling links, loops, byte-preserving targets, and link removal/rename.
Symlink GUIDs can fall back to a path-derived identity, which does not survive
an out-of-band rename; hardening concurrent namespace mutations remains open.

## 4. `rename` of a directory onto a non-empty directory (1 test)

`mutation_conformance::rename_directory_onto_non_empty_directory_fails_with_enotempty`

POSIX requires `ENOTEMPTY`. The server gets `io::ErrorKind::DirectoryNotEmpty`
and maps it to `FailedPrecondition`, which the client turns into a generic
`Io` error (`EIO`). Worse, the source ends up nested inside the destination.

Two fixes, both needed: map `DirectoryNotEmpty` to `ENOTEMPTY` on the client,
and make the server refuse the operation instead of performing it.

## 5. `rename` does not preserve entry identity (1 test)

`mutation_conformance::rename_preserves_the_inode_of_the_moved_entry`

`Store::rename` calls `after_mutation(old)` (which *removes* the entry) before
`rename_entry(old, new)`, so the moved entry is re-allocated a fresh local
inode. The ordering is wrong: invalidate the parents, then move the entry.

## 6. `access(2)` always succeeds (1 test)

`metadata_conformance::access_reports_a_read_only_file_as_not_writable`

`access` is not implemented, so the fuser trait default answers "allowed".
A program checking writability of a mode-0400 file is told yes and then fails
on write, which is worse than a plain refusal. Either implement `access` from
the cached mode, or return `ENOSYS` so libc falls back on the real `stat`
mode.

## 7. Multi-node: a duplicate name after a node returns (1 test)

`multinode_conformance::a_node_returning_with_a_duplicate_name_is_refused_not_guessed`

Fixed in this iteration: a node going down no longer deletes its entries
(`reconcile_from` only drops entries a node itself did not report), reading
from a down node reports `EHOSTDOWN`, and the mount keeps working.

Remaining gap: when a node comes back claiming a name another node already
serves, the merge keeps the first node's entry and serves it. The collision
*is* detected and logged to stderr (`duplicate name across nodes, refusing
to pick one: /collide.txt`) and the process stays alive, but the mount cannot
yet turn it into a visible failure. Picking a winner silently is the part
that is wrong: a caller gets one file's bytes with no indication the other
exists.

Closing it needs a decision on the namespace (refuse the whole directory?
suffix the loser?), which is why the test is left failing as a specification.

Fixed during manual verification: a directory present on two nodes was
reported as a duplicate name, which made any overlapping tree unusable.
Directories merge; only files can collide. Guarded by
`a_shared_directory_is_merged_without_being_reported_as_a_conflict`.

Note that `the_store_reports_a_duplicate_name_collision` passes: the store
*does* record conflicts and exposes them via `Store::conflicts()`. Only the
FUSE layer does not surface them yet.

## Deliberate non-conformance (not gaps)

These are product decisions, and the tests assert the refusal so a silent
change would be caught:

- Client-only mounts refuse entry creation. Combined-node mounts create regular
  files, directories, and symlinks locally; hard links and special files remain
  unsupported.
- Ownership changes (`chown`) are refused.
- Cross-node moves are refused with `EXDEV`. A rename that would move a file
  between backing nodes is a copy, not a rename. Directory moves are
  unchanged for now (still allowed), deferred rather than restricted.

See `mutation_conformance` for those tests.
