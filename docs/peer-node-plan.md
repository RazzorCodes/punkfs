# Bidirectional punkfs nodes: implementation plan

Status: process integration is in progress. The combined process, shared
pre-mount backing descriptor, deadlines, and three-node playground are
implemented. Descriptor-relative path confinement and shared runtime ownership
are not yet implemented; see `docs/node.md` for the current limitations. Peer
handshake and periodic metadata publication remain future milestones. None of
these stages replicates file contents or changes where new files are placed.

## Contract

One `punkfs` process owns one backing directory, mounts one merged view over
that directory, and listens on one gRPC address. All nodes run the same binary.
A node can read or modify another node's entries through its mount, using the
existing operation support and limitations. The serving node accesses **only
its own backing directory**, never its merged mount or a peer. If A sees a
file from C in its mount, B cannot get that file by asking A's backing service;
B must be configured to talk directly to C. There is no loopback RPC for local
I/O and no multi-hop forwarding. A new file or directory is still created on
the node whose mount received the create. Two peers can independently create
conflicting names; refuse ambiguous access rather than guessing an owner.

Keep one peer set per mount for now: configure the three nodes of a playground
to talk directly to the other two. A stable configured node ID identifies the
export in this one-export-per-node model; IPs/ports are locations, and FUSE
inode numbers never leave the local catalog. The backing GUID identifies a
file on its owner, not a second copy of its contents.

## 1. Run the backing service in the mounting process

### Code boundaries

| Component | Responsibility | Must not depend on |
|---|---|---|
| `backing` (new crate-internal module) | Own the pre-mount directory descriptor, resolve relative paths, perform local I/O, assign/read backing GUIDs and return shared attributes | FUSE, the merged catalog, gRPC, peer state |
| `filesystem/passthrough` | Route local operations through the backing object; enforce merged namespace rules and update the catalog after local mutations | An outbound RPC to itself |
| `api/grpc/service.rs` | Validate ingress context, translate wire types/errors, run backing I/O off async worker threads | The merged `Store`, routing, the mountpoint path |
| `core/store` | Own the merged catalog, select the owner and fetch from it; accept owner-scoped listing observations | The inbound service or process lifecycle |
| Node runner (new internal module, called by `main.rs`) | Own configuration, listener, mount session, runtime, startup and shutdown | Filesystem operation policy |

Today `Local` opens the directory before mount but uses a `PunkFsService` to
extract attributes; the service itself stores a `PathBuf`. Invert that
relationship. Open the backing once and give both adapters an `Arc` of the
same object. Do not give the service the mountpoint string or canonicalize a
`/proc/self/fd` path back to that string. Extract resolution, metadata and GUID
handling first, then move overlapping list/read/write/sync/mutation operations
in small commits. Preserve intentional differences in policy in the adapters
(e.g. the merged mount refuses remote unlink). Reuse the shared attribute
types; don't duplicate wire conversions in the backing layer or introduce a
second general-purpose store trait. Handle concurrent first observation of a
file by keeping the existing valid GUID or reading back the winner of an
exclusive xattr assignment.

Pinning a root descriptor avoids FUSE recursion but does not itself confine
symlinks. For the new combined service, enforce the single-export boundary
with descriptor-relative resolution that cannot follow paths out of the root.
Keep metadata operations on a final symlink distinct from following its
target; refuse unsupported traversal rather than fall back to the merged
path. An already covered or overlapping backing/mount configuration is not a
supported topology. Continue to document the trusted-host/network limitation:
path scoping is not peer authentication or user authorization.

### Runtime, startup and failure handling

- Proposed CLI: `punkfs <mountpoint> <node-id> --listen <addr> [<peer-id>=<url> ...]`.
  Without `--listen`, keep the existing client-only CLI behavior. Reject empty
  or repeated IDs and self-peers; prefer a loopback listener in examples, and
  never implicitly bind a public interface. Keep `punkfs-server` for existing
  tests and legacy standalone usage, sharing the same backing implementation.
  Do not expose its test shutdown RPC in combined mode, even in dev builds.
