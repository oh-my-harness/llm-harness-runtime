//! Vision degradation — strip image/document blocks from history when the
//! provider cannot consume them (issue #145 gap 2).
//!
//! Once image blocks enter harness-owned history, a text-only provider rejects
//! the entire request (HTTP 400 `InvalidRequest`). Without repair the run is
//! stuck: the host cannot edit history (it lives inside the loop), so every
//! subsequent turn fails identically. This module performs the one-shot strip
//! and appends a repair-guidance tool message so the model understands why its
//! earlier tool-produced images vanished.

use llm_harness_types::{
    AgentMessage, ContentBlock, DataBlock, PersistedToolProjection, ProjectionMetadata,
    ToolProjectionKind,
};

/// Repair guidance appended after stripping, phrased as environment feedback
/// so the model understands why earlier tool-produced images vanished.
const VISION_REMOVAL_NOTICE: &str = "[system] Image and document blocks were removed from this conversation: the current model does not support multimodal input and rejected the request. Do not attempt to re-attach the removed images. If visual information is essential, tell the user that this model cannot process images.";

/// Projection for stripped tool messages: keep full projection so session
/// persistence is unaffected by the in-place repair.
#[allow(dead_code)] // used by unit tests below
fn full_projection() -> PersistedToolProjection {
    PersistedToolProjection {
        kind: ToolProjectionKind::Full,
        metadata: ProjectionMetadata::default(),
    }
}

/// Whether a data block references visual/document content.
fn is_visual_block(block: &DataBlock) -> bool {
    matches!(block, DataBlock::Image { .. } | DataBlock::Document { .. })
}

/// Strip image/document blocks from `ctx.messages` in place.
///
/// Returns `None` when the context contains no visual blocks (nothing to do —
/// the provider error has a different cause and must surface unchanged).
/// Returns `Some(count)` after stripping and inserting the repair notice.
///
/// Message kinds handled:
/// - `ToolResult`: visual `DataBlock`s are removed; a notice `DataBlock::text`
///   is appended to the *same* tool message (keeps trajectory pairing intact).
/// - `User`: visual `ContentBlock`s are removed; the notice is appended as a
///   text block on the same user message.
/// - Assistant/summary/custom messages never carry tool-produced visual data
///   in provider-bound history (assistant Image/Document blocks are already
///   downgraded to placeholder text by `content_block_to_response`).
pub(crate) fn strip_vision_from_context(messages: &mut [AgentMessage]) -> Option<usize> {
    let mut stripped = 0usize;
    for msg in messages.iter_mut() {
        match msg {
            AgentMessage::ToolResult(t) => {
                let before = t.content.len();
                t.content.retain(|b| !is_visual_block(b));
                stripped += before - t.content.len();
                if before != t.content.len() {
                    t.content.push(DataBlock::text(VISION_REMOVAL_NOTICE));
                }
            }
            AgentMessage::User(u) => {
                let before = u.content.len();
                u.content.retain(|b| {
                    !matches!(
                        b,
                        ContentBlock::Image { .. } | ContentBlock::Document { .. }
                    )
                });
                stripped += before - u.content.len();
                if before != u.content.len() {
                    u.content.push(ContentBlock::Text {
                        text: VISION_REMOVAL_NOTICE.into(),
                    });
                }
            }
            _ => {}
        }
    }
    (stripped > 0).then_some(stripped)
}

/// Strip visual blocks from both the loop context and the new-messages
/// mirror, so the provider request and `AgentEnd` payload stay consistent.
/// Returns the number of removed blocks from the primary context, or `None`
/// when there was nothing to strip (the provider error then surfaces
/// unchanged).
pub(crate) fn strip_vision(
    ctx_messages: &mut [AgentMessage],
    new_messages: &mut [AgentMessage],
) -> Option<usize> {
    let stripped = strip_vision_from_context(ctx_messages)?;
    let _ = strip_vision_from_context(new_messages);
    Some(stripped)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use llm_harness_types::{BinarySource, ImageSource, ToolResultMessage, UserMessage};

    fn image_tool_message() -> AgentMessage {
        AgentMessage::ToolResult(ToolResultMessage {
            tool_use_id: "t1".into(),
            content: vec![
                DataBlock::text("screenshot saved"),
                DataBlock::Image {
                    source: ImageSource::Base64 {
                        media_type: "image/png".into(),
                        data: "AAAA".into(),
                    },
                },
            ],
            is_error: false,
            projection: full_projection(),
            timestamp: Utc::now(),
        })
    }

    #[test]
    fn strips_image_and_appends_notice() {
        let mut msgs = vec![image_tool_message()];
        let stripped = strip_vision_from_context(&mut msgs).unwrap();
        assert_eq!(stripped, 1);
        let AgentMessage::ToolResult(t) = &msgs[0] else {
            panic!("expected tool result");
        };
        assert_eq!(t.content.len(), 2, "text + notice");
        assert!(matches!(t.content[0], DataBlock::Text { .. }));
        assert!(matches!(t.content[1], DataBlock::Text { .. }));
        let DataBlock::Text { text, .. } = &t.content[1] else {
            panic!("expected text notice");
        };
        assert!(text.contains("removed"));
    }

    #[test]
    fn returns_none_when_no_visual_blocks() {
        let mut msgs = vec![image_tool_message()];
        if let AgentMessage::ToolResult(t) = &mut msgs[0] {
            t.content.pop(); // drop the image
        }
        assert!(strip_vision_from_context(&mut msgs).is_none());
    }

    #[test]
    fn strips_user_image_blocks() {
        let mut msgs = vec![AgentMessage::User(UserMessage {
            content: vec![
                ContentBlock::Text {
                    text: "look".into(),
                },
                ContentBlock::Image {
                    source: ImageSource::Url {
                        url: "https://x/y.png".into(),
                    },
                },
            ],
            timestamp: Utc::now(),
        })];
        let stripped = strip_vision_from_context(&mut msgs).unwrap();
        assert_eq!(stripped, 1);
        let AgentMessage::User(u) = &msgs[0] else {
            panic!("expected user message");
        };
        assert_eq!(u.content.len(), 2, "text + notice");
    }

    #[test]
    fn document_blocks_are_stripped_too() {
        let mut msgs = vec![image_tool_message()];
        if let AgentMessage::ToolResult(t) = &mut msgs[0] {
            t.content.push(DataBlock::Document {
                name: Some("d.pdf".into()),
                media_type: "application/pdf".into(),
                data: BinarySource::Base64 {
                    data: "AAAA".into(),
                },
            });
        }
        let stripped = strip_vision_from_context(&mut msgs).unwrap();
        assert_eq!(stripped, 2);
    }
}
