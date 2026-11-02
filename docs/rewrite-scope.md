# Rewrite scope and decision trail

This document records what the Rust rewrite covers, what it deliberately leaves out, and the evidence behind the main choices. The upstream tree is large: 54 package groups, an Electron desktop app, a browser client, two SDKs, and a Cordis plugin loader. The rewrite targets the headless product first and keeps the upstream seams that make later slices cheap.

## Definition of done for this pass

The pass is complete when all of the following hold:

- A task can run from the CLI against the DeepSeek Messages API and print the final answer.
- The turn is durable: user input, assistant output, tool calls, tool results, and the terminal outcome survive a process exit.
- A stored session can be resumed and inspected.
- Tools are model-facing, permission-checked, and always settle a result.
- The workspace passes `cargo test`, `cargo clippy --deny warnings`, and `cargo fmt --check`.
- `cargo-mutants` reports no missed mutant in any crate.

All six are verified. See the root README for the measured counts.

## Milestone status

- **M1, foundation: complete.** Session header, fork, export, search, and repair; bounded retry; durable compaction; todo, plan, goal, working-directory, and session-search tools; `glob`, `str_replace_editor`, and `skill`.
- **M2, delegation and work: complete for this scope.** `subagent`, `workflow`, `ralph`, the seven team tools, the four `job_*` tools, and the `schedule_*` tools with `schedules run-due` are in.
- **M3, integrations and execution: partial.** MCP stdio, `web_search`, persistent PTY terminals, the LSP tool, inline image input, and bubblewrap confinement for `bash` are in. SSH, file attachments, browser use, and computer use are not.
- **M4, surfaces and distribution: partial.** The browser client and the JSON-RPC SDK are in. ACP, the API/BFF, plugin manager and market, stored credentials, telemetry, and i18n are not.
- **M5, hardening: pending.** Cross-platform checks, session-format migrations, performance gates, and the remaining snapshot matrix are not done.

The mutation gate is green across all ten crates: 619 caught viable mutants, 209 unviable mutants, and 0 missed or timed out.

## Upstream coverage

| Upstream group | Status in this rewrite | Notes |
|---|---|---|
| `core/session` | Ported shape | SQLite instead of JSONL generations. Append-only facts, projection, fork, export, search, repair, and header reads are preserved. |
| `core/agent`, `core/agent-loop` | Ported subset | Turn/step loop, streaming assembly, cancellation, bounded retry, compaction, subagents, step budget. No inbox projection, goal continuation, or plan approval flow. |
| `core/tools` | Ported shape | Name-keyed registry, schema admission, guarded execution, ordered results, concurrency flag, session-owned facts. No post-execute waterfall or presentation layer. |
| `core/system-prompt` | Simplified | One configured instruction block. No section registry or per-agent prompt composition. |
| `core/scope` | Not ported | One agent per process run. Scope keys and shadowing have no consumer yet. |
| `llm/llm` | Ported seam | Provider-neutral request/stream vocabulary and `LlmProvider`. |
| `llm/llm-deepseek` | Ported subset | Messages request, SSE, usage, thinking, tool calls. No Files API, image input, model listing, account auth, or request extensions. |
| `llm/llm-retry` | Ported policy | Pure retry classification and capped backoff. The loop does not yet apply it automatically. |
| `shell/bash-local`, `tool-bash` | Ported | `bash -lc`, timeout, cancellation, 64 KiB per-stream cap. No persistent shell. |
| `subprocess` | Uses Tokio | Child spawning is local to the bash tool. |
| `terminal` | Ported | `terminal_open`, `terminal_read`, `terminal_write`, and `terminal_close` over one process-local PTY registry. |
| `ssh` | Not ported | No remote execution. |
| `ptc-runtime` | Not ported | No programmatic tool calling. |
| `sandbox` | Partial | Permission modes, lexical path policy, and bubblewrap confinement for `bash`. Terminal PTYs are not wrapped. |
| `fs/fs-local`, `tool-fs`, `tool-fs-search`, `tool-str-replace-editor` | Ported subset | `read_file`, `write_file`, `edit_file`, `str_replace_editor`, `list_dir`, `glob`, `grep`. No patch format, observation policy, or LSP-guided edits. |
| `web/tool-web` | Partial | `web_fetch` and `web_search`. No alternate search provider. |
| `lsp` | Ported subset | Hover, definition, references, and document symbols over Content-Length framed JSON-RPC. No diagnostics, completion, rename, or code actions. |
| `skill` | Not ported | |
| `computer-use`, `browser-use` | Not ported | |
| `compaction` | Not ported | History grows until the provider limit. |
| `context` | Partial | Session working directory only. |
| `subagent` | Ported subset | `subagent`, `workflow`, `ralph`, and a durable team roster with seven team tools. No external provider backends or continuable cross-product children. |
| `jobs` | Ported | `job_start`, `job_list`, `job_output`, `job_kill` over a durable job directory. |
| `bundle` | Replaced | The CLI composes `Plugin` values directly. |
| `workflow` | Partial | Ordered child-session steps with stop-on-failure. No script engine, branching, or parallel stages. |
| `ralph` | Partial | Bounded fresh child rounds toward one objective with a text handoff. No structured handoff schema or shared-workspace policy beyond the recorded cwd. |
| `todo`, `plan`, `goal` | Partial | Session facts and model tools for each. No automatic goal continuation or plan approval UI. |
| `schedule` | Partial | Interval schedules, create/list/cancel tools, and `schedules run-due`. No RFC 5545 recurrence. |
| `preset`, `guard` | Not ported | `max_steps` is the only loop guard. |
| `extensions` | Not ported | Runtime self-modification would require a dynamic loader. |
| `session-query` | Partial | `sessions list` and `sessions show`. |
| `attachment` | Partial | `read_image` attaches inline PNG, JPEG, WebP, or GIF bytes up to 10 MiB. No file attachments, file API, or image offload. |
| `spill` | Partial | Hard caps on stream, file, and web body sizes. |
| `storage`, `workspace` | Partial | `DSH_HOME`, SQLite database, and session working directory. |
| `feedback`, `identity`, `settings`, `credentials` | Partial | Environment configuration and one credential variable. |
| `acp` | Not ported | |
| `interaction`, `boot` | Replaced | CLI interaction and composition root. |
| `sdk` | Partial | Line-delimited JSON-RPC over stdio: `run`, `sessions.list`, `sessions.show`, `tools.list`, `config`. |
| `host`, `client` | Partial | Browser client with HTML, JSON session routes, and POST SSE runs. Desktop is out of scope. |
| `mcp` | Ported shape | Stdio JSON-RPC client, tool discovery, and `mcp_<server>_<tool>` registration. |
| `experimental` | Not ported | |
| `test-support` | Inline | Mocks live beside their tests. |
| `util` | Inline | Zero-dependency helpers moved to the owning crate. |
| `apps/cli` | Replaced | `dsh-cli`. |
| `apps/web` | Ported minimal | `dsh-web` serves the page, session routes, and POST SSE runs. |
| `apps/desktop`, `apps/desktop-host`, `apps/plugin-market-generator` | Out of scope | Desktop is explicitly excluded; the plugin market is not ported. |
| `python/` | Out of scope | SDK and runtime. |