- Validate config, open and validate the root, bind the listener, build lazy
  outbound connections, then mount using the same backing object. Start serving
  the bound listener when local setup succeeds. Fail locally and roll back if
  bind or mount fails; no peer need be up to start. Test readiness by calling the
  gRPC service, not merely by connecting to the listening socket.
- Use one multi-thread Tokio runtime owned by the synchronous node runner.
  Inject a runtime handle for the combined mode's outbound client; leave the
  existing library constructors compatible. Synchronous FUSE callbacks may
  block waiting on RPCs from their FUSE thread; async server handlers and future
  peer workers await async work, not the synchronous `Store::block_on` path.
  Run filesystem work on bounded blocking workers. Do not hold a catalog lock
  across disk work, network calls, or `.await`.
- Set connection and RPC deadlines. For merged listings, use bounded parallel
  fan-out with one listing budget, then reconcile the results; do not multiply
  a serial timeout by every offline peer. Keep results from answering nodes and
  retain cached entries from non-answering nodes. An existing-file mutation
  requires a known, unambiguous owner; do not try another peer on failure. A
  timed-out mutation can have taken effect; report failure/uncertainty, do not
  retry it automatically.
- On SIGINT/SIGTERM or unexpected termination of the mount or server, stop
  admitting new work, request unmount and graceful server shutdown, and wait
  for both within a documented shutdown budget. Keep runtime and backing alive
  while work drains; flush telemetry last. Report a missed shutdown deadline
  rather than claiming that all work completed. A remote outage is not grounds
  for shutting down the local mount or listener. Until Phase 2, peer endpoints
  are trusted static configuration; don't reassign an address to another export
  while clients are running.

### Phase 1 acceptance

- Start A with B offline and verify A's mount and RPC service can use A's files.
  Start B and verify reads in both directions through the mounts; verify a
  local write and a remote append affect only the physical owner's file.
- As a direct gRPC client of A, verify B-only names are absent and cannot be
  read via A even after A has cached them. Verify listing and reading A's own
  entries through gRPC while FUSE covers the backing directory (no recursion).
- Test path scoping, bind/mount rollback, signal cleanup and bounded delay on an
  unresponsive peer. Retain the existing local passthrough, standalone-server,
  and correlation coverage.
- Rewrite the playground to launch **three `punkfs` processes, three backing
  directories total**, each backing its own merged mount. No fourth store or
  separate server processes. Print the three mount paths; show local versus
  remote routes from each perspective. Use temp directories and live FUSE for
  end-to-end coverage; also test the backing and service without FUSE.

## 2. Handshake and liveness

Add a separate peer-control service to the same listener; leave the existing
file RPCs as the data plane. Start with statically configured peers: discovery,
automatic enrollment, and address learning are not part of `Hello`. Both
sides can dial independently, with deadlines and jittered backoff, so either
startup order works without requiring a synchronous callback during `Hello`.

`Hello` exchanges the claimed stable node ID, a new instance ID generated at
each process start, the supported protocol version/capabilities, and the state
of the backing export. Check that the identity matches the configured peer;
reject self, unknown peers, incompatible versions and mismatched identities.
An ID check is not authentication. A matching ID alone does not prove that the
expected disk is mounted: use an explicitly configured backing identity where
that matters, and do not treat failed validation as an empty export.

Track `unreachable`, `incompatible`/`identity mismatch`, `reachable but backing
not ready`, and `ready` separately from the freshness of individual directory
listings. Probe periodically (reuse `Hello` initially); transport loss or a
changed instance ID invalidates the verified session and triggers targeted
revalidation. Bind post-handshake data RPCs to the expected owner and instance;
validate before touching disk, especially for mutations. Checking `Hello` once
is not enough when a channel can reconnect to a different process. Preserve
cached ownership while offline or incompatible. Keep legacy clients in an
explicit compatibility path, not silently trusted after a peer-mode handshake
fails. Test restart, reversed startup order, wrong identity at the same
address, and reachable-but-unready storage. A restart should not be interpreted
as a new owner or a deletion.

