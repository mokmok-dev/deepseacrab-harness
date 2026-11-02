#![cfg_attr(test, allow(clippy::expect_used, clippy::unwrap_used, clippy::panic))]
#![allow(clippy::missing_errors_doc, clippy::missing_panics_doc)]

//! Line-delimited JSON-RPC 2.0 access to the harness.
//!
//! One JSON object per line carries a request or a response. A `run` request
//! also pushes one `event` notification per live [`RunEvent`] before its own
//! response, so a client can render progress without a second channel.

pub mod protocol;

pub use protocol::{
    DecodeError, INTERNAL_ERROR, INVALID_PARAMS, METHOD_NOT_FOUND, PARSE_ERROR, Request, RequestId,
};

use dsh_agent::{AgentConfig, AgentLoop, EventSink};
use dsh_core::{Clock, LlmProvider, SessionId, SessionStore, StoreError, ToolRegistry};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::sync::Arc;
use thiserror::Error;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{Mutex, mpsc};
use tokio_util::sync::CancellationToken;

/// Everything one SDK connection may use.
#[derive(Clone)]
pub struct SdkState {
    /// Durable session storage.
    pub store: Arc<dyn SessionStore>,
    /// Model route behind every run.
    pub provider: Arc<dyn LlmProvider>,
    /// Tools offered to the model.
    pub tools: ToolRegistry,
    /// Per-run model, policy, and workspace settings.
    pub config: AgentConfig,
    /// Wall clock stamped on durable writes.
    pub clock: Arc<dyn Clock>,
}

/// Failures that stop the transport itself.
#[derive(Debug, Error)]
pub enum SdkError {
    /// Reading a request or writing a frame failed.
    #[error("sdk io failure: {0}")]
    Io(#[from] std::io::Error),
}

/// A dispatch failure that becomes a JSON-RPC error response.
struct RpcError {
    code: i32,
    message: String,
}

impl RpcError {
    fn invalid_params(message: impl Into<String>) -> Self {
        Self {
            code: INVALID_PARAMS,
            message: message.into(),
        }
    }

    fn internal(message: impl Into<String>) -> Self {
        Self {
            code: INTERNAL_ERROR,
            message: message.into(),
        }
    }

