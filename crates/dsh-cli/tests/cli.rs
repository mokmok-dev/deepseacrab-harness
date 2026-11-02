#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use assert_cmd::Command;
use predicates::prelude::*;
use std::io::{ErrorKind, Read, Write};
use std::net::TcpListener;
use std::time::{Duration, Instant};

fn dsh() -> Command {
    let mut command = Command::cargo_bin("dsh").expect("built dsh binary");
    command.write_stdin("");
    command
}

fn fresh_home() -> tempfile::TempDir {
    tempfile::tempdir().expect("temporary home")
}

const SSE_BODY: &str = r#"event: message_start
data: {"type":"message_start","message":{"usage":{"input_tokens":3,"output_tokens":0}}}

event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hello"}}

event: content_block_stop
data: {"type":"content_block_stop","index":0}

event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":1}}

event: message_stop
data: {"type":"message_stop"}

"#;

fn spawn_sse_server() -> (u16, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let port = listener.local_addr().expect("local address").port();
    let handle = std::thread::spawn(move || {
        listener
            .set_nonblocking(true)
            .expect("nonblocking listener");
        let deadline = Instant::now() + Duration::from_secs(10);
        let accepted = loop {
            match listener.accept() {
                Ok(pair) => break Some(pair),
                Err(error) if error.kind() == ErrorKind::WouldBlock => {
                    if Instant::now() >= deadline {
                        break None;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(_) => break None,
            }
        };
        let Some((mut stream, _)) = accepted else {
            return;
        };
        stream.set_nonblocking(false).expect("blocking connection");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("read timeout");
        let mut request = Vec::new();
        let mut buffer = [0u8; 1024];
        loop {
            match stream.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(read) => {
                    request.extend_from_slice(&buffer[..read]);
                    if let Some(index) = request.windows(4).position(|window| window == b"\r\n\r\n")
                    {
                        let header_end = index + 4;
                        let headers = String::from_utf8_lossy(&request[..index]);
                        let content_length = headers
                            .lines()
                            .find_map(|line| {
                                line.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .and_then(|value| value.trim().parse::<usize>().ok())
                            })
                            .unwrap_or(0);
                        if request.len() >= header_end + content_length {
                            break;
                        }
                    }
                }
            }
        }
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            SSE_BODY.len(),
            SSE_BODY
        );
        stream
            .write_all(response.as_bytes())
            .expect("write response");
        stream.flush().expect("flush response");
    });
    (port, handle)
}

#[test]
fn help_lists_the_run_command() {
    dsh()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("run"))
        .stdout(predicate::str::contains("sessions"));
}

#[test]
fn config_reports_defaults_without_a_key() {
    let home = fresh_home();
    dsh()
        .env("DSH_HOME", home.path())
        .env_remove("DEEPSEEK_API_KEY")
        .arg("config")
        .assert()
        .success()
        .stdout(predicate::str::contains("\"model\""))
        .stdout(predicate::str::contains("deepseek-flash"))
        .stdout(predicate::str::contains("\"api_key_present\": false"));
}