## Fidelity decisions

**SQLite instead of JSONL generations.** The upstream session log is a versioned JSONL file with adjacent migrations and generation selection. The rewrite uses one SQLite table with a per-session sequence. The observable contract that matters for this pass is the same: append-only ordered facts, a projection into model history, and durable resume. SQLite gives atomic sequence assignment and cheap listing without a migration engine. A future format-version column can carry the upstream generation protocol if compatibility becomes a requirement.

**Compile-time plugin composition.** Upstream loads Cordis plugins from YAML with hot reload. A Rust binary cannot safely load arbitrary code without a dynamic ABI or an embedded runtime. The rewrite keeps the plugin seam but fixes the set at build time. Adding a provider or a tool is a small code change, and a failed plugin still rolls back the contributions it made. This is a deliberate trade of deployment flexibility for memory safety and a small binary surface.

**Messages API, not OpenAI chat completions.** The upstream DeepSeek adapter speaks the Anthropic Messages subset at `/anthropic/v1/messages`. The rewrite follows that wire format: top-level system, content blocks, `tool_use`/`tool_result`, thinking blocks, and SSE. This keeps the adapter faithful and makes a later file/image slice a matter of extending the block vocabulary.

**No automatic retry in the loop.** The retry policy is implemented and tested, but the agent does not yet replay a failed stream. A failed turn is recorded with its machine code. Automatic retry needs request-freeze evidence and a duplicate-effect policy that this pass does not have.

**Lexical path policy.** The workspace boundary is enforced after lexical normalization. It does not resolve symlinks. A symlink inside the workspace can point outside it. This is a known hardening gap, not an accident. The safe next step is to canonicalize the workspace root once at session creation and canonicalize the existing parent of every write target before the boundary check.

**Tool names match upstream.** `bash`, `read_file`, `write_file`, `edit_file`, `list_dir`, `grep`, and `web_fetch` are the model-visible names. A model prompted against upstream documentation can use this harness without a translation layer.

## Decision trail

