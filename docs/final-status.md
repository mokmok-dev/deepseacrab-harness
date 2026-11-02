# Final status

This rewrite covers the headless harness. It is eighteen crates and 40 first-party tools in a run, plus dynamic LSP, SSH, and MCP tools when configured. The desktop application and the browser-only client product are out of scope by request; a local browser client is present.

## Verified

- `cargo test --workspace`: 820 tests pass.
- `cargo clippy --workspace --all-targets -- --deny warnings`: clean.
- `cargo fmt --all -- --check`: clean.
- `nix flake check`: clippy, test, treefmt, and pre-commit pass.
- `cargo-mutants`: 1350 mutants, 1006 caught, 344 unviable, 0 missed, 0 timeouts.

The counts come from per-crate mutation runs. The command is `./scripts/mutants.sh`.

## Implemented surface

| Area | Status |
|---|---|
| Agent loop, streaming, cancellation, retry, compaction | Complete for this scope |
| SQLite session log, fork, export, search, repair, header | Complete |
| Messages adapter for the built-in route and configured routes, SSE, tools, thinking, inline images | Complete |
| Provider routes, selection, and route-keyed credentials | Complete |
| CLI, browser client, JSON-RPC SDK | Complete for this scope |
| Filesystem, search, web, image, patch, worktree tools | Complete |
| Subagent, workflow, Ralph, jobs, schedules, teams | Complete |
| MCP, LSP, SSH, persistent PTY, bubblewrap `bash` | Complete for this scope |
| Local credentials and telemetry | Complete |
| Permission modes, lexical path policy, tool panic containment | Complete |

## Remaining specialized surfaces

These are separate programs rather than one missing slice.

- **ACP**: a JSON-RPC automation server with codecs, session multiplexing, approval prompts, and semantic updates. The upstream package is a full protocol bridge; it is not a thin CLI wrapper.
- **Remote API/BFF**: remote access, workspace-files transfer, a job controller, and authentication. The local browser client already serves the local JSON and SSE routes.
- **Browser use and computer use**: a browser automation runtime and an OS input/screenshot runtime. Neither dependency exists in this workspace.
- **Plugin manager and market**: this rewrite composes compiled-in Rust plugins. Loading arbitrary third-party code needs a dynamic ABI or a WASM runtime, plus a trust and signature model.
- **i18n**: the CLI and docs are English. Localizing them is a content pass across every crate, not a runtime switch.
- **M5 hardening**: cross-platform checks beyond Linux, session-format migrations with a real predecessor, performance gates, and a broader snapshot matrix.

## How to verify a change

```sh
./scripts/verify.sh
./scripts/mutants.sh
```

`verify.sh` runs the formatting, clippy, workspace test, and Nix checks. `mutants.sh` runs cargo-mutants inside a transient systemd scope with memory limits when available.
