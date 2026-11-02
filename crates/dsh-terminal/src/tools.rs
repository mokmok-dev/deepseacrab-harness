//! The four model-facing terminal tools.

use crate::manager::{Opened, ReadChunk, SharedTerminals, TerminalManager, lock_manager};
use async_trait::async_trait;
use dsh_core::{Tool, ToolContext, ToolOutcome, ToolSchema};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

/// Build one tool set that shares a single terminal manager.
pub fn terminal_tools(terminals: &SharedTerminals) -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(TerminalOpenTool {
            terminals: Arc::clone(terminals),
        }),
        Arc::new(TerminalReadTool {
            terminals: Arc::clone(terminals),
        }),
        Arc::new(TerminalWriteTool {
            terminals: Arc::clone(terminals),
        }),
        Arc::new(TerminalCloseTool {
            terminals: Arc::clone(terminals),
        }),
    ]
}

/// Build a fresh manager and the four tools over it.
pub fn new_tool_set() -> Vec<Arc<dyn Tool>> {
    let terminals = Arc::new(Mutex::new(TerminalManager::new()));
    terminal_tools(&terminals)
}

struct TerminalOpenTool {
    terminals: SharedTerminals,
}

#[derive(Deserialize)]
struct OpenArguments {
    command: Option<String>,
    cwd: Option<String>,
}

#[async_trait]
impl Tool for TerminalOpenTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "terminal_open".to_string(),
            description: "Start a command in a persistent pseudo-terminal and return \
                          `opened <id> pid <pid>`. The terminal keeps running between calls; \
                          use terminal_write, terminal_read, and terminal_close to drive it. \
                          Refused in read-only mode."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "Program and space-separated arguments to run. Defaults to `bash`."
                    },
                    "cwd": {
                        "type": "string",
                        "description": "Working directory. Defaults to the session working directory."
                    }
                }
            }),
        }
    }

    async fn execute(&self, context: &ToolContext, arguments: Value) -> ToolOutcome {
        let arguments: OpenArguments = match serde_json::from_value(arguments) {
            Ok(arguments) => arguments,
            Err(error) => return ToolOutcome::error(format!("invalid arguments: {error}")),
        };
        if !context.permission.allows_write() {
            return ToolOutcome::error(format!(
                "permission mode {} refuses terminal_open",
                context.permission.as_str()
            ));
        }
        let cwd = match arguments
            .cwd
            .as_deref()
            .map_or_else(|| Ok(context.cwd.clone()), |raw| context.readable_path(raw))
        {
            Ok(cwd) => cwd,
            Err(error) => return ToolOutcome::error(error.to_string()),
        };
        let command = arguments.command.unwrap_or_else(|| "bash".to_string());
        match lock_manager(&self.terminals).open(&command, &cwd) {
            Ok(opened) => ToolOutcome::text(open_text(&opened)),
            Err(error) => ToolOutcome::error(error.to_string()),
        }
    }
}

struct TerminalReadTool {
    terminals: SharedTerminals,
}

#[derive(Deserialize)]
struct ReadArguments {
    id: String,
}

#[async_trait]
impl Tool for TerminalReadTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "terminal_read".to_string(),
            description: "Return the output a terminal produced since the previous read, \
                          followed by a `bytes: <n>` line. Returns `no new output` when \
                          nothing new arrived. The buffer keeps the newest 1 MiB."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "id": {
                        "type": "string",
                        "description": "Terminal id returned by terminal_open."
                    }
                },
                "required": ["id"]
            }),
        }
    }

    async fn execute(&self, _context: &ToolContext, arguments: Value) -> ToolOutcome {
        let arguments: ReadArguments = match serde_json::from_value(arguments) {
            Ok(arguments) => arguments,
            Err(error) => return ToolOutcome::error(format!("invalid arguments: {error}")),
        };
        match lock_manager(&self.terminals).read(&arguments.id) {
            Ok(chunk) => ToolOutcome::text(read_text(chunk)),
            Err(error) => ToolOutcome::error(error.to_string()),
        }
    }
}

struct TerminalWriteTool {
    terminals: SharedTerminals,
}

#[derive(Deserialize)]
struct WriteArguments {
    id: String,
    input: String,
}

