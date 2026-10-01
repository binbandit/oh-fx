use crate::error::McpError;

pub(crate) const MAX_RETRY_MS: u32 = 60_000;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Event {
    pub(crate) kind: Option<String>,
    pub(crate) data: String,
    pub(crate) id: Option<String>,
    pub(crate) retry_ms: Option<u32>,
}

#[derive(Debug, Default)]
pub(crate) struct Parser {
    line: Vec<u8>,
    data: Option<Vec<u8>>,
    kind: Option<Vec<u8>>,
    id: Option<Vec<u8>>,
    retry_ms: Option<u32>,
    total_bytes: usize,
    event_count: usize,
    max_total_bytes: usize,
    max_event_bytes: usize,
    max_events: usize,
    pending_cr: bool,
}

impl Parser {
    pub(crate) fn new(max_total_bytes: usize, max_event_bytes: usize, max_events: usize) -> Self {
        Self {
            max_total_bytes,
            max_event_bytes,
            max_events,
            ..Self::default()
        }
    }

    pub(crate) fn feed(&mut self, chunk: &[u8], events: &mut Vec<Event>) -> Result<(), McpError> {
        self.total_bytes = self
            .total_bytes
            .checked_add(chunk.len())
            .ok_or(McpError::ResponseTooLarge)?;
        if self.max_total_bytes != 0 && self.total_bytes > self.max_total_bytes {
            return Err(McpError::ResponseTooLarge);
        }
        for &byte in chunk {
            if self.pending_cr {
                self.pending_cr = false;
                if byte == b'\n' {
                    continue;
                }
            }
            if byte == b'\r' || byte == b'\n' {
                let line = std::mem::take(&mut self.line);
                self.process_line(&line, events)?;
                self.line = line;
                self.line.clear();
                self.pending_cr = byte == b'\r';
                continue;
            }
            if self.line.len() >= self.max_event_bytes {
                return Err(McpError::SseEventTooLarge);
            }
            self.line.push(byte);
        }
        Ok(())
    }

    pub(crate) fn finish(&self) -> Result<(), McpError> {
        if !self.line.is_empty() || self.has_pending_event() {
            return Err(McpError::BrokenSseStream);
        }
        Ok(())
    }

