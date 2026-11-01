//! Command implementations.

use crate::cli::{Cli, Command, RunArgs, SessionsArgs, SessionsCommand};
use anyhow::{Context, anyhow, bail};
use clap::Parser;
use dsh_agent::{AgentConfig, AgentLoop, EventSink, RunEvent};
use dsh_core::{
    Clock, Config, Env, ModelId, PermissionMode, Plugin, PluginContext, PluginError, Runtime,
    SessionEvent, SessionId, SessionStore, SystemClock, SystemEnv, TurnOutcome, compose,
};
use dsh_llm::{DeepSeekConfig, DeepSeekProvider};
use dsh_session::SqliteSessionStore;
use dsh_tools::ToolsPlugin;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use tokio::io::AsyncReadExt;
use tokio_util::sync::CancellationToken;

/// Parse the command line and dispatch.
pub async fn run() -> anyhow::Result<ExitCode> {
    let cli = Cli::parse();
    match cli.command {
        Command::Run(args) => run_task(args).await,
        Command::Sessions(args) => sessions(args),
        Command::Tools => tools(),
        Command::Config => show_config(),
    }
}

async fn run_task(args: RunArgs) -> anyhow::Result<ExitCode> {
    let mut config = config_from_env()?;
    if let Some(model) = args.model {
        config.model = model;
    }
    if let Some(raw) = args.permission_mode {
        config.permission = PermissionMode::parse(&raw).ok_or_else(|| {
            anyhow!(
                "invalid permission mode `{raw}`; use read-only, workspace-write, or danger-full-access"
            )
        })?;
    }
    if let Some(steps) = args.max_steps {
        if steps == 0 {
            bail!("--max-steps must be greater than zero");
        }
        config.max_steps = steps;
    }

    let cwd = resolve_cwd(args.cwd)?;
    let task = read_task(&args.task).await?;
    if task.trim().is_empty() {
        bail!("task must not be empty");
    }

    let session_id = match args.session_id {
        Some(raw) => SessionId::parse(raw)?,
        None => SessionId::generate(),
    };
    let env = SystemEnv;
    let runtime = build_runtime(&config, &env)?;
    let agent_config = AgentConfig {
        model: ModelId::parse(&config.model)?,
        system_prompt: config.system_prompt.clone(),
        max_tokens: config.max_tokens,
        effort: config.effort,
        max_steps: config.max_steps,
        max_parallel_tools: config.max_parallel_tools,
        permission: config.permission,
        cwd,
    };
    let agent = AgentLoop::new(
        runtime.store,
        runtime.provider,
        runtime.tools,
        agent_config,
        Arc::new(SystemClock) as Arc<dyn Clock>,
    )?;

    let sink = if args.json { json_sink() } else { text_sink() };
    let cancel = CancellationToken::new();
    install_cancel_with(cancel.clone(), async {
        let _ = tokio::signal::ctrl_c().await;
    });
    let outcome = agent
        .run(&session_id, &task, cancel, Some(sink))
        .await
        .map_err(|error| anyhow!("{error}"))?;

    if !args.json {
        println!("{}", outcome.text);
    }
    if outcome.is_success() {
        Ok(ExitCode::SUCCESS)
    } else {
        eprintln!("dsh: {}", describe_outcome(&outcome.outcome));
        Ok(ExitCode::FAILURE)
    }
}

fn config_from_env() -> anyhow::Result<Config> {
    Ok(Config::from_lookup(|key| std::env::var(key).ok())?)
}

fn build_runtime(config: &Config, env: &dyn Env) -> anyhow::Result<Runtime> {
    let plugins: Vec<Box<dyn Plugin>> = vec![
        Box::new(SqliteStorePlugin),
        Box::new(DeepSeekPlugin),
        Box::new(ToolsPlugin),
    ];
    compose(&plugins, config, env).map_err(|error| anyhow!("{error}"))
}

