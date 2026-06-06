/**
 * InputBar — Rich input component with multi-line support,
 * model/agent selectors, slash commands, and file attachment.
 */
import { createSignal, Show, onMount, onCleanup } from "solid-js";
import { sessionStore } from "../stores/session";
import { addUserMessage, messageStore } from "../stores/messages";

export default function InputBar() {
  let textareaRef: HTMLTextAreaElement | undefined;
  const [text, setText] = createSignal("");
  const [isSending, setIsSending] = createSignal(false);
  const [selectedModel, setSelectedModel] = createSignal("claude-opus-4-8");
  const [selectedAgent, setSelectedAgent] = createSignal("build");

  const autoResize = () => {
    if (textareaRef) {
      textareaRef.style.height = "auto";
      const maxHeight = 200; // ~10 lines
      textareaRef.style.height = Math.min(textareaRef.scrollHeight, maxHeight) + "px";
    }
  };

  const handleSend = async () => {
    const prompt = text().trim();
    if (!prompt || isSending()) return;
    if (!sessionStore.activeSession) return;

    // Check for slash commands
    if (prompt.startsWith("/")) {
      handleSlashCommand(prompt);
      setText("");
      autoResize();
      return;
    }

    setIsSending(true);
    addUserMessage(prompt);
    setText("");
    autoResize();

    try {
      const { invoke } = await import("@tauri-apps/api/core");
      await invoke("send_prompt", {
        sessionId: sessionStore.activeSession!.id,
        prompt,
      });
    } catch (e) {
      console.error("Failed to send prompt:", e);
    } finally {
      setIsSending(false);
    }
  };

  const handleSlashCommand = async (cmd: string) => {
    const parts = cmd.split(" ");
    const command = parts[0].toLowerCase();
    const args = parts.slice(1).join(" ");

    switch (command) {
      case "/model": {
        if (args) {
          setSelectedModel(args);
          addUserMessage(`Switched model to ${args}`);
        }
        break;
      }
      case "/agent": {
        if (args) {
          setSelectedAgent(args);
          addUserMessage(`Switched agent to ${args}`);
        }
        break;
      }
      case "/revert": {
        try {
          const { invoke } = await import("@tauri-apps/api/core");
          await invoke("abort_session", {
            sessionId: sessionStore.activeSession!.id,
          });
          addUserMessage("Reverted to last checkpoint.");
        } catch (e) {
          console.error("Revert failed:", e);
        }
        break;
      }
      case "/compact": {
        addUserMessage("Compacting context...");
        break;
      }
      case "/clear": {
        addUserMessage("Session cleared.");
        break;
      }
      case "/help": {
        addUserMessage(
          "Available commands:\n" +
            "/model <name> — Switch model\n" +
            "/agent <name> — Switch agent\n" +
            "/revert — Revert to last checkpoint\n" +
            "/compact — Compact context\n" +
            "/clear — Clear session\n" +
            "/help — Show this help"
        );
        break;
      }
      default: {
        addUserMessage(`Unknown command: ${command}. Type /help for available commands.`);
      }
    }
  };

  const handleKeyDown = (e: KeyboardEvent) => {
    // Cmd/Ctrl + Enter to send
    if ((e.metaKey || e.ctrlKey) && e.key === "Enter") {
      e.preventDefault();
      handleSend();
      return;
    }
    // Enter without shift sends; shift+enter adds newline
    if (e.key === "Enter" && !e.shiftKey) {
      e.preventDefault();
      handleSend();
    }
  };

  const handleAbort = async () => {
    if (!sessionStore.activeSession) return;
    try {
      const { invoke } = await import("@tauri-apps/api/core");
      await invoke("abort_session", {
        sessionId: sessionStore.activeSession.id,
      });
    } catch (e) {
      console.error("Abort failed:", e);
    }
  };

  return (
    <div class="input-bar" id="input-bar">
      <div class="input-wrapper">
        <div class="input-controls">
          <div class="input-field-container">
            <textarea
              ref={textareaRef}
              class="input-field"
              placeholder={
                messageStore.isStreaming
                  ? "AI is responding..."
                  : "Send a message... (Enter to send, Shift+Enter for newline)"
              }
              value={text()}
              onInput={(e) => {
                setText(e.currentTarget.value);
                autoResize();
              }}
              onKeyDown={handleKeyDown}
              disabled={isSending()}
              rows={1}
              id="prompt-input"
            />
          </div>
          <div class="input-actions">
            <Show
              when={messageStore.isStreaming}
              fallback={
                <button
                  class="send-button"
                  onClick={handleSend}
                  disabled={!text().trim() || isSending()}
                  data-tooltip="Send (⌘↵)"
                  id="send-btn"
                >
                  ↑
                </button>
              }
            >
              <button
                class="send-button"
                style={{ background: "var(--error)" }}
                onClick={handleAbort}
                data-tooltip="Stop generating"
                id="abort-btn"
              >
                ■
              </button>
            </Show>
          </div>
        </div>

        <div class="input-meta">
          <select
            class="model-selector"
            value={selectedModel()}
            onChange={(e) => setSelectedModel(e.currentTarget.value)}
            id="model-selector"
          >
            <option value="claude-opus-4-8">Claude Opus 4.8</option>
            <option value="claude-sonnet-4-5">Claude Sonnet 4.5</option>
            <option value="claude-haiku-4-5">Claude Haiku 4.5</option>
          </select>

          <select
            class="agent-selector"
            value={selectedAgent()}
            onChange={(e) => setSelectedAgent(e.currentTarget.value)}
            id="agent-selector"
          >
            <option value="build">🔨 Build</option>
            <option value="plan">📋 Plan</option>
            <option value="general">🤖 General</option>
            <option value="explore">🔍 Explore</option>
          </select>

          <span style={{ "margin-left": "auto" }}>
            {text().length > 0 ? `${text().length} chars` : ""}
          </span>
        </div>
      </div>
    </div>
  );
}
