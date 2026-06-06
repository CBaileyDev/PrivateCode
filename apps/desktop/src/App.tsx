import { createSignal, Show, onMount, onCleanup } from "solid-js";
import Sidebar from "./components/Sidebar";
import MessageList from "./components/MessageList";
import InputBar from "./components/InputBar";
import UsagePanel from "./components/UsagePanel";
import PermissionDialog from "./components/PermissionDialog";
import CommandPalette from "./components/CommandPalette";
import { sessionStore, loadSessions, setActiveSession } from "./stores/session";
import { messageStore, loadMessages } from "./stores/messages";
import { usageStore } from "./stores/usage";
import { permissionStore } from "./stores/permissions";

export default function App() {
  const [sidebarOpen, setSidebarOpen] = createSignal(true);
  const [rightPanelOpen, setRightPanelOpen] = createSignal(true);
  const [paletteOpen, setPaletteOpen] = createSignal(false);
  const [theme, setTheme] = createSignal<"dark" | "light">("dark");

  const toggleTheme = () => {
    const next = theme() === "dark" ? "light" : "dark";
    setTheme(next);
    document.documentElement.setAttribute("data-theme", next);
  };

  // Global keyboard shortcuts
  const handleKeyDown = (e: KeyboardEvent) => {
    const mod = e.metaKey || e.ctrlKey;
    if (mod && e.key === "b") {
      e.preventDefault();
      setSidebarOpen(!sidebarOpen());
    }
    if (mod && e.key === "e") {
      e.preventDefault();
      setRightPanelOpen(!rightPanelOpen());
    }
    if (mod && e.key === "k") {
      e.preventDefault();
      setPaletteOpen(!paletteOpen());
    }
    if (e.key === "Escape" && paletteOpen()) {
      setPaletteOpen(false);
    }
  };

  onMount(() => {
    document.addEventListener("keydown", handleKeyDown);
    loadSessions();
  });

  onCleanup(() => {
    document.removeEventListener("keydown", handleKeyDown);
  });

  return (
    <div class="app-layout">
      {/* Left Sidebar */}
      <div class={`sidebar ${sidebarOpen() ? "" : "collapsed"}`}>
        <Sidebar />
      </div>

      {/* Main Content */}
      <div class="main-content">
        <div class="main-header">
          <div style={{ display: "flex", "align-items": "center", gap: "var(--space-3)" }}>
            <button
              class="btn icon-only"
              onClick={() => setSidebarOpen(!sidebarOpen())}
              data-tooltip="Toggle sidebar (⌘B)"
              id="toggle-sidebar-btn"
            >
              ☰
            </button>
            <span class="header-title">
              {sessionStore.activeSession ? sessionStore.activeSession.title : "Private Code"}
            </span>
          </div>
          <div class="header-actions">
            <div class="connection-status">
              <span class={`connection-dot ${sessionStore.connectionStatus}`} />
              <span>
                {sessionStore.connectionStatus === "connected"
                  ? "Connected"
                  : sessionStore.connectionStatus === "reconnecting"
                  ? "Reconnecting..."
                  : "Offline"}
              </span>
            </div>
            <button
              class="theme-toggle"
              onClick={toggleTheme}
              data-tooltip="Toggle theme"
              id="theme-toggle-btn"
            >
              {theme() === "dark" ? "☀" : "🌙"}
            </button>
            <button
              class="btn icon-only"
              onClick={() => setRightPanelOpen(!rightPanelOpen())}
              data-tooltip="Toggle details (⌘E)"
              id="toggle-right-panel-btn"
            >
              📊
            </button>
          </div>
        </div>

        {/* Message Area */}
        <Show
          when={sessionStore.activeSession}
          fallback={
            <div class="empty-state">
              <div class="empty-icon">⚡</div>
              <h3>Welcome to Private Code</h3>
              <p>
                Create a new session to start an AI-powered coding conversation.
                Your code stays private — everything runs locally.
              </p>
            </div>
          }
        >
          <MessageList />
        </Show>

        {/* Input Bar */}
        <Show when={sessionStore.activeSession}>
          <InputBar />
        </Show>
      </div>

      {/* Right Panel */}
      <div class={`right-panel ${rightPanelOpen() ? "" : "collapsed"}`}>
        <UsagePanel />
      </div>

      {/* Modals */}
      <Show when={permissionStore.pending}>
        <PermissionDialog />
      </Show>

      <Show when={paletteOpen()}>
        <CommandPalette onClose={() => setPaletteOpen(false)} />
      </Show>
    </div>
  );
}
