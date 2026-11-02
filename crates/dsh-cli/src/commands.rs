//! Command implementations.

use crate::cli::{
    Cli, Command, RunArgs, SchedulesArgs, SchedulesCommand, SessionsArgs, SessionsCommand,
};
use anyhow::{Context, anyhow, bail};
use clap::Parser;
use dsh_agent::{AgentConfig, AgentLoop, EventSink, RunEvent};
use dsh_core::{
    Clock, Config, Env, EventSeq, ModelId, PermissionMode, Plugin, PluginContext, PluginError,
    Runtime, SessionEvent, SessionHeader, SessionId, SessionStore, SystemClock, SystemEnv,
    TurnOutcome, compose,
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
        Command::Schedules(args) => schedules(args).await,
        Command::Tools => tools(),
        Command::Config => show_config(),
        Command::Web { bind } => web(bind).await,
        Command::Sdk => sdk().await,
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
    let agent_config = agent_config(&config, cwd)?;
    let mut tools = runtime.tools;
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    add_delegation_tools(
        &mut tools,
        &runtime.store,
        &runtime.provider,
        &agent_config,
        &clock,
    )?;
    let agent = AgentLoop::new(
        runtime.store,
        runtime.provider,
        tools,
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

async fn web(bind: String) -> anyhow::Result<ExitCode> {
    let config = config_from_env()?;
    let cwd = resolve_cwd(None)?;
    let env = SystemEnv;
    let runtime = build_runtime(&config, &env)?;
    let agent_config = agent_config(&config, cwd)?;
    let mut tools = runtime.tools;
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    add_delegation_tools(
        &mut tools,
        &runtime.store,
        &runtime.provider,
        &agent_config,
        &clock,
    )?;
    let address: std::net::SocketAddr = bind.parse().context("invalid bind address")?;
    let state = dsh_web::WebState {
        store: runtime.store,
        provider: runtime.provider,
        tools,
        config: agent_config,
        clock: Arc::new(SystemClock) as Arc<dyn Clock>,
    };
    let cancel = CancellationToken::new();
    install_cancel_with(cancel.clone(), async {
        let _ = tokio::signal::ctrl_c().await;
    });
    println!("dsh web listening on http://{address}");
    dsh_web::serve(state, address, cancel)
        .await
        .map_err(|error| anyhow!("{error}"))?;
    Ok(ExitCode::SUCCESS)
}

async fn sdk() -> anyhow::Result<ExitCode> {
    let config = config_from_env()?;
    let cwd = resolve_cwd(None)?;
    let env = SystemEnv;
    let runtime = build_runtime(&config, &env)?;
    let agent_config = agent_config(&config, cwd)?;
    let mut tools = runtime.tools;
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    add_delegation_tools(
        &mut tools,
        &runtime.store,
        &runtime.provider,
        &agent_config,
        &clock,
    )?;
    let state = dsh_sdk::SdkState {
        store: runtime.store,
        provider: runtime.provider,
        tools,
        config: agent_config,
        clock: Arc::new(SystemClock) as Arc<dyn Clock>,
    };
    let cancel = CancellationToken::new();
    install_cancel_with(cancel.clone(), async {
        let _ = tokio::signal::ctrl_c().await;
    });
    let reader = tokio::io::BufReader::new(tokio::io::stdin());
    dsh_sdk::serve_io(state, reader, tokio::io::stdout(), cancel)
        .await
        .map_err(|error| anyhow!("{error}"))?;
    Ok(ExitCode::SUCCESS)
}

fn agent_config(config: &Config, cwd: PathBuf) -> anyhow::Result<AgentConfig> {
    Ok(AgentConfig {
        model: ModelId::parse(&config.model)?,
        system_prompt: config.system_prompt.clone(),
        max_tokens: config.max_tokens,
        effort: config.effort,
        max_steps: config.max_steps,
        max_parallel_tools: config.max_parallel_tools,
        permission: config.permission,
        cwd,
        state_dir: config.home.clone(),
        max_retries: config.max_retries,
        retry_base_ms: config.retry_base_ms,
        max_context_chars: config.max_context_chars,
        parent_session: None,
    })
}

fn add_delegation_tools(
    tools: &mut dsh_core::ToolRegistry,
    store: &Arc<dyn SessionStore>,
    provider: &Arc<dyn dsh_core::LlmProvider>,
    config: &AgentConfig,
    clock: &Arc<dyn Clock>,
) -> anyhow::Result<()> {
    let base = tools.clone();
    tools.insert(Arc::new(dsh_agent::SubagentTool::new(
        Arc::clone(store),
        Arc::clone(provider),
        base.clone(),
        config.clone(),
        Arc::clone(clock),
    )))?;
    tools.insert(Arc::new(dsh_agent::WorkflowTool::new(
        Arc::clone(store),
        Arc::clone(provider),
        base.clone(),
        config.clone(),
        Arc::clone(clock),
    )))?;
    tools.insert(Arc::new(dsh_agent::RalphTool::new(
        Arc::clone(store),
        Arc::clone(provider),
        base,
        config.clone(),
        Arc::clone(clock),
    )))?;
    Ok(())
}

fn build_runtime(config: &Config, env: &dyn Env) -> anyhow::Result<Runtime> {
    let plugins: Vec<Box<dyn Plugin>> = vec![
        Box::new(SqliteStorePlugin),
        Box::new(DeepSeekPlugin),
        Box::new(ToolsPlugin),
        Box::new(dsh_jobs::JobsPlugin),
        Box::new(dsh_schedule::SchedulePlugin),
        Box::new(dsh_terminal::TerminalPlugin),
        Box::new(dsh_mcp::McpPlugin::from_env()),
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
        RunEvent::Retry { attempt, code } => {
            eprintln!("dsh: retry {attempt} after {code}");
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
        SessionsCommand::Fork {
            source,
            target,
            through,
        } => {
            let source = SessionId::parse(source)?;
            let target = SessionId::parse(target)?;
            let through = match through {
                Some(seq) => EventSeq::new(seq),
                None => store
                    .load(&source)?
                    .last()
                    .map_or(EventSeq::FIRST, |record| record.seq),
            };
            let cwd = std::env::current_dir()
                .ok()
                .map(|path| path.display().to_string());
            let header =
                SessionHeader::new(target.clone(), chrono::Utc::now().timestamp_millis(), cwd);
            store.fork(&source, &header, through)?;
            println!("{target}");
        }
        SessionsCommand::Export { session_id, output } => {
            let id = SessionId::parse(session_id)?;
            let exported = store.export(&id)?;
            if let Some(path) = output {
                std::fs::write(&path, exported)?;
                println!("{}", path.display());
            } else {
                print!("{exported}");
            }
        }
        SessionsCommand::Search { query, limit } => {
            for hit in store.search(&query, limit)? {
                println!(
                    "{}\t{}\t{}\t{}",
                    hit.session_id, hit.seq, hit.kind, hit.snippet
                );
            }
        }
        SessionsCommand::Repair { session_id } => {
            let id = SessionId::parse(session_id)?;
            let repaired = store.repair(&id, chrono::Utc::now().timestamp_millis())?;
            println!(
                "{}",
                if repaired {
                    "repaired"
                } else {
                    "nothing to repair"
                }
            );
        }
    }
    Ok(ExitCode::SUCCESS)
}

async fn schedules(args: SchedulesArgs) -> anyhow::Result<ExitCode> {
    let config = config_from_env()?;
    let file = dsh_schedule::ScheduleFile::new(config.home.join("schedules.json"));
    match args.command {
        SchedulesCommand::List => {
            let schedules = file.list()?;
            if schedules.is_empty() {
                println!("no schedules");
                return Ok(ExitCode::SUCCESS);
            }
            for schedule in schedules {
                println!(
                    "{}\t{}\t{}\t{}",
                    schedule.id, schedule.next_run_at, schedule.enabled, schedule.task
                );
            }
        }
        SchedulesCommand::RunDue => {
            let now = chrono::Utc::now().timestamp_millis();
            let due = file.due(now)?;
            if due.is_empty() {
                println!("no due schedules");
                return Ok(ExitCode::SUCCESS);
            }
            let env = SystemEnv;
            let runtime = build_runtime(&config, &env)?;
            let cwd = resolve_cwd(None)?;
            let agent_config = agent_config(&config, cwd)?;
            let mut tools = runtime.tools;
            let clock: Arc<dyn Clock> = Arc::new(SystemClock);
            add_delegation_tools(
                &mut tools,
                &runtime.store,
                &runtime.provider,
                &agent_config,
                &clock,
            )?;
            let runs = dsh_schedule::run_due(
                &file,
                runtime.store,
                runtime.provider,
                tools,
                agent_config,
                clock,
            )
            .await?;
            for run in runs {
                print!("{}", schedule_run_text(&run));
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn tools() -> anyhow::Result<ExitCode> {
    let mut registry = dsh_tools::default_registry()?;
    for tool in dsh_jobs::default_jobs() {
        registry.insert(tool)?;
    }
    for tool in dsh_schedule::default_schedule_tools() {
        registry.insert(tool)?;
    }
    for tool in dsh_terminal::default_terminal_tools() {
        registry.insert(tool)?;
    }
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
        "max_retries": config.max_retries,
        "retry_base_ms": config.retry_base_ms,
        "max_context_chars": config.max_context_chars,
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
        SessionEvent::TodoWritten { items } => format!("{} todos", items.len()),
        SessionEvent::PlanSet { steps, approved } => {
            format!("{} steps, approved: {approved}", steps.len())
        }
        SessionEvent::GoalSet { objective, phase } => format!("{phase:?}: {objective}"),
        SessionEvent::WorkingDirectoryChanged { cwd } => cwd.clone(),
        SessionEvent::Compaction { summary, .. } => truncate(summary, 120),
        SessionEvent::RetryAttempt { attempt, code, .. } => format!("attempt {attempt}: {code}"),
    }
}

fn schedule_run_text(run: &dsh_schedule::ScheduleRun) -> String {
    let mut output = format!(
        "{}\t{}\t{}\n",
        run.schedule_id,
        run.session_id,
        describe_outcome(&run.outcome.outcome)
    );
    if !run.outcome.text.is_empty() {
        output.push_str(&run.outcome.text);
        output.push('\n');
    }
    output
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
    use dsh_agent::RunOutcome;
    use dsh_core::{
        AssistantMessage, ContentBlock, EventRecord, MessageId, SessionSummary, StepId, StoreError,
        TokenUsage, ToolCallId, ToolMessage, TurnId, UserMessage,
    };

    struct NoopStore;

    impl SessionStore for NoopStore {
        fn create(&self, _header: &SessionHeader) -> Result<(), StoreError> {
            Ok(())
        }
        fn append(
            &self,
            _session_id: &SessionId,
            at: i64,
            event: SessionEvent,
        ) -> Result<EventRecord, StoreError> {
            Ok(EventRecord {
                seq: EventSeq::FIRST,
                at,
                event,
            })
        }
        fn load(&self, _session_id: &SessionId) -> Result<Vec<EventRecord>, StoreError> {
            Ok(Vec::new())
        }
        fn list(&self) -> Result<Vec<SessionSummary>, StoreError> {
            Ok(Vec::new())
        }
        fn has(&self, _session_id: &SessionId) -> Result<bool, StoreError> {
            Ok(false)
        }
        fn delete(&self, _session_id: &SessionId) -> Result<bool, StoreError> {
            Ok(false)
        }
    }

    struct NoopProvider;

    #[async_trait::async_trait]
    impl dsh_core::LlmProvider for NoopProvider {
        fn name(&self) -> &'static str {
            "noop"
        }
        async fn stream(
            &self,
            _request: dsh_core::LlmRequest,
            _cancel: CancellationToken,
        ) -> Result<dsh_core::LlmStream, dsh_core::LlmError> {
            Ok(Box::pin(futures_util::stream::empty()))
        }
    }

    #[test]
    fn delegation_tools_are_installed_for_every_run_surface() {
        let mut tools = dsh_tools::default_registry().expect("registry");
        let store: Arc<dyn SessionStore> = Arc::new(NoopStore);
        let provider: Arc<dyn dsh_core::LlmProvider> = Arc::new(NoopProvider);
        let config = AgentConfig::new(
            ModelId::parse("test-model").expect("model"),
            PathBuf::from("/work"),
        );
        let clock: Arc<dyn Clock> = Arc::new(SystemClock);
        add_delegation_tools(&mut tools, &store, &provider, &config, &clock).expect("delegation");
        let names = tools.names();
        assert!(names.iter().any(|name| name == "subagent"));
        assert!(names.iter().any(|name| name == "workflow"));
        assert!(names.iter().any(|name| name == "ralph"));
    }

    #[test]
    fn schedule_run_text_includes_non_empty_answers() {
        let session_id = SessionId::generate();
        let run = dsh_schedule::ScheduleRun {
            schedule_id: "schedule-1".to_string(),
            session_id: session_id.clone(),
            outcome: RunOutcome {
                session_id: session_id.clone(),
                outcome: TurnOutcome::Completed,
                text: "done".to_string(),
                usage: TokenUsage::default(),
                steps: 1,
            },
        };
        let text = schedule_run_text(&run);
        assert!(text.contains("done"));
        assert_eq!(text.lines().count(), 2);

        let empty = dsh_schedule::ScheduleRun {
            schedule_id: "schedule-2".to_string(),
            session_id,
            outcome: RunOutcome {
                session_id: SessionId::generate(),
                outcome: TurnOutcome::Completed,
                text: String::new(),
                usage: TokenUsage::default(),
                steps: 1,
            },
        };
        assert_eq!(schedule_run_text(&empty).lines().count(), 1);
    }

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
