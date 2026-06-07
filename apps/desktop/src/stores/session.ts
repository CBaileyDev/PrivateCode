/**
 * Session store — manages the list of sessions, active session,
 * and connection status. Communicates with the Tauri backend via invoke().
 */
import { createStore } from "solid-js/store";
import {
  clearMessages,
  handleProtocolEvent,
  loadMessages,
  setDisplayedSession,
} from "./messages";
import { clearCandidates } from "./candidates";
import { clearCheckpoints } from "./checkpoints";
import { showToast } from "./toast";
import { loadProviderStatus } from "./providers";

// Monotonic subscription generation. Each `setActiveSession` mints a new value;
// only the LATEST subscription's channel applies events. This subsumes the
// stale-channel-reactivation case on A→B→A (an old channel for A, still live on
// the backend, carries a lower generation and is permanently muted), so a
// background session's deltas can never double-apply to the active view.
let subscriptionSeq = 0;

const ACTIVE_SESSION_KEY = "privatecode:activeSessionId";

function persistActiveSessionId(id: string | null) {
  try {
    if (id) localStorage.setItem(ACTIVE_SESSION_KEY, id);
    else localStorage.removeItem(ACTIVE_SESSION_KEY);
  } catch {
    /* localStorage unavailable (non-webview) — best-effort */
  }
}

function readPersistedSessionId(): string | null {
  try {
    return localStorage.getItem(ACTIVE_SESSION_KEY);
  } catch {
    return null;
  }
}

// Types matching the Rust SessionInfo struct
export interface SessionInfo {
  id: string;
  project_id: string;
  title: string;
  agent_id: string;
  model_config: string;
  cost: number;
  tokens_input: number;
  tokens_output: number;
  tokens_reasoning: number;
  tokens_cache_read: number;
  tokens_cache_write: number;
  created_at: number;
  updated_at: number;
}

export interface ProjectInfo {
  id: string;
  name: string;
  directory: string;
  created_at: number;
}

interface SessionState {
  sessions: SessionInfo[];
  activeSession: SessionInfo | null;
  activeProjectId: string | null;
  projects: ProjectInfo[];
  connectionStatus: "connected" | "reconnecting" | "offline";
  isLoading: boolean;
}

const [sessionStore, setSessionStore] = createStore<SessionState>({
  sessions: [],
  activeSession: null,
  activeProjectId: null,
  projects: [],
  connectionStatus: "connected",
  isLoading: false,
});

/** Load all projects and their sessions from the backend. */
async function loadSessions() {
  setSessionStore("isLoading", true);
  try {
    const { invoke } = await import("@tauri-apps/api/core");
    const projects = (await invoke("list_projects")) as ProjectInfo[];
    setSessionStore("projects", projects);

    if (projects.length > 0) {
      const projectId = projects[0].id;
      setSessionStore("activeProjectId", projectId);
      const sessions = (await invoke("list_sessions", {
        projectId,
      })) as SessionInfo[];
      setSessionStore("sessions", sessions);

      // Reload restore: re-attach to the previously-active session so a webview
      // reload mid-conversation resumes streaming instead of silently dropping
      // it until the user re-clicks the session.
      const savedId = readPersistedSessionId();
      if (savedId) {
        const match = sessions.find((s) => s.id === savedId);
        if (match) await setActiveSession(match);
        else persistActiveSessionId(null);
      }
    }
  } catch (e) {
    console.error("Failed to load sessions:", e);
    // Offline / not inside Tauri — use mock data for development
    setSessionStore("connectionStatus", "offline");
  } finally {
    setSessionStore("isLoading", false);
  }
}

