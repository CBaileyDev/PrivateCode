/**
 * Sidebar — Session list with create, delete, switch, search, and filter.
 * Shows session metadata: title, agent, model, cost, last active time.
 */
import { createSignal, For, Show, createMemo } from "solid-js";
import {
  sessionStore,
  setActiveSession,
  createNewSession,
  deleteSession,
  type SessionInfo,
} from "../stores/session";
import { loadMessages } from "../stores/messages";
import { formatCost } from "../stores/usage";

export default function Sidebar() {
  const [searchQuery, setSearchQuery] = createSignal("");
  const [showNewSession, setShowNewSession] = createSignal(false);
  const [newTitle, setNewTitle] = createSignal("");

  const filteredSessions = createMemo(() => {
    const q = searchQuery().toLowerCase();
    if (!q) return sessionStore.sessions;
    return sessionStore.sessions.filter(
      (s) =>
        s.title.toLowerCase().includes(q) ||
        s.agent_id.toLowerCase().includes(q)
    );
  });

  const handleSelectSession = async (session: SessionInfo) => {
    await setActiveSession(session);
    await loadMessages(session.id);
  };

  const handleCreateSession = async () => {
    const title = newTitle().trim() || `Session ${sessionStore.sessions.length + 1}`;
    await createNewSession(title, ".");
    setNewTitle("");
    setShowNewSession(false);
  };

  const handleDeleteSession = async (e: MouseEvent, sessionId: string) => {
    e.stopPropagation();
    if (confirm("Delete this session?")) {
      await deleteSession(sessionId);
    }
  };

  const formatTime = (timestamp: number): string => {
    const date = new Date(timestamp * 1000);
    const now = new Date();
    const diff = now.getTime() - date.getTime();
    const minutes = Math.floor(diff / 60000);
    if (minutes < 1) return "just now";
    if (minutes < 60) return `${minutes}m ago`;
    const hours = Math.floor(minutes / 60);
    if (hours < 24) return `${hours}h ago`;
    const days = Math.floor(hours / 24);
    return `${days}d ago`;
  };

  const agentEmoji = (agent: string): string => {
    switch (agent) {
      case "build": return "🔨";
      case "plan": return "📋";
      case "general": return "🤖";
      case "explore": return "🔍";
      default: return "⚙";
    }
  };

  return (
    <>
      <div class="sidebar-header">
        <h2>Sessions</h2>
        <button
          class="btn icon-only"
          onClick={() => setShowNewSession(!showNewSession())}
          data-tooltip="New session"
          id="new-session-btn"
        >
          +
        </button>
      </div>

      <div class="sidebar-content">
        {/* Search */}
        <div style={{ "margin-bottom": "var(--space-3)" }}>
          <input
            type="text"
            class="input-field"
            style={{
              "min-height": "36px",
              "font-size": "var(--text-sm)",
              padding: "var(--space-2) var(--space-3)",
            }}
            placeholder="Search sessions..."
            value={searchQuery()}
            onInput={(e) => setSearchQuery(e.currentTarget.value)}
            id="session-search-input"
          />
        </div>

        {/* New Session Form */}
        <Show when={showNewSession()}>
          <div
            style={{
              padding: "var(--space-3)",
              background: "var(--bg-tertiary)",
              "border-radius": "var(--radius-md)",
              "margin-bottom": "var(--space-3)",
              border: "1px solid var(--border)",
            }}
          >
            <input
              type="text"
              class="input-field"
              style={{
                "min-height": "36px",
                "font-size": "var(--text-sm)",
                padding: "var(--space-2) var(--space-3)",
                "margin-bottom": "var(--space-2)",
              }}
              placeholder="Session title..."
              value={newTitle()}
              onInput={(e) => setNewTitle(e.currentTarget.value)}
              onKeyDown={(e) => e.key === "Enter" && handleCreateSession()}
              id="new-session-title-input"
            />
            <div style={{ display: "flex", gap: "var(--space-2)" }}>
              <button class="btn primary" style={{ flex: 1 }} onClick={handleCreateSession}>
                Create
              </button>
              <button class="btn" onClick={() => setShowNewSession(false)}>
                Cancel
              </button>
            </div>
          </div>
        </Show>

        {/* Session List */}
        <Show
          when={filteredSessions().length > 0}
          fallback={
            <div style={{ padding: "var(--space-4)", "text-align": "center" }}>
              <p style={{ color: "var(--text-muted)", "font-size": "var(--text-sm)" }}>
                {sessionStore.isLoading ? "Loading..." : "No sessions yet"}
              </p>
              <Show when={!sessionStore.isLoading && sessionStore.sessions.length === 0}>
                <button
                  class="new-session-btn"
                  style={{ "margin-top": "var(--space-3)" }}
                  onClick={() => setShowNewSession(true)}
                  id="create-first-session-btn"
                >
                  + Create your first session
                </button>
              </Show>
            </div>
          }
        >
          <For each={filteredSessions()}>
            {(session) => (
              <div
                class={`session-item ${
                  sessionStore.activeSession?.id === session.id ? "active" : ""
                }`}
                onClick={() => handleSelectSession(session)}
                id={`session-${session.id}`}
              >
                <div class="session-title">
                  {agentEmoji(session.agent_id)} {session.title}
                </div>
                <div class="session-meta">
                  <span>{session.agent_id}</span>
                  <span>•</span>
                  <span>{formatTime(session.updated_at)}</span>
                  <Show when={session.cost > 0}>
                    <span class="session-cost">{formatCost(session.cost)}</span>
                  </Show>
                </div>
                <button
                  class="btn icon-only danger"
                  style={{
                    position: "absolute",
                    right: "8px",
                    top: "8px",
                    opacity: 0,
                    width: "24px",
                    height: "24px",
                    "min-width": "24px",
                    "min-height": "24px",
                    "font-size": "var(--text-xs)",
                  }}
                  onClick={(e) => handleDeleteSession(e, session.id)}
                  id={`delete-session-${session.id}`}
                >
                  ✕
                </button>
              </div>
            )}
          </For>
        </Show>
      </div>

      <div class="sidebar-footer">
        <div
          style={{
            "font-size": "var(--text-xs)",
            color: "var(--text-muted)",
            display: "flex",
            "align-items": "center",
            gap: "var(--space-2)",
          }}
        >
          <span>⚡ Private Code</span>
          <span style={{ "margin-left": "auto" }}>v0.1.0</span>
        </div>
      </div>
    </>
  );
}