| Date | Decision | Evidence |
|---|---|---|
| 2026-11-02 | Build the headless product first; exclude desktop and web | The request excludes the desktop app. A full 54-group port would not reach a verifiable state in one pass. |
| 2026-11-02 | Six crates with downward-only dependencies | `dsh-agent` codes against `dsh-core` traits, so the whole loop is testable with mocks. |
| 2026-11-02 | Append-only SQLite log | A session must survive process exit and resume. SQLite gives atomic sequence assignment and listing in one dependency. |
| 2026-11-02 | Plugin host with reversible effects | "Everything is a plugin" is the upstream premise. Reverse-order rollback is tested directly. |
| 2026-11-02 | Messages API adapter with a hand-written SSE parser | The wire format is the upstream contract. A pure parser makes chunk-boundary behavior a property test instead of a network test. |
| 2026-11-02 | Tool batching by explicit concurrency flag | Parallel calls must not reorder results. A pure batch plan and index-keyed writeback make order a testable invariant. |
| 2026-11-02 | Mutation testing on every crate | The request requires 100% killed mutants. The first runs exposed missing observations in config, messages, tool context, event emission, and SSE progress. Tests were added; no mutant was excluded. |
| 2026-11-02 | End-to-end CLI test against a local SSE server | A unit test of the adapter would not prove `clap`, config, plugin composition, SQLite, and output projection work together. The test runs the real binary. |
| 2026-11-02 | Session admin stays on `SessionStore` | Fork, export, search, repair, and header reads all need the same connection and transaction. Default trait methods keep mock stores small. |
| 2026-11-02 | Retry is bounded by a `for` loop | A guard-only loop could retry forever if a mutation or a bug removed the attempt bound. The loop bound is structural. |
| 2026-11-02 | Compaction keeps the current turn by sequence | A summary event appended after the current user message would otherwise erase it. `keep_from` retains facts at or after the turn's first user fact. |
| 2026-11-02 | Subagents build a child loop without the subagent tool | The child registry is a clone taken before the tool inserts itself, so delegation depth is bounded by construction. |
| 2026-11-02 | Workflow is an ordered child-session list | It reuses the subagent seams and stops after a failed step by default. A script engine needs a parser, a sandbox, and a result protocol that this pass does not have. |
| 2026-11-02 | Schedules live in an atomically replaced JSON file | A scheduled follow-up is small, global, and human-inspectable. SQLite would add a schema and a second connection for no gain at this size. |
| 2026-11-02 | Ralph rounds are bounded fresh child sessions | Each round gets the immutable objective and the previous handoff, and the child registry removes all delegation tools. The loop ends after the round cap or a failed round. |
| 2026-11-02 | Terminals are process-local and bounded | A PTY cannot survive the harness process without a supervisor. A per-terminal cursor and a 1 MiB rolling log give the model incremental output without unbounded memory. |
| 2026-11-02 | LSP uses Content-Length framing and a bounded unmatched-frame budget | LSP framing differs from MCP newline framing. A server that never answers must not hang the tool, so unmatched frames are bounded. |
| 2026-11-02 | Images are inline base64 content blocks | `read_image` attaches the pixels beside a caption, and the Messages serializer maps the block to an inline `image` source. No Files API or image offload in this pass. |
| 2026-11-02 | Sandbox is a command resolver, not a second tool path | `bash` asks the configured provider for a program and arguments, so confinement changes the process it spawns without changing the tool contract. Bubblewrap gets a read-only root and a writable session directory. |
| 2026-11-02 | Team state is one JSON file plus in-process tasks | The roster and tasks must survive a restart; live joins must not. One locked state and one atomic file keep the seven tools consistent without a second database. |
| 2026-11-02 | Jobs use a durable directory, not in-memory handles | A job must survive the process that started it. Pid, output, and status files make listing and output reads independent of the agent. |
| 2026-11-02 | MCP tools are prefixed with the server name | Two servers can expose the same tool name; the registry rejects duplicates. The prefix keeps both available. |
| 2026-11-02 | The web run is a POST SSE stream | `EventSource` only supports GET. A POST stream lets the browser send the task and consume frames from one request. |
| 2026-11-02 | The SDK owns generic IO, the CLI owns stdio | `serve_io` is testable over byte buffers. The process stdio boundary stays in the CLI, where it belongs. |
| 2026-11-02 | Mutation gate rerun after every slice | New behavior repeatedly exposed missing observations in retry bounds, compaction boundaries, tool size limits, and process status. Tests were added; no exclusion was used. |

## Next slices

In order of leverage:

1. **Symlink-safe path policy** and an OS sandbox backend behind the same `ToolContext` boundary.
2. **Automatic retry** with request freeze and duplicate-effect accounting.
3. **Compaction** with a token budget and a logged replacement event.
4. **Session fork** at a turn boundary.
5. **Subagents** as a tool that creates a child session and returns a durable notice.
6. **MCP** as an external tool provider behind the existing `Tool` trait.
7. **Web client** over a small HTTP/SSE server, reusing `RunEvent`.
