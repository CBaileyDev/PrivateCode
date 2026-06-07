/**
 * InputBar — Rich input component with multi-line support,
 * model/agent selectors, slash commands, and file attachment.
 */
import { createSignal, For, Show } from "solid-js";
import {
  sessionStore,
  setActiveModel,
  setActiveAgent,
  revertActiveSession,
  compactActiveSession,
} from "../stores/session";
import {
  addSystemMessage,
  clearMessages,
  messageStore,
  resetStreaming,
  sendPrompt,
} from "../stores/messages";
import { usageStore, formatCost } from "../stores/usage";
import { connectedModels } from "../stores/providers";
import { openSettings } from "../stores/ui";

/** Selectable models. `value` is `provider|model_id` (model_id may itself
 * contain `/`, e.g. NVIDIA's "meta/llama-…", so we split on the first `|`). */
const MODEL_OPTIONS: { value: string; label: string }[] = [
  { value: "anthropic|claude-opus-4-8", label: "Claude Opus 4.8" },
  { value: "anthropic|claude-sonnet-4-6", label: "Claude Sonnet 4.6" },
  { value: "anthropic|claude-haiku-4-5", label: "Claude Haiku 4.5" },
  { value: "nvidia|meta/llama-3.3-70b-instruct", label: "Llama 3.3 70B (NVIDIA)" },
];

const AGENT_OPTIONS: { value: string; label: string }[] = [
  { value: "build", label: "🔨 Build" },
  { value: "plan", label: "📋 Plan" },
  { value: "general", label: "🤖 General" },
  { value: "explore", label: "🔍 Explore" },
];

function parseModelConfig(value: string): string {
  const sep = value.indexOf("|");
  const provider_id = value.slice(0, sep);
  const model_id = value.slice(sep + 1);
  return JSON.stringify({ provider_id, model_id });
}

