/**
 * Message store — manages chat message history and streaming state.
 * Handles Protocol events for text deltas, tool calls, and completions.
 */
import { createStore } from "solid-js/store";

export interface ContentBlock {
  type: "text" | "reasoning" | "tool_use" | "tool_result";
  text?: string;
  reasoning?: string;
  id?: string;
  name?: string;
  input?: any;
  tool_use_id?: string;
  content?: any;
  is_error?: boolean;
}

export interface ChatMessage {
  id: string;
  role: "user" | "assistant" | "system";
  content: ContentBlock[];
  created_at: number;
}

export interface ParsedMessage {
  id: string;
  role: "user" | "assistant" | "system";
  blocks: ContentBlock[];
  isStreaming: boolean;
  createdAt: number;
}

interface MessageState {
  messages: ParsedMessage[];
  isStreaming: boolean;
  streamingText: string;
  streamingReasoning: string;
  streamingToolCalls: Map<string, { name: string; input: string }>;
}

const [messageStore, setMessageStore] = createStore<MessageState>({
  messages: [],
  isStreaming: false,
  streamingText: "",
  streamingReasoning: "",
  streamingToolCalls: new Map(),
});

// The session whose conversation is currently DISPLAYED. Set synchronously by
// the session store on every switch (before any await). Every store write that
// could resolve late (a `loadMessages` fetch) or arrive from a background
// session (a stray channel event) is gated on this id, so a previous session's
// turn can never clobber or bleed into the view you're looking at.
let displayedSessionId: string | null = null;
function setDisplayedSession(id: string | null) {
  displayedSessionId = id;
}
/** The session currently displayed. Other stores gate their late-resolving
 * writes on this (e.g. checkpoints), matching loadMessages' own guard. */
function getDisplayedSession(): string | null {
  return displayedSessionId;
}

/** Load messages from the backend for a given session. */
async function loadMessages(sessionId: string) {
  try {
    const { invoke } = await import("@tauri-apps/api/core");
    const rawMessages = (await invoke("get_messages", { sessionId })) as any[];

    const parsed: ParsedMessage[] = rawMessages.map((m) => {
      // Compaction markers are not ChatMessages — render a divider, never the
      // raw `{compacted_through_seq,summary}` JSON.
      if (m.type === "compaction") {
        return {
          id: m.id,
          role: "system" as const,
          blocks: [{ type: "text" as const, text: "— history compacted —" }],
          isStreaming: false,
          createdAt: m.created_at,
        };
      }

      let chat: ChatMessage | null = null;
      try {
        chat = JSON.parse(m.data);
      } catch {
        chat = null;
      }
      const role =
        chat?.role === "user" || chat?.role === "assistant" || chat?.role === "system"
          ? chat.role
          : "system";
      // NEVER pass `undefined` blocks to the renderer (it would crash iterating
      // them): fall back to the raw row data as a single text block.
      const blocks = Array.isArray(chat?.content)
        ? chat!.content
        : [{ type: "text" as const, text: typeof m.data === "string" ? m.data : "" }];
      return {
        id: chat?.id ?? m.id,
        role,
        blocks,
        isStreaming: false,
        createdAt: chat?.created_at ?? m.created_at,
      };
    });

    // Drop the result if the user switched away while this fetch was in flight.
    if (sessionId !== displayedSessionId) return;
    setMessageStore("messages", parsed);
  } catch (e) {
    console.error("Failed to load messages:", e);
  }
}

/** Reset the live streaming buffers WITHOUT clearing the message list. Used on
 * completion, on an error event, and on abort so the input never gets stuck in
 * the "AI is responding…" / abort-only state when no completion arrives. */
function resetStreaming() {
  setMessageStore("isStreaming", false);
  setMessageStore("streamingText", "");
  setMessageStore("streamingReasoning", "");
  setMessageStore("streamingToolCalls", new Map());
}

