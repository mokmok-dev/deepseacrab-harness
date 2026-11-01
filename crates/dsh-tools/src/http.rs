//! The `web_fetch` tool.

use crate::input::parse;
use async_trait::async_trait;
use dsh_core::{PermissionMode, Tool, ToolContext, ToolOutcome, ToolSchema};
use serde::Deserialize;
use serde_json::{Value, json};
use std::time::Duration;

const REQUEST_TIMEOUT_SECS: u64 = 20;
const MAX_BODY_BYTES: usize = 1024 * 1024;
const MAX_REDIRECTS: usize = 10;

pub struct WebFetchTool;

#[derive(Deserialize)]
struct WebFetchArguments {
    url: String,
}

#[async_trait]
impl Tool for WebFetchTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "web_fetch".to_string(),
            description: "Fetch a URL with GET, following redirects. HTML is reduced to text. \
                          Returns an HTTP status line, the final URL, and at most 1 MiB of body."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "url": {
                        "type": "string",
                        "description": "Absolute HTTP or HTTPS URL to fetch."
                    }
                },
                "required": ["url"]
            }),
        }
    }

    async fn execute(&self, context: &ToolContext, arguments: Value) -> ToolOutcome {
        let arguments: WebFetchArguments = match parse(arguments) {
            Ok(arguments) => arguments,
            Err(outcome) => return outcome,
        };
        if context.permission == PermissionMode::ReadOnly {
            return ToolOutcome::error(format!(
                "permission mode {} refuses web_fetch",
                context.permission.as_str()
            ));
        }
        fetch(&arguments.url).await
    }
}

async fn fetch(url: &str) -> ToolOutcome {
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(REQUEST_TIMEOUT_SECS))
        .redirect(reqwest::redirect::Policy::limited(MAX_REDIRECTS))
        .build()
    {
        Ok(client) => client,
        Err(error) => return ToolOutcome::error(format!("cannot build HTTP client: {error}")),
    };

    let response = match client.get(url).send().await {
        Ok(response) => response,
        Err(error) => return ToolOutcome::error(format!("request to {url} failed: {error}")),
    };
    let status = response.status().as_u16();
    let final_url = response.url().to_string();
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let body = match read_body(response).await {
        Ok(body) => body,
        Err(error) => return ToolOutcome::error(error),
    };

    let text = body_to_text(content_type.as_deref(), &body);
    ToolOutcome::text(format_response(status, &final_url, &text))
}

async fn read_body(mut response: reqwest::Response) -> Result<Vec<u8>, String> {
    let mut body = Vec::new();
    loop {
        let chunk = response
            .chunk()
            .await
            .map_err(|error| format!("failed to read response body: {error}"))?;
        let Some(chunk) = chunk else {
            break;
        };
        if append_capped(&mut body, &chunk, MAX_BODY_BYTES) {
            break;
        }
    }
    Ok(body)
}

fn append_capped(buffer: &mut Vec<u8>, chunk: &[u8], cap: usize) -> bool {
    let remaining = cap.saturating_sub(buffer.len());
    if chunk.len() >= remaining {
        buffer.extend_from_slice(&chunk[..remaining]);
        true
    } else {
        buffer.extend_from_slice(chunk);
        false
    }
}

fn body_to_text(content_type: Option<&str>, body: &[u8]) -> String {
    let text = String::from_utf8_lossy(body);
    let is_html =
        content_type.is_some_and(|value| value.to_ascii_lowercase().contains("text/html"));
    if is_html {
        html_to_text(&text)
    } else {
        text.into_owned()
    }
}

fn html_to_text(html: &str) -> String {
    decode_entities(&strip_tags(html))
}

fn strip_tags(html: &str) -> String {
    let mut output = String::with_capacity(html.len());
    let mut in_tag = false;
    for character in html.chars() {
        match character {
            '<' => in_tag = true,
            '>' if in_tag => in_tag = false,
            _ if in_tag => {}
            _ => output.push(character),
        }
    }
    output
}

fn decode_entities(text: &str) -> String {
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&amp;", "&")
}

