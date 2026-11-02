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
    /// Inspect or run durable schedules.
    Schedules(SchedulesArgs),
    /// Manage locally stored provider credentials.
    Auth(AuthArgs),
    /// Inspect or edit durable provider routes.
    Providers(ProvidersArgs),
    /// Print local run telemetry.
    Telemetry,
    /// List registered tools.
    Tools,
    /// Print effective configuration.
    Config,
    /// Serve the browser client.
    Web {
        /// Bind address.
        #[arg(long, default_value = "127.0.0.1:3080")]
        bind: String,
    },
    /// Serve the line-delimited JSON-RPC SDK on stdio.
    Sdk,
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
    /// Provider route to use for this run.
    #[arg(long, value_name = "NAME")]
    pub provider: Option<String>,
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
    /// Copy a session prefix into a new session.
    Fork {
        /// Source session identity.
        source: String,
        /// New session identity.
        target: String,
        /// Last source sequence to copy; defaults to the latest.
        #[arg(long)]
        through: Option<u64>,
    },
    /// Export a session as newline-delimited JSON.
    Export {
        /// Exact session identity.
        session_id: String,
        /// Write to this path instead of stdout.
        #[arg(long)]
        output: Option<PathBuf>,
    },
    /// Search durable event payloads.
    Search {
        /// Text to find.
        query: String,
        /// Maximum hits.
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Seal a turn left open by a process loss.
    Repair {
        /// Exact session identity.
        session_id: String,
    },
}

/// Schedule inspection and execution commands.
#[derive(Debug, Args)]
pub struct SchedulesArgs {
    /// Schedule operation.
    #[command(subcommand)]
    pub command: SchedulesCommand,
}

/// Schedule operations.
#[derive(Debug, Subcommand)]
pub enum SchedulesCommand {
    /// List stored schedules.
    List,
    /// Run every schedule whose next run time has passed.
    RunDue,
}

/// Credential commands.
#[derive(Debug, Args)]
pub struct AuthArgs {
    /// Credential operation.
    #[command(subcommand)]
    pub command: AuthCommand,
}

/// Credential operations.
#[derive(Debug, Subcommand)]
pub enum AuthCommand {
    /// Store a provider secret read from stdin.
    Set {
        /// Provider name, such as `local-gw`.
        provider: String,
    },
    /// List providers with stored secrets.
    List,
    /// Remove a provider secret.
    Remove {
        /// Provider name.
        provider: String,
    },
}

#[derive(Debug, Args)]
pub struct ProvidersArgs {
    #[command(subcommand)]
    pub command: ProvidersCommand,
}

