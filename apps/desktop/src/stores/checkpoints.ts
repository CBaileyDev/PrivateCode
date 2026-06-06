/**
 * Checkpoint-history store (Phase 4, Step 4.13) — the data layer behind the
 * checkpoint timeline in the GUI right panel.
 *
 * Each turn (and each mutating tool) snapshots the workspace as a git tree; the
 * backend exposes them via the `list_checkpoints` command. This store fetches and
 * shapes them for display: newest-first, with the internal `revert_backup`
 * snapshots filtered out (they are bookkeeping for unrevert, not user history)
 * and the tree hash truncated for a compact commit-hash-style label.
 */
import { createStore } from "solid-js/store";

export interface CheckpointInfo {
  id: string;
  session_id: string;
  message_id: string;
  tree_hash: string;
  tool_name: string;
  created_at: number;
}

interface CheckpointState {
  checkpoints: CheckpointInfo[];
  loading: boolean;
}

const [checkpointStore, setCheckpointStore] = createStore<CheckpointState>({
  checkpoints: [],
  loading: false,
});

/** Truncate a tree hash to a git-style short id. */
function shortHash(treeHash: string): string {
  return treeHash.slice(0, 8);
}

/**
 * Shape raw checkpoint rows for the timeline: drop `revert_backup` bookkeeping
 * snapshots and order newest-first. Pure (no IPC) so it is unit-testable.
 */
function shapeCheckpoints(rows: CheckpointInfo[]): CheckpointInfo[] {
  return rows
    .filter((c) => c.tool_name !== "revert_backup")
    .slice()
    .sort((a, b) => b.created_at - a.created_at);
}

/** Fetch and shape the checkpoint history for a session. */
async function loadCheckpoints(sessionId: string) {
  setCheckpointStore("loading", true);
  try {
    const { invoke } = await import("@tauri-apps/api/core");
    const rows = (await invoke("list_checkpoints", { sessionId })) as CheckpointInfo[];
    setCheckpointStore("checkpoints", shapeCheckpoints(rows));
  } catch (e) {
    console.error("Failed to load checkpoints:", e);
    setCheckpointStore("checkpoints", []);
  } finally {
    setCheckpointStore("loading", false);
  }
}

/** Clear the timeline (e.g. on session switch). */
function clearCheckpoints() {
  setCheckpointStore({ checkpoints: [], loading: false });
}

export {
  checkpointStore,
  loadCheckpoints,
  clearCheckpoints,
  shapeCheckpoints,
  shortHash,
};
