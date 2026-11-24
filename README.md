# embarch-api

Part of the [EmbArch](https://github.com/gabrieltetar/embarch-doc) suite — a set of tools for firmware engineers that spans from software to the physical hardware bench.

`embarch-api` is the layer between an MCP client (e.g. [Claude Code](https://claude.com/claude-code), or any other MCP-speaking agent or tool) — or a human directly at a terminal, via its own CLI — and [`embarch-core`](https://github.com/gabrieltetar/embarch-core). Core owns the debug probe and serial connection and exposes a bearer-token-authed HTTP API; `embarch-api` gives it meaning to an agent or a human, and adds the one capability Core intentionally doesn't have — running a project's build and handing the resulting artifact to Core's flash endpoint. Core's endpoints are specified in [embarch-core's interface docs](https://github.com/gabrieltetar/embarch-doc/tree/main/embarch-core).

## What it does

- **MCP server** (stdio transport): exposes the project, build, flash, reset, serial and bench operations as MCP tools, for any MCP client — Claude Code included. The tools and their arguments are specified in [the tools interface doc](https://github.com/gabrieltetar/embarch-doc/blob/main/embarch-api/interfaces/tools.md).
- **CLI**: the same operations, invoked directly by a human at a terminal (kebab-case there, while the MCP tools are snake_case) — no agent required. `embarch-api --help` is authoritative.
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

This repo is the build-from-source path. To **use** EmbArch, follow [the user guide](https://github.com/gabrieltetar/embarch-doc/blob/main/suite/user-guide.md): download the release archive, run `embarch setup` to install Core and the binaries, then `embarch init` inside your firmware repo to create `embarch/embarch.toml`. `init` writes the project config and Core is found automatically, so there is no config to copy and no token to export by hand.

**This repo does not build on its own.** Two sibling repos must be cloned into the
same parent directory, because they are depended on by relative path:

| Sibling | Why |
|---|---|
| [`embarch-study-designer`](https://github.com/gabrieltetar/embarch-study-designer) | the shared study/registry type model, named by this repo's own `Cargo.toml` |
| [`embarch-topology`](https://github.com/gabrieltetar/embarch-topology) | the board registry — named not here but by `crates/embarch-core-client`, this repo's own sub-crate, so it is easy to miss |

So the layout cargo expects is `<parent>/embarch-api`, `<parent>/embarch-study-designer`,
`<parent>/embarch-topology`. Clone this repo on its own and `cargo build` fails with
`failed to read ../../../embarch-topology/Cargo.toml` — an error naming a path outside
this repo, which means nothing more than "the sibling is not there".

Path dependencies rather than git or registry ones is a deliberate choice, not an
oversight: `embarch-study-designer` decision 8 and `embarch-topology` decision 13.

```sh
cargo build --release
```

With the binary on your `PATH` and `embarch init` run in your firmware repo, the config is found by searching upward from the current directory for `embarch/embarch.toml`; outside a repo, pass `--config <path>` or set `EMBARCH_API_CONFIG`.

Run as an MCP server (no subcommand — this is what an MCP client spawns):

```sh
embarch-api
```

Run directly from a terminal, no MCP client involved (see the user guide's section on using it from a terminal):

```sh
embarch-api list-projects
embarch-api build-and-flash my-project
embarch-api --json status    # machine-readable output
```

The configuration schema is owned by [the config interface doc](https://github.com/gabrieltetar/embarch-doc/blob/main/embarch-api/interfaces/config.md); [config.example.toml](config.example.toml) is an annotated example.

## Design doc

The full design record — current truth, open questions, architecture decisions, configuration schema reference, tool surface, and CLI subcommand surface — lives in [embarch-doc/embarch-api/spec.md](https://github.com/gabrieltetar/embarch-doc/blob/main/embarch-api/spec.md) and its linked `decisions/`, `open.md` and `interfaces/` files, treated as the durable source of truth ahead of any chat history that produced it.

## License

MIT — see [LICENSE](LICENSE).
