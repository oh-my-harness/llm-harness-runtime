use serde::{Deserialize, Serialize};

/// 图片数据来源。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ImageSource {
    /// Base64 编码的内联图片。
    Base64 {
        /// MIME 类型，如 `"image/png"`。
        media_type: String,
        /// Base64 编码的图片数据。
        data: String,
    },
    /// 公网可访问的图片 URL。
    Url {
        /// 图片 URL。
        url: String,
    },
}

/// Binary data that is safe to pass through the runtime as data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BinarySource {
    /// Base64-encoded bytes owned by the current value.
    Base64 { data: String },
    /// Opaque handle to a runtime-owned blob. Handles are intentionally not
    /// persistable in a Session projection.
    Blob { id: String },
    /// Publicly accessible URL owned by the value.
    Url { url: String },
}

/// External or tool-produced data that may be shown to a model.
///
/// Unlike [`ContentBlock`], this type cannot represent model reasoning or a
/// tool invocation. That distinction is the security boundary used by
/// Knowledge, Memory, and Session Projection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DataBlock {
    Text {
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        mime_type: Option<String>,
    },
    Image {
        source: ImageSource,
    },
    Document {
        #[serde(skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        media_type: String,
        data: BinarySource,
    },
}

impl DataBlock {
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text {
            text: text.into(),
            mime_type: None,
        }
    }

    /// Whether this block can be stored durably in a Session.
    pub fn is_persistable(&self) -> bool {
        !matches!(
            self,
            Self::Document {
                data: BinarySource::Blob { .. },
                ..
            }
        )
    }
}

/// 消息内容的最小单元。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    /// 普通文本。
    Text {
        /// 文本内容。
        text: String,
    },
    /// Provider 推理/思考内容（Anthropic、OpenAI o 系列、DeepSeek R 系列等）。
    Thinking {
        /// 思考内容。
        thinking: String,
        /// Anthropic 特有的内容签名；其他 provider 置 `None`。
        #[serde(skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },
    /// 图片内容。
    Image {
        /// 图片数据来源。
        source: ImageSource,
    },
    /// LLM 发出的工具调用请求。
    ToolUse {
        /// LLM 分配的工具调用唯一 ID。
        id: String,
        /// 工具名称。
        name: String,
        /// 工具调用参数（JSON）。
        input: serde_json::Value,
    },
    /// 文档内容（如 PDF），供支持文档输入的模型消费。
    ///
    /// 出现在用户消息（应用直发附件）与工具结果中。是否可发送由 provider
    /// capability 决定：Anthropic 支持 URL/base64 `document` block，智谱系
    /// 网关仅支持 URL `file_url`，多数网关不支持文档 part。
    ///
    /// 注意：`"type": "document"` 是 2026-08 新增的 serde tag。旧版本二进制
    /// 读取含 document 块的新 session 会反序列化失败——与 `ImageSource::Url`
    /// 同一演进策略（schema 前向不兼容，session 文件不做向后迁移）。
    ///
    /// 大小上限：与 [`DataBlock`] 不同，用户消息里的文档不做内联大小截断
    /// （工具与 MCP 路径有 4 MiB cap，因为其内容来自 agent/第三方）。用户
    /// 附件属于应用侧的信任边界——由应用自行限制附件大小。
    Document {
        /// 文件名（可选，展示用）。
        #[serde(skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        /// MIME 类型，如 `"application/pdf"`。
        media_type: String,
        /// 文档数据来源。
        data: BinarySource,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_block_serde_text() {
        let cb = ContentBlock::Text {
            text: "hello".into(),
        };
        let json = serde_json::to_string(&cb).unwrap();
        let cb2: ContentBlock = serde_json::from_str(&json).unwrap();
        assert!(matches!(cb2, ContentBlock::Text { text } if text == "hello"));
    }

    #[test]
    fn content_block_serde_tool_use() {
        let cb = ContentBlock::ToolUse {
            id: "call_1".into(),
            name: "read_file".into(),
            input: serde_json::json!({"path": "/tmp/x"}),
        };
        let json = serde_json::to_string(&cb).unwrap();
        let cb2: ContentBlock = serde_json::from_str(&json).unwrap();
        assert!(matches!(cb2, ContentBlock::ToolUse { name, .. } if name == "read_file"));
    }

    #[test]
    fn content_block_serde_thinking() {
        let cb = ContentBlock::Thinking {
            thinking: "let me think".into(),
            signature: Some("sig123".into()),
        };
        let json = serde_json::to_string(&cb).unwrap();
        let cb2: ContentBlock = serde_json::from_str(&json).unwrap();
        assert!(matches!(cb2, ContentBlock::Thinking { signature: Some(s), .. } if s == "sig123"));
    }

    #[test]
    fn content_block_serde_thinking_none_signature() {
        let cb = ContentBlock::Thinking {
            thinking: "some thought".into(),
            signature: None,
        };
        let json = serde_json::to_string(&cb).unwrap();
        let cb2: ContentBlock = serde_json::from_str(&json).unwrap();
        assert!(
            matches!(
                cb2,
                ContentBlock::Thinking {
                    signature: None,
                    ..
                }
            ),
            "expected signature to round-trip as None, got: {json}"
        );
    }

    #[test]
    fn data_block_cannot_represent_instruction_blocks() {
        let block = DataBlock::text("external data");
        let json = serde_json::to_value(block).unwrap();
        assert_eq!(json["type"], "text");
        assert!(json.get("thinking").is_none());
        assert!(json.get("tool_use").is_none());
    }

    #[test]
    fn runtime_blob_is_not_persistable() {
        let block = DataBlock::Document {
            name: Some("report.pdf".into()),
            media_type: "application/pdf".into(),
            data: BinarySource::Blob {
                id: "blob-1".into(),
            },
        };
        assert!(!block.is_persistable());
    }

    #[test]
    fn image_source_url_roundtrips_and_persists() {
        let src = ImageSource::Url {
            url: "https://example.com/img.png".into(),
        };
        let json = serde_json::to_string(&src).unwrap();
        assert_eq!(
            json,
            r#"{"type":"url","url":"https://example.com/img.png"}"#
        );
        let back: ImageSource = serde_json::from_str(&json).unwrap();
        assert_eq!(back, src);
        let block = DataBlock::Image { source: src };
        assert!(block.is_persistable());
    }

    #[test]
    fn content_block_document_url_roundtrips() {
        let cb = ContentBlock::Document {
            name: Some("doc.pdf".into()),
            media_type: "application/pdf".into(),
            data: BinarySource::Url {
                url: "https://example.com/doc.pdf".into(),
            },
        };
        let json = serde_json::to_string(&cb).unwrap();
        assert!(json.contains("\"type\":\"document\""));
        assert!(json.contains("\"type\":\"url\""));
        let back: ContentBlock = serde_json::from_str(&json).unwrap();
        assert!(
            matches!(&back, ContentBlock::Document { name, .. } if name.as_deref() == Some("doc.pdf"))
        );
    }

    #[test]
    fn binary_source_url_roundtrips_and_persists() {
        let src = BinarySource::Url {
            url: "https://example.com/doc.pdf".into(),
        };
        let json = serde_json::to_string(&src).unwrap();
        assert_eq!(
            json,
            r#"{"type":"url","url":"https://example.com/doc.pdf"}"#
        );
        let back: BinarySource = serde_json::from_str(&json).unwrap();
        assert_eq!(back, src);
        let block = DataBlock::Document {
            name: Some("doc.pdf".into()),
            media_type: "application/pdf".into(),
            data: src,
        };
        assert!(block.is_persistable());
    }
}
