# Bidirectional node mode

`punkfs` can mount one directory and serve that same directory in a single
process. Configure each peer directly on every node:

```sh
punkfs /srv/node-a node-a --listen 127.0.0.1:10001 node-b=http://127.0.0.1:10002
punkfs /srv/node-b node-b --listen 127.0.0.1:10002 node-a=http://127.0.0.1:10001
```

The local directory is opened before FUSE covers it. Its descriptor is shared
between the local adapter and the inbound gRPC service; inbound calls never go
through the merged mount or forward to peers. Each node can see its own and its
configured peers' files in its merged view, but a peer's file must be requested
from its owner directly. On-demand directory listings merge reachable peers and
retain previously cached names from unreachable peers. New files and directories
are created on the node whose mount received the request; existing file I/O is
routed to the cached owner. There is no payload replication or forwarding chain.

The CLI without `--listen` still starts a client-only local mount. The standalone
`punkfs-server` remains available. Library callers should use
`PunkFsService::try_new` for configurable roots: the compatibility `new`
constructor now opens the root immediately and panics if it cannot be opened.
Node IDs must be unique and nonempty, and the
local ID must not also be configured as a peer. The combined service rejects
the dev-only gRPC shutdown method. Listen addresses are explicit: the example
uses loopback; set an appropriate address and peer URLs for a trusted network.

`./scripts/playground.sh` starts three nodes and mounts. Each `node-N` directory
is both that node's mount and its covered backing directory. Writes can be
inspected in the original directory after shutdown. By default the script stops
the nodes on Enter; `--keep` prints the command to stop them. Route logging
includes paths and is explicitly enabled in the playground; use it only when
filenames are not sensitive. Telemetry files are outside all three mounts.

Startup opens the backing directory and binds the listener before mounting; a
failure does not leave a newly mounted view. A root already on FUSE is refused
in node mode, including other FUSE filesystems, to avoid serving a covered
mount. This is not a general verification of disk identity or nested mounts.
SIGINT/SIGTERM requests unmount and server shutdown; unsuccessful or timed-out
cleanup is reported. FUSE unmount and server draining each have a five-second
budget; a timeout does not guarantee that a blocked OS operation completed.

Outgoing RPCs have two-second connection/request deadlines. Directory listings
fan out with bounded concurrency and a 2.5-second aggregate budget; a missing
response retains known metadata rather than declaring deletion. A timed-out
mutation may have succeeded at its owner and is not automatically retried.

## Important limitations

This remains an experimental trusted-environment prototype, not a safe sole
copy of data. There is **no peer authentication or authorization**. More
importantly, lexical path checks do **not** confine symlink traversal beneath
the export: a symlink in a backing tree can lead outside it, including through
the inbound service. Use only trusted backing trees and peers, keep the
listener off untrusted networks, and do not serve secrets or unrelated files.
Descriptor-relative confinement remains necessary before considering this a
secure or generally safe export boundary.

There is no handshake or verification that a peer address still belongs to its
configured node ID. Do not reuse peer addresses for different exports while
nodes are running. Reassigning an address can route writes to the wrong node.
There is no periodic metadata publication yet: changes are discovered during
on-demand listing. The in-memory catalog does not remember nodes that have
never answered after restart. Conflict recovery and reliable backing-disk
identity checks remain unfinished. The current implementation also keeps a
separate small Tokio runtime for the synchronous client store; consolidating
runtime ownership is future work. See `docs/peer-node-plan.md` for the next
milestones and `tests/CONFORMANCE.md` for existing filesystem gaps.
