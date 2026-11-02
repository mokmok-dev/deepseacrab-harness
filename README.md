# DeepSeacrab Harness

A Rust rewrite of [DeepSeek Harness](https://github.com/deepseek-ai/deepseek-harness) for headless use. It keeps the upstream architecture where it earns its place: an append-only session log, a provider-neutral model seam, a guarded tool pipeline, and plugin composition. It drops the desktop application and the browser client.

The binary is `dsh`.

## Status

The headless product works end to end. A task can be submitted from the CLI, the browser client, or the JSON-RPC SDK, streamed from the DeepSeek Messages API, executed through built-in and MCP tools, and persisted to SQLite. Sessions can be resumed, forked, searched, exported, and inspected.

Verified on 2026-11-02:

- `cargo test --workspace`: 705 tests passed.
- `cargo clippy --workspace --all-targets -- --deny warnings`: clean.
- `cargo fmt --all -- --check`: clean.
- `cargo mutants` across all crates: 0 missed mutants and 0 timeouts. The fifteen crates produced 900 caught viable mutants and 313 unviable mutants.
- `dsh web` was exercised over a real socket with a local SSE provider: health, the HTML page, the tool catalog, and a completed run with a `text` and `final` event.

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

Open the browser client:

```sh
cargo run -p dsh-cli --bin dsh -- web
# http://127.0.0.1:3080
```

Serve the JSON-RPC SDK on stdio:

```sh
cargo run -p dsh-cli --bin dsh -- sdk
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
| `dsh sessions fork <source> <target>` | Copy a session prefix into a new session. |
| `dsh sessions export <id>` | Export a session as NDJSON. |
| `dsh sessions search <query>` | Search durable event payloads. |
| `dsh sessions repair <id>` | Seal a turn left open by a process loss. |
| `dsh schedules list` | List durable scheduled follow-ups. |
| `dsh schedules run-due` | Run every schedule whose next run time has passed. |
| `dsh tools` | List the model-facing tool registry. |
| `dsh config` | Print effective configuration without secrets. |
| `dsh web` | Serve the browser client on `127.0.0.1:3080`. |
| `dsh sdk` | Serve the JSON-RPC SDK on stdio. |

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
| `DSH_MAX_RETRIES` | `2` | Retryable provider failures retried per step. |
| `DSH_RETRY_BASE_MS` | `500` | Base delay for exponential retry backoff. |
| `DSH_MAX_CONTEXT_CHARS` | `200000` | Estimated history size that triggers compaction. |
| `DSH_MCP_SERVERS` | none | JSON array of MCP stdio server configurations. |
| `DSH_LSP_SERVERS` | none | JSON array of LSP stdio server configurations. |
| `DSH_REASONING_EFFORT` | `high` | `off`, `low`, `high`, or `max`. |
| `DSH_PERMISSION_MODE` | `workspace-write` | Tool permission mode. |
| `DSH_SANDBOX` | `off` | `off` or `bubblewrap` for shell execution. |
| `DSH_HOME` | platform data directory | Session database and harness state. |
| `DSH_SYSTEM_PROMPT` | built-in prompt | System instruction block. |

## Architecture

The workspace is fifteen crates. Dependencies point downward only.

| Crate | Owns |
|---|---|
| `dsh-core` | Domain types, capability traits, permission policy, plugin host. |
| `dsh-session` | SQLite append-only session store. |
| `dsh-llm` | DeepSeek Messages adapter, SSE framing, request translation, retry policy. |
| `dsh-tools` | Model-facing tools and their plugin. |
| `dsh-jobs` | Background shell job tools. |
| `dsh-schedule` | Durable scheduled follow-ups and their tools. |
| `dsh-team` | Durable agent team roster, tasks, and messaging tools. |
| `dsh-terminal` | Persistent PTY terminals and their tools. |
| `dsh-lsp` | LSP stdio client and the `lsp` tool. |
| `dsh-sandbox` | Bubblewrap confinement for shell execution. |
| `dsh-mcp` | MCP stdio client that registers external tools. |
| `dsh-agent` | Turn and step driver, streaming assembly, tool batching, subagents. |
| `dsh-web` | Browser client with JSON and SSE routes. |
| `dsh-sdk` | Line-delimited JSON-RPC SDK. |
| `dsh-cli` | `clap` command grammar, plugin composition root, output projection. |

The full design is in [docs/architecture.md](docs/architecture.md). The upstream coverage table and the deliberate omissions are in [docs/rewrite-scope.md](docs/rewrite-scope.md).

The model-facing registry contains 40 first-party tools in a run: `apply_patch`, `bash`, `read_file`, `read_image`, `write_file`, `edit_file`, `str_replace_editor`, `list_dir`, `glob`, `grep`, `web_fetch`, `web_search`, `todo_write`, `update_plan`, `create_goal`, `working_directory`, `session_search`, `skill`, `create_worktree`, `subagent`, `workflow`, `ralph`, `spawn_teammate`, `list_agents`, `send_message`, `wait_agent`, `team_task_create`, `team_task_list`, `team_task_update`, `job_start`, `job_list`, `job_output`, `job_kill`, `schedule_create`, `schedule_list`, `schedule_cancel`, `terminal_open`, `terminal_read`, `terminal_write`, and `terminal_close`. `dsh tools` lists the 30 statically composed tools; the delegation and team tools are added with their runtime seams. An `lsp` tool appears when `DSH_LSP_SERVERS` configures language servers, and MCP servers add one tool per remote declaration.

## Safety

The default permission mode is `workspace-write`. Filesystem tools resolve paths lexically and refuse paths outside the session directory. `read-only` refuses writes, shell execution, and network fetches. `danger-full-access` removes the path boundary and should be used deliberately.

Set `DSH_SANDBOX=bubblewrap` to wrap `bash` execution in a bubblewrap mount namespace with a read-only root and a writable session directory. The terminal tools remain process-local PTYs and are not wrapped yet.

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
