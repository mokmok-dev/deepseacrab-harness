//! Live PTY sessions and the buffer each one exposes to the tools.

use crate::id::valid_id;
use crate::output::{MAX_OUTPUT_BYTES, Output, lock as lock_output};
use portable_pty::{Child, CommandBuilder, PtySize, native_pty_system};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use thiserror::Error;
use uuid::Uuid;

/// A manager shared by every terminal tool in one tool set.
pub type SharedTerminals = Arc<Mutex<TerminalManager>>;

/// Lock the shared manager, recovering a guard from a poisoned mutex.
///
/// Tool panics are contained by [`dsh_core::Tool::run`], which can poison the
/// mutex while a terminal map is still structurally intact.
pub fn lock_manager(terminals: &SharedTerminals) -> MutexGuard<'_, TerminalManager> {
    terminals.lock().unwrap_or_else(PoisonError::into_inner)
}

/// One live pseudo-terminal.
struct Terminal {
    child: Box<dyn Child + Send + Sync>,
    writer: Box<dyn Write + Send>,
    output: Arc<Mutex<Output>>,
}

/// Identity of a freshly opened terminal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Opened {
    /// Registry key used by the other three tools.
    pub id: String,
    /// Process id of the command running in the terminal.
    pub pid: u32,
}

/// New output returned by one read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadChunk {
    /// Nothing arrived since the previous read.
    Empty,
    /// Text accumulated since the previous read.
    Data {
        /// UTF-8 replacement of the raw bytes.
        text: String,
        /// Number of raw bytes the text was decoded from.
        bytes: usize,
    },
}

/// Failures raised while driving a terminal.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum TerminalError {
    /// The id is not a token.
    #[error("invalid terminal id: {0}")]
    InvalidId(String),
    /// No terminal is registered under the id.
    #[error("unknown terminal: {0}")]
    Unknown(String),
    /// The command string named no program.
    #[error("invalid command: {0:?}")]
    InvalidCommand(String),
    /// The pseudo-terminal could not be created.
    #[error("cannot open pty: {0}")]
    Open(String),
    /// The command could not be started.
    #[error("cannot start command: {0}")]
    Spawn(String),
    /// The PTY writer rejected the input.
    #[error("cannot write to terminal {id}: {message}")]
    Write {
        /// Terminal whose writer failed.
        id: String,
        /// Underlying I/O error.
        message: String,
    },
}

/// Registry of live terminals keyed by the id shown to the model.
#[derive(Default)]
pub struct TerminalManager {
    terminals: BTreeMap<String, Terminal>,
}

impl TerminalManager {
    /// An empty registry.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            terminals: BTreeMap::new(),
        }
    }

    /// Spawn `command` in a new PTY rooted at `cwd`.
    ///
    /// `command` is split on whitespace into a program and its arguments; an
    /// empty command is refused instead of falling back to a shell.
    pub fn open(&mut self, command: &str, cwd: &Path) -> Result<Opened, TerminalError> {
        let mut parts = command.split_whitespace();
        let Some(program) = parts.next() else {
            return Err(TerminalError::InvalidCommand(command.to_string()));
        };
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|error| TerminalError::Open(error.to_string()))?;
        let mut builder = CommandBuilder::new(program);
        for argument in parts {
            builder.arg(argument);
        }
        builder.cwd(cwd);
        let child = pair
            .slave
            .spawn_command(builder)
            .map_err(|error| TerminalError::Spawn(error.to_string()))?;
        drop(pair.slave);
        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|error| TerminalError::Open(error.to_string()))?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|error| TerminalError::Open(error.to_string()))?;
        let pid = child.process_id().unwrap_or_default();
        let output = Arc::new(Mutex::new(Output::new()));
        let buffer = Arc::clone(&output);
        drop(thread::spawn(move || pump(reader, &buffer)));
        let id = Uuid::new_v4().simple().to_string();
        self.terminals.insert(
            id.clone(),
            Terminal {
                child,
                writer,
                output,
            },
        );
        Ok(Opened { id, pid })
    }

    /// Output accumulated since the previous read of `id`.
    pub fn read(&mut self, id: &str) -> Result<ReadChunk, TerminalError> {
        validate(id)?;
        let Some(terminal) = self.terminals.get(id) else {
            return Err(TerminalError::Unknown(id.to_string()));
        };
        let bytes = lock_output(&terminal.output).take();
        if bytes.is_empty() {
            Ok(ReadChunk::Empty)
        } else {
            Ok(ReadChunk::Data {
                text: String::from_utf8_lossy(&bytes).into_owned(),
                bytes: bytes.len(),
            })
        }
    }

    /// Write `input` to `id` and flush the PTY.
    pub fn write(&mut self, id: &str, input: &str) -> Result<usize, TerminalError> {
        validate(id)?;
        let Some(terminal) = self.terminals.get_mut(id) else {
            return Err(TerminalError::Unknown(id.to_string()));
        };
        write_input(terminal.writer.as_mut(), id, input)
    }

    /// Remove `id`, drop its writer, and kill its child.
    pub fn close(&mut self, id: &str) -> Result<(), TerminalError> {
        validate(id)?;
        let Some(mut terminal) = self.terminals.remove(id) else {
            return Err(TerminalError::Unknown(id.to_string()));
        };
        drop(terminal.writer);
        // The child is already unreachable through the registry, and dropping
        // the writer sent EOF, so a kill racing with exit needs no report.
        let _ = terminal.child.kill();
        Ok(())
    }

    /// Retained byte count, used by tests to observe the cap without reading.
    #[cfg(test)]
    pub(crate) fn output_len(&self, id: &str) -> Option<usize> {
        self.terminals
            .get(id)
            .map(|terminal| lock_output(&terminal.output).buffered())
    }
}