## 3. Publish metadata changes periodically

Start with **push invalidations, pull authoritative directory listings**.
Publishing a compact list of changed directories gives peers a timely update
without adding a second metadata merge protocol, streaming file contents or
requiring a durable change log. Begin with `/` and directories peers have
listed, not a recursive scan of the entire disk on every tick. Record affected
parents on successful local and inbound mutations (both parents on rename).
Poll the observed directories on a bounded, jittered schedule to also catch
out-of-band changes; compare names, GUIDs and relevant attributes, not atime
alone. Reconnect re-establishes the observed set and reschedules cached
listings. Unobserved directories are fetched on demand; there is no promise of
an up-to-date inventory of an untouched subtree.

- Each notice carries the publisher ID, instance ID, sequence and a bounded
  batch of dirty directory paths (or `resync required`). Use a cursor per
  receiver; resending a notice is idempotent. An ack means the receiver recorded
  the invalidation, not that refresh succeeded. If a queue overflows, a gap is
  detected or a publisher restarts, refresh that peer's observed directories
  rather than replay unbounded history. Validate that notices come from the
  configured, handshaken session and describe only that sender's export.
- Send periodic, manual and on-demand refreshes through one owner-scoped
  reconcile path. Schedule/coalesce by `(owner, directory)` and use a local
  invalidation generation so a result started before a newer invalidation or
  known mutation cannot overwrite it. Invalidate old-instance work on
  reconnect. Route FUSE through the same cache, not a parallel catalog. Serve
  known listings from cache where possible; fetch on demand for first access
  and use bounded revalidation before changes that depend on fresh ownership.
- Only a complete listing from the validated export can remove that owner's
  missing children; a removed directory removes just that owner's remembered
  descendants. Distinguish confirmed directory absence on a ready export from
  a partial scan, timeout, permission denial or missing backing disk. Failed
  refresh retains last-known entries. Introduce an explicit snapshot outcome
  for confirmed absence if existing `ListDirectory` responses can't establish
  it safely; never turn arbitrary RPC errors into empty listings. A complete
  directory observation is not an atomic snapshot against concurrent external
  writers; polling repairs changes that race with a scan.
- Before enabling background reconciliation, retain every owner contribution
  at a path (including shared directories and duplicates). The current cache
  drops some contenders and keeps a sticky conflict flag; refreshing it faster
  cannot make conflicts recover correctly. Derive conflict state from retained
  contributions so a validated removal can clear it, but keep ambiguous names
  unavailable while an offline contender remains. Automatic indexed rename is
  a separate change; publication never renames or overwrites data.
- Test push-driven refresh without filesystem access, direct backing changes,
  cross-parent renames, duplicate/missed notices, out-of-order refreshes,
  peer restarts, and partial listings. A disconnection must not remove a known
  name or route a write to a different owner. Assert eventual convergence within
  the configured healthy scan/refresh window, not instantaneous consistency.

## Observability and scope

Keep the existing operation context across FUSE → Store → gRPC → backing. A
handshake attempt, scan or publication started by a worker has its own ingress
context; keep it through the RPC and link any separately scheduled/coalesced
refresh rather than attributing unrelated work to one request. Log structured
lifecycle events, peer transitions, timeouts and reconciliation outcomes with
node IDs and counts. Don't log data or secrets, and keep paths out of routine
peer/cache diagnostics. Put logs outside all mounts/exports used by punkfs.

The current baseline is 114 unit tests passing and 126 integration tests
passing with 16 known conformance failures. Track the named failures, not only
the count. Process integration does not fix remote truncate/chmod, symlinks,
open handles after unlink, or other unrelated POSIX gaps. Catalog persistence,
crash-safe mutation recovery, indexed conflict rename, full inventory scans,
replication, peer discovery and authentication are separate milestones.
