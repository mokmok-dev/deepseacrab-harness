# DeepSeacrab Harness

A Rust rewrite of [DeepSeek Harness](https://github.com/deepseek-ai/deepseek-harness) for headless use. It keeps the upstream architecture where it earns its place: an append-only session log, a provider-neutral model seam, a guarded tool pipeline, and plugin composition. It drops the desktop application and the browser client.

The binary is `dsh`.

## Status

The headless product works end to end. A task can be submitted from the CLI, the browser client, or the JSON-RPC SDK, streamed from a Messages-compatible provider, executed through built-in and MCP tools, and persisted to SQLite. Sessions can be resumed, forked, searched, exported, and inspected.

Verified on 2026-11-02:

- `cargo test --workspace`: 839 tests passed.
- `cargo clippy --workspace --all-targets -- --deny warnings`: clean.
- `cargo fmt --all -- --check`: clean.
- `cargo mutants` across all crates: 0 missed mutants and 0 timeouts. The eighteen crates produced 1031 caught viable mutants and 347 unviable mutants.
- `dsh web` was exercised over a real socket with a local SSE provider: health, the HTML page, the tool catalog, and a completed run with a `text` and `final` event.

## Quick start

```sh
export DEEPSEEK_API_KEY=...
cargo run -p dsh-cli --bin dsh -- run "read Cargo.toml and summarize the workspace"
```

Use any Messages-compatible endpoint by adding a durable route. Routes live in `$DSH_HOME/providers.json`, and the route name keys the credential store:

```sh
cargo run -p dsh-cli --bin dsh -- providers add local-gw --base-url http://127.0.0.1:1234 --model qwen3-coder
cargo run -p dsh-cli --bin dsh -- auth set local-gw
cargo run -p dsh-cli --bin dsh -- run "say hello"
```

No `DEEPSEEK_*` variable is read for a route that is not the built-in `deepseek` one. `--api-key-env LOCAL_GW_KEY` names a variable instead of a stored secret, and is optional.

The credential travels as `x-api-key` by default. Pass `--auth bearer` for an endpoint that expects `Authorization: Bearer`, such as Ollama Cloud:

```sh
cargo run -p dsh-cli --bin dsh -- providers add ollama-cloud --base-url https://ollama.com/v1 --model deepseek-v4.1-flash:1017 --auth bearer
cargo run -p dsh-cli --bin dsh -- auth set ollama-cloud
```

The `DSH_PROVIDERS` environment variable overrides the file when it is set:

```sh
export DSH_PROVIDERS='[{"name":"local-gw","base_url":"http://127.0.0.1:1234","model":"qwen3-coder","api_key_env":"LOCAL_GW_KEY"}]'
export LOCAL_GW_KEY=...
cargo run -p dsh-cli --bin dsh -- run "say hello"
```

When several routes are declared and none is selected, `dsh run` fails with `no provider selected`. Inspection commands such as `dsh sessions`, `dsh auth`, `dsh providers list`, `dsh telemetry`, `dsh schedules list`, and `dsh tools` still work, because they do not need a model route. `dsh config` prints the whole table and reports `"provider": null` with `"provider_error"` until a route is selected.

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
| `dsh run --provider <name>` | Select a configured route for this run. |
| `dsh run --max-steps <n>` | Bound the steps in one turn. |
| `dsh sessions list` | List stored sessions. |
| `dsh sessions show <id>` | Print one session's durable event log. |
| `dsh sessions fork <source> <target>` | Copy a session prefix into a new session. |
| `dsh sessions export <id>` | Export a session as NDJSON. |
| `dsh sessions search <query>` | Search durable event payloads. |
| `dsh sessions repair <id>` | Seal a turn left open by a process loss. |
| `dsh schedules list` | List durable scheduled follow-ups. |
| `dsh schedules run-due` | Run every schedule whose next run time has passed. |
| `dsh auth set <provider>` | Store a provider secret read from stdin. |
| `dsh auth list` | List providers with stored secrets. |
| `dsh auth remove <provider>` | Remove a stored provider secret. |
| `dsh providers list` | List durable provider routes. |
| `dsh providers add <name> --base-url <url> --model <id> [--api-key-env <var>] [--auth <style>]` | Store or replace one route in `$DSH_HOME/providers.json`. `--auth` is `x-api-key` (default) or `bearer`. |
| `dsh providers remove <name>` | Remove one durable provider route. |
| `dsh telemetry` | Print local run counters. |
| `dsh tools` | List the model-facing tool registry. |
| `dsh config` | Print effective configuration and the provider table without secrets. |
| `dsh web` | Serve the browser client on `127.0.0.1:3080`. |
| `dsh sdk` | Serve the JSON-RPC SDK on stdio. |

Exit code `0` means the turn completed. `1` means the turn failed, was cancelled, or hit its step limit. Clap usage errors exit `2`.

## Configuration

Provider routes written by `dsh providers` live in `$DSH_HOME/providers.json`. When `DSH_PROVIDERS` is set, it replaces the file for that process.

| Variable | Default | Meaning |
|---|---|---|
| `DSH_PROVIDERS` | none | JSON array of provider routes: `name`, `base_url`, `model`, optional `api_key_env`, optional `auth` (`x-api-key` or `bearer`, default `x-api-key`). |
| `DSH_PROVIDER` | The built-in `deepseek` route when no route is configured, otherwise the sole configured route | Route to use. Required when two or more routes are configured. |
| `DEEPSEEK_API_KEY` | none | Credential for the built-in `deepseek` route. Or run `dsh auth set deepseek`. |
| `DEEPSEEK_BASE_URL` | `https://api.deepseek.com/anthropic` | Built-in route's Messages root. |
| `DEEPSEEK_MODEL` | `deepseek-flash` | Built-in route's model id. |
| `DSH_API_KEY_ENV` | `DEEPSEEK_API_KEY` | Built-in route's credential variable name. |
| `DSH_MAX_TOKENS` | `8192` | Per-request output cap. |
| `DSH_MAX_STEPS` | `24` | Step budget for one turn. |
| `DSH_MAX_PARALLEL_TOOLS` | `4` | Concurrent concurrency-safe tools. |
| `DSH_MAX_RETRIES` | `2` | Retryable provider failures retried per step. |
| `DSH_RETRY_BASE_MS` | `500` | Base delay for exponential retry backoff. |
| `DSH_MAX_CONTEXT_CHARS` | `200000` | Estimated history size that triggers compaction. |
| `DSH_MCP_SERVERS` | none | JSON array of MCP stdio server configurations. |
| `DSH_LSP_SERVERS` | none | JSON array of LSP stdio server configurations. |
| `DSH_SSH_HOSTS` | none | JSON array of named SSH hosts for `ssh_run`. |
| `DSH_REASONING_EFFORT` | `high` | `off`, `low`, `high`, or `max`. |
| `DSH_PERMISSION_MODE` | `workspace-write` | Tool permission mode. |
| `DSH_SANDBOX` | `off` | `off` or `bubblewrap` for shell execution. |
| `DSH_HOME` | platform data directory | Session database and harness state. |
| `DSH_SYSTEM_PROMPT` | built-in prompt | System instruction block. |

## Architecture

The workspace is eighteen crates. Dependencies point downward only.

| Crate | Owns |
|---|---|
| `dsh-core` | Domain types, capability traits, permission policy, plugin host. |
| `dsh-session` | SQLite append-only session store. |
| `dsh-llm` | Messages adapter for any compatible route, SSE framing, request translation, retry policy. |
| `dsh-tools` | Model-facing tools and their plugin. |
| `dsh-jobs` | Background shell job tools. |
| `dsh-schedule` | Durable scheduled follow-ups and their tools. |
| `dsh-team` | Durable agent team roster, tasks, and messaging tools. |
| `dsh-terminal` | Persistent PTY terminals and their tools. |
| `dsh-lsp` | LSP stdio client and the `lsp` tool. |
| `dsh-sandbox` | Bubblewrap confinement for shell execution. |
| `dsh-ssh` | SSH host configuration and the `ssh_run` tool. |
| `dsh-telemetry` | Local run counters. |
| `dsh-credentials` | Local provider credential store with owner-only file mode. |
| `dsh-mcp` | MCP stdio client that registers external tools. |
| `dsh-agent` | Turn and step driver, streaming assembly, tool batching, subagents. |
| `dsh-web` | Browser client with JSON and SSE routes. |
| `dsh-sdk` | Line-delimited JSON-RPC SDK. |
| `dsh-cli` | `clap` command grammar, plugin composition root, output projection. |

The full design is in [docs/architecture.md](docs/architecture.md). The upstream coverage table and the deliberate omissions are in [docs/rewrite-scope.md](docs/rewrite-scope.md).

The model-facing registry contains 40 first-party tools in a run: `apply_patch`, `bash`, `read_file`, `read_image`, `write_file`, `edit_file`, `str_replace_editor`, `list_dir`, `glob`, `grep`, `web_fetch`, `web_search`, `todo_write`, `update_plan`, `create_goal`, `working_directory`, `session_search`, `skill`, `create_worktree`, `subagent`, `workflow`, `ralph`, `spawn_teammate`, `list_agents`, `send_message`, `wait_agent`, `team_task_create`, `team_task_list`, `team_task_update`, `job_start`, `job_list`, `job_output`, `job_kill`, `schedule_create`, `schedule_list`, `schedule_cancel`, `terminal_open`, `terminal_read`, `terminal_write`, and `terminal_close`. `dsh tools` lists the 30 statically composed tools; the delegation and team tools are added with their runtime seams. An `lsp` tool appears when `DSH_LSP_SERVERS` is set, an `ssh_run` tool appears when `DSH_SSH_HOSTS` is set, and MCP servers add one tool per remote declaration.

## Safety

The default permission mode is `workspace-write`. Filesystem tools resolve paths lexically and refuse paths outside the session directory. `read-only` refuses writes, shell execution, and network fetches. `danger-full-access` removes the path boundary and should be used deliberately.

Set `DSH_SANDBOX=bubblewrap` to wrap `bash` execution in a bubblewrap mount namespace with a read-only root and a writable session directory. The terminal tools remain process-local PTYs and are not wrapped yet.

The credential fallback reads `$DSH_HOME/credentials.json`, written with mode `0600` and keyed by route name. The variable a route names in `api_key_env` takes precedence over the stored secret; a route without one reads only the stored secret, so a custom route never picks up the `deepseek` credential.

Tool panics are contained and returned as tool errors, so the matching tool result is always settled in the log.

## Testing

```sh
nix flake check
nix run .#mutants
```

Property tests cover session event round-trips, message projection, SSE chunk boundaries, retry policy invariants, tool path policy, edit semantics, and event batch planning. There is no example-based test suite.

`nix run .#mutants` builds the `mutants` app and runs `cargo-mutants` with the flake's toolchain and linker settings. When the systemd user manager is reachable, it runs inside a transient scope with `MemoryMax=80%` and `MemoryHigh=70%`. Otherwise it runs directly. Set `JOBS` and `TIMEOUT` to override the default 4 jobs and 300 second timeout.

## What is out of scope

The desktop application, browser automation, computer use, ACP, the remote API/BFF, dynamic plugin loading and the plugin market, and full i18n are not ported. The local browser client, MCP, LSP, SSH, PTY, sandbox, credentials, and telemetry are present. See [docs/final-status.md](docs/final-status.md) for the reason each remaining surface is a separate program.

## License

MIT.
