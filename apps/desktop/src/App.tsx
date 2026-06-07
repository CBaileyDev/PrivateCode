import { createSignal, createEffect, Show, onMount, onCleanup } from "solid-js";
import Sidebar from "./components/Sidebar";
import MessageList from "./components/MessageList";
import InputBar from "./components/InputBar";
import UsagePanel from "./components/UsagePanel";
import ComparisonView from "./components/ComparisonView";
import CheckpointTimeline from "./components/CheckpointTimeline";
import PermissionDialog from "./components/PermissionDialog";
import CommandPalette from "./components/CommandPalette";
import Settings from "./components/Settings";
import Toast from "./components/Toast";
import {
  sessionStore,
  loadSessions,
  flushPendingModelChange,
  createSessionInFolder,
} from "./stores/session";
import { messageStore } from "./stores/messages";
import { permissionStore } from "./stores/permissions";
import { loadProviderStatus, anyConnected } from "./stores/providers";
import { settingsOpen, openSettings, closeSettings } from "./stores/ui";

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
    if (mod && e.key === ",") {
      e.preventDefault();
      openSettings();
    }
    if (e.key === "Escape") {
      if (paletteOpen()) setPaletteOpen(false);
      if (settingsOpen()) closeSettings();
    }
  };

  // Re-apply a model/provider change that was deferred because a turn was
  // active, as soon as streaming ends (else it lingers until the idle reaper).
  createEffect(() => {
    if (!messageStore.isStreaming) void flushPendingModelChange();
  });

  onMount(() => {
    document.addEventListener("keydown", handleKeyDown);
    loadSessions();
    void loadProviderStatus();
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
              onClick={openSettings}
              data-tooltip="Model providers (⌘,)"
              id="open-settings-btn"
            >
              ⚙
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
                Open a project folder to start an AI-powered coding session. Your
                code stays private — everything runs locally.
              </p>
              <div class="empty-actions">
                <button
                  class="new-session-btn"
                  onClick={() => void createSessionInFolder()}
                  id="empty-open-folder-btn"
                >
                  📂 Open a folder
                </button>
                <Show when={!anyConnected()}>
                  <button class="btn" onClick={openSettings} id="empty-add-key-btn">
                    🔌 Connect a model
                  </button>
                </Show>
              </div>
              <Show when={!anyConnected()}>
                <p class="empty-hint">
                  No model connected yet — add a provider API key in Settings (⌘,) so
                  your sessions can talk to a model.
                </p>
              </Show>
            </div>
          }
        >
          <MessageList />
        </Show>

        {/* Multi-model candidate comparison (Phase 4): only renders while a
            fan-out / role-based turn has produced candidate panes. */}
        <ComparisonView />

        {/* Input Bar */}
        <Show when={sessionStore.activeSession}>
          <InputBar />
        </Show>
      </div>

      {/* Right Panel */}
      <div class={`right-panel ${rightPanelOpen() ? "" : "collapsed"}`}>
        <UsagePanel />
        <Show when={sessionStore.activeSession}>
          <CheckpointTimeline />
        </Show>
      </div>

      {/* Modals */}
      <Show when={permissionStore.pending}>
        <PermissionDialog />
      </Show>

      <Show when={paletteOpen()}>
        <CommandPalette onClose={() => setPaletteOpen(false)} />
      </Show>

      <Show when={settingsOpen()}>
        <Settings />
      </Show>

      {/* Transient notifications */}
      <Toast />
    </div>
  );
}
