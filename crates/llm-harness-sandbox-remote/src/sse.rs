//! Minimal SSE decoder used by the remote sandbox client.

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct SseEvent {
    pub event: Option<String>,
    pub data: String,
}

#[derive(Default)]
pub(crate) struct SseDecoder {
    buffer: Vec<u8>,
    pending_carriage_return: bool,
    current_event: Option<String>,
    current_data: String,
}

impl SseDecoder {
    pub(crate) fn push(
        &mut self,
        bytes: &[u8],
        max_event_bytes: usize,
    ) -> Result<Vec<SseEvent>, String> {
        let mut events = Vec::new();
        let mut offset = 0;

        if self.pending_carriage_return && !bytes.is_empty() {
            self.pending_carriage_return = false;
            if bytes.first() == Some(&b'\n') {
                offset = 1;
            }
        }

        while let Some(relative_terminator) = bytes[offset..]
            .iter()
            .position(|byte| *byte == b'\n' || *byte == b'\r')
        {
            let terminator = offset + relative_terminator;
            let line = &bytes[offset..terminator];
            let terminator_length =
                if bytes[terminator] == b'\r' && bytes.get(terminator + 1) == Some(&b'\n') {
                    2
                } else {
                    1
                };
            if bytes[terminator] == b'\r' && terminator + 1 == bytes.len() {
                if self.buffer.len() + self.current_data.len() + line.len() > max_event_bytes {
                    return Err(format!("SSE event exceeds {max_event_bytes} bytes"));
                }
                self.buffer.extend_from_slice(line);
                let line_bytes = std::mem::take(&mut self.buffer);
                if let Some(event) = self.process_line_bytes(line_bytes)? {
                    events.push(event);
                }
                self.pending_carriage_return = true;
                offset = bytes.len();
                break;
            }
            if self.buffer.len() + self.current_data.len() + line.len() > max_event_bytes {
                return Err(format!("SSE event exceeds {max_event_bytes} bytes"));
            }
            self.buffer.extend_from_slice(line);
            let line_bytes = std::mem::take(&mut self.buffer);
            if let Some(event) = self.process_line_bytes(line_bytes)? {
                events.push(event);
            }
            offset = terminator + terminator_length;
        }

        let tail = &bytes[offset..];
        if !tail.is_empty() {
            if self.buffer.len() + self.current_data.len() + tail.len() > max_event_bytes {
                return Err(format!("SSE event exceeds {max_event_bytes} bytes"));
            }
            self.buffer.extend_from_slice(tail);
        }

        Ok(events)
    }

    pub(crate) fn finish(&mut self) -> Result<Option<SseEvent>, String> {
        self.pending_carriage_return = false;
        if self.buffer.is_empty() {
            return Ok(self.dispatch());
        }
        let line_bytes = std::mem::take(&mut self.buffer);
        if let Some(event) = self.process_line_bytes(line_bytes)? {
            return Ok(Some(event));
        }
        Ok(self.dispatch())
    }

    fn process_line_bytes(&mut self, mut line_bytes: Vec<u8>) -> Result<Option<SseEvent>, String> {
        if line_bytes.last() == Some(&b'\n') {
            line_bytes.pop();
        }
        if line_bytes.last() == Some(&b'\r') {
            line_bytes.pop();
        }
        let line = String::from_utf8(line_bytes)
            .map_err(|error| format!("invalid UTF-8 in SSE stream: {error}"))?;
        Ok(self.process_line(&line))
    }

    fn process_line(&mut self, line: &str) -> Option<SseEvent> {
        if line.is_empty() {
            return self.dispatch();
        }
        if line.starts_with(':') {
            return None;
        }

        let (field, value) = line.split_once(':').unwrap_or((line, ""));
        let value = value.strip_prefix(' ').unwrap_or(value);
        match field {
            "event" => self.current_event = Some(value.to_owned()),
            "data" => {
                if !self.current_data.is_empty() {
                    self.current_data.push('\n');
                }
                self.current_data.push_str(value);
            }
            _ => {}
        }
        None
    }

    fn dispatch(&mut self) -> Option<SseEvent> {
        if self.current_event.is_none() && self.current_data.is_empty() {
            return None;
        }
        Some(SseEvent {
            event: self.current_event.take(),
            data: std::mem::take(&mut self.current_data),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_chunked_events() {
        let mut decoder = SseDecoder::default();
        let events = decoder
            .push(b"event: started\r\ndata: {\"id\":\"one\"}\r\n\r\n", 1024)
            .unwrap();
        assert_eq!(
            events,
            vec![SseEvent {
                event: Some("started".to_owned()),
                data: "{\"id\":\"one\"}".to_owned(),
            }]
        );
    }

    #[test]
    fn decodes_multiline_data_and_final_event() {
        let mut decoder = SseDecoder::default();
        let events = decoder
            .push(b"event: exit\ndata: first\ndata: second\n\n", 1024)
            .unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "first\nsecond");

        let mut final_decoder = SseDecoder::default();
        assert!(final_decoder.push(b"data: tail", 1024).unwrap().is_empty());
        assert_eq!(final_decoder.finish().unwrap().unwrap().data, "tail");
    }

    #[test]
    fn decodes_crlf_split_across_chunks() {
        let mut decoder = SseDecoder::default();
        assert!(decoder.push(b"event: exit\r", 1024).unwrap().is_empty());
        let events = decoder.push(b"\ndata: result\r\n\r\n", 1024).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event.as_deref(), Some("exit"));
        assert_eq!(events[0].data, "result");
    }

    #[test]
    fn decodes_standalone_carriage_return_at_chunk_boundary() {
        let mut decoder = SseDecoder::default();
        assert!(decoder.push(b"event: exit\r", 1024).unwrap().is_empty());
        let events = decoder.push(b"data: result\r\r", 1024).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event.as_deref(), Some("exit"));
        assert_eq!(events[0].data, "result");
    }

    #[test]
    fn finishes_an_event_ending_with_carriage_return() {
        let mut decoder = SseDecoder::default();
        assert!(
            decoder
                .push(b"event: exit\rdata: result\r", 1024)
                .unwrap()
                .is_empty()
        );
        let event = decoder.finish().unwrap().unwrap();
        assert_eq!(event.event.as_deref(), Some("exit"));
        assert_eq!(event.data, "result");
    }

    #[test]
    fn decodes_bare_carriage_return_lines() {
        let mut decoder = SseDecoder::default();
        let events = decoder
            .push(b"event: exit\rdata: result\r\r", 1024)
            .unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event.as_deref(), Some("exit"));
        assert_eq!(events[0].data, "result");
    }

    #[test]
    fn ignores_comments_and_empty_events() {
        let mut decoder = SseDecoder::default();
        let events = decoder.push(b": heartbeat\n\n", 1024).unwrap();
        assert!(events.is_empty());
    }

    #[test]
    fn rejects_events_exceeding_the_configured_limit() {
        let mut decoder = SseDecoder::default();
        let error = decoder
            .push(b"data: 0123456789\n\n", 8)
            .expect_err("event should exceed limit");
        assert!(error.contains("exceeds 8 bytes"));
    }

    #[test]
    fn allows_multiple_small_events_in_one_chunk() {
        let mut decoder = SseDecoder::default();
        let events = decoder
            .push(b"data: one\n\ndata: two\n\n", 10)
            .expect("events should be limited individually");
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].data, "one");
        assert_eq!(events[1].data, "two");
    }
}
