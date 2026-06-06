import { beforeEach, describe, expect, it } from "vitest";
import {
  clearMessages,
  handleProtocolEvent,
  messageStore,
  setDisplayedSession,
} from "./messages";

// These exercise the synchronous guard paths only — `message_delta`/`error`
// never touch the dynamic `@tauri-apps/api/core` import, so they run in the
// node environment with no mockIPC. The positive paths that go through
// `loadMessages` (an `invoke`) are deferred to the C15 jsdom/mockIPC suite.

beforeEach(() => {
  setDisplayedSession(null);
  clearMessages();
});

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