export default function InputBar() {
  let textareaRef: HTMLTextAreaElement | undefined;
  const [text, setText] = createSignal("");
  const [isSending, setIsSending] = createSignal(false);

  // The dropdowns reflect the ACTIVE session's persisted config (single source
  // of truth), not a detached local signal.
  const currentModelValue = () => {
    try {
      const cfg = JSON.parse(sessionStore.activeSession?.model_config ?? "{}");
      if (cfg.provider_id && cfg.model_id) return `${cfg.provider_id}|${cfg.model_id}`;
    } catch {
      /* fall through */
    }
    return "anthropic|claude-opus-4-8";
  };
  const currentAgent = () => sessionStore.activeSession?.agent_id ?? "build";

  // The model picker shows models from CONNECTED providers; before any provider
  // is connected it falls back to the static list so the control is never empty
  // (sending without a key surfaces a clear error toast).
  const modelOptions = () => {
    const connected = connectedModels();
    return connected.length > 0 ? connected : MODEL_OPTIONS;
  };

  const onModelChange = async (value: string) => {
    const live = await setActiveModel(parseModelConfig(value));
    if (!live) {
      addSystemMessage(
        "⏳ Model change saved — it takes effect after the current turn finishes.",
      );
    }
  };

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
    setText("");
    autoResize();

    try {
      await sendPrompt(sessionStore.activeSession!.id, prompt);
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
          // Accept "provider/model" (provider ∈ {anthropic,nvidia}) or a bare
          // model id (defaults to anthropic).
          let provider = "anthropic";
          let model = args;
          if (args.startsWith("nvidia/")) {
            provider = "nvidia";
            model = args.slice("nvidia/".length);
          } else if (args.startsWith("anthropic/")) {
            model = args.slice("anthropic/".length);
          }
          const cfg = JSON.stringify({ provider_id: provider, model_id: model });
          const live = await setActiveModel(cfg);
          addSystemMessage(
            live
              ? `Switched model to ${provider}/${model}.`
              : `Model change to ${provider}/${model} saved — applies after the current turn.`,
          );
        }
        break;
      }
      case "/agent": {
        if (args) {
          await setActiveAgent(args);
          addSystemMessage(`Switched agent to ${args}.`);
        }
        break;
      }
      case "/revert": {
        const err = await revertActiveSession();
        if (err) addSystemMessage(`Revert failed: ${err}`);
        break;
      }
      case "/compact": {
        const err = await compactActiveSession();
        addSystemMessage(err ? `Compact failed: ${err}` : "Context compacted.");
        break;
      }
      case "/clear": {
        // View-only: clears the local pane (the DB transcript is unchanged and
        // reloads on the next reconcile/switch).
        clearMessages();
        break;
      }
      case "/help": {
        addSystemMessage(
          "Available commands:\n" +
            "/model <provider/name> — Switch model\n" +
            "/agent <name> — Switch agent\n" +
            "/revert — Revert workspace to last checkpoint\n" +
            "/compact — Compact context\n" +
            "/clear — Clear the local view\n" +
            "/cost — Show session cost breakdown\n" +
            "/share — Export session to Markdown\n" +
            "/init — Generate AGENTS.md for this project\n" +
            "/help — Show this help"
        );
        break;
      }
      case "/cost": {
        const u = usageStore.session;
        addSystemMessage(
          `Session cost: ${formatCost(u.cost)}\n` +
            `Tokens — in: ${u.input_tokens}, out: ${u.output_tokens}, ` +
            `cache read: ${u.cache_read_tokens}, cache write: ${u.cache_write_tokens}`
        );
        break;
      }
      case "/share": {
        if (!sessionStore.activeSession) break;
        try {
          const { invoke } = await import("@tauri-apps/api/core");
          const path = await invoke<string>("export_session", {
            sessionId: sessionStore.activeSession.id,
            format: "markdown",
          });
          addSystemMessage(`Session exported to ${path}`);
        } catch (e) {
          addSystemMessage(`Export failed: ${String(e)}`);
        }
        break;
      }
      case "/init": {
        if (!sessionStore.activeSession) break;
        try {
          // Actually RUN the turn (previously this only showed an optimistic
          // bubble and never invoked send_prompt, so no AGENTS.md was generated).
          await sendPrompt(
            sessionStore.activeSession.id,
            "Please analyze this repository and generate an AGENTS.md file at the project root with: project overview, architecture notes, coding conventions, testing instructions, and key file paths."
          );
        } catch (e) {
          addSystemMessage(`/init failed: ${String(e)}`);
        }
        break;
      }
      default: {
        addSystemMessage(`Unknown command: ${command}. Type /help for available commands.`);
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
      // Abort cancels the turn without necessarily emitting a completion event,
      // so clear the streaming lock here rather than waiting for one.
      resetStreaming();
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
            value={currentModelValue()}
            onChange={(e) => onModelChange(e.currentTarget.value)}
            id="model-selector"
          >
            {/* Show a session's model even if it isn't in the connected list (e.g.
                set via a slash command) so the select never renders blank. */}
            <Show when={!modelOptions().some((o) => o.value === currentModelValue())}>
              <option value={currentModelValue()}>{currentModelValue()}</option>
            </Show>
            <For each={modelOptions()}>
              {(opt) => <option value={opt.value}>{opt.label}</option>}
            </For>
          </select>
          <button
            class="btn icon-only"
            onClick={openSettings}
            data-tooltip="Model providers (⌘,)"
            id="inputbar-settings-btn"
          >
            ⚙
          </button>

          <select
            class="agent-selector"
            value={currentAgent()}
            onChange={(e) => setActiveAgent(e.currentTarget.value)}
            id="agent-selector"
          >
            <For each={AGENT_OPTIONS}>
              {(opt) => <option value={opt.value}>{opt.label}</option>}
            </For>
          </select>

          <span style={{ "margin-left": "auto" }}>
            {text().length > 0 ? `${text().length} chars` : ""}
          </span>
        </div>
      </div>
    </div>
  );
}