/** Handle incoming ProtocolEvent from the Tauri Channel. */
function handleProtocolEvent(event: any) {
  // Cross-session bleed guard: ignore events for any session that is not the
  // one currently displayed (a background session's still-streaming turn must
  // not corrupt the view, the usage panel, or pop a permission dialog here).
  if (event.session_id && event.session_id !== displayedSessionId) return;

  switch (event.type) {
    case "message_delta": {
      setMessageStore("isStreaming", true);
      const delta = event.delta;
      if (delta.kind === "text") {
        setMessageStore("streamingText", (prev) => prev + delta.text);
      } else if (delta.kind === "reasoning") {
        setMessageStore("streamingReasoning", (prev) => prev + delta.reasoning);
      } else if (delta.kind === "tool_use") {
        const toolCalls = new Map(messageStore.streamingToolCalls);
        const existing = toolCalls.get(delta.id);
        if (existing) {
          existing.input += delta.input_delta;
        } else if (delta.name) {
          toolCalls.set(delta.id, { name: delta.name, input: delta.input_delta });
        }
        setMessageStore("streamingToolCalls", toolCalls);
      }
      break;
    }

    case "message_completed": {
      // The assistant message (and the user prompt) are now durably persisted.
      // Clear the live stream buffers and reconcile from the DB (source of
      // truth) — this makes attach/replay correct and avoids divergence between
      // what's shown and what's stored. (Also handles replay, where the stream
      // buffers are empty and the old "build-from-buffers" path showed nothing.)
      resetStreaming();
      if (event.session_id) void loadMessages(event.session_id);
      break;
    }

    case "tool_output": {
      // Tool result persisted by the engine — reconcile from the DB rather than
      // synthesizing a message (which risked duplicates / wrong ordering).
      if (event.session_id) void loadMessages(event.session_id);
      break;
    }

    case "tool_permission_required": {
      // Delegate to permission store
      import("./permissions").then(({ addPendingPermission }) => {
        addPendingPermission({
          permissionId: event.permission_id,
          toolName: event.tool_name,
          action: event.action,
          resources: event.resources,
          preview: event.preview,
          sessionId: event.session_id,
        });
      });
      break;
    }

    case "usage_updated": {
      import("./usage").then(({ updateUsage }) => {
        updateUsage(event.usage);
      });
      break;
    }

    case "error": {
      console.error(`Session error [${event.code}]: ${event.message}`);
      addSystemMessage(`⚠️ Error [${event.code}]: ${event.message}`);
      // Also surface it as a toast so a failed turn (e.g. missing API key) is
      // unmistakable, not just a faint system line.
      import("./toast").then(({ showToast }) =>
        showToast(`${event.message || "Turn failed"}`, "error"),
      );
      resetStreaming();
      break;
    }

    // Multi-model fan-out / role-based candidate streams (Phase 4). Ephemeral —
    // delegated to the comparison store; the guard above already gated the
    // session, so only the displayed session's candidates are applied.
    case "candidate_started":
    case "candidate_delta":
    case "candidate_completed": {
      import("./candidates").then(({ handleCandidateEvent }) => {
        handleCandidateEvent(event);
      });
      break;
    }

    // A new workspace snapshot was taken — keep the checkpoint timeline live.
    case "checkpoint_created": {
      if (event.session_id) {
        import("./checkpoints").then(({ loadCheckpoints }) => {
          void loadCheckpoints(event.session_id);
        });
      }
      break;
    }
  }
}

/** Add a user message to the local store (optimistic). */
function addUserMessage(text: string) {
  const msg: ParsedMessage = {
    id: `user-${Date.now()}`,
    role: "user",
    blocks: [{ type: "text", text }],
    isStreaming: false,
    createdAt: Date.now() / 1000,
  };
  setMessageStore("messages", [...messageStore.messages, msg]);
}

/** Optimistically add the user's prompt to the view and dispatch it to the
 * engine. Shared by the composer (Enter/Send) and the `/init` slash command — so
 * `/init` actually runs a turn (it previously only added the optimistic bubble
 * and never invoked `send_prompt`, generating no AGENTS.md). */
async function sendPrompt(sessionId: string, prompt: string): Promise<void> {
  addUserMessage(prompt);
  const { invoke } = await import("@tauri-apps/api/core");
  await invoke("send_prompt", { sessionId, prompt });
}

/** Add a transient local system note (slash-command feedback). NOT persisted —
 * the next DB reconcile (`loadMessages`) replaces it; it is a UI hint only. */
function addSystemMessage(text: string) {
  const msg: ParsedMessage = {
    id: `sys-${Date.now()}`,
    role: "system",
    blocks: [{ type: "text", text }],
    isStreaming: false,
    createdAt: Date.now() / 1000,
  };
  setMessageStore("messages", [...messageStore.messages, msg]);
}

/** Clear all messages. */
function clearMessages() {
  setMessageStore("messages", []);
  setMessageStore("isStreaming", false);
  setMessageStore("streamingText", "");
  setMessageStore("streamingReasoning", "");
  setMessageStore("streamingToolCalls", new Map());
}

export {
  messageStore,
  setMessageStore,
  loadMessages,
  handleProtocolEvent,
  addUserMessage,
  sendPrompt,
  addSystemMessage,
  clearMessages,
  resetStreaming,
  setDisplayedSession,
  getDisplayedSession,
};