#[async_trait]
impl Tool for TerminalWriteTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "terminal_write".to_string(),
            description: "Write input to a terminal and flush it, returning `wrote <n> bytes`. \
                          A trailing newline is needed to submit a shell command."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "id": {
                        "type": "string",
                        "description": "Terminal id returned by terminal_open."
                    },
                    "input": {
                        "type": "string",
                        "description": "Bytes to write to the terminal."
                    }
                },
                "required": ["id", "input"]
            }),
        }
    }

    async fn execute(&self, _context: &ToolContext, arguments: Value) -> ToolOutcome {
        let arguments: WriteArguments = match serde_json::from_value(arguments) {
            Ok(arguments) => arguments,
            Err(error) => return ToolOutcome::error(format!("invalid arguments: {error}")),
        };
        match lock_manager(&self.terminals).write(&arguments.id, &arguments.input) {
            Ok(bytes) => ToolOutcome::text(format!("wrote {bytes} bytes")),
            Err(error) => ToolOutcome::error(error.to_string()),
        }
    }
}

struct TerminalCloseTool {
    terminals: SharedTerminals,
}

#[derive(Deserialize)]
struct CloseArguments {
    id: String,
}

#[async_trait]
impl Tool for TerminalCloseTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "terminal_close".to_string(),
            description: "Close a terminal, drop its writer, and kill its child process, \
                          returning `closed <id>`. Closing an unknown id is an error."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "id": {
                        "type": "string",
                        "description": "Terminal id returned by terminal_open."
                    }
                },
                "required": ["id"]
            }),
        }
    }

    async fn execute(&self, _context: &ToolContext, arguments: Value) -> ToolOutcome {
        let arguments: CloseArguments = match serde_json::from_value(arguments) {
            Ok(arguments) => arguments,
            Err(error) => return ToolOutcome::error(format!("invalid arguments: {error}")),
        };
        match lock_manager(&self.terminals).close(&arguments.id) {
            Ok(()) => ToolOutcome::text(format!("closed {}", arguments.id)),
            Err(error) => ToolOutcome::error(error.to_string()),
        }
    }
}

fn open_text(opened: &Opened) -> String {
    format!("opened {} pid {}", opened.id, opened.pid)
}

