# embarch-api

Part of the [EmbArch](https://github.com/gabrieltetar/embarch-doc) suite — a set of tools for firmware engineers that spans from software to the physical hardware bench.

`embarch-api` is the layer between an MCP client (e.g. [Claude Code](https://claude.com/claude-code), or any other MCP-speaking agent or tool) — or a human directly at a terminal, via its own CLI — and [`embarch-core`](https://github.com/gabrieltetar/embarch-core). Core owns the debug probe and serial connection and exposes four bearer-token-authed HTTP endpoints (`/status`, `/flash`, `/reset`, `/serial-log`); `embarch-api` gives those endpoints meaning to an agent or a human, and adds the one capability Core intentionally doesn't have — running a project's build and handing the resulting artifact to Core's `/flash`.

## What it does

- **MCP server** (stdio transport): exposes `list_projects`, `status`, `build`, `flash`, `build_and_flash`, `reset`, and `serial_log` as MCP tools, for any MCP client — Claude Code included.
- **CLI**: the identical six operations, invoked directly by a human at a terminal (`embarch-api build my-project`, `embarch-api flash my-project --firmware-path ...`, etc.) — no agent required.
- **Build orchestrator**: runs a project's configured build command (`west`, `arduino-cli`, or anything else — it's toolchain-agnostic) as a subprocess, checks the resulting artifact is actually fresh, and hands it to Core's `/flash`.

Both front-ends — MCP and CLI — converge on the same underlying modules; there's no privileged or special code path for either.

```
       build_command subprocess (west / arduino-cli / etc.)
                                  |
MCP client --stdio(spawn)--> embarch-api --HTTP+Bearer--> embarch-core --probe-rs/serialport--> hardware
                                  |
                  human, direct: `embarch-api <subcommand> ...`
```

## Scope

- Single-engineer scope: no multi-tenancy, no user/permission model. Each engineer runs their own full stack (own `embarch-api`, own `embarch-core`).
- Not a database-backed system — all state is a single TOML config file.
- Not toolchain-aware — it runs whatever command a project's config specifies, with no built-in understanding of `west`, `idf.py`, `arduino-cli`, or any other build tool.

## Getting started

Requires a running [`embarch-core`](https://github.com/gabrieltetar/embarch-core) instance and its `EMBARCH_TOKEN`.

```sh
cargo build --release
cp config.example.toml ~/.config/embarch/api.toml
# edit ~/.config/embarch/api.toml: set core.base_url, EMBARCH_TOKEN (via token_env),
# and add a [[projects]] entry per project you want to build/flash.
```

Run as an MCP server (no subcommand — this is what an MCP client spawns):

```sh
embarch-api --config ~/.config/embarch/api.toml
```

Run directly from a terminal, no MCP client involved:

```sh
embarch-api --config ~/.config/embarch/api.toml list_projects
embarch-api --config ~/.config/embarch/api.toml build my-project
embarch-api --config ~/.config/embarch/api.toml build_and_flash my-project
embarch-api --config ~/.config/embarch/api.toml --json build my-project   # machine-readable output
```

See [config.example.toml](config.example.toml) for the full configuration schema with inline documentation.

## Design doc

The full design record — architecture decisions, configuration schema reference, MCP tool surface, CLI subcommand surface, build orchestration details, and security model — lives in [embarch-doc/embarch-api/design.md](https://github.com/gabrieltetar/embarch-doc/blob/main/embarch-api/design.md), treated as the durable source of truth ahead of any chat history that produced it.

## License

MIT — see [LICENSE](LICENSE).
