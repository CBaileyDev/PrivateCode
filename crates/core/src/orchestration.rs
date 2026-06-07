//! Multi-model orchestration (Phase 4, Steps 4.8–4.11): the configuration model,
//! the parallel **fan-out** engine, and the synthesis/single-stream helpers.
//!
//! A fan-out turn dispatches one shared request to N candidate models in
//! parallel, streams each candidate's tokens to the client as **ephemeral**
//! `Candidate*` events (see [`private_code_protocol::event::ProtocolEvent`] — they
//! are excluded from `is_durable_event`, so they never enter the durable history
//! buffer; only the synthesized answer persists), and returns each candidate's
//! assembled text + usage + outcome. [`stream_single`] + [`build_synthesis_messages`]
//! drive the synthesis pass and the role-based final stage. This module stays
//! provider-agnostic and unit-testable with scripted providers (no DB, no network);
//! the live turn integration that wires it in — resolving providers, persisting the
//! one durable answer — lives in `orchestrator::run_orchestrated_turn` (C9).
//!
//! Design invariants (advisor-confirmed):
//! - **Candidate events are ephemeral.** Only the synthesized `MessageCompleted`
//!   is durable; a replay of a multi-model turn shows one answer, not N
//!   transcripts.
//! - **Proceed on partial failure.** A candidate that errors or times out does
//!   not abort the turn; its [`CandidateOutcome::error`] is `Some` and the turn
//!   proceeds with the survivors. The return type lets the caller cleanly detect
//!   "0 survivors" (every outcome has `error: Some`) and emit a durable `Error`
//!   rather than synthesizing from empty text.
//! - **Cancellation is honored.** Every candidate races the turn's
//!   `CancellationToken`; an abort drops all in-flight streams promptly.

use private_code_protocol::event::{DeltaPayload, ProtocolEvent, UsageStats};
use private_code_protocol::message::ChatMessage;
use private_code_providers::provider::{ModelProvider, ProviderEvent};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use futures_util::future::join_all;
use futures_util::stream::StreamExt;

/// Default per-candidate wall-clock budget. A slow or wedged candidate can't
/// stall the whole fan-out past this; it's recorded as a `timeout` outcome and
/// the turn proceeds with the others.
pub const DEFAULT_CANDIDATE_TIMEOUT: Duration = Duration::from_secs(120);

/// How a turn dispatches to models.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "snake_case")]
pub enum OrchestrationMode {
    /// One model — the ordinary single-model turn (no fan-out). The default.
    #[default]
    Single,
    /// N candidates in parallel, then a synthesizer folds them into one answer.
    /// Accepts the documented kebab-case spelling `"fan-out"` as an alias for the
    /// canonical snake_case `"fan_out"` (the config schema in the docs uses the
    /// hyphenated form).
    #[serde(alias = "fan-out")]
    FanOut,
    /// Candidates selected by role (planner / coder / reviewer …). Accepts the
    /// kebab-case `"role-based"` as an alias for `"role_based"`.
    #[serde(alias = "role-based")]
    RoleBased,
}

/// Declarative multi-model config, persisted per session (alongside
/// `model_config`). Model references are `"provider/model"` strings resolved
/// against the coordinator's registered providers at turn time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, Default)]
#[serde(deny_unknown_fields)]
pub struct OrchestrationConfig {
    #[serde(default)]
    pub mode: OrchestrationMode,
    /// Candidate model refs (`"provider/model"`); fan-out dispatches all of them
    /// in parallel.
    #[serde(default)]
    pub candidates: Vec<String>,
    /// The model ref that synthesizes the candidates' outputs into the final
    /// answer. When unset, C9 falls back to the first candidate.
    #[serde(default)]
    pub synthesizer: Option<String>,
    /// Role → model ref, for [`OrchestrationMode::RoleBased`]. `BTreeMap` so
    /// serialization is deterministic.
    #[serde(default)]
    pub roles: BTreeMap<String, String>,
    /// Explicit execution order for [`OrchestrationMode::RoleBased`]. Each entry
    /// must be a key in `roles`. When empty, the order falls back to `roles`'
    /// keys (alphabetical, via `BTreeMap`) — relying on that accidental order is
    /// a latent bug, so set this explicitly for any real pipeline
    /// (e.g. `["architect", "implementer", "reviewer"]`).
    #[serde(default)]
    pub role_order: Vec<String>,
    /// Per-candidate timeout in seconds; `0` means [`DEFAULT_CANDIDATE_TIMEOUT`].
    #[serde(default)]
    pub candidate_timeout_secs: u64,
}

impl OrchestrationConfig {
    /// The resolved per-candidate timeout (`candidate_timeout_secs`, or the
    /// default when zero).
    pub fn candidate_timeout(&self) -> Duration {
        if self.candidate_timeout_secs == 0 {
            DEFAULT_CANDIDATE_TIMEOUT
        } else {
            Duration::from_secs(self.candidate_timeout_secs)
        }
    }

