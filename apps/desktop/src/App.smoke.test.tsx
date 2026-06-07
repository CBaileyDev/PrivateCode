// @vitest-environment jsdom
//
// Mount smoke: the store/unit suites never execute a component body, so a
// render-time throw (a bad reactive access at mount, etc.) would be a white
// screen on launch that compile + typecheck + store tests all miss. This mounts
// the real component tree in jsdom under mockIPC and asserts it renders.
//
// Scope: the WELCOME / no-session state + the modals. Not the active-session
// message view — that mounts virtua's <VList>, which has no layout in jsdom.
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { render } from "solid-js/web";
import { clearMocks, mockIPC } from "@tauri-apps/api/mocks";
import App from "./App";
import Settings from "./components/Settings";
import Toast from "./components/Toast";
import { showToast, clearToasts } from "./stores/toast";

beforeEach(() => {
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
  // Fresh app with no projects / no connected providers (the first-launch state).
  mockIPC((cmd) => {
    if (cmd === "list_projects") return [];
    if (cmd === "provider_status") return [];
    return null;
  });
});

afterEach(() => {
  clearMocks();
  clearToasts();
});

describe("component mount smoke", () => {
  it("renders <App /> without throwing and shows the welcome empty state", () => {
    const root = document.createElement("div");
    document.body.appendChild(root);
    const dispose = render(() => <App />, root);
    expect(root.textContent).toContain("Welcome to Private Code");
    expect(root.textContent).toContain("Open a folder");
    // No connected provider on first launch -> the connect-a-model hint is shown.
    expect(root.textContent).toContain("Connect a model");
    dispose();
    root.remove();
  });

  it("mounts <Settings /> without throwing", () => {
    const root = document.createElement("div");
    const dispose = render(() => <Settings />, root);
    expect(root.textContent).toContain("Model Providers");
    dispose();
  });

  it("mounts <Toast /> and renders a pushed notification", () => {
    const root = document.createElement("div");
    const dispose = render(() => <Toast />, root);
    showToast("smoke-toast-message", "info", 0);
    expect(root.textContent).toContain("smoke-toast-message");
    dispose();
  });
});
