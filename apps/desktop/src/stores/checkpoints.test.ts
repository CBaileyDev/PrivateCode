// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { clearMocks, mockIPC } from "@tauri-apps/api/mocks";
import {
  checkpointStore,
  clearCheckpoints,
  loadCheckpoints,
  shapeCheckpoints,
  shortHash,
  type CheckpointInfo,
} from "./checkpoints";
import { setDisplayedSession } from "./messages";

const mk = (id: string, tool: string, hash: string, at: number): CheckpointInfo => ({
  id,
  session_id: "A",
  message_id: "m",
  tree_hash: hash,
  tool_name: tool,
  created_at: at,
});

beforeEach(() => {
  clearCheckpoints();
  setDisplayedSession(null);
});
afterEach(() => clearMocks());

describe("checkpoint shaping (pure)", () => {
  it("drops revert_backup snapshots and orders newest-first", () => {
    const rows = [
      mk("1", "turn", "aaaaaaaa1111", 100),
      mk("2", "edit_file", "bbbbbbbb2222", 200),
      mk("3", "revert_backup", "cccccccc3333", 150),
    ];
    const shaped = shapeCheckpoints(rows);
    expect(shaped.map((c) => c.id)).toEqual(["2", "1"]); // 200 then 100; backup gone
    expect(shaped.some((c) => c.tool_name === "revert_backup")).toBe(false);
  });

  it("shortHash truncates to a git-style id", () => {
    expect(shortHash("0123456789abcdef")).toBe("01234567");
  });
});

describe("loadCheckpoints (IPC)", () => {
  it("fetches, shapes, and stores the timeline", async () => {
    setDisplayedSession("A");
    mockIPC((cmd) => {
      if (cmd === "list_checkpoints") {
        return [
          mk("1", "turn", "deadbeefcafe", 10),
          mk("2", "write_file", "feedface0001", 20),
          mk("3", "revert_backup", "0000", 15),
        ];
      }
      return null;
    });

    await loadCheckpoints("A");
    expect(checkpointStore.loading).toBe(false);
    expect(checkpointStore.checkpoints.map((c) => c.id)).toEqual(["2", "1"]);
  });

  it("clears to empty on an IPC error", async () => {
    setDisplayedSession("A");
    mockIPC(() => {
      throw new Error("boom");
    });
    await loadCheckpoints("A");
    expect(checkpointStore.checkpoints).toEqual([]);
    expect(checkpointStore.loading).toBe(false);
  });

  it("ignores a late load whose session is no longer displayed (out-of-order guard)", async () => {
    // The user is now on session B; a stale load for A resolves afterward.
    setDisplayedSession("B");
    mockIPC((cmd) => {
      if (cmd === "list_checkpoints") return [mk("1", "turn", "aaaabbbb", 10)];
      return null;
    });
    await loadCheckpoints("A");
    expect(checkpointStore.checkpoints).toEqual([]); // A's rows must NOT render under B
  });
});
