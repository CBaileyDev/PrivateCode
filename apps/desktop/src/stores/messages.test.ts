// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { clearMocks, mockIPC } from "@tauri-apps/api/mocks";
import {
  clearMessages,
  handleProtocolEvent,
  loadMessages,
  messageStore,
  sendPrompt,
  setDisplayedSession,
} from "./messages";

let mockMessages: unknown[] = [];

beforeEach(() => {
  setDisplayedSession(null);
  clearMessages();
  mockMessages = [];
  mockIPC((cmd) => {
    if (cmd === "get_messages") return mockMessages;
    return null;
  });
});

afterEach(() => clearMocks());

// ── session-bleed / lock guards (synchronous, no IPC) ──────────────────────

describe("session-bleed guard", () => {
  it("ignores an event for a session that is not displayed", () => {
    setDisplayedSession("A");
    handleProtocolEvent({
      type: "message_delta",
      session_id: "B",
      delta: { kind: "text", text: "leak" },
    });
    expect(messageStore.streamingText).toBe("");
    expect(messageStore.isStreaming).toBe(false);
  });

  it("applies an event for the displayed session", () => {
    setDisplayedSession("A");
    handleProtocolEvent({
      type: "message_delta",
      session_id: "A",
      delta: { kind: "text", text: "hi" },
    });
    expect(messageStore.streamingText).toBe("hi");
    expect(messageStore.isStreaming).toBe(true);
  });
});

describe("streaming lock reset", () => {
  it("an error event clears the streaming lock (no completion arrives)", () => {
    setDisplayedSession("A");
    handleProtocolEvent({
      type: "message_delta",
      session_id: "A",
      delta: { kind: "text", text: "partial" },
    });
    expect(messageStore.isStreaming).toBe(true);

    handleProtocolEvent({
      type: "error",
      session_id: "A",
      code: "provider_error",
      message: "boom",
    });
    expect(messageStore.isStreaming).toBe(false);
    expect(messageStore.streamingText).toBe("");
  });
});

// ── loadMessages parsing + guards (mockIPC) ────────────────────────────────

function chatRow(id: string, role: string, text: string) {
  return {
    id,
    session_id: "s1",
    seq: 1,
    type: role,
    data: JSON.stringify({
      id,
      role,
      content: [{ type: "text", text }],
      created_at: 1,
    }),
    created_at: 1,
  };
}

describe("loadMessages", () => {
  it("parses normal chat rows", async () => {
    setDisplayedSession("s1");
    mockMessages = [chatRow("m1", "user", "hello"), chatRow("m2", "assistant", "hi")];
    await loadMessages("s1");
    expect(messageStore.messages.length).toBe(2);
    expect(messageStore.messages[0].role).toBe("user");
    expect(messageStore.messages[1].blocks[0].text).toBe("hi");
  });

  it("renders a compaction row as a divider, not raw JSON", async () => {
    setDisplayedSession("s1");
    mockMessages = [
      {
        id: "c1",
        session_id: "s1",
        seq: 1,
        type: "compaction",
        data: JSON.stringify({ compacted_through_seq: 5, summary: "stuff" }),
        created_at: 1,
      },
    ];
    await loadMessages("s1");
    expect(messageStore.messages.length).toBe(1);
    expect(messageStore.messages[0].blocks[0].text).toContain("compacted");
    expect(messageStore.messages[0].blocks[0].text).not.toContain("compacted_through_seq");
  });

  it("falls back to a text block for malformed data without throwing", async () => {
    setDisplayedSession("s1");
    mockMessages = [
      { id: "x", session_id: "s1", seq: 1, type: "assistant", data: "not json", created_at: 1 },
    ];
    await loadMessages("s1");
    expect(messageStore.messages.length).toBe(1);
    expect(Array.isArray(messageStore.messages[0].blocks)).toBe(true);
    expect(messageStore.messages[0].blocks[0].text).toBe("not json");
  });

  it("drops a late load whose session is no longer displayed", async () => {
    setDisplayedSession("A");
    mockMessages = [chatRow("m1", "user", "hello")];
    // We are displaying A, but a stale fetch for B resolves — its write must be
    // ignored so it can't clobber the active view.
    await loadMessages("B");
    expect(messageStore.messages.length).toBe(0);
  });
});

// ── sendPrompt: optimistic bubble + send_prompt invoke (powers /init) ───────

describe("sendPrompt", () => {
  it("adds the user message AND invokes send_prompt with the prompt", async () => {
    const calls: { cmd: string; args: any }[] = [];
    mockIPC((cmd, args) => {
      calls.push({ cmd, args });
      return null;
    });

    await sendPrompt("sess-1", "generate AGENTS.md");

    // Optimistic user bubble added.
    const last = messageStore.messages.at(-1);
    expect(last?.role).toBe("user");
    expect(last?.blocks[0].text).toBe("generate AGENTS.md");

    // The engine was actually invoked (the /init regression: it used to skip this).
    const sp = calls.find((c) => c.cmd === "send_prompt");
    expect(sp, "send_prompt must be invoked").toBeTruthy();
    expect(sp?.args).toMatchObject({ sessionId: "sess-1", prompt: "generate AGENTS.md" });
  });
});
