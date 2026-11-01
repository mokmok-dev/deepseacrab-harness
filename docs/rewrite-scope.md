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

## Upstream coverage

| Upstream group | Status in this rewrite | Notes |
|---|---|---|
| `core/session` | Ported shape | SQLite instead of JSONL generations. Append-only facts and projection are preserved. |
| `core/agent`, `core/agent-loop` | Ported subset | Turn/step loop, streaming assembly, cancellation, step budget. No inbox projection, no retries, no goal/plan interaction. |
| `core/tools` | Ported shape | Name-keyed registry, schema admission, guarded execution, ordered results, concurrency flag. No post-execute waterfall or presentation layer. |
| `core/system-prompt` | Simplified | One configured instruction block. No section registry or per-agent prompt composition. |
| `core/scope` | Not ported | One agent per process run. Scope keys and shadowing have no consumer yet. |
| `llm/llm` | Ported seam | Provider-neutral request/stream vocabulary and `LlmProvider`. |
| `llm/llm-deepseek` | Ported subset | Messages request, SSE, usage, thinking, tool calls. No Files API, image input, model listing, account auth, or request extensions. |
| `llm/llm-retry` | Ported policy | Pure retry classification and capped backoff. The loop does not yet apply it automatically. |
| `shell/bash-local`, `tool-bash` | Ported | `bash -lc`, timeout, cancellation, 64 KiB per-stream cap. No persistent shell. |
| `subprocess` | Uses Tokio | Child spawning is local to the bash tool. |
| `ssh`, `terminal` | Not ported | No remote execution or PTY. |
| `ptc-runtime` | Not ported | No programmatic tool calling. |
| `sandbox` | Partial | Permission modes and lexical path policy. No OS-level confinement. |
| `fs/fs-local`, `tool-fs`, `tool-fs-search`, `tool-str-replace-editor` | Ported subset | `read_file`, `write_file`, `edit_file`, `list_dir`, `grep`. No patch format, observation policy, or LSP-guided edits. |
| `web/tool-web` | Partial | `web_fetch` only. No search provider. |
| `lsp` | Not ported | |
| `skill` | Not ported | |
| `computer-use`, `browser-use` | Not ported | |
| `compaction` | Not ported | History grows until the provider limit. |
| `context` | Partial | Session working directory only. |
| `subagent` | Not ported | |
| `jobs` | Not ported | |
| `bundle` | Replaced | The CLI composes `Plugin` values directly. |
| `workflow`, `todo`, `plan`, `goal`, `schedule`, `preset`, `guard` | Not ported | `max_steps` is the only loop guard. |
| `extensions` | Not ported | Runtime self-modification would require a dynamic loader. |
| `session-query` | Partial | `sessions list` and `sessions show`. |
| `attachment` | Not ported | Text only. |
| `spill` | Partial | Hard caps on stream, file, and web body sizes. |
| `storage`, `workspace` | Partial | `DSH_HOME`, SQLite database, and session working directory. |
| `feedback`, `identity`, `settings`, `credentials` | Partial | Environment configuration and one credential variable. |
| `acp` | Not ported | |
| `interaction`, `boot` | Replaced | CLI interaction and composition root. |
| `sdk` | Not ported | |
| `host`, `client` | Not ported | Desktop and browser surfaces. |
| `mcp` | Not ported | |
| `experimental` | Not ported | |
| `test-support` | Inline | Mocks live beside their tests. |
| `util` | Inline | Zero-dependency helpers moved to the owning crate. |
| `apps/cli` | Replaced | `dsh-cli`. |
| `apps/desktop`, `apps/desktop-host`, `apps/web`, `apps/plugin-market-generator` | Out of scope | Explicitly excluded by the request. |
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

## Next slices

In order of leverage:

1. **Symlink-safe path policy** and an OS sandbox backend behind the same `ToolContext` boundary.
2. **Automatic retry** with request freeze and duplicate-effect accounting.
3. **Compaction** with a token budget and a logged replacement event.
4. **Session fork** at a turn boundary.
5. **Subagents** as a tool that creates a child session and returns a durable notice.
6. **MCP** as an external tool provider behind the existing `Tool` trait.
7. **Web client** over a small HTTP/SSE server, reusing `RunEvent`.
