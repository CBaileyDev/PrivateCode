// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { clearMocks, mockIPC } from "@tauri-apps/api/mocks";
import {
  createSessionInFolder,
  flushPendingModelChange,
  loadSessions,
  revertActiveSession,
  sessionStore,
  setActiveAgent,
  setActiveModel,
  setSessionStore,
  type SessionInfo,
} from "./session";

type Call = { cmd: string; args: unknown };
let calls: Call[] = [];
let responses: Record<string, unknown> = {};

function fakeSession(
  id: string,
  model = '{"provider_id":"anthropic","model_id":"claude-opus-4-8"}',
): SessionInfo {
  return {
    id,
    project_id: "p",
    title: "t",
    agent_id: "build",
    model_config: model,
    cost: 0,
    tokens_input: 0,
    tokens_output: 0,
    tokens_reasoning: 0,
    tokens_cache_read: 0,
    tokens_cache_write: 0,
    created_at: 0,
    updated_at: 0,
  };
}

beforeEach(() => {
  // jsdom in this vitest setup doesn't expose a functional localStorage; the
  // session store persists the active id there, so provide an in-memory one.
  if (
    typeof globalThis.localStorage === "undefined" ||
    typeof globalThis.localStorage.setItem !== "function"
  ) {
    const store = new Map<string, string>();
    Object.defineProperty(globalThis, "localStorage", {
      configurable: true,
      value: {
        getItem: (k: string) => store.get(k) ?? null,
        setItem: (k: string, v: string) => void store.set(k, String(v)),
        removeItem: (k: string) => void store.delete(k),
        clear: () => store.clear(),
        key: () => null,
        length: 0,
      },
    });
  }

  calls = [];
  responses = {};
  mockIPC((cmd, args) => {
    calls.push({ cmd, args });
    const r = responses[cmd];
    return typeof r === "function" ? (r as (a: unknown) => unknown)(args) : (r ?? null);
  });
  setSessionStore("activeSession", null);
  setSessionStore("sessions", []);
  setSessionStore("projects", []);
  setSessionStore("activeProjectId", null);
  try {
    localStorage.clear();
  } catch {
    /* ignore */
  }
});

afterEach(() => clearMocks());

describe("createSessionInFolder", () => {
  it("picks a folder, opens a project, and activates a new session", async () => {
    responses["plugin:dialog|open"] = "/Users/me/proj";
    responses["open_or_create_project"] = {
      id: "p1",
      name: "proj",
      directory: "/Users/me/proj",
      created_at: 0,
    };
    responses["create_session"] = fakeSession("s1");
    responses["get_messages"] = [];
    responses["subscribe_session"] = null;
    responses["provider_status"] = [];

    await createSessionInFolder();

    expect(
      calls.find((c) => c.cmd === "open_or_create_project")?.args,
    ).toMatchObject({ directory: "/Users/me/proj" });
    expect(calls.find((c) => c.cmd === "create_session")?.args).toMatchObject({
      projectId: "p1",
      workspacePath: "/Users/me/proj",
    });
    expect(sessionStore.activeSession?.id).toBe("s1");
    expect(sessionStore.activeProjectId).toBe("p1");
  });

  it("does nothing when the folder dialog is cancelled", async () => {
    responses["plugin:dialog|open"] = null; // user cancelled
    await createSessionInFolder();
    expect(calls.some((c) => c.cmd === "create_session")).toBe(false);
    expect(sessionStore.activeSession).toBeNull();
  });

  it("defaults a new session to a CONNECTED model when its default provider isn't connected", async () => {
    responses["plugin:dialog|open"] = "/Users/me/p";
    responses["open_or_create_project"] = {
      id: "p1",
      name: "p",
      directory: "/Users/me/p",
      created_at: 0,
    };
    // create_session defaults to anthropic (not connected); openai IS connected.
    responses["create_session"] = fakeSession(
      "s1",
      '{"provider_id":"anthropic","model_id":"claude-opus-4-8"}',
    );
    responses["get_messages"] = [];
    responses["subscribe_session"] = null;
    responses["set_model"] = true;
    responses["provider_status"] = [
      {
        id: "anthropic",
        display_name: "Anthropic",
        connected: false,
        requires_key: true,
        local: false,
        models: [{ id: "claude-opus-4-8", display_name: "Opus" }],
      },
      {
        id: "openai",
        display_name: "OpenAI",
        connected: true,
        requires_key: true,
        local: false,
        models: [{ id: "gpt-4.1", display_name: "GPT-4.1" }],
      },
    ];

    await createSessionInFolder();

    expect(calls.find((c) => c.cmd === "set_model")?.args).toMatchObject({
      modelConfig: '{"provider_id":"openai","model_id":"gpt-4.1"}',
    });
  });
});

