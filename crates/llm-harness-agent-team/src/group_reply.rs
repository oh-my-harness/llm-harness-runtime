/// Typed run context marking the current turn as a reply to a group message.
#[derive(Clone)]
pub struct GroupReplyContext {
    pub broadcast_id: String,
    pub responder: Option<String>,
}
