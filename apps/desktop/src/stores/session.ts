/**
 * Session store — manages the list of sessions, active session,
 * and connection status. Communicates with the Tauri backend via invoke().
 */
import { createStore } from "solid-js/store";
import { handleProtocolEvent, loadMessages } from "./messages";

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

  // Load the existing conversation from the DB (the source of truth) before we
  // start streaming live events. This is what makes switching/attaching show
  // real history instead of an empty pane.
  await loadMessages(session.id);

  try {
    const { invoke, Channel } = await import("@tauri-apps/api/core");

    // Subscribe to the session's typed event channel. (Static import of the
    // dispatcher — `require` is not defined in the Tauri/ESM webview and was
    // throwing on every event, so no backend events reached the UI.)
    const channel = new Channel();
    channel.onmessage = (event: any) => handleProtocolEvent(event);

    await invoke("subscribe_session", {
      sessionId: session.id,
      channel,
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
  if (!projectId) return;

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
    console.error("Failed to create session:", e);
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

export {
  sessionStore,
  setSessionStore,
  loadSessions,
  setActiveSession,
  createNewSession,
  deleteSession,
  initProject,
};