    /// Reject a config that the engine could not act on, BEFORE a turn starts —
    /// per mode. Every referenced model must parse as `provider/model`.
    pub fn validate(&self) -> Result<(), OrchestrationError> {
        match self.mode {
            // Single mode ignores the orchestration fields entirely; nothing to
            // validate (the session's normal model_config drives the turn).
            OrchestrationMode::Single => {}
            OrchestrationMode::FanOut => {
                if self.candidates.len() < 2 {
                    return Err(OrchestrationError::TooFewCandidates(self.candidates.len()));
                }
                for c in &self.candidates {
                    ModelRef::parse(c)?;
                }
                if let Some(s) = &self.synthesizer {
                    ModelRef::parse(s)?;
                }
            }
            OrchestrationMode::RoleBased => {
                if self.roles.is_empty() {
                    return Err(OrchestrationError::NoRoles);
                }
                for v in self.roles.values() {
                    ModelRef::parse(v)?;
                }
                for r in &self.role_order {
                    if !self.roles.contains_key(r) {
                        return Err(OrchestrationError::UnknownRole(r.clone()));
                    }
                }
                // A non-empty `role_order` must list EVERY role — otherwise
                // `ordered_roles` would silently drop the omitted ones, running a
                // shorter pipeline than configured with no diagnostic. (Validated
                // both directions: role_order ⊆ roles above, roles ⊆ role_order
                // here ⇒ a permutation.)
                if !self.role_order.is_empty() {
                    for k in self.roles.keys() {
                        if !self.role_order.contains(k) {
                            return Err(OrchestrationError::RoleOrderIncomplete(k.clone()));
                        }
                    }
                }
                if let Some(s) = &self.synthesizer {
                    ModelRef::parse(s)?;
                }
            }
        }
        Ok(())
    }

    /// The role execution order: `role_order` when set, else `roles`' keys in
    /// `BTreeMap` (alphabetical) order. Pairs each role with its model ref string.
    pub fn ordered_roles(&self) -> Vec<(String, String)> {
        let order: Vec<String> = if self.role_order.is_empty() {
            self.roles.keys().cloned().collect()
        } else {
            self.role_order.clone()
        };
        order
            .into_iter()
            .filter_map(|r| self.roles.get(&r).map(|m| (r, m.clone())))
            .collect()
    }
}

/// A parsed `"provider/model"` reference. The provider id is the FIRST path
/// segment; the model id is the remainder (model ids may themselves contain `/`,
/// e.g. `nvidia/meta/llama-3.3-70b-instruct` → provider `nvidia`, model
/// `meta/llama-3.3-70b-instruct`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelRef {
    pub provider_id: String,
    pub model_id: String,
}