fn read_text(chunk: ReadChunk) -> String {
    match chunk {
        ReadChunk::Empty => "no new output".to_string(),
        ReadChunk::Data { text, bytes } => {
            let mut content = text;
            if !content.ends_with('\n') {
                content.push('\n');
            }
            content.push_str("bytes: ");
            content.push_str(&bytes.to_string());
            content
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manager::lock_manager;
    use crate::output::MAX_OUTPUT_BYTES;
    use dsh_core::PermissionMode;
    use std::path::Path;
    use std::thread;
    use std::time::{Duration, Instant};
    use tokio_util::sync::CancellationToken;

    const WAIT: Duration = Duration::from_secs(3);

    fn context(directory: &Path, permission: PermissionMode) -> ToolContext {
        ToolContext::new(
            directory.to_path_buf(),
            permission,
            CancellationToken::new(),
        )
    }

    fn build() -> (SharedTerminals, Vec<Arc<dyn Tool>>) {
        let terminals = Arc::new(Mutex::new(TerminalManager::new()));
        let tools = terminal_tools(&terminals);
        (terminals, tools)
    }

    fn tool<'a>(tools: &'a [Arc<dyn Tool>], name: &str) -> &'a dyn Tool {
        tools
            .iter()
            .find(|tool| tool.schema().name == name)
            .map(AsRef::as_ref)
            .expect("registered tool")
    }

    async fn call(
        tools: &[Arc<dyn Tool>],
        name: &str,
        context: &ToolContext,
        arguments: Value,
    ) -> ToolOutcome {
        tool(tools, name).execute(context, arguments).await
    }

    fn parse_opened(text: &str) -> (String, u32) {
        let rest = text.strip_prefix("opened ").expect("opened prefix");
        let (id, pid) = rest.split_once(" pid ").expect("pid separator");
        (id.to_string(), pid.parse().expect("numeric pid"))
    }

    async fn open(
        tools: &[Arc<dyn Tool>],
        context: &ToolContext,
        arguments: Value,
    ) -> (String, u32) {
        let outcome = call(tools, "terminal_open", context, arguments).await;
        assert!(!outcome.is_error, "{}", outcome.text_content());
        parse_opened(&outcome.text_content())
    }

    async fn close(tools: &[Arc<dyn Tool>], context: &ToolContext, id: &str) {
        let outcome = call(tools, "terminal_close", context, json!({"id": id})).await;
        assert!(!outcome.is_error, "{}", outcome.text_content());
        assert_eq!(outcome.text_content(), format!("closed {id}"));
    }

    async fn read_until(
        tools: &[Arc<dyn Tool>],
        context: &ToolContext,
        id: &str,
        needle: &str,
    ) -> String {
        let deadline = Instant::now() + WAIT;
        let mut seen = String::new();
        loop {
            let outcome = call(tools, "terminal_read", context, json!({"id": id})).await;
            assert!(!outcome.is_error, "{}", outcome.text_content());
            let text = outcome.text_content();
            if text != "no new output" {
                seen.push_str(&text);
            }
            if seen.contains(needle) {
                return seen;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {needle:?}; saw {seen:?}"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[tokio::test]
    async fn a_shell_round_trips_written_input() {
        let directory = tempfile::tempdir().expect("tempdir");
        let context = context(directory.path(), PermissionMode::WorkspaceWrite);
        let (_terminals, tools) = build();
        let (id, pid) = open(&tools, &context, json!({})).await;
        assert!(pid > 0);
        assert!(Path::new("/proc").join(pid.to_string()).exists());

        let input = "printf hello\n";
        let written = call(
            &tools,
            "terminal_write",
            &context,
            json!({"id": id, "input": input}),
        )
        .await;
        assert!(!written.is_error, "{}", written.text_content());
        assert_eq!(
            written.text_content(),
            format!("wrote {} bytes", input.len())
        );

        let seen = read_until(&tools, &context, &id, "hello").await;
        assert!(seen.contains("hello"), "{seen:?}");
        assert!(seen.contains("bytes: "), "{seen:?}");
        close(&tools, &context, &id).await;
    }

    #[tokio::test]
    async fn a_terminal_starts_in_the_requested_directory() {
        let directory = tempfile::tempdir().expect("tempdir");
        let subdirectory = directory.path().join("sub");
        std::fs::create_dir(&subdirectory).expect("subdirectory");
        let context = context(directory.path(), PermissionMode::WorkspaceWrite);
        let (_terminals, tools) = build();
        let (id, _pid) = open(&tools, &context, json!({"command": "pwd -P", "cwd": "sub"})).await;
        let expected = std::fs::canonicalize(&subdirectory).expect("canonical");
        let seen = read_until(&tools, &context, &id, &expected.display().to_string()).await;
        assert!(seen.contains(&expected.display().to_string()), "{seen:?}");
        close(&tools, &context, &id).await;
    }

    #[tokio::test]
    async fn a_quiet_terminal_reports_no_new_output() {
        let directory = tempfile::tempdir().expect("tempdir");
        let context = context(directory.path(), PermissionMode::WorkspaceWrite);
        let (_terminals, tools) = build();
        let (id, _pid) = open(&tools, &context, json!({"command": "cat"})).await;
        let outcome = call(&tools, "terminal_read", &context, json!({"id": id})).await;
        assert!(!outcome.is_error, "{}", outcome.text_content());
        assert_eq!(outcome.text_content(), "no new output");
        close(&tools, &context, &id).await;
    }

    #[tokio::test]
    async fn writes_report_their_byte_count() {
        let directory = tempfile::tempdir().expect("tempdir");
        let context = context(directory.path(), PermissionMode::WorkspaceWrite);
        let (_terminals, tools) = build();
        let (id, _pid) = open(&tools, &context, json!({"command": "cat"})).await;
        let outcome = call(
            &tools,
            "terminal_write",
            &context,
            json!({"id": id, "input": "hello"}),
        )
        .await;
        assert!(!outcome.is_error, "{}", outcome.text_content());
        assert_eq!(outcome.text_content(), "wrote 5 bytes");
        close(&tools, &context, &id).await;
    }

    #[tokio::test]
    async fn writing_after_close_is_an_error() {
        let directory = tempfile::tempdir().expect("tempdir");
        let context = context(directory.path(), PermissionMode::WorkspaceWrite);
        let (_terminals, tools) = build();
        let (id, _pid) = open(&tools, &context, json!({"command": "cat"})).await;
        close(&tools, &context, &id).await;
        let outcome = call(
            &tools,
            "terminal_write",
            &context,
            json!({"id": id, "input": "late"}),
        )
        .await;
        assert!(outcome.is_error);
        assert_eq!(outcome.text_content(), format!("unknown terminal: {id}"));
    }

    #[tokio::test]
    async fn closing_twice_reports_the_second_close_as_unknown() {
        let directory = tempfile::tempdir().expect("tempdir");
        let context = context(directory.path(), PermissionMode::WorkspaceWrite);
        let (_terminals, tools) = build();
        let (id, _pid) = open(&tools, &context, json!({"command": "cat"})).await;
        close(&tools, &context, &id).await;
        let outcome = call(&tools, "terminal_close", &context, json!({"id": id})).await;
        assert!(outcome.is_error);
        assert_eq!(outcome.text_content(), format!("unknown terminal: {id}"));
    }

    #[tokio::test]
    async fn invalid_ids_are_refused_by_every_terminal_tool() {
        let directory = tempfile::tempdir().expect("tempdir");
        let context = context(directory.path(), PermissionMode::WorkspaceWrite);
        let (_terminals, tools) = build();
        for name in ["terminal_read", "terminal_write", "terminal_close"] {
            for id in ["", "a b", "../escape", "a/b"] {
                let mut arguments = json!({"id": id});
                if name == "terminal_write" {
                    arguments["input"] = json!("x");
                }
                let outcome = call(&tools, name, &context, arguments).await;
                assert!(outcome.is_error, "{name} {id:?}");
                assert_eq!(
                    outcome.text_content(),
                    format!("invalid terminal id: {id}"),
                    "{name} {id:?}"
                );
            }
        }
    }

    #[tokio::test]
    async fn unknown_ids_are_refused_by_every_terminal_tool() {
        let directory = tempfile::tempdir().expect("tempdir");
        let context = context(directory.path(), PermissionMode::WorkspaceWrite);
        let (_terminals, tools) = build();
        for name in ["terminal_read", "terminal_write", "terminal_close"] {
            let mut arguments = json!({"id": "deadbeef"});
            if name == "terminal_write" {
                arguments["input"] = json!("x");
            }
            let outcome = call(&tools, name, &context, arguments).await;
            assert!(outcome.is_error, "{name}");
            assert_eq!(
                outcome.text_content(),
                "unknown terminal: deadbeef",
                "{name}"
            );
        }
    }

    #[tokio::test]
    async fn read_only_mode_refuses_opening_a_terminal() {
        let directory = tempfile::tempdir().expect("tempdir");
        let context = context(directory.path(), PermissionMode::ReadOnly);
        let (_terminals, tools) = build();
        let outcome = call(&tools, "terminal_open", &context, json!({})).await;
        assert!(outcome.is_error);
        assert_eq!(
            outcome.text_content(),
            "permission mode read-only refuses terminal_open"
        );
    }

    #[tokio::test]
    async fn a_cwd_outside_the_workspace_is_refused() {
        let directory = tempfile::tempdir().expect("tempdir");
        let context = context(directory.path(), PermissionMode::WorkspaceWrite);
        let (_terminals, tools) = build();
        let outcome = call(&tools, "terminal_open", &context, json!({"cwd": "/"})).await;
        assert!(outcome.is_error);
        assert!(
            outcome.text_content().contains("outside workspace"),
            "{}",
            outcome.text_content()
        );
    }

    #[tokio::test]
    async fn malformed_arguments_are_refused() {
        let directory = tempfile::tempdir().expect("tempdir");
        let context = context(directory.path(), PermissionMode::WorkspaceWrite);
        let (_terminals, tools) = build();
        for (name, arguments) in [
            ("terminal_open", json!({"command": 7})),
            ("terminal_read", json!({})),
            ("terminal_write", json!({"id": "t1"})),
            ("terminal_close", json!({"id": 7})),
        ] {
            let outcome = call(&tools, name, &context, arguments).await;
            assert!(outcome.is_error, "{name}");
            assert!(
                outcome.text_content().contains("invalid arguments"),
                "{name}: {}",
                outcome.text_content()
            );
        }
    }

    #[tokio::test]
    async fn empty_commands_are_refused() {
        let directory = tempfile::tempdir().expect("tempdir");
        let context = context(directory.path(), PermissionMode::WorkspaceWrite);
        let (_terminals, tools) = build();
        let outcome = call(&tools, "terminal_open", &context, json!({"command": "  "})).await;
        assert!(outcome.is_error);
        assert_eq!(outcome.text_content(), "invalid command: \"  \"");
    }

    #[tokio::test]
    async fn a_missing_program_is_a_spawn_error() {
        let directory = tempfile::tempdir().expect("tempdir");
        let context = context(directory.path(), PermissionMode::WorkspaceWrite);
        let (_terminals, tools) = build();
        let outcome = call(
            &tools,
            "terminal_open",
            &context,
            json!({"command": "dsh-no-such-program"}),
        )
        .await;
        assert!(outcome.is_error);
        assert!(
            outcome.text_content().starts_with("cannot start command: "),
            "{}",
            outcome.text_content()
        );
    }

    #[tokio::test]
    async fn non_utf8_output_is_replaced_lossily() {
        let directory = tempfile::tempdir().expect("tempdir");
        let context = context(directory.path(), PermissionMode::WorkspaceWrite);
        let (_terminals, tools) = build();
        let (id, _pid) = open(&tools, &context, json!({})).await;
        let written = call(
            &tools,
            "terminal_write",
            &context,
            json!({"id": id, "input": "printf '\\377\\n'\n"}),
        )
        .await;
        assert!(!written.is_error, "{}", written.text_content());
        let seen = read_until(&tools, &context, &id, "\u{FFFD}").await;
        assert!(seen.contains('\u{FFFD}'), "{seen:?}");
        close(&tools, &context, &id).await;
    }

    #[tokio::test]
    async fn closing_kills_the_child_process() {
        let directory = tempfile::tempdir().expect("tempdir");
        let context = context(directory.path(), PermissionMode::WorkspaceWrite);
        let (_terminals, tools) = build();
        let (id, pid) = open(&tools, &context, json!({"command": "sleep 30"})).await;
        let process = Path::new("/proc").join(pid.to_string());
        assert!(process.exists());
        close(&tools, &context, &id).await;

        let deadline = Instant::now() + WAIT;
        while process.exists() {
            assert!(Instant::now() < deadline, "child {pid} is still alive");
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[tokio::test]
    async fn output_is_capped_at_one_mebibyte() {
        let directory = tempfile::tempdir().expect("tempdir");
        let context = context(directory.path(), PermissionMode::WorkspaceWrite);
        let (terminals, tools) = build();
        let (id, _pid) = open(&tools, &context, json!({})).await;
        let produced = MAX_OUTPUT_BYTES + 4096;
        let written = call(
            &tools,
            "terminal_write",
            &context,
            json!({"id": id, "input": format!("head -c {produced} /dev/zero | tr '\\0' x\n")}),
        )
        .await;
        assert!(!written.is_error, "{}", written.text_content());

        let deadline = Instant::now() + WAIT;
        loop {
            if lock_manager(&terminals).output_len(&id) == Some(MAX_OUTPUT_BYTES) {
                break;
            }
            assert!(Instant::now() < deadline, "terminal buffer never filled");
            thread::sleep(Duration::from_millis(10));
        }

        let outcome = call(&tools, "terminal_read", &context, json!({"id": id})).await;
        assert!(!outcome.is_error, "{}", outcome.text_content());
        let text = outcome.text_content();
        let (body, count) = text.rsplit_once("bytes: ").expect("byte count line");
        assert_eq!(count, MAX_OUTPUT_BYTES.to_string());
        assert!(
            (MAX_OUTPUT_BYTES..=MAX_OUTPUT_BYTES + 1).contains(&body.len()),
            "body length {}",
            body.len()
        );
        assert!(body.starts_with('x'), "{:?}", &body[..body.len().min(16)]);
        close(&tools, &context, &id).await;
    }

    #[test]
    fn read_text_marks_the_byte_count_after_a_newline() {
        assert_eq!(
            read_text(ReadChunk::Data {
                text: "abc".to_string(),
                bytes: 3,
            }),
            "abc\nbytes: 3"
        );
        assert_eq!(
            read_text(ReadChunk::Data {
                text: "abc\n".to_string(),
                bytes: 4,
            }),
            "abc\nbytes: 4"
        );
        assert_eq!(read_text(ReadChunk::Empty), "no new output");
    }
}
