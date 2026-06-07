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
        #[serde(default, skip_serializing_if = "Option::is_none")]
        finish_reason: Option<String>,
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
    /// A fan-out candidate (one parallel model in a multi-model turn) started
    /// streaming. EPHEMERAL: this and the two variants below carry no replay
    /// cursor (no `seq`) and MUST be excluded from `is_durable_event` — only the
    /// synthesized final answer persists as a durable `MessageCompleted`. Replays
    /// of a multi-model turn show one answer, not N candidate transcripts.
    CandidateStarted {
        session_id: String,
        candidate_index: u32,
        model_id: String,
    },
    /// A token delta from candidate `candidate_index`. EPHEMERAL (see
    /// [`CandidateStarted`]). Reuses [`DeltaPayload`] so a comparison UI can render
    /// each candidate's stream exactly like the primary one.
    CandidateDelta {
        session_id: String,
        candidate_index: u32,
        delta: DeltaPayload,
    },
    /// Candidate `candidate_index` finished (or failed). EPHEMERAL (see
    /// [`CandidateStarted`]). `usage` is that candidate's own spend (summed into
    /// the turn total alongside the synthesizer); `error` is `Some` when the
    /// candidate failed and the turn proceeded with the survivors.
    CandidateCompleted {
        session_id: String,
        candidate_index: u32,
        usage: UsageStats,
        finish_reason: Option<String>,
        error: Option<String>,
    },
}
