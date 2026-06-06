/**
 * ComparisonView (Phase 4, Step 4.12) — side-by-side multi-model candidate panes.
 *
 * Renders one pane per fan-out / role-based candidate as it streams: model label,
 * status, and the candidate's answer (markdown + syntax-highlighted code via the
 * shared MarkdownRenderer). Keyboard navigation: ←/→ select a pane, Enter / `m`
 * copy the selected candidate's text to the clipboard (the durable synthesized
 * answer still arrives in the message list — this view is for inspecting the raw
 * candidates that fed it).
 *
 * Headless ceiling: rendering/scroll/keyboard interaction require a human GUI
 * launch to verify. The pane-assembly + selection logic lives in the
 * `candidates` store and IS unit-tested (candidates.test.ts).
 */
import { For, Show, onCleanup, onMount } from "solid-js";
import {
  candidateStore,
  selectCandidate,
  selectNext,
  selectPrev,
  selectedPane,
  type CandidatePane,
} from "../stores/candidates";
import MarkdownRenderer from "./MarkdownRenderer";

function statusLabel(p: CandidatePane): string {
  if (p.status === "streaming") return "streaming…";
  if (p.status === "error") return `error: ${p.error ?? "failed"}`;
  return p.finishReason ?? "done";
}

export default function ComparisonView() {
  function onKey(e: KeyboardEvent) {
    if (candidateStore.panes.length === 0) return;
    if (e.key === "ArrowRight") {
      e.preventDefault();
      selectNext();
    } else if (e.key === "ArrowLeft") {
      e.preventDefault();
      selectPrev();
    } else if (e.key === "Enter" || e.key.toLowerCase() === "m") {
      const pane = selectedPane();
      if (pane) void navigator.clipboard?.writeText(pane.text);
    }
  }

  onMount(() => window.addEventListener("keydown", onKey));
  onCleanup(() => window.removeEventListener("keydown", onKey));

  return (
    <Show when={candidateStore.panes.length > 0}>
      <div class="comparison-view">
        <div class="comparison-header">
          <span class="comparison-title">
            Candidates ({candidateStore.panes.length})
            <Show when={candidateStore.active}>
              <span class="comparison-live"> · live</span>
            </Show>
          </span>
          <span class="comparison-hint">←/→ select · Enter/m copy</span>
        </div>
        <div class="comparison-panes">
          <For each={candidateStore.panes}>
            {(pane, i) => (
              <div
                classList={{
                  "comparison-pane": true,
                  selected: i() === candidateStore.selected,
                  errored: pane.status === "error",
                }}
                onClick={() => selectCandidate(i())}
              >
                <div class="comparison-pane-header">
                  <span class="comparison-model" title={pane.modelId}>
                    {pane.modelId || `candidate ${pane.index}`}
                  </span>
                  <span
                    classList={{
                      "comparison-status": true,
                      [pane.status]: true,
                    }}
                  >
                    {statusLabel(pane)}
                  </span>
                </div>
                <div class="comparison-pane-body">
                  <Show
                    when={pane.text.length > 0}
                    fallback={
                      <span class="comparison-empty">
                        {pane.status === "error" ? (pane.error ?? "failed") : "…"}
                      </span>
                    }
                  >
                    <MarkdownRenderer text={pane.text} />
                  </Show>
                </div>
              </div>
            )}
          </For>
        </div>
      </div>
    </Show>
  );
}
