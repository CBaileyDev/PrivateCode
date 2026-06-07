// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { clearMocks, mockIPC } from "@tauri-apps/api/mocks";
import {
  providerStore,
  setProviderStore,
  loadProviderStatus,
  saveProviderKey,
  removeProviderKey,
  connectedModels,
  anyConnected,
} from "./providers";

type Call = { cmd: string; args: unknown };
let calls: Call[] = [];

function statuses() {
  return [
    {
      id: "anthropic",
      display_name: "Anthropic",
      connected: true,
      requires_key: true,
      local: false,
      models: [{ id: "claude-opus-4-8", display_name: "Claude Opus 4.8" }],
    },
    {
      id: "openai",
      display_name: "OpenAI",
      connected: false,
      requires_key: true,
      local: false,
      models: [{ id: "gpt-4.1", display_name: "GPT-4.1" }],
    },
    {
      id: "ollama",
      display_name: "Ollama",
      connected: false,
      requires_key: false,
      local: true,
      models: [],
    },
  ];
}

beforeEach(() => {
  calls = [];
  setProviderStore("providers", []);
  mockIPC((cmd, args) => {
    calls.push({ cmd, args });
    if (cmd === "provider_status") return statuses();
    return null;
  });
});

afterEach(() => clearMocks());

describe("providers store", () => {
  it("loads provider status from the backend", async () => {
    await loadProviderStatus();
    expect(providerStore.providers.length).toBe(3);
    expect(anyConnected()).toBe(true);
  });

  it("connectedModels lists only connected providers' models", async () => {
    await loadProviderStatus();
    expect(connectedModels()).toEqual([
      { value: "anthropic|claude-opus-4-8", label: "Claude Opus 4.8" },
    ]);
  });

  it("saveProviderKey invokes set_provider_key then refreshes status", async () => {
    const err = await saveProviderKey("openai", "sk-test");
    expect(err).toBeNull();
    const setCall = calls.find((c) => c.cmd === "set_provider_key");
    expect(setCall?.args).toMatchObject({ provider: "openai", key: "sk-test" });
    expect(calls.some((c) => c.cmd === "provider_status")).toBe(true);
  });

  it("removeProviderKey invokes remove_provider_key then refreshes", async () => {
    const err = await removeProviderKey("anthropic");
    expect(err).toBeNull();
    expect(
      calls.find((c) => c.cmd === "remove_provider_key")?.args,
    ).toMatchObject({ provider: "anthropic" });
    expect(calls.some((c) => c.cmd === "provider_status")).toBe(true);
  });
});
