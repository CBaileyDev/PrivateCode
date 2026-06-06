pub mod event;
pub mod message;

pub use event::{DeltaPayload, ProtocolEvent, UsageStats};
pub use message::{ChatMessage, ContentBlock, Role, ToolResultContent};

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_message_serde() {
        let msg = ChatMessage {
            id: "msg-123".to_string(),
            role: Role::Assistant,
            content: vec![
                ContentBlock::Text {
                    text: "Hello, world!".to_string(),
                },
                ContentBlock::Reasoning {
                    reasoning: "Thinking about greeting...".to_string(),
                },
                ContentBlock::ToolUse {
                    id: "call-1".to_string(),
                    name: "read_file".to_string(),
                    input: json!({"path": "src/main.rs"}),
                },
            ],
            created_at: 1625097600,
        };

        let serialized = serde_json::to_string(&msg).unwrap();
        let deserialized: ChatMessage = serde_json::from_str(&serialized).unwrap();
        assert_eq!(msg, deserialized);
    }

    #[test]
    fn test_event_serde() {
        let event = ProtocolEvent::ToolPermissionRequired {
            session_id: "sess-1".to_string(),
            seq: 42,
            permission_id: "perm-x".to_string(),
            tool_name: "bash".to_string(),
            action: "bash".to_string(),
            resources: vec!["cargo build".to_string()],
            preview: "Run cargo build in terminal".to_string(),
        };

        let serialized = serde_json::to_string(&event).unwrap();
        let deserialized: ProtocolEvent = serde_json::from_str(&serialized).unwrap();
        assert_eq!(event, deserialized);
    }
}
