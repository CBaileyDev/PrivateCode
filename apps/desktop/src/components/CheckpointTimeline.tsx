/**
 * CheckpointTimeline (Phase 4, Step 4.13) — visual checkpoint history.
 *
 * Lists the session's workspace snapshots newest-first (timestamp, tool name,
 * truncated tree hash), with a one-click revert. The backend's `revert_session`
 * restores the most-recent non-backup checkpoint (and snapshots the current state
 * first so it's reversible via unrevert) — arbitrary per-row revert needs a
 * backend "restore to tree X" command and is a documented follow-up; the per-row
 * button reverts to the latest and is disabled for older rows to avoid implying
 * a capability the engine does not yet expose.
 *
 * Headless ceiling: rendering requires a human GUI launch. The fetch + shaping
 * (filter revert_backup, newest-first) lives in the `checkpoints` store and IS
 * unit-tested (checkpoints.test.ts).
 */
import { For, Show, createEffect } from "solid-js";
import { checkpointStore, loadCheckpoints, shortHash } from "../stores/checkpoints";
import { sessionStore, revertActiveSession } from "../stores/session";

function formatTime(epochSecs: number): string {
  try {
    return new Date(epochSecs * 1000).toLocaleString();
  } catch {
    return String(epochSecs);
  }
}

export default function CheckpointTimeline() {
  // Refresh the timeline whenever the active session changes.
  createEffect(() => {
    const s = sessionStore.activeSession;
    if (s) void loadCheckpoints(s.id);
  });

  return (
    <div class="checkpoint-timeline">
      <div class="checkpoint-header">
        <h4>Checkpoints</h4>
        <Show when={sessionStore.activeSession}>
          <button
            class="checkpoint-refresh"
            onClick={() => loadCheckpoints(sessionStore.activeSession!.id)}
            title="Refresh"
          >
            ⟳
          </button>
        </Show>
      </div>

      <Show
        when={checkpointStore.checkpoints.length > 0}
        fallback={
          <div class="checkpoint-empty">
            {checkpointStore.loading ? "Loading…" : "No checkpoints yet"}
          </div>
        }
      >
        <ul class="checkpoint-list">
          <For each={checkpointStore.checkpoints}>
            {(cp, i) => (
              <li class="checkpoint-row">
                <div class="checkpoint-main">
                  <span class="checkpoint-tool">{cp.tool_name}</span>
                  <span class="checkpoint-hash" title={cp.tree_hash}>
                    {shortHash(cp.tree_hash)}
                  </span>
                </div>
                <div class="checkpoint-meta">
                  <span class="checkpoint-time">{formatTime(cp.created_at)}</span>
                  <button
                    class="checkpoint-revert"
                    disabled={i() !== 0}
                    title={
                      i() === 0
                        ? "Revert the workspace to this checkpoint"
                        : "Only the latest checkpoint can be reverted to (engine limitation)"
                    }
                    onClick={() => void revertActiveSession()}
                  >
                    Revert
                  </button>
                </div>
              </li>
            )}
          </For>
        </ul>
      </Show>
    </div>
  );
}