struct SqliteStorePlugin;

impl Plugin for SqliteStorePlugin {
    fn name(&self) -> &'static str {
        "sqlite-session-store"
    }

    fn install(&self, context: &mut PluginContext<'_>) -> Result<(), PluginError> {
        let store = SqliteSessionStore::open(context.config().database_path())
            .map_err(|error| PluginError::Failed(error.to_string()))?;
        context.set_store(Arc::new(store))
    }
}

struct DeepSeekPlugin;

impl Plugin for DeepSeekPlugin {
    fn name(&self) -> &'static str {
        "deepseek"
    }

    fn install(&self, context: &mut PluginContext<'_>) -> Result<(), PluginError> {
        let config = context.config();
        let api_key = config.api_key(context.env()).ok_or_else(|| {
            PluginError::Failed(format!(
                "MISSING_CREDENTIAL: set {} before running a task",
                config.api_key_env
            ))
        })?;
        let provider = DeepSeekProvider::new(DeepSeekConfig {
            base_url: config.base_url.clone(),
            api_key,
            timeout: std::time::Duration::from_secs(300),
        })
        .map_err(|error| PluginError::Failed(error.to_string()))?;
        context.set_provider(Arc::new(provider))
    }
}

fn resolve_cwd(raw: Option<PathBuf>) -> anyhow::Result<PathBuf> {
    let cwd = match raw {
        Some(path) => path,
        None => std::env::current_dir().context("cannot read the process working directory")?,
    };
    let cwd = cwd
        .canonicalize()
        .with_context(|| format!("cannot resolve working directory {}", cwd.display()))?;
    if !cwd.is_dir() {
        bail!("working directory is not a directory: {}", cwd.display());
    }
    Ok(cwd)
}

async fn read_task(parts: &[String]) -> anyhow::Result<String> {
    if let Some(task) = task_from_parts(parts)? {
        return Ok(task);
    }
    let mut task = String::new();
    tokio::io::stdin()
        .read_to_string(&mut task)
        .await
        .context("cannot read the task from stdin")?;
    Ok(task)
}

fn task_from_parts(parts: &[String]) -> anyhow::Result<Option<String>> {
    if parts.is_empty() || (parts.len() == 1 && parts[0] == "-") {
        return Ok(None);
    }
    if parts.iter().any(|part| part == "-") {
        bail!("`-` must be the only task argument");
    }
    Ok(Some(parts.join(" ")))
}

fn install_cancel_with(
    cancel: CancellationToken,
    signal: impl std::future::Future<Output = ()> + Send + 'static,
) {
    drop(tokio::spawn(async move {
        signal.await;
        cancel.cancel();
    }));
}

fn json_sink() -> EventSink {
    Arc::new(|event: RunEvent| {
        if let Ok(line) = serde_json::to_string(&event) {
            println!("{line}");
        }
    })
}

fn text_sink() -> EventSink {
    Arc::new(|event: RunEvent| match event {
        RunEvent::Thinking { text } => eprint!("{text}"),
        RunEvent::ToolCall { name, .. } => eprintln!("dsh: tool {name}"),
        RunEvent::ToolResult {
            name,
            output,
            is_error,
            ..
        } => {
            let first = output.lines().next().unwrap_or_default();
            eprintln!(
                "dsh: {name} {}: {}",
                if is_error { "error" } else { "ok" },
                truncate(first, 160)
            );
        }
        RunEvent::Error { code, message } => eprintln!("dsh: {code}: {message}"),
        RunEvent::Session { .. }
        | RunEvent::Status { .. }
        | RunEvent::Text { .. }
        | RunEvent::Final { .. } => {}
    })
}

