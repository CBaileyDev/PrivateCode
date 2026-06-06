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

/** Load messages from the backend for a given session. */
async function loadMessages(sessionId: string) {
  try {
    const { invoke } = await import("@tauri-apps/api/core");
    const rawMessages = (await invoke("get_messages", { sessionId })) as any[];

    const parsed: ParsedMessage[] = rawMessages.map((m) => {
      let chat: ChatMessage;
      try {
        chat = JSON.parse(m.data);
      } catch {
        chat = {
          id: m.id,
          role: "system",
          content: [{ type: "text", text: m.data }],
          created_at: m.created_at,
        };
      }
      return {
        id: chat.id,
        role: chat.role,
        blocks: chat.content,
        isStreaming: false,
        createdAt: chat.created_at,
      };
    });

    setMessageStore("messages", parsed);
  } catch (e) {
    console.error("Failed to load messages:", e);
  }
}

/** Handle incoming ProtocolEvent from the Tauri Channel. */
function handleProtocolEvent(event: any) {
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
      setMessageStore("isStreaming", false);
      setMessageStore("streamingText", "");
      setMessageStore("streamingReasoning", "");
      setMessageStore("streamingToolCalls", new Map());
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
  clearMessages,
};
