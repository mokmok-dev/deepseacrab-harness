# DeepSeacrab Harness

A Rust rewrite of [DeepSeek Harness](https://github.com/deepseek-ai/deepseek-harness) for headless use. It keeps the upstream architecture where it earns its place: an append-only session log, a provider-neutral model seam, a guarded tool pipeline, and plugin composition. It drops the desktop application and the browser client.

The binary is `dsh`.

## Status

The headless product works end to end. A task can be submitted from the CLI, streamed from the DeepSeek Messages API, executed through built-in tools, and persisted to SQLite. Sessions can be resumed and inspected.

Verified on 2026-11-02:

- `cargo test --workspace`: 291 tests passed.
- `cargo clippy --workspace --all-targets -- --deny warnings`: clean.
- `cargo fmt --all -- --check`: clean.
- `cargo mutants --workspace`: 0 missed mutants. The six crates produced 395 caught viable mutants and 108 unviable mutants.

## Quick start

```sh
export DEEPSEEK_API_KEY=...
cargo run -p dsh-cli --bin dsh -- run "read Cargo.toml and summarize the workspace"
```

Pipe a task when it is long:

```sh
{ echo "Review this diff:"; git diff; } | cargo run -p dsh-cli --bin dsh -- run -
```

Resume a session:

```sh
cargo run -p dsh-cli --bin dsh -- run --session-id session-demo "now run the tests"
cargo run -p dsh-cli --bin dsh -- sessions show session-demo
```

Inspect what the harness will use:

```sh
cargo run -p dsh-cli --bin dsh -- config
cargo run -p dsh-cli --bin dsh -- tools
```

## Commands

| Command | Behavior |
|---|---|
| `dsh run [TASK]` | Run one task. Omit `TASK`, or pass `-`, to read stdin. |
| `dsh run --json` | Emit newline-delimited run events on stdout. |
| `dsh run --session-id <id>` | Create or resume an exact session identity. |
| `dsh run --cwd <path>` | Set the session working directory. |
| `dsh run --permission-mode <mode>` | `read-only`, `workspace-write`, or `danger-full-access`. |
| `dsh run --model <id>` | Override the route model. |
| `dsh run --max-steps <n>` | Bound the steps in one turn. |
| `dsh sessions list` | List stored sessions. |
| `dsh sessions show <id>` | Print one session's durable event log. |
| `dsh tools` | List the model-facing tool registry. |
| `dsh config` | Print effective configuration without secrets. |

Exit code `0` means the turn completed. `1` means the turn failed, was cancelled, or hit its step limit. Clap usage errors exit `2`.

## Configuration

| Variable | Default | Meaning |
|---|---|---|
| `DEEPSEEK_API_KEY` | none | Credential for the Messages API. Required for `run`. |
| `DEEPSEEK_BASE_URL` | `https://api.deepseek.com/anthropic` | Messages-compatible root. |
| `DEEPSEEK_MODEL` | `deepseek-flash` | Route model id. |
| `DSH_API_KEY_ENV` | `DEEPSEEK_API_KEY` | Alternate credential variable name. |
| `DSH_MAX_TOKENS` | `8192` | Per-request output cap. |
| `DSH_MAX_STEPS` | `24` | Step budget for one turn. |
| `DSH_MAX_PARALLEL_TOOLS` | `4` | Concurrent concurrency-safe tools. |
| `DSH_REASONING_EFFORT` | `high` | `off`, `low`, `high`, or `max`. |
| `DSH_PERMISSION_MODE` | `workspace-write` | Tool permission mode. |
| `DSH_HOME` | platform data directory | Session database and harness state. |
| `DSH_SYSTEM_PROMPT` | built-in prompt | System instruction block. |

## Architecture

The workspace is six crates. Dependencies point downward only.

| Crate | Owns |
|---|---|
| `dsh-core` | Domain types, capability traits, permission policy, plugin host. |
| `dsh-session` | SQLite append-only session store. |
| `dsh-llm` | DeepSeek Messages adapter, SSE framing, request translation, retry policy. |
| `dsh-tools` | Seven model-facing tools and their plugin. |
| `dsh-agent` | Turn and step driver, streaming assembly, tool batching. |
| `dsh-cli` | `clap` command grammar, plugin composition root, output projection. |

The full design is in [docs/architecture.md](docs/architecture.md). The upstream coverage table and the deliberate omissions are in [docs/rewrite-scope.md](docs/rewrite-scope.md).

## Safety

The default permission mode is `workspace-write`. Filesystem tools resolve paths lexically and refuse paths outside the session directory. `read-only` refuses writes, shell execution, and network fetches. `danger-full-access` removes the path boundary and should be used deliberately.

Tool panics are contained and returned as tool errors, so the matching tool result is always settled in the log.

## Testing

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- --deny warnings
cargo fmt --all -- --check
./scripts/mutants.sh
```

Property tests cover session event round-trips, message projection, SSE chunk boundaries, retry policy invariants, tool path policy, edit semantics, and event batch planning. There is no example-based test suite.

`scripts/mutants.sh` runs `cargo-mutants` inside a transient systemd user scope when `systemd-run` is available, with `MemoryMax=80%` and `MemoryHigh=70%`. It falls back to plain `cargo mutants` otherwise.

## What is out of scope

The desktop application, browser client, MCP, LSP, browser and computer use, ACP, subagents, workflows, goals, schedules, compaction, image input, and the Python/TypeScript SDKs are not ported. See [docs/rewrite-scope.md](docs/rewrite-scope.md) for the reason each is absent and what a next slice would cover.

## License

MIT.