#[derive(Debug, Subcommand)]
pub enum ProvidersCommand {
    /// List durable provider routes.
    List,
    /// Store or replace one provider route.
    Add {
        /// Route name, such as `local-gw`.
        name: String,
        /// Messages root, without `/v1/messages`.
        #[arg(long)]
        base_url: String,
        /// Model id sent with every request on this route.
        #[arg(long)]
        model: String,
        /// Variable holding the key; omit to use a stored credential.
        #[arg(long)]
        api_key_env: Option<String>,
    },
    /// Remove a provider route.
    Remove {
        /// Route name.
        name: String,
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
            "--provider",
            "local-gw",
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
        assert_eq!(args.provider.as_deref(), Some("local-gw"));
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
    fn parses_session_admin_commands() {
        let cli = Cli::try_parse_from([
            "dsh",
            "sessions",
            "fork",
            "source",
            "target",
            "--through",
            "3",
        ])
        .expect("fork");
        let Command::Sessions(args) = cli.command else {
            panic!("expected sessions");
        };
        assert!(matches!(
            args.command,
            SessionsCommand::Fork { ref source, ref target, through: Some(3) }
                if source == "source" && target == "target"
        ));

        let cli = Cli::try_parse_from(["dsh", "sessions", "search", "needle", "--limit", "5"])
            .expect("search");
        let Command::Sessions(args) = cli.command else {
            panic!("expected sessions");
        };
        assert!(matches!(
            args.command,
            SessionsCommand::Search { ref query, limit: 5 } if query == "needle"
        ));

        let cli = Cli::try_parse_from(["dsh", "sessions", "repair", "session-1"]).expect("repair");
        assert!(matches!(cli.command, Command::Sessions(_)));
        let cli = Cli::try_parse_from(["dsh", "sessions", "export", "session-1"]).expect("export");
        assert!(matches!(cli.command, Command::Sessions(_)));
    }

    #[test]
    fn parses_web_bind_address() {
        let cli = Cli::try_parse_from(["dsh", "web", "--bind", "127.0.0.1:0"]).expect("web");
        assert!(matches!(
            cli.command,
            Command::Web { ref bind } if bind == "127.0.0.1:0"
        ));
        let cli = Cli::try_parse_from(["dsh", "web"]).expect("web default");
        assert!(matches!(
            cli.command,
            Command::Web { ref bind } if bind == "127.0.0.1:3080"
        ));
    }

    #[test]
    fn parses_schedule_commands() {
        let cli = Cli::try_parse_from(["dsh", "schedules", "list"]).expect("list");
        assert!(matches!(cli.command, Command::Schedules(_)));
        let cli = Cli::try_parse_from(["dsh", "schedules", "run-due"]).expect("run-due");
        let Command::Schedules(args) = cli.command else {
            panic!("expected schedules");
        };
        assert!(matches!(args.command, SchedulesCommand::RunDue));
    }

    #[test]
    fn parses_auth_commands() {
        let cli = Cli::try_parse_from(["dsh", "auth", "set", "deepseek"]).expect("set");
        let Command::Auth(args) = cli.command else {
            panic!("expected auth");
        };
        assert!(matches!(
            args.command,
            AuthCommand::Set { ref provider } if provider == "deepseek"
        ));
        let cli = Cli::try_parse_from(["dsh", "auth", "list"]).expect("list");
        assert!(matches!(cli.command, Command::Auth(_)));
        let cli = Cli::try_parse_from(["dsh", "auth", "remove", "deepseek"]).expect("remove");
        assert!(matches!(cli.command, Command::Auth(_)));
    }

    #[test]
    fn rejects_a_missing_subcommand() {
        assert!(Cli::try_parse_from(["dsh"]).is_err());
        assert!(Cli::try_parse_from(["dsh", "unknown"]).is_err());
    }

    #[test]
    fn parses_provider_list() {
        let cli = Cli::try_parse_from(["dsh", "providers", "list"]).expect("list");
        let Command::Providers(args) = cli.command else {
            panic!("expected providers");
        };
        assert!(matches!(args.command, ProvidersCommand::List));
    }

    #[test]
    fn parses_provider_add() {
        let cli = Cli::try_parse_from([
            "dsh",
            "providers",
            "add",
            "local-gw",
            "--base-url",
            "http://127.0.0.1:1234",
            "--model",
            "qwen3-coder",
            "--api-key-env",
            "LOCAL_GW_KEY",
        ])
        .expect("add");
        let Command::Providers(args) = cli.command else {
            panic!("expected providers");
        };
        assert!(matches!(
            args.command,
            ProvidersCommand::Add {
                ref name,
                ref base_url,
                ref model,
                api_key_env: Some(ref var),
            } if name == "local-gw"
                && base_url == "http://127.0.0.1:1234"
                && model == "qwen3-coder"
                && var == "LOCAL_GW_KEY"
        ));

        let cli = Cli::try_parse_from([
            "dsh",
            "providers",
            "add",
            "local-gw",
            "--base-url",
            "http://127.0.0.1:1234",
            "--model",
            "qwen3-coder",
        ])
        .expect("add without a variable");
        let Command::Providers(args) = cli.command else {
            panic!("expected providers");
        };
        assert!(matches!(
            args.command,
            ProvidersCommand::Add {
                api_key_env: None,
                ..
            }
        ));
    }

    #[test]
    fn parses_provider_remove() {
        let cli = Cli::try_parse_from(["dsh", "providers", "remove", "local-gw"]).expect("remove");
        let Command::Providers(args) = cli.command else {
            panic!("expected providers");
        };
        assert!(matches!(
            args.command,
            ProvidersCommand::Remove { ref name } if name == "local-gw"
        ));
    }
}
