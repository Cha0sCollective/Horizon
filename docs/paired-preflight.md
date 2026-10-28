# Experimental paired-folder preflight

The optional `horizon-mods/paired-packages` feature checks a proposed package with
one content owner and two executable declarations. It is a library prototype:
the game does not discover, activate or execute it. Existing `.hmod` files and
`horizon-mod.toml` keep their current format. No WIT API number is allocated.

An experimental folder contains `horizon-paired.toml`, `client.wasm`, `server.wasm`
and its ordinary `assets/`/`data/`. Descriptor format 1 requires exact contract
`paired-preview-1`, with `[content]`, `[client]` and `[server]` sections. Content
uses the existing manifest fields with API 0.2, side `both` and capability
`registries` only. Each code section declares its fixed entry name, existing API
line, capabilities and experimental capabilities. Client code uses API 0.1/0.2;
server code uses API 0.3. Neither executable claims the registry a second time.

The library reuses existing manifest permission checks and content file/path/size
validation. Descriptors are capped at 64 KiB, each nonempty code file at 32 MiB;
code entries must be regular files. Both halves are required even for a server-only
plan. Namespace collision checks accept paired and legacy content owners together.
The caller passes each package's content owner once, independently of code count.

Plans select client code for a client and server code for a server. The in-frame
fallback selects both, requiring separate instances/sandboxes when execution is
implemented. Default singleplayer must request a client plan in its window and a
server plan in its integrated server. A plan itself performs no registration or
world mutation.

`PreparedBundle` contains bounded, **unvalidated Wasm bytes**. It is not permission
to execute them. The subsequent admission step must type/link-check both worlds,
enforce declared imports/API functions, compute executable/content identity, reject
incompatible peers before writable-world admission and instantiate only the selected
side with separate state/storage. Quotas must not be silently expanded. Folder
capture pins bytes; archive admission and atomic publisher replacement remain future work.

With both `runtime` and `paired-packages`, `Mods::preflight_pair` compiles the
captured code, checks declared interface capabilities and supported API functions,
and validates required exports with generated client/server `*Pre` bindings.
It uses pre-instantiation only: no Store or instance is made and no guest callback
or init runs. The returned `CheckedPair` exposes immutable compiled components and
their prepared declarations. It does not perform peer/world admission, discovery,
registry registration or side activation.

`PreparedBundle::read_folder` now captures its descriptor, both code files and
accepted content bytes through directory handles, refusing symlinks and special
files. It validates declarations through the existing `Content` validator using
those captured bytes. Later `Content::reader` calls use memory even if the source
is edited or removed. Cloning content shares captured bytes via `Arc`; host-owned
parsed metadata remains ordinary Rust data. `Content::source` is an origin label
for snapshots, never their byte source or cache key. The resource-cache stamp uses
`snapshot_identity`, leaving legacy folder/archive metadata behavior unchanged.

The experimental identity scheme is `paired-folder-sha256-1`: SHA-256 over its
ASCII name, one zero byte, then a little-endian u64 entry count. Entries are the
descriptor, client and server in that order, followed by content in sorted relative
path order. Each entry is framed as u64 path-byte length, UTF-8 path bytes, u64
content-byte length and content bytes. Integers are little-endian. Descriptor
whitespace and all captured resources count; origins and enumeration order do not.
An equal digest identifies captured bytes, not trust, guest validity or admission.

Raw content stays within the existing 16 MiB/file and 512 MiB/content limits; code
is at most 64 MiB total and the descriptor 64 KiB. Parsed metadata and allocations
add overhead; a reader may copy one bounded file. This prototype retains that
payload in memory rather than creating a second mutable staging directory. Folder
traversal additionally caps depth at 64 and content entries (including directories)
at 65,536; root scans also cap entries at 65,536. Multi-package admission still needs
an aggregate memory policy. Capture is not an atomic filesystem epoch: a concurrent
writer may produce a mixed version, whose exact captured bytes are validated and
identified. A later capture is a new snapshot; existing snapshots do not change.

No machine lifecycle, inventory, save, replication or retained rendering API is
implemented here. This format remains experimental until those concrete callers
and admission checks exist. It is deliberately absent from `manifest::find`, so a
parser-only success cannot accidentally load a bundle into the legacy runtime.

Focused checks:

```sh
cargo test -p horizon-mods --features paired-packages
cargo clippy -p horizon-mods --all-targets --features paired-packages -- -D warnings
cargo test -p horizon-mods --features paired-packages,runtime,pack
# Needs prepared resources; fails if they are absent, does not launch a game:
cargo test -p horizon-client --lib --features paired-packages captured_content_root -- --ignored
```