    fn unknown_method(method: &str) -> Self {
        Self {
            code: METHOD_NOT_FOUND,
            message: format!("no method named {method:?}"),
        }
    }
}

/// Serve requests read from `reader` until EOF, answering on `writer`.
///
/// Requests are handled one at a time so a response never overtakes a
/// notification from the run it asked for. The writer lock is what keeps two
/// frames from interleaving on one line.
///
/// # Errors
///
/// Returns [`SdkError::Io`] when the transport fails; request-level failures
/// become JSON-RPC error responses instead.
// The public signature owns the state for one connection and leaves the
// reader's `Send`-ness to the caller; neither is a lint fix but the transport
// contract itself.
#[allow(clippy::needless_pass_by_value, clippy::future_not_send)]
pub async fn serve_io<R, W>(
    state: SdkState,
    mut reader: R,
    writer: W,
    cancel: CancellationToken,
) -> Result<(), SdkError>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin + Send,
{
    let writer = Mutex::new(writer);
    let mut line = String::new();
    loop {
        line.clear();
        let read = tokio::select! {
            biased;
            () = cancel.cancelled() => break,
            count = reader.read_line(&mut line) => count?,
        };
        if read == 0 {
            break;
        }
        let frame = match protocol::decode(line.trim()) {
            Ok(request) => match dispatch(&state, &writer, &request, &cancel).await {
                Ok(result) => protocol::result_frame(request.id.as_ref(), &result),
                Err(error) => {
                    protocol::error_frame(request.id.as_ref(), error.code, &error.message)
                }
            },
            Err(DecodeError::Syntax(message)) => protocol::error_frame(None, PARSE_ERROR, &message),
            Err(DecodeError::Shape { id, message }) => {
                protocol::error_frame(id.as_ref(), INVALID_PARAMS, &message)
            }
        };
        write_frame(&writer, &frame).await?;
    }
    Ok(())
}

async fn write_frame<W>(writer: &Mutex<W>, frame: &Value) -> Result<(), SdkError>
where
    W: AsyncWrite + Unpin,
{
    let mut line = frame.to_string();
    line.push('\n');
    let mut guard = writer.lock().await;
    guard.write_all(line.as_bytes()).await?;
    // A piped stdout is block-buffered, so a frame stays invisible until the
    // flush lands. Each line is flushed so a reader sees it as soon as it exists.
    guard.flush().await?;
    drop(guard);
    Ok(())
}

async fn dispatch<W>(
    state: &SdkState,
    writer: &Mutex<W>,
    request: &Request,
    cancel: &CancellationToken,
) -> Result<Value, RpcError>
where
    W: AsyncWrite + Unpin,
{
    match request.method.as_str() {
        "run" => run(state, writer, &request.params, cancel).await,
        "sessions.list" => list_sessions(state),
        "sessions.show" => show_session(state, &request.params),
        "tools.list" => encode(&state.tools.schemas()),
        "config" => Ok(configuration(&state.config)),
        other => Err(RpcError::unknown_method(other)),
    }
}

/// The wire body of a `run` request.
#[derive(serde::Deserialize)]
struct RunParams {
    task: String,
    #[serde(default)]
    session_id: Option<String>,
}

/// The wire body of a `sessions.show` request.
#[derive(serde::Deserialize)]
struct ShowParams {
    session_id: String,
}

fn list_sessions(state: &SdkState) -> Result<Value, RpcError> {
    let sessions = state
        .store
        .list()
        .map_err(|error| RpcError::internal(error.to_string()))?;
    encode(&sessions)
}

fn show_session(state: &SdkState, params: &Value) -> Result<Value, RpcError> {
    let params: ShowParams = parse_params(params, "sessions.show")?;
    let session_id = SessionId::parse(params.session_id)
        .map_err(|error| RpcError::invalid_params(format!("invalid session id: {error}")))?;
    let records = match state.store.load(&session_id) {
        Ok(records) => records,
        Err(StoreError::NotFound(id)) => {
            return Err(RpcError::invalid_params(format!("no session {id}")));
        }
        Err(error) => return Err(RpcError::internal(error.to_string())),
    };
    encode(&records)
}

fn configuration(config: &AgentConfig) -> Value {
    json!({
        "model": config.model,
        "max_steps": config.max_steps,
        "max_tokens": config.max_tokens,
        "permission": config.permission,
    })
}

async fn run<W>(
    state: &SdkState,
    writer: &Mutex<W>,
    params: &Value,
    cancel: &CancellationToken,
) -> Result<Value, RpcError>
where
    W: AsyncWrite + Unpin,
{
    let params: RunParams = parse_params(params, "run")?;
    let session_id = params.session_id.map_or_else(
        || Ok(SessionId::generate()),
        |raw| {
            SessionId::parse(raw)
                .map_err(|error| RpcError::invalid_params(format!("invalid session id: {error}")))
        },
    )?;
    let agent = AgentLoop::new(
        Arc::clone(&state.store),
        Arc::clone(&state.provider),
        state.tools.clone(),
        state.config.clone(),
        Arc::clone(&state.clock),
    )
    .map_err(|error| RpcError::internal(error.to_string()))?;

    let (sender, mut receiver) = mpsc::unbounded_channel();
    let sink: EventSink = Arc::new(move |event| {
        let _ = sender.send(protocol::event_frame(&event));
    });
    // The driver's sink is synchronous, so frames are queued here and the pump
    // writes them while the turn is still running.
    let pump = async {
        while let Some(frame) = receiver.recv().await {
            write_frame(writer, &frame).await?;
        }
        Ok::<(), SdkError>(())
    };
    let (outcome, pumped) = tokio::join!(
        agent.run(&session_id, &params.task, cancel.clone(), Some(sink)),
        pump
    );
    pumped.map_err(|error| RpcError::internal(error.to_string()))?;
    let outcome = outcome.map_err(|error| RpcError::internal(error.to_string()))?;
    Ok(json!({
        "session_id": outcome.session_id,
        "outcome": outcome.outcome,
        "text": outcome.text,
    }))
}

fn parse_params<T: DeserializeOwned>(params: &Value, method: &str) -> Result<T, RpcError> {
    serde_json::from_value(params.clone())
        .map_err(|error| RpcError::invalid_params(format!("invalid {method} params: {error}")))
}

fn encode<T: Serialize>(value: &T) -> Result<Value, RpcError> {
    serde_json::to_value(value)
        .map_err(|error| RpcError::internal(format!("cannot encode the result: {error}")))
}

#[cfg(test)]
mod tests;