impl ModelRef {
    pub fn parse(s: &str) -> Result<Self, OrchestrationError> {
        match s.split_once('/') {
            Some((p, m)) if !p.is_empty() && !m.is_empty() => Ok(Self {
                provider_id: p.to_string(),
                model_id: m.to_string(),
            }),
            _ => Err(OrchestrationError::BadModelRef(s.to_string())),
        }
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum OrchestrationError {
    #[error("model reference '{0}' must be in 'provider/model' form")]
    BadModelRef(String),
    #[error("fan-out mode requires at least 2 candidates (got {0})")]
    TooFewCandidates(usize),
    #[error("role-based mode requires at least one role mapping")]
    NoRoles,
    #[error("role_order references role '{0}' that is not in roles")]
    UnknownRole(String),
    #[error("role_order is set but omits role '{0}' (it must list every role)")]
    RoleOrderIncomplete(String),
}

/// A fan-out candidate with its provider already resolved. The caller (the
/// coordinator/orchestrator, which owns the providers map) resolves each
/// `"provider/model"` ref to a concrete provider before calling [`fan_out`],
/// keeping this engine free of any provider registry.
pub struct Candidate {
    pub index: u32,
    pub provider: Arc<dyn ModelProvider>,
    pub model_id: String,
}

/// The shared request fanned out to every candidate (borrowed for the duration
/// of the call).
pub struct FanOutRequest<'a> {
    pub session_id: &'a str,
    pub system_prompt: Option<&'a str>,
    pub max_tokens: u32,
    pub messages: &'a [ChatMessage],
    pub tools: &'a [serde_json::Value],
}

/// One candidate's result. `text` is the assembled answer (the `TextDelta`
/// stream; reasoning is streamed to the UI but not folded into `text`, since the
/// synthesizer consumes the answer, not the thinking). `error` is `Some` when the
/// candidate failed or timed out — in which case `text` is whatever streamed
/// before the failure and should not be synthesized from.
#[derive(Debug, Clone)]
pub struct CandidateOutcome {
    pub index: u32,
    pub model_id: String,
    pub text: String,
    pub usage: UsageStats,
    pub finish_reason: Option<String>,
    pub error: Option<String>,
}

impl CandidateOutcome {
    /// A candidate that produced a usable answer (no error).
    pub fn succeeded(&self) -> bool {
        self.error.is_none()
    }
}

/// Sum the usage across all candidate outcomes (failed candidates report
/// whatever they spent, typically zero). C9 adds the synthesizer's usage on top
/// for the turn total.
pub fn sum_candidate_usage(outcomes: &[CandidateOutcome]) -> UsageStats {
    let mut total = UsageStats::default();
    for o in outcomes {
        total.input_tokens += o.usage.input_tokens;
        total.output_tokens += o.usage.output_tokens;
        total.cache_read_tokens += o.usage.cache_read_tokens;
        total.cache_write_tokens += o.usage.cache_write_tokens;
        total.reasoning_tokens += o.usage.reasoning_tokens;
        total.cost += o.usage.cost;
    }
    total
}

/// Dispatch `req` to every candidate in parallel, streaming each candidate's
/// tokens as ephemeral `Candidate*` events on `event_tx`, and return one
/// [`CandidateOutcome`] per candidate (in `candidates` order, regardless of which
/// finished first).
///
/// - **Parallel:** candidates are polled concurrently via `join_all`; the
///   wall-clock cost is the slowest candidate, not the sum.
/// - **Proceed on partial failure:** a candidate that errors or exceeds
///   `per_candidate_timeout` yields an outcome with `error: Some(..)`; the others
///   are unaffected.
/// - **Cancellable:** each candidate races `cancel`; a fired token makes every
///   in-flight candidate return a `cancelled` outcome promptly.
pub async fn fan_out(
    candidates: &[Candidate],
    req: &FanOutRequest<'_>,
    event_tx: &mpsc::Sender<ProtocolEvent>,
    cancel: &CancellationToken,
    per_candidate_timeout: Duration,
) -> Vec<CandidateOutcome> {
    let futures = candidates
        .iter()
        .map(|c| run_candidate(c, req, event_tx, cancel, per_candidate_timeout));
    join_all(futures).await
}

/// Drive one candidate's stream to completion (or failure/cancellation),
/// emitting its ephemeral events and assembling its outcome.
async fn run_candidate(
    candidate: &Candidate,
    req: &FanOutRequest<'_>,
    event_tx: &mpsc::Sender<ProtocolEvent>,
    cancel: &CancellationToken,
    per_candidate_timeout: Duration,
) -> CandidateOutcome {
    let index = candidate.index;
    let model_id = candidate.model_id.clone();

    let _ = event_tx
        .send(ProtocolEvent::CandidateStarted {
            session_id: req.session_id.to_string(),
            candidate_index: index,
            model_id: model_id.clone(),
        })
        .await;

    let mut text = String::new();
    let mut usage = UsageStats::default();
    let mut finish_reason: Option<String> = None;

    // The streaming future borrows the accumulators; it is dropped when the
    // select resolves (so the borrows end before we read the accumulators back).
    let stream_fut = async {
        let mut stream = candidate
            .provider
            .stream_chat(
                &model_id,
                req.system_prompt,
                req.max_tokens,
                req.messages,
                req.tools,
            )
            .await
            .map_err(|e| e.to_string())?;

        while let Some(ev) = stream.next().await {
            match ev {
                Ok(ProviderEvent::TextDelta(t)) => {
                    text.push_str(&t);
                    let _ = event_tx
                        .send(ProtocolEvent::CandidateDelta {
                            session_id: req.session_id.to_string(),
                            candidate_index: index,
                            delta: DeltaPayload::Text { text: t },
                        })
                        .await;
                }
                Ok(ProviderEvent::ReasoningDelta(r)) => {
                    // Streamed to the UI for fidelity, but NOT folded into `text`
                    // (the synthesizer consumes the answer, not the thinking).
                    let _ = event_tx
                        .send(ProtocolEvent::CandidateDelta {
                            session_id: req.session_id.to_string(),
                            candidate_index: index,
                            delta: DeltaPayload::Reasoning { reasoning: r },
                        })
                        .await;
                }
                Ok(ProviderEvent::ToolUseComplete { id, name, input }) => {
                    // Forward the COMPLETED tool call (carries id+name+full input);
                    // partial start/delta frames are skipped to avoid emitting a
                    // ToolUse delta with a not-yet-known name.
                    let _ = event_tx
                        .send(ProtocolEvent::CandidateDelta {
                            session_id: req.session_id.to_string(),
                            candidate_index: index,
                            delta: DeltaPayload::ToolUse {
                                id,
                                name,
                                input_delta: input.to_string(),
                            },
                        })
                        .await;
                }
                Ok(ProviderEvent::MessageStop {
                    usage: u,
                    finish_reason: fr,
                }) => {
                    usage = u;
                    finish_reason = fr;
                    break;
                }
                // ToolUseStart / ToolUseDelta / ReasoningSignatureDelta: not
                // surfaced as candidate deltas (see ToolUseComplete above).
                Ok(_) => {}
                Err(e) => return Err(e.to_string()),
            }
        }
        Ok::<(), String>(())
    };

    let result: Result<(), String> = tokio::select! {
        biased;
        _ = cancel.cancelled() => Err("cancelled".to_string()),
        r = tokio::time::timeout(per_candidate_timeout, stream_fut) => match r {
            Ok(inner) => inner,
            Err(_elapsed) => Err("timeout".to_string()),
        }
    };

    let error = result.err();

    let _ = event_tx
        .send(ProtocolEvent::CandidateCompleted {
            session_id: req.session_id.to_string(),
            candidate_index: index,
            usage: usage.clone(),
            finish_reason: finish_reason.clone(),
            error: error.clone(),
        })
        .await;

    CandidateOutcome {
        index,
        model_id,
        text,
        usage,
        finish_reason,
        error,
    }
}

/// The default synthesis instruction. The synthesizer receives the user's request
/// plus every surviving candidate's answer and must produce one best answer —
/// merging strengths, correcting errors, resolving disagreements — WITHOUT
/// revealing that multiple candidates existed (the user sees a single answer).
pub const DEFAULT_SYNTHESIS_SYSTEM_PROMPT: &str = "\
You are a synthesis engine combining multiple AI models' answers to one request. \
You are given the user's request followed by several candidate answers. Produce a \
single, best response: merge the strongest ideas, correct any errors, resolve \
disagreements in favor of correctness, and fill gaps. Do not mention that there \
were multiple candidates, do not refer to them by number, and do not describe \
your process — output only the final answer as if you authored it directly.";

/// Build the one-user-message request for the synthesizer: the original request
/// followed by each surviving candidate's labeled answer. `candidates` is
/// `(label, text)` pairs (the label is the candidate's model id or role name).
pub fn build_synthesis_messages(
    user_request: &str,
    candidates: &[(String, String)],
) -> Vec<ChatMessage> {
    use private_code_protocol::message::{ContentBlock, Role};
    let mut body = format!("User request:\n{user_request}\n\n");
    for (i, (label, text)) in candidates.iter().enumerate() {
        body.push_str(&format!(
            "--- Candidate {} ({}) ---\n{}\n\n",
            i + 1,
            label,
            text
        ));
    }
    body.push_str("Synthesize these into a single best answer.");
    vec![ChatMessage {
        id: "synthesis-input".to_string(),
        role: Role::User,
        content: vec![ContentBlock::Text { text: body }],
        created_at: 0,
    }]
}

/// Build a role stage's request: the base conversation messages plus, when prior
/// stages have run, an appended user message carrying their outputs and this
/// role's instruction. The first stage gets the base messages unchanged.
pub fn build_role_stage_messages(
    base: &[ChatMessage],
    role: &str,
    prior: &[(String, String)],
) -> Vec<ChatMessage> {
    use private_code_protocol::message::{ContentBlock, Role};
    let mut msgs = base.to_vec();
    if !prior.is_empty() {
        let mut body = String::from("Prior pipeline stages produced:\n\n");
        for (stage_role, text) in prior {
            body.push_str(&format!("--- {stage_role} ---\n{text}\n\n"));
        }
        body.push_str(&format!(
            "You are the '{role}' stage. Build on the prior stages' work to perform your role on the user's request."
        ));
        msgs.push(ChatMessage {
            id: format!("role-stage-{role}"),
            role: Role::User,
            content: vec![ContentBlock::Text { text: body }],
            created_at: 0,
        });
    }
    msgs
}

/// Stream ONE model's response as ordinary `MessageDelta` events (Text +
/// Reasoning), accumulating the answer text and final usage. Used for the
/// synthesis pass and the final role stage — i.e. the streams the user sees as
/// "the assistant's answer". Races `cancel` and `timeout`; on either, returns
/// `Err` and the caller persists nothing (a partial synthesis is not a real
/// answer). No tools are offered (a documented fan-out/synthesis ceiling).
#[allow(clippy::too_many_arguments)]
pub async fn stream_single(
    provider: &Arc<dyn ModelProvider>,
    model_id: &str,
    session_id: &str,
    system_prompt: Option<&str>,
    max_tokens: u32,
    messages: &[ChatMessage],
    event_tx: &mpsc::Sender<ProtocolEvent>,
    cancel: &CancellationToken,
    timeout: Duration,
) -> Result<(String, UsageStats, Option<String>), String> {
    let mut text = String::new();
    let mut usage = UsageStats::default();
    let mut finish_reason: Option<String> = None;

    let stream_fut = async {
        let mut stream = provider
            .stream_chat(model_id, system_prompt, max_tokens, messages, &[])
            .await
            .map_err(|e| e.to_string())?;
        while let Some(ev) = stream.next().await {
            match ev {
                Ok(ProviderEvent::TextDelta(t)) => {
                    text.push_str(&t);
                    let _ = event_tx
                        .send(ProtocolEvent::MessageDelta {
                            session_id: session_id.to_string(),
                            delta: DeltaPayload::Text { text: t },
                        })
                        .await;
                }
                Ok(ProviderEvent::ReasoningDelta(r)) => {
                    let _ = event_tx
                        .send(ProtocolEvent::MessageDelta {
                            session_id: session_id.to_string(),
                            delta: DeltaPayload::Reasoning { reasoning: r },
                        })
                        .await;
                }
                Ok(ProviderEvent::MessageStop {
                    usage: u,
                    finish_reason: fr,
                }) => {
                    usage = u;
                    finish_reason = fr;
                    break;
                }
                Ok(_) => {}
                Err(e) => return Err(e.to_string()),
            }
        }
        Ok::<(), String>(())
    };

    let result: Result<(), String> = tokio::select! {
        biased;
        _ = cancel.cancelled() => Err("cancelled".to_string()),
        r = tokio::time::timeout(timeout, stream_fut) => match r {
            Ok(inner) => inner,
            Err(_elapsed) => Err("timeout".to_string()),
        }
    };
    result?;
    Ok((text, usage, finish_reason))
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use futures_util::stream::BoxStream;
    use private_code_providers::provider::ProviderError;
    use private_code_providers::testkit::{PendingProvider, ScriptedProvider};

    fn user_msg(text: &str) -> Vec<ChatMessage> {
        use private_code_protocol::message::{ContentBlock, Role};
        vec![ChatMessage {
            id: "u1".into(),
            role: Role::User,
            content: vec![ContentBlock::Text { text: text.into() }],
            created_at: 0,
        }]
    }

    fn req<'a>(session_id: &'a str, messages: &'a [ChatMessage]) -> FanOutRequest<'a> {
        FanOutRequest {
            session_id,
            system_prompt: None,
            max_tokens: 256,
            messages,
            tools: &[],
        }
    }

    /// Drain everything currently queued on a receiver without blocking.
    fn drain(rx: &mut mpsc::Receiver<ProtocolEvent>) -> Vec<ProtocolEvent> {
        let mut out = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            out.push(ev);
        }
        out
    }

