//! Web search through the `DuckDuckGo` HTML endpoint.

use crate::http::{decode_entities, strip_tags};
use crate::input::parse;
use async_trait::async_trait;
use dsh_core::{PermissionMode, Tool, ToolContext, ToolOutcome, ToolSchema};
use regex::Regex;
use serde::Deserialize;
use serde_json::{Value, json};
use std::time::Duration;

/// One parsed search result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchHit {
    /// Result title.
    pub title: String,
    /// Destination URL.
    pub url: String,
    /// Result snippet.
    pub snippet: String,
}

/// Search the web through a provider endpoint.
pub struct WebSearchTool {
    endpoint: String,
}

impl Default for WebSearchTool {
    fn default() -> Self {
        Self::new("https://html.duckduckgo.com/html/")
    }
}

impl WebSearchTool {
    /// Build a search tool for a test or alternate provider endpoint.
    #[must_use]
    pub fn new(endpoint: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into(),
        }
    }
}

#[derive(Deserialize)]
struct SearchArgs {
    query: String,
    #[serde(default = "default_limit")]
    limit: usize,
}

const fn default_limit() -> usize {
    5
}

#[async_trait]
impl Tool for WebSearchTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "web_search".to_string(),
            description: "Search the web and return titles, URLs, and snippets.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string"},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 20}
                },
                "required": ["query"]
            }),
        }
    }

    async fn execute(&self, context: &ToolContext, arguments: Value) -> ToolOutcome {
        if context.permission == PermissionMode::ReadOnly {
            return ToolOutcome::error("web_search is disabled in read-only mode");
        }
        let args: SearchArgs = match parse(arguments) {
            Ok(args) => args,
            Err(error) => return error,
        };
        if args.query.trim().is_empty() {
            return ToolOutcome::error("search query must not be empty");
        }
        if args.limit == 0 || args.limit > 20 {
            return ToolOutcome::error("search limit must be between 1 and 20");
        }
        let mut url = match reqwest::Url::parse(&self.endpoint) {
            Ok(url) => url,
            Err(error) => return ToolOutcome::error(format!("invalid search endpoint: {error}")),
        };
        url.query_pairs_mut().append_pair("q", &args.query);
        let client = match reqwest::Client::builder()
            .timeout(Duration::from_secs(20))
            .build()
        {
            Ok(client) => client,
            Err(error) => return ToolOutcome::error(format!("cannot build HTTP client: {error}")),
        };
        let response = match client
            .get(url)
            .header(reqwest::header::USER_AGENT, "deepseacrab-harness/1")
            .send()
            .await
        {
            Ok(response) => response,
            Err(error) => return ToolOutcome::error(format!("search failed: {error}")),
        };
        if !response.status().is_success() {
            return ToolOutcome::error(format!("search returned status {}", response.status()));
        }
        let html = match response.text().await {
            Ok(html) => html,
            Err(error) => return ToolOutcome::error(format!("cannot read search page: {error}")),
        };
        let html: String = html.chars().take(524_288).collect();
        let hits = parse_results(&html, args.limit);
        if hits.is_empty() {
            return ToolOutcome::text("no results");
        }
        ToolOutcome::text(format_results(&hits))
    }
}

