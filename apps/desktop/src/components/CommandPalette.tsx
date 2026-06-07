/**
 * CommandPalette — Cmd-K overlay with searchable command list.
 * Keyboard-first navigation with arrow keys and Enter to execute.
 */
import { createSignal, createMemo, For, onMount, onCleanup } from "solid-js";
import {
  revertActiveSession,
  compactActiveSession,
  createSessionInFolder,
} from "../stores/session";
import { clearMessages } from "../stores/messages";
import { openSettings, toggleSidebar, toggleRightPanel, toggleTheme } from "../stores/ui";
import { showToast } from "../stores/toast";

interface PaletteCommand {
  id: string;
  label: string;
  icon: string;
  shortcut?: string;
  action: () => void;
}

interface Props {
  onClose: () => void;
}

export default function CommandPalette(props: Props) {
  const [query, setQuery] = createSignal("");
  const [selectedIndex, setSelectedIndex] = createSignal(0);
  let inputRef: HTMLInputElement | undefined;

  const commands: PaletteCommand[] = [
    {
      id: "new-session",
      label: "New Session (open a folder)",
      icon: "➕",
      action: () => {
        props.onClose();
        void createSessionInFolder();
      },
    },
    {
      id: "model-providers",
      label: "Model Providers (Settings)",
      icon: "🔌",
      shortcut: "⌘,",
      action: () => {
        props.onClose();
        openSettings();
      },
    },
    {
      id: "revert",
      label: "Revert to Checkpoint",
      icon: "⏪",
      shortcut: "/revert",
      action: () => {
        props.onClose();
        void revertActiveSession().then((err) => {
          showToast(err ? `Revert failed: ${err}` : "Workspace reverted.", err ? "error" : "success");
        });
      },
    },
    {
      id: "compact",
      label: "Compact Context",
      icon: "📦",
      shortcut: "/compact",
      action: () => {
        props.onClose();
        void compactActiveSession().then((err) => {
          showToast(err ? `Compact failed: ${err}` : "Context compacted.", err ? "error" : "success");
        });
      },
    },
    {
      id: "clear",
      label: "Clear Session",
      icon: "🗑",
      shortcut: "/clear",
      action: () => {
        props.onClose();
        clearMessages();
      },
    },
    {
      id: "toggle-sidebar",
      label: "Toggle Sidebar",
      icon: "◧",
      shortcut: "⌘B",
      action: () => {
        props.onClose();
        toggleSidebar();
      },
    },
    {
      id: "toggle-details",
      label: "Toggle Details Panel",
      icon: "◨",
      shortcut: "⌘E",
      action: () => {
        props.onClose();
        toggleRightPanel();
      },
    },
    {
      id: "toggle-theme",
      label: "Toggle Theme",
      icon: "🎨",
      action: () => {
        props.onClose();
        toggleTheme();
      },
    },
    {
      id: "help",
      label: "Show Help",
      icon: "❓",
      shortcut: "/help",
      action: () => {
        props.onClose();
        showToast(
          "Commands: /model <provider/name>, /agent <name>, /revert, /compact, /clear, /help. " +
            "Cmd+K palette, Cmd+, settings, Cmd+B sidebar, Cmd+E details.",
          "info",
          12000,
        );
      },
    },
  ];

  const filtered = createMemo(() => {
    const q = query().toLowerCase();
    if (!q) return commands;
    return commands.filter(
      (c) =>
        c.label.toLowerCase().includes(q) ||
        c.id.includes(q) ||
        (c.shortcut && c.shortcut.toLowerCase().includes(q))
    );
  });

  const handleKeyDown = (e: KeyboardEvent) => {
    switch (e.key) {
      case "ArrowDown":
        e.preventDefault();
        setSelectedIndex((i) => Math.min(i + 1, filtered().length - 1));
        break;
      case "ArrowUp":
        e.preventDefault();
        setSelectedIndex((i) => Math.max(i - 1, 0));
        break;
      case "Enter": {
        e.preventDefault();
        const cmd = filtered()[selectedIndex()];
        if (cmd) cmd.action();
        break;
      }
      case "Escape":
        e.preventDefault();
        props.onClose();
        break;
    }
  };

  onMount(() => {
    inputRef?.focus();
    document.addEventListener("keydown", handleKeyDown);
  });

  onCleanup(() => {
    document.removeEventListener("keydown", handleKeyDown);
  });

  return (
    <div class="palette-overlay" onClick={props.onClose} id="command-palette-overlay">
      <div class="palette-dialog" onClick={(e) => e.stopPropagation()} id="command-palette">
        <input
          ref={inputRef}
          class="palette-input"
          placeholder="Type a command..."
          value={query()}
          onInput={(e) => {
            setQuery(e.currentTarget.value);
            setSelectedIndex(0);
          }}
          id="palette-search-input"
        />
        <div class="palette-results">
          <For each={filtered()}>
            {(cmd, i) => (
              <div
                class={`palette-item ${i() === selectedIndex() ? "selected" : ""}`}
                onClick={() => cmd.action()}
                onMouseEnter={() => setSelectedIndex(i())}
                id={`palette-cmd-${cmd.id}`}
              >
                <span class="item-icon">{cmd.icon}</span>
                <span class="item-label">{cmd.label}</span>
                {cmd.shortcut && (
                  <span class="item-shortcut">{cmd.shortcut}</span>
                )}
              </div>
            )}
          </For>
          {filtered().length === 0 && (
            <div
              style={{
                padding: "var(--space-4) var(--space-5)",
                color: "var(--text-muted)",
                "font-size": "var(--text-sm)",
                "text-align": "center",
              }}
            >
              No matching commands
            </div>
          )}
        </div>
      </div>
    </div>
  );
}
