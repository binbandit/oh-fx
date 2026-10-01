const BYTE_ORDER_MARK: &[u8] = b"\xef\xbb\xbf";

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SseError {
    #[error("EventTooLarge")]
    EventTooLarge,
    #[error("StreamTooLarge")]
    StreamTooLarge,
}

enum Line<'a> {
    Boundary,
    Data(&'a [u8]),
    Ignored,
}

fn classify(line: &[u8]) -> Line<'_> {
    if line.is_empty() {
        return Line::Boundary;
    }
    let colon = line.iter().position(|byte| *byte == b':');
    let name = &line[..colon.unwrap_or(line.len())];
    if name != b"data" {
        return Line::Ignored;
    }
    let value = colon.map_or(&[][..], |index| &line[index + 1..]);
    Line::Data(value.strip_prefix(b" ").unwrap_or(value))
}

enum LineSource {
    Pending(usize, usize),
    Assembled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EventState {
    Empty,
    Collecting,
    Dispatched,
}

#[derive(Debug)]
pub struct SseDecoder {
    max_event_bytes: usize,
    max_total_bytes: Option<usize>,
    pending: Vec<u8>,
    offset: usize,
    line: Vec<u8>,
    line_assembled: bool,
    data: Vec<u8>,
    state: EventState,
    total_bytes: usize,
    first_line: bool,
    skip_lf: bool,
}

impl SseDecoder {
    pub fn new(max_event_bytes: usize) -> Self {
        Self {
            max_event_bytes,
            max_total_bytes: None,
            pending: Vec::new(),
            offset: 0,
            line: Vec::new(),
            line_assembled: false,
            data: Vec::new(),
            state: EventState::Empty,
            total_bytes: 0,
            first_line: true,
            skip_lf: false,
        }
    }

    pub fn set_total_limit(&mut self, max_total_bytes: Option<usize>) {
        self.max_total_bytes = max_total_bytes;
    }

    pub fn total_bytes(&self) -> usize {
        self.total_bytes
    }

    pub fn push(&mut self, bytes: &[u8]) {
        if self.offset > 0 {
            self.pending.drain(..self.offset);
            self.offset = 0;
        }
        self.pending.extend_from_slice(bytes);
    }

    pub fn next_event(&mut self) -> Result<Option<&[u8]>, SseError> {
        if self.state == EventState::Dispatched {
            self.state = EventState::Empty;
            self.data.clear();
        }
        while let Some(source) = self.next_line()? {
            let raw = match source {
                LineSource::Pending(start, end) => &self.pending[start..end],
                LineSource::Assembled => &self.line[..],
            };
            let line = if self.first_line {
                raw.strip_prefix(BYTE_ORDER_MARK).unwrap_or(raw)
            } else {
                raw
            };
            self.first_line = false;
            match classify(line) {
                Line::Boundary if self.state == EventState::Collecting => {
                    self.state = EventState::Dispatched;
                    return Ok(Some(&self.data));
                }
                Line::Ignored | Line::Boundary => {}
                Line::Data(value) => {
                    append_data(
                        &mut self.data,
                        self.state == EventState::Collecting,
                        value,
                        self.max_event_bytes,
                    )?;
                    self.state = EventState::Collecting;
                }
            }
        }
        Ok(None)
    }

    fn next_line(&mut self) -> Result<Option<LineSource>, SseError> {
        if self.line_assembled {
            self.line_assembled = false;
            self.line.clear();
        }
        loop {
            let Some(&first) = self.pending.get(self.offset) else {
                return Ok(None);
            };
            if self.skip_lf {
                self.skip_lf = false;
                if first == b'\n' {
                    self.consume(1)?;
                    continue;
                }
            }
            let start = self.offset;
            let available = &self.pending[start..];
            let end = available
                .iter()
                .position(|byte| *byte == b'\r' || *byte == b'\n');
            let length = end.unwrap_or(available.len());
            if length > self.max_event_bytes - self.line.len() {
                return Err(SseError::EventTooLarge);
            }
            let Some(end) = end else {
                self.line.extend_from_slice(available);
                self.consume(length)?;
                continue;
            };
            self.skip_lf = available[end] == b'\r';
            self.consume(end + 1)?;
            if self.line.is_empty() {
                return Ok(Some(LineSource::Pending(start, start + length)));
            }
            self.line
                .extend_from_slice(&self.pending[start..start + length]);
            self.line_assembled = true;
            return Ok(Some(LineSource::Assembled));
        }
    }

