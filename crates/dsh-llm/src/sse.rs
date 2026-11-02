//! Server-sent-event framing for the Messages streaming response.
//!
//! The parser keeps incomplete bytes buffered, so callers may feed arbitrary
//! transport chunks and only complete, blank-line-terminated frames are
//! decoded. An unterminated tail at end of stream is discarded rather than
//! mistaken for a final event.

use dsh_core::LlmError;

/// One dispatched server-sent event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseEvent {
    /// Optional `event:` name.
    pub event: Option<String>,
    /// Joined `data:` payload.
    pub data: String,
}

/// Incremental SSE frame decoder.
#[derive(Debug, Default)]
pub struct SseParser {
    buffer: Vec<u8>,
}

impl SseParser {
    /// Create an empty parser.
    #[must_use]
    pub const fn new() -> Self {
        Self { buffer: Vec::new() }
    }

    /// Feed transport bytes and return every event the feed completed.
    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<SseEvent>, LlmError> {
        self.buffer.extend_from_slice(chunk);
        let mut events = Vec::new();
        loop {
            let before = self.buffer.len();
            let Some(frame) = self.take_frame() else {
                break;
            };
            if self.buffer.len() == before {
                return Err(LlmError::malformed("SSE parser made no progress"));
            }
            if let Some(event) = decode_frame(&frame)? {
                events.push(event);
            }
        }
        Ok(events)
    }

    /// Remove one complete frame, leaving an unterminated tail buffered.
    fn take_frame(&mut self) -> Option<Vec<u8>> {
        let mut line_start = 0;
        loop {
            let offset = self.buffer[line_start..]
                .iter()
                .position(|byte| *byte == b'\n')?;
            let line_end = line_start + offset;
            let line = &self.buffer[line_start..line_end];
            if line.is_empty() || line == b"\r" {
                let frame = self.buffer[..line_start].to_vec();
                self.buffer.drain(..=line_end);
                return Some(frame);
            }
            let next = line_end + 1;
            if next <= line_start {
                return None;
            }
            line_start = next;
        }
    }
}