fn validate(id: &str) -> Result<(), TerminalError> {
    if valid_id(id) {
        Ok(())
    } else {
        Err(TerminalError::InvalidId(id.to_string()))
    }
}

fn write_input(writer: &mut dyn Write, id: &str, input: &str) -> Result<usize, TerminalError> {
    writer
        .write_all(input.as_bytes())
        .and_then(|()| writer.flush())
        .map_err(|error| TerminalError::Write {
            id: id.to_string(),
            message: error.to_string(),
        })?;
    Ok(input.len())
}

fn pump(mut reader: Box<dyn Read + Send>, output: &Mutex<Output>) {
    let mut buffer = [0_u8; 4096];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(read) => lock_output(output).append(&buffer[..read], MAX_OUTPUT_BYTES),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FailingWrite;

    impl Write for FailingWrite {
        fn write(&mut self, _buffer: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("write failed"))
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    struct FailingFlush;

    impl Write for FailingFlush {
        fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
            Ok(buffer.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Err(std::io::Error::other("flush failed"))
        }
    }

    struct FailingRead;

    impl Read for FailingRead {
        fn read(&mut self, _buffer: &mut [u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("read failed"))
        }
    }

    struct SingleByteRead {
        bytes: Vec<u8>,
        offset: usize,
    }

    impl Read for SingleByteRead {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            if self.offset == self.bytes.len() || buffer.is_empty() {
                return Ok(0);
            }
            buffer[0] = self.bytes[self.offset];
            self.offset += 1;
            Ok(1)
        }
    }

    #[test]
    fn complete_writes_report_the_byte_count() {
        let mut writer = Vec::new();
        assert_eq!(write_input(&mut writer, "t1", "hello").expect("write"), 5);
        assert_eq!(writer, b"hello");
    }

    #[test]
    fn write_failures_name_the_terminal() {
        let error = write_input(&mut FailingWrite, "t1", "hello").expect_err("write");
        assert_eq!(
            error.to_string(),
            "cannot write to terminal t1: write failed"
        );
        assert_eq!(
            error,
            TerminalError::Write {
                id: "t1".to_string(),
                message: "write failed".to_string(),
            }
        );
    }

    #[test]
    fn flush_failures_are_write_errors() {
        let error = write_input(&mut FailingFlush, "t2", "hello").expect_err("flush");
        assert_eq!(
            error.to_string(),
            "cannot write to terminal t2: flush failed"
        );
    }

    #[test]
    fn pump_appends_every_byte_until_eof() {
        let output = Mutex::new(Output::new());
        pump(
            Box::new(SingleByteRead {
                bytes: b"hello".to_vec(),
                offset: 0,
            }),
            &output,
        );
        assert_eq!(lock_output(&output).bytes(), b"hello");
    }

    #[test]
    fn pump_stops_without_output_when_the_reader_fails() {
        let output = Mutex::new(Output::new());
        pump(Box::new(FailingRead), &output);
        assert_eq!(lock_output(&output).buffered(), 0);
    }

    #[test]
    fn empty_commands_are_refused() {
        let mut manager = TerminalManager::new();
        for command in ["", "   ", "\t"] {
            assert_eq!(
                manager.open(command, Path::new("/")),
                Err(TerminalError::InvalidCommand(command.to_string()))
            );
        }
        assert!(manager.terminals.is_empty());
    }

    #[test]
    fn a_poisoned_manager_lock_is_recovered() {
        let terminals = Arc::new(Mutex::new(TerminalManager::new()));
        let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = terminals.lock().expect("lock");
            panic!("poison the mutex");
        }));
        assert!(poisoned.is_err());
        lock_manager(&terminals)
            .open("", Path::new("/"))
            .expect_err("empty");
    }

    #[test]
    fn error_messages_name_their_cause() {
        assert_eq!(
            TerminalError::InvalidId("a b".to_string()).to_string(),
            "invalid terminal id: a b"
        );
        assert_eq!(
            TerminalError::Unknown("t1".to_string()).to_string(),
            "unknown terminal: t1"
        );
        assert_eq!(
            TerminalError::InvalidCommand(" ".to_string()).to_string(),
            "invalid command: \" \""
        );
        assert_eq!(
            TerminalError::Open("no fds".to_string()).to_string(),
            "cannot open pty: no fds"
        );
        assert_eq!(
            TerminalError::Spawn("missing".to_string()).to_string(),
            "cannot start command: missing"
        );
    }
}
