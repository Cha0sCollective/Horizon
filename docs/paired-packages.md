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

## Static state preview

Captured paired block declarations may add `state-definition` alongside their
ordinary `template`, `properties` and `item` fields. For example:

```json
{
  "template": "minecraft:iron_block",
  "state-definition": {
    "format": 1,
    "behavior": "static",
    "properties": [
      {"name": "axis", "values": ["x", "y", "z"], "default": "y"},
      {"name": "enabled", "values": ["false", "true"], "default": "true"}
    ]
  }
}
```

Only the single-state iron-block template is supported by this preview. Every
tuple inherits its geometry and physics; properties are explicit state data, with
no machinery, custom shape, orientation-aware placement or rotation behavior.
Static structure rotations/mirrors preserve each tuple. Supply matching ordinary
blockstate/model resources for all combinations. The example's default is its
fourth canonical state (`axis=y,enabled=true`), not its first state.

Names and values are bounded lowercase ASCII tokens. Property names/values sort
lexicographically; the rightmost property varies fastest. Unknown fields, duplicate
names/values, invalid defaults and unsupported formats reject. A declaration is
at most 64 KiB, with 16 properties, 64 values per property and 4,096 combinations.
Captured packages retain at most 65,536 expanded static states; the resource
materializer separately enforces the total world state limit including vanilla.
The combined vanilla/package property vocabulary is limited to 4,096 names, leaving
headroom in the engine's key table. `waterlogged` is unsupported because the host
assigns fluid semantics to it independently of this static template.

Legacy folders/zips and feature-disabled builds refuse this new field. This is an
experimental content format; it does not add a stable SDK API or machine save schema.