fn format_response(status: u16, url: &str, body: &str) -> String {
    format!("status: {status}\nurl: {url}\n\n{body}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::context;
    use proptest::prelude::*;

    #[test]
    fn html_tags_are_stripped_and_entities_decoded() {
        assert_eq!(
            html_to_text("<p>Hello &amp; <b>world</b></p>"),
            "Hello & world"
        );
        assert_eq!(html_to_text("<div>one</div><div>two</div>"), "onetwo");
        assert_eq!(strip_tags("a > b"), "a > b");
        assert_eq!(strip_tags("a <b> c"), "a  c");
    }

    #[tokio::test]
    async fn read_body_returns_response_bytes() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback");
        let address = listener.local_addr().expect("local address");
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept");
            let mut request = [0u8; 1024];
            let _ = stream.read(&mut request).await;
            let body = b"hello";
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            stream
                .write_all(response.as_bytes())
                .await
                .expect("write headers");
            stream.write_all(body).await.expect("write body");
        });
        let response = reqwest::Client::new()
            .get(format!("http://{address}"))
            .send()
            .await
            .expect("response");
        let body = read_body(response).await.expect("body");
        assert_eq!(body, b"hello");
        server.await.expect("server");
    }

    #[test]
    fn entities_decode_in_order() {
        assert_eq!(
            decode_entities("&lt;tag&gt; &quot;q&quot; &#39;a&#39;"),
            "<tag> \"q\" 'a'"
        );
        assert_eq!(decode_entities("&amp;amp;"), "&amp;");
    }

    #[test]
    fn responses_are_prefixed_with_status_and_url() {
        assert_eq!(
            format_response(200, "https://example.com/", "body"),
            "status: 200\nurl: https://example.com/\n\nbody"
        );
    }

    #[test]
    fn html_content_types_are_converted_to_text() {
        assert_eq!(
            body_to_text(Some("text/html; charset=utf-8"), b"<p>hi</p>"),
            "hi"
        );
        assert_eq!(body_to_text(Some("TEXT/HTML"), b"<p>hi</p>"), "hi");
    }

    #[test]
    fn other_content_types_are_returned_verbatim() {
        assert_eq!(body_to_text(Some("text/plain"), b"<p>hi</p>"), "<p>hi</p>");
        assert_eq!(body_to_text(None, b"\xFF raw"), "\u{FFFD} raw");
    }

    #[test]
    fn body_buffers_stop_at_the_cap() {
        let mut buffer = Vec::new();
        assert!(!append_capped(&mut buffer, b"abc", 5));
        assert_eq!(buffer, b"abc");
        assert!(append_capped(&mut buffer, b"defg", 5));
        assert_eq!(buffer, b"abcde");
        assert!(append_capped(&mut buffer, b"more", 5));
        assert_eq!(buffer, b"abcde");
    }

    #[test]
    fn body_cap_is_one_megabyte() {
        assert_eq!(MAX_BODY_BYTES, 1_048_576);
        assert_eq!(REQUEST_TIMEOUT_SECS, 20);
        assert_eq!(MAX_REDIRECTS, 10);
    }

    #[tokio::test]
    async fn invalid_urls_are_tool_errors() {
        let directory = tempfile::tempdir().expect("tempdir");
        let context = context(directory.path(), PermissionMode::WorkspaceWrite);
        let outcome = WebFetchTool
            .execute(&context, json!({"url": "http://["}))
            .await;
        assert!(outcome.is_error);
        assert!(outcome.text_content().contains("failed"));
    }

    #[tokio::test]
    async fn read_only_mode_refuses_fetches() {
        let directory = tempfile::tempdir().expect("tempdir");
        let context = context(directory.path(), PermissionMode::ReadOnly);
        let outcome = WebFetchTool
            .execute(&context, json!({"url": "https://example.com"}))
            .await;
        assert!(outcome.is_error);
        assert!(outcome.text_content().contains("read-only"));
    }

    #[tokio::test]
    async fn missing_urls_are_rejected() {
        let directory = tempfile::tempdir().expect("tempdir");
        let context = context(directory.path(), PermissionMode::WorkspaceWrite);
        let outcome = WebFetchTool.execute(&context, json!({})).await;
        assert!(outcome.is_error);
        assert!(outcome.text_content().contains("invalid arguments"));
    }

    fn plain_text() -> impl Strategy<Value = String> {
        "[A-Za-z0-9 ]{0,10}"
    }

    fn tag() -> impl Strategy<Value = String> {
        prop::sample::select(vec![
            "<p>",
            "</p>",
            "<b>",
            "</b>",
            "<div class=\"x\">",
            "</div>",
        ])
        .prop_map(str::to_string)
    }

    proptest! {
        #[test]
        fn stripping_tags_leaves_no_angle_brackets(
            parts in prop::collection::vec(prop_oneof![plain_text(), tag()], 0..30),
        ) {
            let html = parts.concat();
            let stripped = strip_tags(&html);
            prop_assert!(!stripped.contains('<'));
            prop_assert!(!stripped.contains('>'));
        }

        #[test]
        fn text_without_tags_is_unchanged(text in plain_text()) {
            prop_assert_eq!(strip_tags(&text), text);
        }
    }
}