fn sessions(args: SessionsArgs) -> anyhow::Result<ExitCode> {
    let config = config_from_env()?;
    let store = SqliteSessionStore::open(config.database_path())?;
    match args.command {
        SessionsCommand::List => {
            let sessions = store.list()?;
            if sessions.is_empty() {
                println!("no sessions");
                return Ok(ExitCode::SUCCESS);
            }
            for session in sessions {
                println!(
                    "{}\t{}\t{}\t{}",
                    session.id,
                    format_time(session.created_at),
                    session.event_count,
                    session.cwd.unwrap_or_default()
                );
            }
        }
        SessionsCommand::Show { session_id } => {
            let id = SessionId::parse(session_id)?;
            let events = store.load(&id)?;
            for record in events {
                println!(
                    "{}\t{}\t{}",
                    record.seq,
                    record.event.kind(),
                    summarize_event(&record.event)
                );
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn tools() -> anyhow::Result<ExitCode> {
    let registry = dsh_tools::default_registry()?;
    for schema in registry.schemas() {
        println!("{}\t{}", schema.name, schema.description);
    }
    Ok(ExitCode::SUCCESS)
}

fn show_config() -> anyhow::Result<ExitCode> {
    let config = config_from_env()?;
    let env = SystemEnv;
    let api_key_present = config.api_key(&env).is_some();
    let home = config.home.display().to_string();
    let database = config.database_path().display().to_string();
    let effort = config.effort.as_str();
    let permission = config.permission.as_str();
    let value = serde_json::json!({
        "model": config.model,
        "base_url": config.base_url,
        "api_key_env": config.api_key_env,
        "api_key_present": api_key_present,
        "max_tokens": config.max_tokens,
        "effort": effort,
        "max_steps": config.max_steps,
        "max_parallel_tools": config.max_parallel_tools,
        "permission": permission,
        "home": home,
        "database": database,
    });
    println!("{}", serde_json::to_string_pretty(&value)?);
    Ok(ExitCode::SUCCESS)
}

fn summarize_event(event: &SessionEvent) -> String {
    match event {
        SessionEvent::SessionCreated { cwd, .. } => cwd.clone().unwrap_or_default(),
        SessionEvent::TurnStarted { turn_id } => turn_id.to_string(),
        SessionEvent::StepStarted { step_id, .. } => step_id.to_string(),
        SessionEvent::UserMessage { message } => truncate(&message.text_content(), 120),
        SessionEvent::AssistantMessage { message } => truncate(&message.text(), 120),
        SessionEvent::ToolResult { message } => format!(
            "{} {}",
            if message.is_error { "error" } else { "ok" },
            truncate(&message.text_content(), 120)
        ),
        SessionEvent::TurnEnded { outcome, .. } => describe_outcome(outcome),
    }
}

fn describe_outcome(outcome: &TurnOutcome) -> String {
    match outcome {
        TurnOutcome::Completed => "completed".to_string(),
        TurnOutcome::StepLimit => "step limit reached".to_string(),
        TurnOutcome::Cancelled { reason } => format!("cancelled: {reason}"),
        TurnOutcome::Failed { code, message } => format!("{code}: {message}"),
    }
}

fn format_time(millis: i64) -> String {
    chrono::DateTime::from_timestamp_millis(millis)
        .map_or_else(|| millis.to_string(), |time| time.to_rfc3339())
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut shortened: String = text.chars().take(max).collect();
    shortened.push_str("...");
    shortened
}

#[cfg(test)]
mod tests {
    use super::*;
    use dsh_core::{
        AssistantMessage, ContentBlock, MessageId, StepId, TokenUsage, ToolCallId, ToolMessage,
        TurnId, UserMessage,
    };

    #[test]
    fn truncation_counts_characters_not_bytes() {
        assert_eq!(truncate("short", 10), "short");
        assert_eq!(truncate("abcdefgh", 3), "abc...");
        assert_eq!(truncate("日本語です", 2), "日本...");
    }

    #[test]
    fn plugin_names_are_stable() {
        assert_eq!(SqliteStorePlugin.name(), "sqlite-session-store");
        assert_eq!(DeepSeekPlugin.name(), "deepseek");
    }

    #[test]
    fn event_summaries_name_the_durable_fact() {
        let turn_id = TurnId::generate();
        let events = [
            SessionEvent::SessionCreated {
                cwd: Some("/work".to_string()),
                created_at: 0,
                format_version: 1,
            },
            SessionEvent::TurnStarted {
                turn_id: turn_id.clone(),
            },
            SessionEvent::StepStarted {
                turn_id: turn_id.clone(),
                step_id: StepId::generate(),
            },
            SessionEvent::UserMessage {
                message: UserMessage::text("prompt"),
            },
            SessionEvent::AssistantMessage {
                message: AssistantMessage {
                    id: MessageId::generate(),
                    content: vec![ContentBlock::text("answer")],
                    provider: dsh_core::ProviderId::parse("deepseek").expect("provider"),
                    model: dsh_core::ModelId::parse("deepseek-flash").expect("model"),
                    usage: TokenUsage::default(),
                },
            },
            SessionEvent::ToolResult {
                message: ToolMessage::text(ToolCallId::generate(), "tool ok"),
            },
            SessionEvent::TurnEnded {
                turn_id,
                outcome: TurnOutcome::Completed,
            },
        ];
        let summaries: Vec<_> = events.iter().map(summarize_event).collect();
        assert!(summaries.iter().all(|summary| !summary.is_empty()));
        assert_eq!(summaries[0], "/work");
        assert_eq!(summaries[3], "prompt");
        assert_eq!(summaries[4], "answer");
        assert_eq!(summaries[5], "ok tool ok");
        assert_eq!(summaries[6], "completed");
    }

    #[tokio::test]
    async fn cancellation_is_installed_from_a_signal() {
        let cancel = CancellationToken::new();
        let (sender, receiver) = tokio::sync::oneshot::channel();
        install_cancel_with(cancel.clone(), async {
            let _ = receiver.await;
        });
        sender.send(()).expect("signal");
        tokio::time::timeout(std::time::Duration::from_secs(1), cancel.cancelled())
            .await
            .expect("cancellation");
    }

    #[test]
    fn outcomes_have_stable_descriptions() {
        assert_eq!(describe_outcome(&TurnOutcome::Completed), "completed");
        assert_eq!(
            describe_outcome(&TurnOutcome::StepLimit),
            "step limit reached"
        );
        assert_eq!(
            describe_outcome(&TurnOutcome::Cancelled {
                reason: "user".to_string()
            }),
            "cancelled: user"
        );
        assert_eq!(
            describe_outcome(&TurnOutcome::Failed {
                code: "X".to_string(),
                message: "boom".to_string()
            }),
            "X: boom"
        );
    }

    #[test]
    fn time_format_falls_back_to_millis_outside_the_supported_range() {
        assert_eq!(format_time(i64::MAX), i64::MAX.to_string());
    }

    #[test]
    fn cwd_requires_an_existing_directory() {
        assert!(resolve_cwd(Some(PathBuf::from("/definitely/not/a/directory"))).is_err());
        let current = resolve_cwd(None).expect("current directory");
        assert!(current.is_absolute());
        assert!(current.is_dir());
    }

    #[test]
    fn task_parts_join_with_spaces() {
        let parts = vec!["fix".to_string(), "tests".to_string()];
        assert_eq!(
            task_from_parts(&parts).expect("task").as_deref(),
            Some("fix tests")
        );
    }

    #[test]
    fn missing_parts_select_stdin() {
        assert_eq!(task_from_parts(&[]).expect("stdin"), None);
        assert_eq!(task_from_parts(&["-".to_string()]).expect("stdin"), None);
    }

    #[test]
    fn mixed_stdin_marker_is_rejected() {
        let parts = vec!["fix".to_string(), "-".to_string()];
        assert!(task_from_parts(&parts).is_err());
    }
}
