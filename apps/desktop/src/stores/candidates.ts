/**
 * Candidate-comparison store (Phase 4, Step 4.12) — the data layer behind the
 * side-by-side multi-model comparison view.
 *
 * A fan-out / role-based turn streams EPHEMERAL `candidate_*` protocol events
 * (one stream per model). This store assembles them into per-candidate panes the
 * GUI renders side by side. The events are ephemeral by design (only the
 * synthesized answer is durable and lands in the message list via
 * `message_completed`), so this store is the ONLY place a candidate's transcript
 * lives — it is intentionally not reconciled from the DB.
 *
 * Cross-session bleed is guarded upstream in `messages.handleProtocolEvent`
 * (which delegates `candidate_*` here only for the displayed session), so the
 * handlers below are pure and need no session gating.
 */
import { createStore } from "solid-js/store";

export type CandidateStatus = "streaming" | "done" | "error";

export interface CandidatePane {
  index: number;
  modelId: string;
  text: string;
  reasoning: string;
  status: CandidateStatus;
  finishReason: string | null;
  error: string | null;
}

interface CandidateState {
  /** Panes keyed by candidate_index, in index order. */
  panes: CandidatePane[];
  /** True while at least one candidate is still streaming. */
  active: boolean;
  /** Keyboard-navigation selection (←/→); clamped to a valid pane. */
  selected: number;
}

const [candidateStore, setCandidateStore] = createStore<CandidateState>({
  panes: [],
  active: false,
  selected: 0,
});

/** Find a pane's array position by its candidate_index. */
function posOf(index: number): number {
  return candidateStore.panes.findIndex((p) => p.index === index);
}

/**
 * Apply a `candidate_*` protocol event. Returns silently for unrelated types so
 * the caller can funnel every event through without pre-filtering.
 */
function handleCandidateEvent(event: any) {
  switch (event.type) {
    case "candidate_started": {
      // candidate_index 0 begins a fresh comparison: a new fan-out turn replaces
      // the previous turn's panes rather than appending to them.
      if (event.candidate_index === 0) {
        setCandidateStore({ panes: [], active: true, selected: 0 });
      }
      const pane: CandidatePane = {
        index: event.candidate_index,
        modelId: event.model_id ?? "",
        text: "",
        reasoning: "",
        status: "streaming",
        finishReason: null,
        error: null,
      };
      const at = posOf(event.candidate_index);
      if (at >= 0) {
        setCandidateStore("panes", at, pane); // re-started: reset it
      } else {
        // Insert keeping panes sorted by candidate_index.
        const next = [...candidateStore.panes, pane].sort((a, b) => a.index - b.index);
        setCandidateStore("panes", next);
      }
      setCandidateStore("active", true);
      break;
    }

    case "candidate_delta": {
      const at = posOf(event.candidate_index);
      if (at < 0) return; // a delta with no started pane: ignore defensively
      const delta = event.delta;
      if (delta?.kind === "text") {
        setCandidateStore("panes", at, "text", (prev) => prev + delta.text);
      } else if (delta?.kind === "reasoning") {
        setCandidateStore("panes", at, "reasoning", (prev) => prev + delta.reasoning);
      }
      // tool_use deltas are not surfaced in the comparison view (candidates are
      // answer generators; see the orchestration fan-out ceiling).
      break;
    }

    case "candidate_completed": {
      const at = posOf(event.candidate_index);
      if (at < 0) return;
      setCandidateStore("panes", at, "status", event.error ? "error" : "done");
      setCandidateStore("panes", at, "finishReason", event.finish_reason ?? null);
      setCandidateStore("panes", at, "error", event.error ?? null);
      // The fan-out is no longer active once every pane has settled.
      const anyStreaming = candidateStore.panes.some((p) => p.status === "streaming");
      setCandidateStore("active", anyStreaming);
      break;
    }
  }
}

/** Move the keyboard selection (←/→). Clamped to the available panes. */
function selectCandidate(index: number) {
  const max = candidateStore.panes.length - 1;
  if (max < 0) return;
  const clamped = Math.max(0, Math.min(index, max));
  setCandidateStore("selected", clamped);
}

function selectNext() {
  selectCandidate(candidateStore.selected + 1);
}
function selectPrev() {
  selectCandidate(candidateStore.selected - 1);
}

/** The currently-selected pane, or null when there are none. */
function selectedPane(): CandidatePane | null {
  return candidateStore.panes[candidateStore.selected] ?? null;
}

/** Clear the comparison (e.g. when switching sessions). */
function clearCandidates() {
  setCandidateStore({ panes: [], active: false, selected: 0 });
}

export {
  candidateStore,
  handleCandidateEvent,
  selectCandidate,
  selectNext,
  selectPrev,
  selectedPane,
  clearCandidates,
};
