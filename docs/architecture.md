# Architecture

The harness is a set of capability seams composed by plugins. The CLI is the composition root; every other crate contributes types, a capability, or a consumer.

## Crate graph

```text
dsh-cli
  ├── dsh-agent
  │     ├── dsh-core
  │     └── dsh-core traits
  ├── dsh-llm      -> dsh-core
  ├── dsh-session  -> dsh-core
  ├── dsh-tools    -> dsh-core
  ├── dsh-jobs     -> dsh-core
  ├── dsh-schedule -> dsh-core, dsh-agent
  ├── dsh-team     -> dsh-core, dsh-agent
  ├── dsh-terminal -> dsh-core
  ├── dsh-lsp      -> dsh-core
  ├── dsh-sandbox  -> dsh-core
  ├── dsh-mcp      -> dsh-core
  ├── dsh-web      -> dsh-agent, dsh-core
  └── dsh-sdk      -> dsh-agent, dsh-core
```

No adapter crate depends on another adapter. The agent depends only on the traits in `dsh-core`, so a test provider, an in-memory store, and a fake tool can drive the full turn machine without a network or a filesystem.

## Domain shapes

`dsh-core` owns the vocabulary:

- `SessionId`, `MessageId`, `TurnId`, `StepId`, `ToolCallId`, and `EventSeq` are newtypes. Each is validated once at a boundary and trusted downstream.
- `Message` is a role-tagged sum: system, developer, user, assistant, tool. Content blocks are text, reasoning, or a tool call.
- `SessionEvent` is a tagged sum of durable facts: session created, turn started, step started, user message, assistant message, tool result, turn ended.
- `EventRecord` pairs a fact with its sequence and wall-clock time.
- `ToolSchema` is the model-facing declaration; `ToolOutcome` is the normalized result.
- `LlmRequest` and `StreamEvent` are provider-neutral. A provider translates its wire format at the edge.

`derive_messages` projects the event log into model history. Lifecycle markers disappear; message facts keep their order. The projection is the only way the loop builds a request, so a resume reads exactly the history the model saw.

## Plugin composition

`Plugin` is the unit of composition. `compose` applies plugins in order into a `Registry`:

- The session store and the model provider are single-valued services.
- Tools are name-keyed and duplicate names are rejected.
- Every contribution records an undo effect.

If a later plugin fails, the effects run in reverse order and the runtime is not produced. The CLI composes three plugins: `sqlite-session-store`, `deepseek`, and `tools`. Tests compose mocks and assert both the produced surface and the rollback order.

The composition is compile-time. Upstream Cordis loads plugins from YAML at boot; this rewrite deliberately uses Rust values so a deployment cannot mount code the binary does not contain. The seam is the same: swapping a provider or a store changes the whole product without touching the loop.

## Session log

`dsh-session` stores one header row per session and an append-only event table:

```sql
sessions(id, created_at, cwd, parent_session, title, format_version)
events(session_id, seq, at, kind, payload, PRIMARY KEY(session_id, seq))
```

`create` writes the header and the opening `session_created` fact in one transaction. `append` checks existence and format version, computes the next sequence in the same transaction, serializes the event, and returns the committed record. `load` reads ordered records. `list` returns summaries with event counts. `delete` relies on `ON DELETE CASCADE`.

The store is synchronous. SQLite on a local disk is fast enough that an async wrapper would add a task boundary and a cancellation hole without buying throughput. The store is behind a `Mutex<Connection>`, so the sequence assignment is serialized.

The admin surface sits on the same trait: `header` reads one session row, `fork` copies a prefix into a new session with the source as its parent, `export` emits one JSON record per line, `search` matches event payloads with a bounded escaped `LIKE`, and `repair` seals the last open turn with an `INTERRUPTED` failure. The CLI exposes each as a `sessions` subcommand.

## Agent loop

One turn admits a user message, then runs model steps:

1. Derive history from the log.
2. Append `step_started`.
3. Build `LlmRequest` from history, the system prompt, and `tools.schemas()`.
4. Stream the provider response. Text and reasoning deltas are forwarded to the live sink; the assembler keeps block order and accumulates tool arguments.
5. Settle the assistant message into the log.
6. If the message requests tools, execute them in batches and settle each result in model order.
7. Repeat until the model stops requesting tools, the step budget runs out, or cancellation wins.

Tool batching is a pure plan over `concurrency_safe` flags: consecutive safe calls share a batch, an exclusive call is its own batch. A batch runs through `buffer_unordered` with `max_parallel_tools`; results are written back by model index, so the model sees the order it asked for.

