use crate::message::ToolResultContent;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct UsageStats {
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
    pub reasoning_tokens: i64,
    pub cost: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DeltaPayload {
    Text {
        text: String,
    },
    Reasoning {
        reasoning: String,
    },
    ToolUse {
        id: String,
        name: String,
        input_delta: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ProtocolEvent {
    SessionCreated {
        session_id: String,
    },
    MessageDelta {
        session_id: String,
        delta: DeltaPayload,
    },
    MessageCompleted {
        session_id: String,
        seq: i64,
        message_id: String,
        usage: UsageStats,
    },
    ToolRequested {
        session_id: String,
        seq: i64,
        tool_call_id: String,
        tool_name: String,
        arguments: serde_json::Value,
    },
    ToolPermissionRequired {
        session_id: String,
        seq: i64,
        permission_id: String,
        tool_name: String,
        action: String,
        resources: Vec<String>,
        preview: String,
    },
    ToolOutput {
        session_id: String,
        seq: i64,
        tool_call_id: String,
        output: ToolResultContent,
        is_error: bool,
    },
    CheckpointCreated {
        session_id: String,
        seq: i64,
        tree_hash: String,
        tool_name: String,
        kind: String, // 'turn_start' | 'pre_step' | 'post_step'
    },
    UsageUpdated {
        session_id: String,
        seq: i64,
        usage: UsageStats,
    },
    Error {
        session_id: String,
        seq: i64,
        code: String,
        message: String,
        retryable: bool,
    },
}
