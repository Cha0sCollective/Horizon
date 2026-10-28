# Experimental paired folders

Build the game with `--features mods,paired-packages` to enable the local folder
prototype. The feature is off by default. Legacy `.hmod` behavior remains unchanged
in builds without it. Paired zip packages are unsupported.

A folder has `horizon-paired.toml`, `client.wasm`, `server.wasm`, and optional
`assets/` and `data/` content. The descriptor uses format `1` and host contract
`paired-preview-1`. Its `[content]` is an API `0.2`, side `both`, registries-only
manifest. `[client]` declares entry `client.wasm`, API `0.1` and client capabilities;
`[server]` declares entry `server.wasm`, API `0.3` and server capabilities. Each
executable has its own capability and experimental opt-ins. They share the content
owner's identity, version and namespace ownership; neither executable owns content.

Discovery rejects selected ambiguous formats, duplicate IDs and namespace owners.
Disabled pairs read declarations only. Selected descriptor, code and content are
captured once; the same immutable snapshots supply registration and execution,
including resource reload and the integrated server. Both halves and optional UI
callbacks are type-checked before any guest starts. One installed row displays
the union of requested permissions. Selection and payload changes require restart.

The client runs only client hooks; its integrated server runs only server hooks.
The in-frame fallback runs both in separate Stores. Each Store retains the existing
64 MiB memory limit, fuel and I/O quotas. Storage lives under separate `client/`
and `server/` directories below the package's settings directory, with existing
per-side storage quotas. Two Stores consume two sets of these budgets; this is
not an aggregate package resource policy. Settings storage remains nontransactional
and is not authoritative world or inventory persistence.

Pair initialization publishes buffered output only if every selected side starts.
A fault stops both Stores of that package and discards that failed callback's
buffered output. These rules do not implement general world transactions or undo
settings writes already completed by an earlier successful call.

This prototype admits local singleplayer only. External clients, LAN publication
and dedicated network startup with selected pairs refuse before network admission;
package identity negotiation is not implemented. The memory connection shares the
same startup snapshots. Mutable legacy content owners cannot be mixed with active
pairs; legacy code-only packages can coexist.

New local worlds record exact paired identities and the host executable hash in
`horizon-paired-packages.json`. Missing/changed packages, a changed executable,
invalid records and unmarked existing-world migration are refused before upgrade,
chunk decoding or world writes. The check also runs in builds without this feature,
so disabling it cannot silently open a paired world. This is admission metadata,
not a machine save format, a migration tool or peer authentication.