describe("setActiveModel / setActiveAgent", () => {
  it("setActiveModel persists and patches the store (live)", async () => {
    setSessionStore("activeSession", fakeSession("s1"));
    setSessionStore("sessions", [fakeSession("s1")]);
    responses["set_model"] = true;
    const cfg = '{"provider_id":"anthropic","model_id":"claude-haiku-4-5"}';

    const live = await setActiveModel(cfg);

    expect(live).toBe(true);
    expect(calls.find((c) => c.cmd === "set_model")?.args).toMatchObject({
      sessionId: "s1",
      modelConfig: cfg,
    });
    expect(sessionStore.activeSession?.model_config).toBe(cfg);
  });

  it("setActiveAgent persists and patches the store", async () => {
    setSessionStore("activeSession", fakeSession("s1"));
    setSessionStore("sessions", [fakeSession("s1")]);
    responses["set_agent"] = null;

    await setActiveAgent("plan");

    expect(calls.find((c) => c.cmd === "set_agent")?.args).toMatchObject({
      sessionId: "s1",
      agentId: "plan",
    });
    expect(sessionStore.activeSession?.agent_id).toBe("plan");
  });
});

describe("revertActiveSession", () => {
  it("invokes revert_session then reloads messages", async () => {
    setSessionStore("activeSession", fakeSession("s1"));
    responses["revert_session"] = fakeSession("s1");
    responses["get_messages"] = [];

    const err = await revertActiveSession();

    expect(err).toBeNull();
    expect(calls.some((c) => c.cmd === "revert_session")).toBe(true);
    expect(calls.some((c) => c.cmd === "get_messages")).toBe(true);
  });
});

describe("deferred provider switch (session-keyed)", () => {
  it("is NOT applied to a different session", async () => {
    setSessionStore("activeSession", fakeSession("A"));
    setSessionStore("sessions", [fakeSession("A"), fakeSession("B")]);
    responses["set_model"] = false; // deferred: a turn is active on A

    const nvidia = '{"provider_id":"nvidia","model_id":"meta/llama-3.3-70b-instruct"}';
    const live = await setActiveModel(nvidia);
    expect(live).toBe(false);

    // Switch to B, then flush (as the turn-end effect would).
    setSessionStore("activeSession", fakeSession("B"));
    calls = [];
    await flushPendingModelChange();

    // The pending change belongs to A — it must NOT be re-issued against B.
    expect(calls.some((c) => c.cmd === "set_model")).toBe(false);
  });
});

describe("reload restore", () => {
  it("re-attaches the persisted active session on loadSessions", async () => {
    localStorage.setItem("privatecode:activeSessionId", "s2");
    responses["list_projects"] = [
      { id: "p", name: "P", directory: "/d", created_at: 0 },
    ];
    responses["list_sessions"] = [fakeSession("s1"), fakeSession("s2")];
    responses["get_messages"] = [];
    responses["subscribe_session"] = null;

    await loadSessions();

    expect(sessionStore.activeSession?.id).toBe("s2");
  });

  it("loads sessions across ALL projects, newest first", async () => {
    responses["list_projects"] = [
      { id: "p1", name: "a", directory: "/a", created_at: 0 },
      { id: "p2", name: "b", directory: "/b", created_at: 0 },
    ];
    responses["list_sessions"] = (args: unknown) => {
      const pid = (args as { projectId: string }).projectId;
      return pid === "p1"
        ? [{ ...fakeSession("s1"), updated_at: 10 }]
        : [{ ...fakeSession("s2"), updated_at: 20 }];
    };

    await loadSessions();

    expect(sessionStore.sessions.map((s) => s.id)).toEqual(["s2", "s1"]);
  });
});