Cancellation is cooperative. The token reaches the provider stream and every `ToolContext`. A cancelled tool call that never dispatched is settled with an error result, so later requests still see a paired history.

A provider or assembly failure closes the turn with `TurnOutcome::Failed` and the machine code. The turn is still durable; the CLI exits `1`.

Retry is bounded by the `for attempt in 0..=max_retries` loop. A retryable failure appends `retry_attempt`, emits a live retry frame, and waits for a capped exponential delay. A non-retryable failure closes the turn immediately. The loop cannot retry forever because the iteration bound is structural.

Compaction runs before each step. When estimated history characters exceed `max_context_chars`, the loop summarizes every message before the current turn's first user message, appends a `compaction` fact with `keep_from` set to that message's sequence, and rebuilds history. `derive_messages` inserts the summary and retains only facts at or after `keep_from`, so the current turn is never summarized away.

## Tool pipeline

`Tool` owns a schema, an async body, and a concurrency flag. `Tool::run` is the loop's entry point. It parses the raw JSON arguments, rejects non-objects, and contains panics as a tool error. `execute` sees typed `serde_json::Value` and returns `ToolOutcome`.

The seven built-ins are `bash`, `read_file`, `write_file`, `edit_file`, `list_dir`, `grep`, and `web_fetch`.

The full first-party surface adds `apply_patch`, `read_image`, `str_replace_editor`, `glob`, `web_search`, `todo_write`, `update_plan`, `create_goal`, `working_directory`, `session_search`, `skill`, `create_worktree`, `subagent`, `workflow`, `ralph`, the seven team tools, the four `job_*` tools, the three `schedule_*` tools, and the four `terminal_*` tools. Session-owned tools append their facts through `ToolContext::session`. The loop reads the latest `working_directory_changed` fact before each step, so a directory change applies to later steps without mutating the current batch.

`subagent` builds a child `AgentLoop` from the same store, provider, and tool registry, removes itself and `workflow` from the child registry, records the parent session on the child header, and returns the child's final text. `workflow` runs an ordered list of child sessions, stops after a failed step by default, and reports one line per settled step. A child cannot spawn another child through the same tools.

`dsh-schedule` stores schedules in `state_dir/schedules.json` with an atomic temp-file rename. `schedule_create`, `schedule_list`, and `schedule_cancel` operate on that file through `ToolContext.state_dir`. `dsh schedules run-due` loads the file, runs each enabled entry whose `next_run_at` has passed in its recorded session and cwd, advances the interval, and saves after every turn. A failed turn is a recorded run, not a scheduler failure.

`ralph` runs bounded fresh child sessions toward one objective. Each round receives the immutable objective and the previous handoff, and returns the next handoff. The child registry removes `subagent`, `workflow`, and `ralph`, so a round cannot re-enter the loop.

`dsh-team` keeps a durable roster and task list in `state_dir/team.json`. `spawn_teammate` starts a child session asynchronously and records its member row; `wait_agent` polls the row until it settles; `send_message` starts a new turn in the same child session when it is idle. The seven team tools share one locked state, and a teammate's child registry removes all delegation and team tools.

Set `DSH_SANDBOX=bubblewrap` to route `bash` through `dsh-sandbox`. The provider resolves a command into a bubblewrap invocation with a read-only root, a writable session directory, and a shared network namespace. `danger-full-access` bypasses the wrapper. Terminal PTYs are not wrapped.

`dsh-terminal` keeps a process-local registry of PTY sessions. Each session owns a child, a writer, and a bounded output log shared with a reader thread. `terminal_read` returns unread bytes and advances a per-terminal cursor; the 1 MiB cap drops the oldest bytes and slides that cursor. The tools refuse to open a terminal in read-only mode. Terminals do not outlive the harness process.

`dsh-lsp` starts each configured language server with Content-Length framed JSON-RPC. It completes the initialize handshake, re-sends `textDocument/didOpen` for each query, and routes `hover`, `definition`, `references`, and `symbols` to the first server whose extension list matches the file. Locations render as `path:line:character`; symbol names render as an indented tree. A server that never answers is cut off after a bounded number of unmatched frames.

`read_image` reads a PNG, JPEG, WebP, or GIF up to 10 MiB, base64-encodes it, and returns an image content block beside a caption. The Messages serializer maps that block to an inline `image` source, so the next request carries the pixels.

`apply_patch` pipes a unified diff to `git apply` in the session directory. `create_worktree` runs `git worktree add` for a validated branch under `.worktrees/<branch>`, then appends a `working_directory_changed` fact so later steps execute in the new checkout.