#[test]
fn run_without_a_key_fails_loudly() {
    let home = fresh_home();
    dsh()
        .env("DSH_HOME", home.path())
        .env_remove("DEEPSEEK_API_KEY")
        .env_remove("DEEPSEEK_BASE_URL")
        .args(["run", "hello"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("MISSING_CREDENTIAL"));
}

#[test]
fn web_and_sdk_without_a_key_fail_loudly() {
    for command in [vec!["web"], vec!["sdk"]] {
        let home = fresh_home();
        dsh()
            .env("DSH_HOME", home.path())
            .env_remove("DEEPSEEK_API_KEY")
            .env_remove("DEEPSEEK_BASE_URL")
            .args(command)
            .assert()
            .failure()
            .stderr(predicate::str::contains("MISSING_CREDENTIAL"));
    }
}

#[test]
fn empty_tasks_are_rejected_before_model_work() {
    let home = fresh_home();
    dsh()
        .env("DSH_HOME", home.path())
        .env_remove("DEEPSEEK_API_KEY")
        .args(["run", "   "])
        .assert()
        .failure()
        .stderr(predicate::str::contains("task must not be empty"));
}

#[test]
fn zero_max_steps_is_rejected() {
    let home = fresh_home();
    dsh()
        .env("DSH_HOME", home.path())
        .env_remove("DEEPSEEK_API_KEY")
        .args(["run", "--max-steps", "0", "hello"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("greater than zero"));
}

#[test]
fn config_uses_environment_overrides() {
    let home = fresh_home();
    dsh()
        .env("DSH_HOME", home.path())
        .env("DEEPSEEK_MODEL", "deepseek-v4-pro")
        .env("DSH_PERMISSION_MODE", "read-only")
        .arg("config")
        .assert()
        .success()
        .stdout(predicate::str::contains("deepseek-v4-pro"))
        .stdout(predicate::str::contains("\"permission\": \"read-only\""));
}

#[test]
fn sessions_list_is_empty_for_a_fresh_home() {
    let home = fresh_home();
    dsh()
        .env("DSH_HOME", home.path())
        .args(["sessions", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("no sessions"));
}

#[test]
fn tools_lists_the_builtin_registry() {
    dsh()
        .arg("tools")
        .assert()
        .success()
        .stdout(predicate::str::contains("bash"))
        .stdout(predicate::str::contains("read_file"))
        .stdout(predicate::str::contains("write_file"));
}

#[test]
fn schedules_are_empty_and_do_not_need_a_key_when_nothing_is_due() {
    let home = fresh_home();
    dsh()
        .env("DSH_HOME", home.path())
        .args(["schedules", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("no schedules"));

    dsh()
        .env("DSH_HOME", home.path())
        .env_remove("DEEPSEEK_API_KEY")
        .args(["schedules", "run-due"])
        .assert()
        .success()
        .stdout(predicate::str::contains("no due schedules"));
}

#[test]
fn run_streams_a_local_sse_response() {
    let (port, server) = spawn_sse_server();
    let home = fresh_home();
    let cwd = tempfile::tempdir().expect("temporary cwd");
    let output = dsh()
        .env("DSH_HOME", home.path())
        .env("DEEPSEEK_API_KEY", "test-key")
        .env("DEEPSEEK_BASE_URL", format!("http://127.0.0.1:{port}"))
        .env("DSH_MAX_STEPS", "1")
        .args([
            "run",
            "--cwd",
            cwd.path().to_str().expect("utf-8 cwd"),
            "say hello",
        ])
        .output()
        .expect("run the harness");
    server.join().expect("sse server");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "hello");
}

#[test]
fn run_persists_the_session_for_later_inspection() {
    let (port, server) = spawn_sse_server();
    let home = fresh_home();
    let cwd = tempfile::tempdir().expect("temporary cwd");
    dsh()
        .env("DSH_HOME", home.path())
        .env("DEEPSEEK_API_KEY", "test-key")
        .env("DEEPSEEK_BASE_URL", format!("http://127.0.0.1:{port}"))
        .env("DSH_MAX_STEPS", "1")
        .args([
            "run",
            "--session-id",
            "session-integration",
            "--cwd",
            cwd.path().to_str().expect("utf-8 cwd"),
            "say hello",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("hello"));
    server.join().expect("sse server");

    dsh()
        .env("DSH_HOME", home.path())
        .args(["sessions", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("session-integration"));

    dsh()
        .env("DSH_HOME", home.path())
        .args(["sessions", "show", "session-integration"])
        .assert()
        .success()
        .stdout(predicate::str::contains("user_message"))
        .stdout(predicate::str::contains("assistant_message"))
        .stdout(predicate::str::contains("turn_ended"));
}

#[test]
fn session_admin_commands_round_trip() {
    let (port, server) = spawn_sse_server();
    let home = fresh_home();
    let cwd = tempfile::tempdir().expect("temporary cwd");
    dsh()
        .env("DSH_HOME", home.path())
        .env("DEEPSEEK_API_KEY", "test-key")
        .env("DEEPSEEK_BASE_URL", format!("http://127.0.0.1:{port}"))
        .env("DSH_MAX_STEPS", "1")
        .args([
            "run",
            "--session-id",
            "session-admin",
            "--cwd",
            cwd.path().to_str().expect("utf-8 cwd"),
            "say hello",
        ])
        .assert()
        .success();
    server.join().expect("sse server");

    dsh()
        .env("DSH_HOME", home.path())
        .args(["sessions", "export", "session-admin"])
        .assert()
        .success()
        .stdout(predicate::str::contains("user_message"))
        .stdout(predicate::str::contains("assistant_message"));

    dsh()
        .env("DSH_HOME", home.path())
        .args(["sessions", "search", "say hello"])
        .assert()
        .success()
        .stdout(predicate::str::contains("session-admin"));

    dsh()
        .env("DSH_HOME", home.path())
        .args(["sessions", "fork", "session-admin", "session-admin-copy"])
        .assert()
        .success()
        .stdout(predicate::str::contains("session-admin-copy"));

    dsh()
        .env("DSH_HOME", home.path())
        .args(["sessions", "show", "session-admin-copy"])
        .assert()
        .success()
        .stdout(predicate::str::contains("user_message"));

    dsh()
        .env("DSH_HOME", home.path())
        .args(["sessions", "repair", "session-admin"])
        .assert()
        .success()
        .stdout(predicate::str::contains("nothing to repair"));
}