    #[test]
    fn model_ref_parses_provider_and_model_taking_first_slash() {
        let r = ModelRef::parse("anthropic/claude-opus-4-8").unwrap();
        assert_eq!(r.provider_id, "anthropic");
        assert_eq!(r.model_id, "claude-opus-4-8");

        // Model ids may contain '/': only the FIRST segment is the provider.
        let r = ModelRef::parse("nvidia/meta/llama-3.3-70b-instruct").unwrap();
        assert_eq!(r.provider_id, "nvidia");
        assert_eq!(r.model_id, "meta/llama-3.3-70b-instruct");

        for bad in ["noprovider", "/model", "provider/", ""] {
            assert!(ModelRef::parse(bad).is_err(), "'{bad}' must be rejected");
        }
    }

    #[test]
    fn config_validate_enforces_per_mode_invariants() {
        // Single: anything goes (the fields are ignored).
        assert!(OrchestrationConfig::default().validate().is_ok());

        // FanOut needs >= 2 well-formed candidates.
        let too_few = OrchestrationConfig {
            mode: OrchestrationMode::FanOut,
            candidates: vec!["anthropic/claude-opus-4-8".into()],
            ..Default::default()
        };
        assert_eq!(
            too_few.validate(),
            Err(OrchestrationError::TooFewCandidates(1))
        );

        let bad_ref = OrchestrationConfig {
            mode: OrchestrationMode::FanOut,
            candidates: vec!["anthropic/claude-opus-4-8".into(), "garbage".into()],
            ..Default::default()
        };
        assert_eq!(
            bad_ref.validate(),
            Err(OrchestrationError::BadModelRef("garbage".into()))
        );

        let ok = OrchestrationConfig {
            mode: OrchestrationMode::FanOut,
            candidates: vec![
                "anthropic/claude-opus-4-8".into(),
                "nvidia/meta/llama-3.3-70b-instruct".into(),
            ],
            synthesizer: Some("anthropic/claude-opus-4-8".into()),
            ..Default::default()
        };
        assert!(ok.validate().is_ok());

        // RoleBased needs at least one (well-formed) role mapping.
        let no_roles = OrchestrationConfig {
            mode: OrchestrationMode::RoleBased,
            ..Default::default()
        };
        assert_eq!(no_roles.validate(), Err(OrchestrationError::NoRoles));
    }