`job_*` writes a small job directory under the session workspace. `job_start` redirects the child's streams to files, records the pid, and a waiter thread publishes an atomic status file. `job_list`, `job_output`, and `job_kill` read that durable state.

Path policy lives in `dsh-core`:

- `resolve_path` joins the session directory and normalizes `.` and `..` lexically.
- `ensure_within` rejects writes outside the session directory unless full access was granted.
- `readable_path` applies the same boundary to reads.

The policy is lexical. It does not resolve symlinks, so a symlink inside the workspace can point outside it. A production hardening pass would canonicalize the workspace root and the existing parent of every target. This is recorded in the scope document.

## Model adapter

`dsh-llm` speaks the DeepSeek Messages API:

- System text is the top-level `system` field.
- User text, assistant text/reasoning/tool calls, and tool results map to Messages content blocks.
- Adjacent same-role turns merge.
- Tool pairing is validated before dispatch: every `tool_use` id must be answered before the next assistant turn.
- The request is validated for `max_tokens`, temperature, stop sequences, and empty tool names.
- The response is SSE. `SseParser` keeps incomplete bytes buffered and only dispatches blank-line-terminated frames. It accepts LF and CRLF, multi-line data, comments, event names, and arbitrary transport chunk boundaries.
- The translator maps `message_start`, `content_block_start`, `content_block_delta`, `content_block_stop`, `message_delta`, and `message_stop` into `StreamEvent`. It validates ordering and accumulates cumulative usage.
- Retry policy is pure: retryable codes, attempt bounds, and capped exponential delay.

The adapter rejects cancellation by returning a transport error, which the loop records as a cancelled turn. The stream itself observes the token between frames.

## CLI

`dsh-cli` parses with `clap`, loads configuration from the environment at the boundary, composes the runtime, and runs one turn.

- `--json` serializes `RunEvent` values with serde. The stream opens with `session`, carries `text`, `thinking`, `tool_call`, `tool_result`, and `error`, and closes with `final`.
- The plain sink sends reasoning and tool progress to stderr and the final answer to stdout.
- Session inspection reads the same store the agent writes.
- `config` prints effective values and only a boolean for the credential.

Configuration parsing is pure through `Config::from_lookup`; the process environment is read only in the CLI boundary adapter. This keeps tests independent of process state.

## External tools

`dsh-mcp` speaks newline-delimited JSON-RPC 2.0 to stdio servers. `ProcessConnection` owns the child process and one request id sequence behind a mutex. `McpPlugin` reads `DSH_MCP_SERVERS`, starts each server, lists its tools, and registers one `McpTool` per declaration under `mcp_<server>_<tool>`. The tool forwards the original name and preserves the server's `isError` flag. Blocking protocol work runs on `spawn_blocking`.

`web_search` uses the DuckDuckGo HTML endpoint. The parser is pure and tested against a fixture; the HTTP request is thin glue. `web_fetch` and `web_search` refuse read-only mode.

## Surfaces

`dsh-web` serves one axum router with the HTML page, `GET /api/health`, `GET /api/sessions`, `GET /api/sessions/{id}`, `GET /api/tools`, and `POST /api/run`. A run is a POST SSE stream. The page reads the response body with `fetch` and a stream reader, appends text and tool frames to the transcript, and refreshes the session list. The router is exercised with `tower::ServiceExt::oneshot`, and the CLI was exercised over a real socket against a local SSE provider.

`dsh-sdk` reads one JSON-RPC request per line from any async reader and writes one frame per line to any async writer. `run` emits an `event` notification per `RunEvent` and settles with `{session_id, outcome, text}`. It also serves `sessions.list`, `sessions.show`, `tools.list`, and `config`. `serve_io` is generic over the reader and writer; the CLI owns the process stdio boundary.

## Testing and mutation

Unit tests sit beside the code they exercise. Integration tests in `dsh-cli/tests/cli.rs` run the real binary against a local TCP server that emits a recorded SSE response. That covers `clap`, configuration, plugin composition, SQLite, the Messages request, SSE translation, assembly, the agent loop, and output projection in one path.

Property tests cover the shapes where examples are weak: event round-trips, projection order, SSE chunk splitting, retry delays, path normalization, edit semantics, and batch plans.

`cargo-mutants` runs on every crate. The measured result is 0 missed and 0 timeouts across fifteen crates, with 900 caught and 313 unviable mutants. The mutation gate is intentionally strict: when a mutant exposes a missing observation, the fix is a behavior test, not an exclusion.