/** Set the active session and subscribe to its event stream. */
async function setActiveSession(session: SessionInfo) {
  setSessionStore("activeSession", session);
  // Record the displayed session SYNCHRONOUSLY (before any await) and mint a new
  // subscription generation so any in-flight load / older channel is superseded.
  setDisplayedSession(session.id);
  persistActiveSessionId(session.id);
  const mySeq = ++subscriptionSeq;
  // Clear the previous conversation immediately so the user never sees stale
  // content during the load below. The candidate-comparison panes and the
  // checkpoint timeline are per-session, so drop them on a switch too (otherwise
  // the prior session's rows linger until the new load resolves).
  clearMessages();
  clearCandidates();
  clearCheckpoints();

  // Load the existing conversation from the DB (the source of truth) before we
  // start streaming live events. This is what makes switching/attaching show
  // real history instead of an empty pane. (The write inside is gated on the
  // displayed-session id, so a late resolve can't clobber a newer switch.)
  await loadMessages(session.id);
  if (mySeq !== subscriptionSeq) return; // superseded by a newer switch

  try {
    const { invoke, Channel } = await import("@tauri-apps/api/core");

    // Subscribe to the session's typed event channel. Apply events ONLY while
    // this is the latest subscription (mySeq) AND they belong to this session —
    // belt-and-suspenders against bleed.
    const channel = new Channel();
    channel.onmessage = (event: any) => {
      if (mySeq === subscriptionSeq && event?.session_id === session.id) {
        handleProtocolEvent(event);
      }
    };

    await invoke("subscribe_session", {
      sessionId: session.id,
      channel,
      afterSeq: 0,
    });

    setSessionStore("connectionStatus", "connected");
  } catch (e) {
    console.error("Failed to subscribe to session:", e);
    setSessionStore("connectionStatus", "offline");
  }
}

/** Create a new session in the active project. */
async function createNewSession(title: string, workspacePath: string) {
  const projectId = sessionStore.activeProjectId;
  if (!projectId) {
    showToast("Pick a folder first — use “Open a folder” to start a session.", "info");
    return;
  }

  try {
    const { invoke } = await import("@tauri-apps/api/core");
    const session = (await invoke("create_session", {
      projectId,
      title,
      workspacePath,
    })) as SessionInfo;

    setSessionStore("sessions", [...sessionStore.sessions, session]);
    await setActiveSession(session);
  } catch (e) {
    showToast(`Failed to create session: ${String(e)}`);
  }
}

/**
 * Start a session by picking a project FOLDER. This is the primary "new session"
 * entry point: it opens a native folder dialog, finds-or-creates the project for
 * that directory, creates a session rooted there, and activates it. Without this
 * (and a real workspace) the agent has no directory to operate in — the original
 * bug where "New Session" did nothing because no project existed.
 */
async function createSessionInFolder(): Promise<void> {
  try {
    const dialog = await import("@tauri-apps/plugin-dialog");
    const picked = await dialog.open({
      directory: true,
      multiple: false,
      title: "Choose a project folder for Private Code",
    });
    if (!picked || typeof picked !== "string") return; // cancelled

    const { invoke } = await import("@tauri-apps/api/core");
    const project = (await invoke("open_or_create_project", {
      directory: picked,
    })) as ProjectInfo;

    // Upsert the project + make it active.
    setSessionStore("projects", (ps) =>
      ps.some((p) => p.id === project.id) ? ps : [...ps, project],
    );
    setSessionStore("activeProjectId", project.id);

    const session = (await invoke("create_session", {
      projectId: project.id,
      title: project.name,
      workspacePath: project.directory,
    })) as SessionInfo;

    setSessionStore("sessions", [...sessionStore.sessions, session]);
    await setActiveSession(session);
    // Refresh provider connectivity so the model picker is accurate.
    void loadProviderStatus();
  } catch (e) {
    showToast(`Failed to start session: ${String(e)}`);
  }
}

/** Delete a session. */
async function deleteSession(sessionId: string) {
  try {
    const { invoke } = await import("@tauri-apps/api/core");
    await invoke("delete_session", { sessionId });
    setSessionStore(
      "sessions",
      sessionStore.sessions.filter((s) => s.id !== sessionId)
    );
    if (sessionStore.activeSession?.id === sessionId) {
      setSessionStore("activeSession", null);
      setDisplayedSession(null);
      persistActiveSessionId(null);
      clearMessages();
      clearCandidates();
      clearCheckpoints();
    }
  } catch (e) {
    console.error("Failed to delete session:", e);
  }
}

/** Initialize a project (first-time setup). */
async function initProject(name: string, directory: string) {
  try {
    const { invoke } = await import("@tauri-apps/api/core");
    const project = (await invoke("init_project", {
      name,
      directory,
    })) as ProjectInfo;

    setSessionStore("projects", [...sessionStore.projects, project]);
    setSessionStore("activeProjectId", project.id);
    setSessionStore("sessions", []);
  } catch (e) {
    console.error("Failed to init project:", e);
  }
}

