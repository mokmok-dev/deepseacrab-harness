//! Command grammar.

use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;

/// Headless `DeepSeek` Harness.
#[derive(Debug, Parser)]
#[command(name = "dsh", version, about = "Headless DeepSeek Harness", long_about = None)]
pub struct Cli {
    /// Operation to run.
    #[command(subcommand)]
    pub command: Command,
}

/// Top-level commands.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run one task and print the final answer.
    Run(RunArgs),
    /// Inspect durable sessions.
    Sessions(SessionsArgs),
    /// List registered tools.
    Tools,
    /// Print effective configuration.
    Config,
}

/// One-shot task arguments.
#[derive(Debug, Args)]
pub struct RunArgs {
    /// Task text. Omit, or pass `-`, to read the task from stdin.
    #[arg(value_name = "TASK", num_args = 0..)]
    pub task: Vec<String>,
    /// Continue or create an exact session identity.
    #[arg(long)]
    pub session_id: Option<String>,
    /// Override the route model.
    #[arg(long)]
    pub model: Option<String>,
    /// Working directory for the session.
    #[arg(long)]
    pub cwd: Option<PathBuf>,
    /// Emit newline-delimited run events instead of the plain answer.
    #[arg(long)]
    pub json: bool,
    /// `read-only`, `workspace-write`, or `danger-full-access`.
    #[arg(long, value_name = "MODE")]
    pub permission_mode: Option<String>,
    /// Step budget for this turn.
    #[arg(long)]
    pub max_steps: Option<u32>,
}

/// Session inspection commands.
#[derive(Debug, Args)]
pub struct SessionsArgs {
    /// Inspection operation.
    #[command(subcommand)]
    pub command: SessionsCommand,
}

/// Session inspection operations.
#[derive(Debug, Subcommand)]
pub enum SessionsCommand {
    /// List stored sessions.
    List,
    /// Print one session's event log.
    Show {
        /// Exact session identity.
        session_id: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn parses_a_multi_word_task() {
        let cli = Cli::try_parse_from(["dsh", "run", "fix", "the", "tests"]).expect("parse");
        let Command::Run(args) = cli.command else {
            panic!("expected run");
        };
        assert_eq!(args.task, vec!["fix", "the", "tests"]);
        assert!(!args.json);
    }

    #[test]
    fn parses_run_options() {
        let cli = Cli::try_parse_from([
            "dsh",
            "run",
            "--json",
            "--session-id",
            "session-1",
            "--model",
            "deepseek-v4-pro",
            "--cwd",
            "/tmp",
            "--permission-mode",
            "read-only",
            "--max-steps",
            "3",
            "task",
        ])
        .expect("parse");
        let Command::Run(args) = cli.command else {
            panic!("expected run");
        };
        assert!(args.json);
        assert_eq!(args.session_id.as_deref(), Some("session-1"));
        assert_eq!(args.model.as_deref(), Some("deepseek-v4-pro"));
        assert_eq!(args.cwd.as_deref(), Some(std::path::Path::new("/tmp")));
        assert_eq!(args.permission_mode.as_deref(), Some("read-only"));
        assert_eq!(args.max_steps, Some(3));
    }

    #[test]
    fn parses_session_commands() {
        let cli = Cli::try_parse_from(["dsh", "sessions", "list"]).expect("list");
        assert!(matches!(cli.command, Command::Sessions(_)));
        let cli = Cli::try_parse_from(["dsh", "sessions", "show", "session-1"]).expect("show");
        let Command::Sessions(args) = cli.command else {
            panic!("expected sessions");
        };
        assert!(matches!(
            args.command,
            SessionsCommand::Show { ref session_id } if session_id == "session-1"
        ));
    }

    #[test]
    fn rejects_a_missing_subcommand() {
        assert!(Cli::try_parse_from(["dsh"]).is_err());
        assert!(Cli::try_parse_from(["dsh", "unknown"]).is_err());
    }
}