    fn consume(&mut self, count: usize) -> Result<(), SseError> {
        if let Some(limit) = self.max_total_bytes
            && count > limit.saturating_sub(self.total_bytes)
        {
            return Err(SseError::StreamTooLarge);
        }
        self.total_bytes += count;
        self.offset += count;
        Ok(())
    }
}

fn append_data(
    data: &mut Vec<u8>,
    continues_event: bool,
    value: &[u8],
    max_event_bytes: usize,
) -> Result<(), SseError> {
    if continues_event {
        if data.len() == max_event_bytes {
            return Err(SseError::EventTooLarge);
        }
        data.push(b'\n');
    }
    if value.len() > max_event_bytes - data.len() {
        return Err(SseError::EventTooLarge);
    }
    data.extend_from_slice(value);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode_in_chunks(
        wire: &[u8],
        chunk_size: usize,
        decoder: &mut SseDecoder,
    ) -> Result<Vec<String>, SseError> {
        let mut events = Vec::new();
        for chunk in wire.chunks(chunk_size.max(1)) {
            decoder.push(chunk);
            while let Some(event) = decoder.next_event()? {
                events.push(String::from_utf8(event.to_vec()).unwrap());
            }
        }
        Ok(events)
    }

    fn decode(wire: &[u8], max_event_bytes: usize) -> Result<Vec<String>, SseError> {
        decode_in_chunks(wire, wire.len(), &mut SseDecoder::new(max_event_bytes))
    }

    #[test]
    fn provider_framing_preserves_data_across_encoding_and_chunk_boundaries() {
        let payloads: [&[u8]; 3] = [
            b"data: one\ndata:  two \n\ndata:three\n\n",
            b"\xef\xbb\xbf: heartbeat\r\nevent: message\r\ndata: one\r\ndata:  two \r\n\r\ndata:three\r\n\r\n",
            b"data: one\rdata:  two \r\rdata:three\r\r",
        ];
        for payload in payloads {
            for size in 1..=payload.len() {
                let mut decoder = SseDecoder::new(128);
                let events = decode_in_chunks(payload, size, &mut decoder).unwrap();
                assert_eq!(events, ["one\n two ", "three"], "chunk size {size}");
                assert_eq!(decoder.next_event(), Ok(None));
            }
        }
    }

    #[test]
    fn provider_framing_never_dispatches_an_unfinished_event() {
        let payloads: [&[u8]; 5] = [
            b"",
            b": comment\n\n",
            b"data: partial",
            b"data: partial\n",
            b"data: one\ndata: two\n",
        ];
        for payload in payloads {
            assert_eq!(decode(payload, 128), Ok(Vec::new()));
        }
    }

    #[test]
    fn provider_framing_preserves_long_fields_with_mixed_delimiters() {
        let text = "x".repeat(129);
        for size in 0..=text.len() {
            let wire = format!("data:{}\r\rdata:tail\n\n", &text[..size]);
            assert_eq!(
                decode(wire.as_bytes(), 256),
                Ok(vec![text[..size].to_owned(), "tail".to_owned()])
            );
        }
    }

    #[test]
    fn provider_framing_stops_at_the_delimiter() {
        let mut decoder = SseDecoder::new(128);
        decoder.push(b"data: first\r\r");
        assert_eq!(decoder.next_event(), Ok(Some(&b"first"[..])));
        assert_eq!(decoder.next_event(), Ok(None));
    }

    #[test]
    fn provider_framing_bounds_lines_assembled_events_and_ignored_wire() {
        let cases: [(&[u8], usize, Option<&str>); 4] = [
            (b"data: 12\n\n", 8, Some("12")),
            (b"data: 12\n\n", 7, None),
            (b"data:12\ndata:34\ndata:56\n\n", 8, Some("12\n34\n56")),
            (b"data:12\ndata:34\ndata:56\n\n", 7, None),
        ];
        for (wire, max, expected) in cases {
            match expected {
                Some(event) => assert_eq!(decode(wire, max), Ok(vec![event.to_owned()])),
                None => assert_eq!(decode(wire, max), Err(SseError::EventTooLarge)),
            }
        }
        let wire = b": ignored\r\nevent: message\r\ndata:x\r\n\r\n";
        for limit in [wire.len(), wire.len() - 2] {
            let mut decoder = SseDecoder::new(64);
            decoder.set_total_limit(Some(limit));
            let events = decode_in_chunks(wire, wire.len(), &mut decoder);
            if limit == wire.len() {
                assert_eq!(events, Ok(vec!["x".to_owned()]));
                assert_eq!(decoder.next_event(), Ok(None));
            } else {
                assert_eq!(events, Err(SseError::StreamTooLarge));
            }
        }
    }

    #[test]
    fn provider_framing_fuzzes_chunk_invariant_event_data() {
        for seed in 0..64_usize {
            let bytes: String = (0..seed * 2)
                .map(|index| char::from(b'a' + u8::try_from((index * 7 + seed) % 26).unwrap()))
                .collect();
            let wire = format!("\u{feff}data:{bytes}\r\ndata: tail\r\n\r\n");
            let expected = format!("{bytes}\ntail");
            let chunk_size = 1 + bytes.len() % 32;
            let mut decoder = SseDecoder::new(256);
            assert_eq!(
                decode_in_chunks(wire.as_bytes(), chunk_size, &mut decoder),
                Ok(vec![expected])
            );
            assert_eq!(decoder.next_event(), Ok(None));
        }
    }
    fn reference_events(wire: &[u8]) -> Vec<Vec<u8>> {
        let wire = wire.strip_prefix(BYTE_ORDER_MARK).unwrap_or(wire);
        let mut lines = Vec::new();
        let mut start = 0;
        let mut index = 0;
        while index < wire.len() {
            if matches!(wire[index], b'\r' | b'\n') {
                lines.push(&wire[start..index]);
                if wire[index] == b'\r' && wire.get(index + 1) == Some(&b'\n') {
                    index += 1;
                }
                start = index + 1;
            }
            index += 1;
        }
        let mut events = Vec::new();
        let mut data: Option<Vec<u8>> = None;
        for line in lines {
            match classify(line) {
                Line::Boundary => events.extend(data.take()),
                Line::Ignored => {}
                Line::Data(value) => match &mut data {
                    Some(existing) => {
                        existing.push(b'\n');
                        existing.extend_from_slice(value);
                    }
                    None => data = Some(value.to_vec()),
                },
            }
        }
        events
    }

    #[test]
    fn provider_framing_matches_a_reference_decoder_for_random_wires_and_splits() {
        let pieces: [&[u8]; 14] = [
            b"data",
            b":",
            b" ",
            b"x",
            b"\xc3\xa9",
            b"\xf0\x9f\x8c\x8d",
            b"\r",
            b"\n",
            b"\r\n",
            b"event",
            b"id",
            b"{\"a\":1}",
            b"[DONE]",
            b"  ",
        ];
        let mut state = 0x9e37_79b9_7f4a_7c15_u64;
        let mut below = |bound: u64| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            usize::try_from(state % bound).unwrap()
        };
        for _ in 0..5_000 {
            let mut wire = Vec::new();
            if below(4) == 0 {
                wire.extend_from_slice(BYTE_ORDER_MARK);
            }
            for _ in 0..below(40) {
                wire.extend_from_slice(pieces[below(14)]);
            }
            let mut decoder = SseDecoder::new(1 << 20);
            let mut events = Vec::new();
            let mut offset = 0;
            while offset < wire.len() {
                let end = (offset + 1 + below(6)).min(wire.len());
                decoder.push(&wire[offset..end]);
                offset = end;
                while let Some(event) = decoder.next_event().unwrap() {
                    events.push(event.to_vec());
                }
            }
            assert_eq!(events, reference_events(&wire), "{wire:?}");
        }
    }
}
