use serde::{Deserialize, Serialize};

/// 图片数据来源。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ImageSource {
    /// Base64 编码的内联图片。
    Base64 {
        /// MIME 类型，如 `"image/png"`。
        media_type: String,
        /// Base64 编码的图片数据。
        data: String,
    },
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
}