    fn process_line(&mut self, line: &[u8], events: &mut Vec<Event>) -> Result<(), McpError> {
        if line.is_empty() {
            return self.dispatch(events);
        }
        if line[0] == b':' {
            return Ok(());
        }
        let colon = line
            .iter()
            .position(|byte| *byte == b':')
            .unwrap_or(line.len());
        let field = &line[..colon];
        let mut value = if colon < line.len() {
            &line[colon + 1..]
        } else {
            &[][..]
        };
        if value.first() == Some(&b' ') {
            value = &value[1..];
        }
        match field {
            b"data" => {
                let separator = usize::from(self.data.is_some());
                let max_event_bytes = self.max_event_bytes;
                let data = self.data.get_or_insert_with(Vec::new);
                if data.len() + separator + value.len() > max_event_bytes {
                    return Err(McpError::SseEventTooLarge);
                }
                if separator == 1 {
                    data.push(b'\n');
                }
                data.extend_from_slice(value);
            }
            b"event" => self.kind = Some(bounded_field(value, self.max_event_bytes)?),
            b"id" => {
                if !value.contains(&0) {
                    self.id = Some(bounded_field(value, self.max_event_bytes)?);
                }
            }
            b"retry" => {
                if let Some(parsed) = std::str::from_utf8(value)
                    .ok()
                    .and_then(|text| text.parse::<u32>().ok())
                {
                    self.retry_ms = Some(parsed.min(MAX_RETRY_MS));
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn dispatch(&mut self, events: &mut Vec<Event>) -> Result<(), McpError> {
        if !self.has_pending_event() {
            return Ok(());
        }
        self.event_count += 1;
        if self.max_events != 0 && self.event_count > self.max_events {
            return Err(McpError::TooManySseEvents);
        }
        events.push(Event {
            kind: self.kind.take().map(|kind| text(&kind)),
            data: self.data.take().map(|data| text(&data)).unwrap_or_default(),
            id: self.id.take().map(|id| text(&id)),
            retry_ms: self.retry_ms.take(),
        });
        Ok(())
    }

    fn has_pending_event(&self) -> bool {
        self.data.is_some() || self.kind.is_some() || self.id.is_some() || self.retry_ms.is_some()
    }
}

fn bounded_field(value: &[u8], max_event_bytes: usize) -> Result<Vec<u8>, McpError> {
    if value.len() > max_event_bytes {
        return Err(McpError::SseEventTooLarge);
    }
    Ok(value.to_vec())
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_sse_parser_preserves_event_data_id_and_bounded_retry() {
        let mut parser = Parser::new(1024, 256, 4);
        let mut events = Vec::new();
        parser
            .feed(
                b"event: message\r\ndata: one\ndata: two\nid: event-7\nretry: 90000\n\n",
                &mut events,
            )
            .unwrap();
        parser.finish().unwrap();
        assert_eq!(
            events,
            vec![Event {
                kind: Some("message".to_owned()),
                data: "one\ntwo".to_owned(),
                id: Some("event-7".to_owned()),
                retry_ms: Some(MAX_RETRY_MS),
            }]
        );
    }

    #[test]
    fn legacy_sse_parser_accepts_cr_lf_and_crlf_across_every_chunk_split() {
        for payload in [
            &b"event: message\ndata: one\ndata: two\nid: event-1\n\n"[..],
            b"event: message\rdata: one\rdata: two\rid: event-1\r\r",
            b"event: message\r\ndata: one\r\ndata: two\r\nid: event-1\r\n\r\n",
            b"event: message\r\ndata: one\rdata: two\r\nid: event-1\r\r\n",
        ] {
            for split in 0..=payload.len() {
                let mut parser = Parser::new(payload.len(), 128, 1);
                let mut events = Vec::new();
                parser.feed(&payload[..split], &mut events).unwrap();
                parser.feed(&payload[split..], &mut events).unwrap();
                parser.finish().unwrap();
                assert_eq!(events.len(), 1);
                assert_eq!(events[0].kind.as_deref(), Some("message"));
                assert_eq!(events[0].data, "one\ntwo");
                assert_eq!(events[0].id.as_deref(), Some("event-1"));
            }
        }
    }

    #[test]
    fn legacy_sse_parser_emits_empty_priming_events() {
        let mut parser = Parser::new(256, 64, 2);
        let mut events = Vec::new();
        parser.feed(b"id:\ndata:\n\n", &mut events).unwrap();
        parser.finish().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "");
        assert_eq!(events[0].id.as_deref(), Some(""));
    }

    #[test]
    fn legacy_sse_parser_rejects_partial_and_over_limit_streams() {
        let mut events = Vec::new();
        let mut partial = Parser::new(64, 32, 1);
        partial.feed(b"data: unfinished", &mut events).unwrap();
        assert_eq!(partial.finish(), Err(McpError::BrokenSseStream));
        let mut over_limit = Parser::new(8, 8, 1);
        assert_eq!(
            over_limit.feed(b"data: too long\n\n", &mut events),
            Err(McpError::ResponseTooLarge)
        );
        let mut too_many = Parser::new(0, 64, 1);
        assert_eq!(
            too_many.feed(b"data: a\n\ndata: b\n\n", &mut events),
            Err(McpError::TooManySseEvents)
        );
    }

    #[test]
    fn comments_and_unknown_fields_are_ignored() {
        let mut parser = Parser::new(0, 64, 0);
        let mut events = Vec::new();
        parser
            .feed(b": keepalive\nfoo: bar\nretry: soon\ndata\n\n", &mut events)
            .unwrap();
        assert_eq!(
            events,
            vec![Event {
                data: String::new(),
                ..Event::default()
            }]
        );
    }
}
