#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct SseEvent {
    pub event: Option<String>,
    pub data: String,
    pub id: Option<String>,
    pub retry_ms: Option<u64>,
}

#[derive(Default)]
pub(crate) struct SseDecoder {
    buffer: String,
    current: SseEvent,
}

impl SseDecoder {
    pub(crate) fn push(&mut self, bytes: &[u8]) -> Result<Vec<SseEvent>, std::str::Utf8Error> {
        self.buffer.push_str(std::str::from_utf8(bytes)?);
        let mut events = Vec::new();

        while let Some(newline) = self.buffer.find('\n') {
            let mut line = self.buffer[..newline].to_owned();
            self.buffer.drain(..=newline);
            if line.ends_with('\r') {
                line.pop();
            }
            if let Some(event) = self.process_line(&line) {
                events.push(event);
            }
        }

        Ok(events)
    }

    pub(crate) fn finish(&mut self) -> Option<SseEvent> {
        if !self.buffer.is_empty() {
            let mut line = std::mem::take(&mut self.buffer);
            if line.ends_with('\r') {
                line.pop();
            }
            if let Some(event) = self.process_line(&line) {
                return Some(event);
            }
        }
        self.dispatch()
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
            "event" => self.current.event = Some(value.to_owned()),
            "data" => {
                if !self.current.data.is_empty() {
                    self.current.data.push('\n');
                }
                self.current.data.push_str(value);
            }
            "id" if !value.contains('\0') => self.current.id = Some(value.to_owned()),
            "retry" => self.current.retry_ms = value.parse().ok(),
            _ => {}
        }
        None
    }

    fn dispatch(&mut self) -> Option<SseEvent> {
        if self.current.data.is_empty()
            && self.current.event.is_none()
            && self.current.id.is_none()
            && self.current.retry_ms.is_none()
        {
            return None;
        }
        Some(std::mem::take(&mut self.current))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_chunked_crlf_and_multiline_data() {
        let mut decoder = SseDecoder::default();
        let mut events = decoder
            .push(b"id: one\r\nevent: message\r\ndata: {\"a\":")
            .unwrap();
        assert!(events.is_empty());
        events.extend(
            decoder
                .push(b"1}\r\ndata: tail\r\nretry: 250\r\n\r\n")
                .unwrap(),
        );

        assert_eq!(
            events,
            vec![SseEvent {
                event: Some("message".to_owned()),
                data: "{\"a\":1}\ntail".to_owned(),
                id: Some("one".to_owned()),
                retry_ms: Some(250),
            }]
        );
    }

    #[test]
    fn ignores_comments_and_dispatches_final_event() {
        let mut decoder = SseDecoder::default();
        let events = decoder.push(b": heartbeat\ndata: hello").unwrap();
        assert!(events.is_empty());
        assert_eq!(decoder.finish().unwrap().data, "hello");
    }
}