fn parse_results(html: &str, limit: usize) -> Vec<SearchHit> {
    if limit == 0 {
        return Vec::new();
    }
    let Ok(link_pattern) =
        Regex::new(r#"(?is)<a([^>]*class="[^"]*result__a[^"]*"[^>]*)>(.*?)</a>"#)
    else {
        return Vec::new();
    };
    let Ok(snippet_pattern) =
        Regex::new(r#"(?is)<a([^>]*class="[^"]*result__snippet[^"]*"[^>]*)>(.*?)</a>"#)
    else {
        return Vec::new();
    };
    let snippets: Vec<String> = snippet_pattern
        .captures_iter(html)
        .map(|capture| text_of(&capture[2]))
        .collect();
    let mut hits = Vec::new();
    for (index, capture) in link_pattern.captures_iter(html).enumerate() {
        if hits.len() >= limit {
            break;
        }
        let Some(href) = attribute(&capture[1], "href") else {
            continue;
        };
        let title = text_of(&capture[2]);
        if title.is_empty() {
            continue;
        }
        hits.push(SearchHit {
            title,
            url: destination(&href),
            snippet: snippets.get(index).cloned().unwrap_or_default(),
        });
    }
    hits
}

fn text_of(html: &str) -> String {
    decode_entities(&strip_tags(html)).trim().to_string()
}

fn attribute(tag: &str, name: &str) -> Option<String> {
    let pattern = format!(r#"(?is){name}\s*=\s*"([^"]*)""#);
    let regex = Regex::new(&pattern).ok()?;
    regex
        .captures(tag)
        .map(|capture| decode_entities(&capture[1]))
}

fn destination(href: &str) -> String {
    let absolute = if href.starts_with("//") {
        format!("https:{href}")
    } else if href.starts_with('/') {
        format!("https://duckduckgo.com{href}")
    } else {
        href.to_string()
    };
    if let Ok(url) = reqwest::Url::parse(&absolute) {
        for (key, value) in url.query_pairs() {
            if key == "uddg" {
                return value.into_owned();
            }
        }
    }
    absolute
}

fn format_results(hits: &[SearchHit]) -> String {
    let mut output = String::new();
    for (index, hit) in hits.iter().enumerate() {
        output.push_str(&format!(
            "{}. {}\n   {}\n   {}\n",
            index + 1,
            hit.title,
            hit.url,
            hit.snippet
        ));
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::context;
    use dsh_core::PermissionMode;

    const FIXTURE: &str = r#"
        <a rel="nofollow" class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fa&amp;rut=1">Example <b>A</b></a>
        <a class="result__snippet" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fa">Snippet <b>one</b> &amp; more</a>
        <a rel="nofollow" class="result__a" href="https://example.com/b">Example B</a>
        <a class="result__snippet">Snippet two</a>
    "#;

    #[test]
    fn results_are_parsed_and_decoded() {
        let hits = parse_results(FIXTURE, 10);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].title, "Example A");
        assert_eq!(hits[0].url, "https://example.com/a");
        assert_eq!(hits[0].snippet, "Snippet one & more");
        assert_eq!(hits[1].title, "Example B");
        assert_eq!(hits[1].url, "https://example.com/b");
        assert_eq!(hits[1].snippet, "Snippet two");
    }

    #[test]
    fn limits_and_empty_pages_are_respected() {
        assert_eq!(parse_results(FIXTURE, 0), Vec::new());
        assert_eq!(parse_results(FIXTURE, 1).len(), 1);
        assert_eq!(parse_results("<html></html>", 5), Vec::new());
    }

    #[test]
    fn formatting_is_numbered() {
        let hits = vec![SearchHit {
            title: "Title".to_string(),
            url: "https://example.com".to_string(),
            snippet: "Snippet".to_string(),
        }];
        assert_eq!(
            format_results(&hits),
            "1. Title\n   https://example.com\n   Snippet\n"
        );
    }

    #[tokio::test]
    async fn read_only_mode_refuses_search() {
        let directory = tempfile::tempdir().expect("tempdir");
        let context = context(directory.path(), PermissionMode::ReadOnly);
        let outcome = WebSearchTool::default()
            .execute(&context, json!({"query": "rust"}))
            .await;
        assert!(outcome.is_error);
        assert!(outcome.text_content().contains("read-only"));
    }

    #[tokio::test]
    async fn invalid_arguments_are_rejected() {
        let directory = tempfile::tempdir().expect("tempdir");
        let context = context(directory.path(), PermissionMode::WorkspaceWrite);
        let tool = WebSearchTool::new("http://127.0.0.1:1/");
        assert_eq!(default_limit(), 5);
        let empty = tool.execute(&context, json!({"query": ""})).await;
        assert!(empty.is_error);
        assert!(empty.text_content().contains("must not be empty"));
        for limit in [0, 21] {
            let outcome = tool
                .execute(&context, json!({"query": "x", "limit": limit}))
                .await;
            assert!(outcome.is_error);
            assert!(outcome.text_content().contains("between 1 and 20"));
        }
    }

    fn serve(status: &str, body: &'static str) -> (u16, std::thread::JoinHandle<()>) {
        use std::io::{ErrorKind, Read, Write};
        use std::time::{Duration, Instant};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("address").port();
        let status = status.to_string();
        let handle = std::thread::spawn(move || {
            listener.set_nonblocking(true).expect("nonblocking");
            let deadline = Instant::now() + Duration::from_secs(2);
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
            let mut request = [0u8; 4096];
            let _ = stream.read(&mut request);
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(response.as_bytes()).expect("write");
        });
        (port, handle)
    }

    #[tokio::test]
    async fn successful_pages_are_parsed_and_status_failures_are_errors() {
        let (port, server) = serve("200 OK", FIXTURE);
        let directory = tempfile::tempdir().expect("tempdir");
        let context = context(directory.path(), PermissionMode::WorkspaceWrite);
        let outcome = WebSearchTool::new(format!("http://127.0.0.1:{port}/"))
            .execute(&context, json!({"query": "rust", "limit": 20}))
            .await;
        server.join().expect("server");
        assert!(!outcome.is_error);
        assert!(outcome.text_content().contains("Example A"));

        let (port, server) = serve("500 Internal Server Error", "boom");
        let outcome = WebSearchTool::new(format!("http://127.0.0.1:{port}/"))
            .execute(&context, json!({"query": "rust"}))
            .await;
        server.join().expect("server");
        assert!(outcome.is_error);
        assert!(outcome.text_content().contains("status 500"));
    }
}
