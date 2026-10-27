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
side with separate state/storage. Quotas must not be silently expanded. File identity
must remain coherent between validation and execution; the prototype does not solve
concurrent package replacement or archive admission.

With both `runtime` and `paired-packages`, `Mods::preflight_pair` compiles the
captured code, checks declared interface capabilities and supported API functions,
and validates required exports with generated client/server `*Pre` bindings.
It uses pre-instantiation only: no Store or instance is made and no guest callback
or init runs. The returned `CheckedPair` exposes immutable compiled components and
their prepared declarations. It does not certify mutable content files or perform
peer/world admission, discovery, registry registration or side activation.

No machine lifecycle, inventory, save, replication or retained rendering API is
implemented here. This format remains experimental until those concrete callers
and admission checks exist. It is deliberately absent from `manifest::find`, so a
parser-only success cannot accidentally load a bundle into the legacy runtime.

Focused checks:

```sh
cargo test -p horizon-mods --features paired-packages
cargo clippy -p horizon-mods --all-targets --features paired-packages -- -D warnings
```