    /// Serde: the documented kebab-case mode works via alias; the canonical
    /// snake_case works; an unknown variant is a HARD error (not silently
    /// defaulted); and a typo'd field name is rejected (deny_unknown_fields) so it
    /// can't silently no-op. These guard the C11 "malformed config silently
    /// degrades to single-model" finding at the deserialization layer.
    #[test]
    fn mode_accepts_kebab_alias_and_rejects_unknown_variant_or_field() {
        let kebab: OrchestrationConfig =
            serde_json::from_str(r#"{"mode":"fan-out","candidates":["a/b","c/d"]}"#).unwrap();
        assert_eq!(kebab.mode, OrchestrationMode::FanOut);

        let snake: OrchestrationConfig = serde_json::from_str(r#"{"mode":"role_based"}"#).unwrap();
        assert_eq!(snake.mode, OrchestrationMode::RoleBased);
        let kebab2: OrchestrationConfig = serde_json::from_str(r#"{"mode":"role-based"}"#).unwrap();
        assert_eq!(kebab2.mode, OrchestrationMode::RoleBased);

        // Unknown variant string → deserialization error (caller surfaces it).
        assert!(serde_json::from_str::<OrchestrationConfig>(r#"{"mode":"fanout"}"#).is_err());
        // Typo'd field name → rejected, not silently ignored.
        assert!(
            serde_json::from_str::<OrchestrationConfig>(
                r#"{"mode":"fan_out","candidates":["a/b","c/d"],"synthezizer":"a/b"}"#
            )
            .is_err()
        );
    }

    /// A non-empty `role_order` must list EVERY role; omitting one is rejected so a
    /// stage can't be silently dropped from the pipeline.
    #[test]
    fn role_order_must_list_every_role() {
        let mut roles = BTreeMap::new();
        roles.insert("architect".to_string(), "p/a".to_string());
        roles.insert("implementer".to_string(), "p/i".to_string());
        let cfg = OrchestrationConfig {
            mode: OrchestrationMode::RoleBased,
            roles,
            role_order: vec!["architect".into()], // omits implementer
            ..Default::default()
        };
        assert_eq!(
            cfg.validate(),
            Err(OrchestrationError::RoleOrderIncomplete(
                "implementer".into()
            ))
        );
    }

    #[test]
    fn candidate_timeout_falls_back_to_default_when_zero() {
        assert_eq!(
            OrchestrationConfig::default().candidate_timeout(),
            DEFAULT_CANDIDATE_TIMEOUT
        );
        let cfg = OrchestrationConfig {
            candidate_timeout_secs: 5,
            ..Default::default()
        };
        assert_eq!(cfg.candidate_timeout(), Duration::from_secs(5));
    }

    /// A provider that waits on a shared barrier BEFORE streaming, then returns a
    /// fixed text. With a barrier sized to the candidate count, every candidate
    /// must be concurrently in-flight for ANY to proceed — a sequential fan-out
    /// would deadlock at the barrier (the test wraps `fan_out` in a timeout so a
    /// regression fails as a timeout, not a hang).
    struct BarrierProvider {
        barrier: Arc<tokio::sync::Barrier>,
        text: String,
    }

    #[async_trait]
    impl ModelProvider for BarrierProvider {
        async fn stream_chat(
            &self,
            _model_id: &str,
            _system_prompt: Option<&str>,
            _max_tokens: u32,
            _messages: &[ChatMessage],
            _tools: &[serde_json::Value],
        ) -> Result<BoxStream<'static, Result<ProviderEvent, ProviderError>>, ProviderError>
        {
            self.barrier.wait().await;
            let evs = vec![
                Ok(ProviderEvent::TextDelta(self.text.clone())),
                Ok(ProviderEvent::MessageStop {
                    usage: UsageStats::default(),
                    finish_reason: Some("end_turn".into()),
                }),
            ];
            Ok(futures_util::stream::iter(evs).boxed())
        }
        fn count_tokens(&self, _m: &str, t: &str) -> usize {
            t.len() / 4
        }
    }

    /// fan_out runs all candidates CONCURRENTLY (proven by the shared barrier),
    /// collects one outcome each in input order, and emits the full ephemeral
    /// event triplet (Started/Delta/Completed) per candidate.
    #[tokio::test]
    async fn fan_out_runs_all_candidates_concurrently_and_collects_outcomes() {
        let barrier = Arc::new(tokio::sync::Barrier::new(3));
        let candidates: Vec<Candidate> = (0..3)
            .map(|i| Candidate {
                index: i,
                provider: Arc::new(BarrierProvider {
                    barrier: barrier.clone(),
                    text: format!("answer-{i}"),
                }),
                model_id: format!("p{i}/m{i}"),
            })
            .collect();

        let (tx, mut rx) = mpsc::channel(256);
        let cancel = CancellationToken::new();
        let msgs = user_msg("hi");
        let r = req("sess", &msgs);

        // A sequential fan-out would deadlock at the 3-way barrier → timeout fail.
        let outcomes = tokio::time::timeout(
            Duration::from_secs(5),
            fan_out(&candidates, &r, &tx, &cancel, Duration::from_secs(5)),
        )
        .await
        .expect("fan_out must run candidates concurrently (barrier would deadlock if sequential)");

        assert_eq!(outcomes.len(), 3);
        for (i, o) in outcomes.iter().enumerate() {
            assert_eq!(o.index, i as u32, "outcomes preserve input order");
            assert!(o.succeeded(), "candidate {i} should succeed");
            assert_eq!(o.text, format!("answer-{i}"));
            assert_eq!(o.finish_reason.as_deref(), Some("end_turn"));
        }

        // Each candidate emitted Started + at least one Delta + Completed.
        let events = drain(&mut rx);
        for i in 0..3u32 {
            assert!(
                events.iter().any(
                    |e| matches!(e, ProtocolEvent::CandidateStarted { candidate_index, .. } if *candidate_index == i)
                ),
                "CandidateStarted for {i}"
            );
            assert!(
                events.iter().any(
                    |e| matches!(e, ProtocolEvent::CandidateDelta { candidate_index, .. } if *candidate_index == i)
                ),
                "CandidateDelta for {i}"
            );
            assert!(
                events.iter().any(
                    |e| matches!(e, ProtocolEvent::CandidateCompleted { candidate_index, error, .. } if *candidate_index == i && error.is_none())
                ),
                "CandidateCompleted (ok) for {i}"
            );
        }
    }

    /// A provider whose `stream_chat` errors immediately, to drive the
    /// proceed-on-partial-failure path.
    struct FailingProvider;

    #[async_trait]
    impl ModelProvider for FailingProvider {
        async fn stream_chat(
            &self,
            _model_id: &str,
            _system_prompt: Option<&str>,
            _max_tokens: u32,
            _messages: &[ChatMessage],
            _tools: &[serde_json::Value],
        ) -> Result<BoxStream<'static, Result<ProviderEvent, ProviderError>>, ProviderError>
        {
            Err(ProviderError::Other("boom".into()))
        }
        fn count_tokens(&self, _m: &str, _t: &str) -> usize {
            1
        }
    }

    /// One failing candidate does NOT abort the fan-out: it yields an `error`
    /// outcome (and a CandidateCompleted carrying the error) while the survivor
    /// completes normally.
    #[tokio::test]
    async fn fan_out_proceeds_on_partial_candidate_failure() {
        let candidates = vec![
            Candidate {
                index: 0,
                provider: Arc::new(FailingProvider),
                model_id: "bad/model".into(),
            },
            Candidate {
                index: 1,
                provider: Arc::new(ScriptedProvider::one_shot_text("good answer")),
                model_id: "good/model".into(),
            },
        ];

        let (tx, mut rx) = mpsc::channel(64);
        let cancel = CancellationToken::new();
        let msgs = user_msg("hi");
        let r = req("sess", &msgs);

        let outcomes = fan_out(&candidates, &r, &tx, &cancel, Duration::from_secs(5)).await;

        assert_eq!(outcomes.len(), 2);
        assert!(!outcomes[0].succeeded(), "the failing candidate is errored");
        assert!(outcomes[0].error.as_deref().unwrap().contains("boom"));
        assert!(outcomes[1].succeeded(), "the survivor proceeds");
        assert_eq!(outcomes[1].text, "good answer");

        // The caller can cleanly tell there IS at least one survivor.
        assert!(
            outcomes.iter().any(|o| o.succeeded()),
            "0-survivor detection: at least one survivor here"
        );

        // The failing candidate still emitted a Completed-with-error.
        let events = drain(&mut rx);
        assert!(events.iter().any(|e| matches!(
            e,
            ProtocolEvent::CandidateCompleted {
                candidate_index: 0,
                error: Some(_),
                ..
            }
        )));
    }

    /// Every candidate failing is surfaceable: all outcomes carry `error: Some`,
    /// so C9 can emit a durable `Error` instead of synthesizing garbage.
    #[tokio::test]
    async fn fan_out_all_failed_is_detectable() {
        let candidates = vec![
            Candidate {
                index: 0,
                provider: Arc::new(FailingProvider),
                model_id: "bad/a".into(),
            },
            Candidate {
                index: 1,
                provider: Arc::new(FailingProvider),
                model_id: "bad/b".into(),
            },
        ];
        let (tx, _rx) = mpsc::channel(64);
        let cancel = CancellationToken::new();
        let msgs = user_msg("hi");
        let r = req("sess", &msgs);

        let outcomes = fan_out(&candidates, &r, &tx, &cancel, Duration::from_secs(5)).await;
        assert!(
            outcomes.iter().all(|o| !o.succeeded()),
            "all candidates failed → caller emits a durable Error, not a synthesis"
        );
    }

    /// A pre-cancelled token makes every candidate return a `cancelled` outcome
    /// promptly, even with providers whose streams never resolve.
    #[tokio::test]
    async fn fan_out_honors_cancellation_token() {
        let candidates = vec![
            Candidate {
                index: 0,
                provider: Arc::new(PendingProvider),
                model_id: "p/0".into(),
            },
            Candidate {
                index: 1,
                provider: Arc::new(PendingProvider),
                model_id: "p/1".into(),
            },
        ];
        let (tx, _rx) = mpsc::channel(64);
        let cancel = CancellationToken::new();
        cancel.cancel(); // already cancelled before fan-out starts
        let msgs = user_msg("hi");
        let r = req("sess", &msgs);

        // A long per-candidate timeout proves it's the CANCEL (not the timeout)
        // that ends these never-resolving streams.
        let outcomes = tokio::time::timeout(
            Duration::from_secs(5),
            fan_out(&candidates, &r, &tx, &cancel, Duration::from_secs(3600)),
        )
        .await
        .expect("cancellation must end the fan-out promptly");

        assert_eq!(outcomes.len(), 2);
        for o in &outcomes {
            assert_eq!(o.error.as_deref(), Some("cancelled"));
        }
    }

    /// A pending provider with a SHORT per-candidate timeout records a `timeout`
    /// outcome (distinct from cancellation) and proceeds.
    #[tokio::test]
    async fn fan_out_times_out_a_wedged_candidate() {
        let candidates = vec![Candidate {
            index: 0,
            provider: Arc::new(PendingProvider),
            model_id: "p/0".into(),
        }];
        let (tx, _rx) = mpsc::channel(64);
        let cancel = CancellationToken::new();
        let msgs = user_msg("hi");
        let r = req("sess", &msgs);

        let outcomes = fan_out(&candidates, &r, &tx, &cancel, Duration::from_millis(50)).await;
        assert_eq!(outcomes[0].error.as_deref(), Some("timeout"));
    }

    #[test]
    fn sum_candidate_usage_adds_all_fields() {
        let mk = |inp, out, cost| CandidateOutcome {
            index: 0,
            model_id: "m".into(),
            text: String::new(),
            usage: UsageStats {
                input_tokens: inp,
                output_tokens: out,
                cost,
                ..Default::default()
            },
            finish_reason: None,
            error: None,
        };
        let total = sum_candidate_usage(&[mk(10, 5, 1.0), mk(20, 7, 2.5)]);
        assert_eq!(total.input_tokens, 30);
        assert_eq!(total.output_tokens, 12);
        assert_eq!(total.cost, 3.5);
    }

    #[test]
    fn ordered_roles_uses_explicit_order_then_falls_back() {
        let mut roles = BTreeMap::new();
        roles.insert("reviewer".to_string(), "p/r".to_string());
        roles.insert("architect".to_string(), "p/a".to_string());
        roles.insert("implementer".to_string(), "p/i".to_string());

        // Explicit order wins.
        let cfg = OrchestrationConfig {
            mode: OrchestrationMode::RoleBased,
            roles: roles.clone(),
            role_order: vec!["architect".into(), "implementer".into(), "reviewer".into()],
            ..Default::default()
        };
        assert!(cfg.validate().is_ok());
        let order: Vec<String> = cfg.ordered_roles().into_iter().map(|(r, _)| r).collect();
        assert_eq!(order, vec!["architect", "implementer", "reviewer"]);

        // No explicit order → BTreeMap (alphabetical) fallback.
        let cfg2 = OrchestrationConfig {
            mode: OrchestrationMode::RoleBased,
            roles,
            ..Default::default()
        };
        let order2: Vec<String> = cfg2.ordered_roles().into_iter().map(|(r, _)| r).collect();
        assert_eq!(order2, vec!["architect", "implementer", "reviewer"]);

        // An unknown role in role_order is rejected by validate.
        let bad = OrchestrationConfig {
            mode: OrchestrationMode::RoleBased,
            roles: cfg.roles.clone(),
            role_order: vec!["ghost".into()],
            ..Default::default()
        };
        assert_eq!(
            bad.validate(),
            Err(OrchestrationError::UnknownRole("ghost".into()))
        );
    }

    #[test]
    fn build_synthesis_messages_includes_request_and_all_candidates() {
        use private_code_protocol::message::{ContentBlock, Role};
        let msgs = build_synthesis_messages(
            "fix the bug",
            &[
                ("modelA".into(), "answer A".into()),
                ("modelB".into(), "answer B".into()),
            ],
        );
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].role, Role::User);
        let ContentBlock::Text { text } = &msgs[0].content[0] else {
            panic!("expected text block");
        };
        assert!(text.contains("fix the bug"));
        assert!(text.contains("answer A") && text.contains("answer B"));
        assert!(text.contains("modelA") && text.contains("modelB"));
    }

    #[test]
    fn build_role_stage_messages_appends_prior_only_after_first_stage() {
        let base = user_msg("do the task");
        // First stage: base unchanged.
        let first = build_role_stage_messages(&base, "architect", &[]);
        assert_eq!(first.len(), base.len());
        // Later stage: prior outputs appended as an extra user message.
        let later = build_role_stage_messages(
            &base,
            "implementer",
            &[("architect".into(), "the design".into())],
        );
        assert_eq!(later.len(), base.len() + 1);
        use private_code_protocol::message::ContentBlock;
        let ContentBlock::Text { text } = &later.last().unwrap().content[0] else {
            panic!("expected text");
        };
        assert!(text.contains("the design") && text.contains("implementer"));
    }

    /// `stream_single` emits ordinary MessageDelta (Text) events and returns the
    /// assembled text + usage + stop reason — the synthesis / final-answer path.
    #[tokio::test]
    async fn stream_single_emits_message_deltas_and_returns_text() {
        let provider: Arc<dyn ModelProvider> =
            Arc::new(ScriptedProvider::one_shot_text("synthesized"));
        let (tx, mut rx) = mpsc::channel(64);
        let cancel = CancellationToken::new();
        let msgs = user_msg("hi");

        let (text, _usage, finish_reason) = stream_single(
            &provider,
            "m",
            "sess",
            None,
            128,
            &msgs,
            &tx,
            &cancel,
            Duration::from_secs(5),
        )
        .await
        .unwrap();
        assert_eq!(text, "synthesized");
        assert_eq!(finish_reason.as_deref(), Some("end_turn"));

        let events = drain(&mut rx);
        assert!(
            events.iter().any(|e| matches!(
                e,
                ProtocolEvent::MessageDelta {
                    delta: DeltaPayload::Text { text },
                    ..
                } if text == "synthesized"
            )),
            "synthesis streams as a normal MessageDelta, not a candidate event"
        );
        // It must NOT emit any candidate events.
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, ProtocolEvent::CandidateDelta { .. })),
            "the final/synthesis stream is the assistant answer, not a candidate"
        );
    }

    #[tokio::test]
    async fn stream_single_honors_cancellation() {
        let provider: Arc<dyn ModelProvider> = Arc::new(PendingProvider);
        let (tx, _rx) = mpsc::channel(8);
        let cancel = CancellationToken::new();
        cancel.cancel();
        let msgs = user_msg("hi");
        let err = stream_single(
            &provider,
            "m",
            "sess",
            None,
            128,
            &msgs,
            &tx,
            &cancel,
            Duration::from_secs(3600),
        )
        .await
        .unwrap_err();
        assert_eq!(err, "cancelled");
    }
}