/** Patch the locally-cached config of a session (active + list copies) so the
 * dropdowns and panels reflect a change the backend already persisted. */
function patchSessionConfig(id: string, patch: Partial<SessionInfo>) {
  setSessionStore("sessions", (arr) =>
    arr.map((s) => (s.id === id ? { ...s, ...patch } : s)),
  );
  setSessionStore("activeSession", (s) =>
    s && s.id === id ? { ...s, ...patch } : s,
  );
}

/**
 * Switch the active session's model. `modelConfig` is a JSON string with
 * `provider_id` + `model_id`. Returns `true` if the change is live now, `false`
 * if a provider change was persisted but deferred (a turn is active) — the
 * backend evicts + recreates the session on a provider change, only when idle.
 */
// A provider change requested mid-turn is persisted but the live session keeps
// the old provider until it is evicted while idle. We stash it — KEYED BY
// SESSION — and re-apply on the next turn-end (see `flushPendingModelChange`) so
// the switch doesn't silently linger until the 30-min reaper. Keying by session
// is load-bearing: switching sessions flips `isStreaming` (via clearMessages),
// which fires the flush; without the key it would write the stashed config onto
// the WRONG session's row.
let pendingModelChange: { sessionId: string; config: string } | null = null;

async function setActiveModel(modelConfig: string): Promise<boolean> {
  const s = sessionStore.activeSession;
  if (!s) return true;
  try {
    const { invoke } = await import("@tauri-apps/api/core");
    const live = (await invoke("set_model", {
      sessionId: s.id,
      modelConfig,
    })) as boolean;
    patchSessionConfig(s.id, { model_config: modelConfig });
    pendingModelChange = live ? null : { sessionId: s.id, config: modelConfig };
    return live;
  } catch (e) {
    showToast(`Failed to switch model: ${String(e)}`);
    return true;
  }
}

/** Re-apply a deferred provider switch once the turn settles. Called by the app
 * when streaming ends; a no-op if nothing is pending or the pending change
 * belongs to a session that is no longer active (its DB row already has the new
 * config — it applies when that session is revisited and goes idle, or via the
 * reaper). */
async function flushPendingModelChange(): Promise<void> {
  if (!pendingModelChange) return;
  if (sessionStore.activeSession?.id !== pendingModelChange.sessionId) return;
  await setActiveModel(pendingModelChange.config); // now idle → evicts → live; clears on success
}

/** Switch the active session's agent (takes effect on the next turn). */
async function setActiveAgent(agentId: string): Promise<void> {
  const s = sessionStore.activeSession;
  if (!s) return;
  try {
    const { invoke } = await import("@tauri-apps/api/core");
    await invoke("set_agent", { sessionId: s.id, agentId });
    patchSessionConfig(s.id, { agent_id: agentId });
  } catch (e) {
    showToast(`Failed to switch agent: ${String(e)}`);
  }
}

/** Revert the active session's workspace to its last checkpoint. Returns an
 * error string on failure (e.g. nothing to revert), else null. */
async function revertActiveSession(): Promise<string | null> {
  const s = sessionStore.activeSession;
  if (!s) return "No active session.";
  try {
    const { invoke } = await import("@tauri-apps/api/core");
    await invoke("revert_session", { sessionId: s.id });
    await loadMessages(s.id); // revert appended a system message
    return null;
  } catch (e) {
    return String(e);
  }
}

/** Compact the active session's transcript. Returns an error string or null. */
async function compactActiveSession(): Promise<string | null> {
  const s = sessionStore.activeSession;
  if (!s) return "No active session.";
  try {
    const { invoke } = await import("@tauri-apps/api/core");
    await invoke("compact_session", { sessionId: s.id });
    await loadMessages(s.id);
    return null;
  } catch (e) {
    return String(e);
  }
}

export {
  sessionStore,
  setSessionStore,
  loadSessions,
  setActiveSession,
  createNewSession,
  createSessionInFolder,
  deleteSession,
  initProject,
  setActiveModel,
  setActiveAgent,
  revertActiveSession,
  compactActiveSession,
  flushPendingModelChange,
};