fn decode_frame(frame: &[u8]) -> Result<Option<SseEvent>, LlmError> {
    let text = std::str::from_utf8(frame)
        .map_err(|_| LlmError::malformed("DeepSeek Messages SSE frame is not valid UTF-8"))?;
    let mut event = None;
    let mut data: Vec<&str> = Vec::new();
    for line in text.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.starts_with(':') {
            continue;
        }
        let (field, value) = match line.split_once(':') {
            Some((field, value)) => (field, value.strip_prefix(' ').unwrap_or(value)),
            None => (line, ""),
        };
        match field {
            "event" => event = Some(value.to_string()),
            "data" => data.push(value),
            _ => {}
        }
    }
    if data.is_empty() {
        return Ok(None);
    }
    Ok(Some(SseEvent {
        event,
        data: data.join("\n"),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn event(event: Option<&str>, data: &str) -> SseEvent {
        SseEvent {
            event: event.map(ToString::to_string),
            data: data.to_string(),
        }
    }

    fn parse(stream: &str) -> Vec<SseEvent> {
        let mut parser = SseParser::new();
        parser.push(stream.as_bytes()).expect("valid stream")
    }

    #[test]
    fn accepts_lf_and_crlf_frames() {
        assert_eq!(parse("data: one\n\n"), vec![event(None, "one")]);
        assert_eq!(parse("data: two\r\n\r\n"), vec![event(None, "two")]);
        assert_eq!(
            parse("data: one\r\ndata: two\r\n\r\n"),
            vec![event(None, "one\ntwo")]
        );
    }

    #[test]
    fn joins_multi_line_data_with_newlines() {
        assert_eq!(
            parse("data: alpha\ndata: beta\ndata:\n\n"),
            vec![event(None, "alpha\nbeta\n")]
        );
    }

    #[test]
    fn ignores_comments_ids_retries_and_empty_frames() {
        assert_eq!(parse(": heartbeat\n\n"), Vec::<SseEvent>::new());
        assert_eq!(
            parse("id: 7\nretry: 100\ndata: payload\n\n"),
            vec![event(None, "payload")]
        );
        assert_eq!(parse(": first\n\nevent: ping\n\n"), Vec::<SseEvent>::new());
        assert_eq!(
            parse("data: kept\n: trailing comment\n\n"),
            vec![event(None, "kept")]
        );
    }

    #[test]
    fn reads_event_names_and_strips_one_leading_space() {
        assert_eq!(
            parse("event: content_block_delta\ndata: {}\n\n"),
            vec![event(Some("content_block_delta"), "{}")]
        );
        assert_eq!(parse("data:no-space\n\n"), vec![event(None, "no-space")]);
        assert_eq!(
            parse("data:  two-spaces\n\n"),
            vec![event(None, " two-spaces")]
        );
    }

    #[test]
    fn treats_a_field_without_a_colon_as_empty() {
        assert_eq!(parse("data\n\n"), vec![event(None, "")]);
        assert_eq!(parse("event\ndata: x\n\n"), vec![event(Some(""), "x")]);
    }

    #[test]
    fn retains_crlf_split_across_chunks_and_ignores_empty_chunks() {
        let mut parser = SseParser::new();
        assert_eq!(
            parser.push(b"data: x\r").expect("prefix"),
            Vec::<SseEvent>::new()
        );
        assert_eq!(parser.push(b"").expect("empty"), Vec::<SseEvent>::new());
        assert_eq!(
            parser.push(b"\n\r\n").expect("tail"),
            vec![event(None, "x")]
        );
    }

    #[test]
    fn buffers_an_unterminated_tail_until_a_blank_line_arrives() {
        let mut parser = SseParser::new();
        assert_eq!(
            parser.push(b"data: tail").expect("tail"),
            Vec::<SseEvent>::new()
        );
        assert_eq!(
            parser.push(b"\ndata: more\n").expect("lines"),
            Vec::<SseEvent>::new()
        );
        assert_eq!(
            parser.push(b"\n").expect("terminator"),
            vec![event(None, "tail\nmore")]
        );
    }

    #[test]
    fn rejects_invalid_utterances_but_accepts_split_utf8() {
        let mut parser = SseParser::new();
        assert_eq!(
            parser.push(b"data: \xF0\x9F").expect("prefix"),
            Vec::<SseEvent>::new()
        );
        assert_eq!(
            parser.push(b"\x98\x80\n\n").expect("completion"),
            vec![event(None, "😀")]
        );

        let mut parser = SseParser::new();
        let error = parser.push(b"data: \xFF\n\n").expect_err("invalid byte");
        assert_eq!(error.code, dsh_core::LlmErrorCode::MalformedResponse);
    }

    proptest! {
        #[test]
        fn arbitrary_chunk_boundaries_preserve_framing(
            cuts in prop::collection::vec(0usize..48, 0..24),
        ) {
            let stream = concat!(
                ": heartbeat\r\n\r\n",
                "event: message_start\n",
                "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":3}}}\n\n",
                "event: content_block_start\n",
                "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
                "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello 世界\"}}\n\n",
                "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
                "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"}}\n\n",
                "data: {\"type\":\"message_stop\"}\n\n",
            );
            let expected = parse(stream);
            let bytes = stream.as_bytes();
            let mut parser = SseParser::new();
            let mut actual = Vec::new();
            let mut offset = 0;
            for cut in cuts {
                let end = (offset + cut).min(bytes.len());
                actual.extend(parser.push(&bytes[offset..end]).expect("valid chunk"));
                offset = end;
            }
            actual.extend(parser.push(&bytes[offset..]).expect("valid tail"));
            prop_assert_eq!(actual, expected);
        }
    }
}
