/**
 * Providers store — connection status for each AI provider (from the backend
 * `provider_status` command) and helpers to add/remove API keys. Drives the
 * Settings panel and the model picker (only connectable models are offered).
 */
import { createStore } from "solid-js/store";

export interface ProviderModel {
  id: string;
  display_name: string;
}

export interface ProviderStatus {
  id: string;
  display_name: string;
  connected: boolean;
  requires_key: boolean;
  local: boolean;
  models: ProviderModel[];
}

interface ProvidersState {
  providers: ProviderStatus[];
  loading: boolean;
}

const [providerStore, setProviderStore] = createStore<ProvidersState>({
  providers: [],
  loading: false,
});

/** Load per-provider connection status + available models from the backend. */
async function loadProviderStatus(): Promise<void> {
  setProviderStore("loading", true);
  try {
    const { invoke } = await import("@tauri-apps/api/core");
    const list = (await invoke("provider_status")) as ProviderStatus[];
    setProviderStore("providers", list);
  } catch (e) {
    console.error("provider_status failed:", e);
  } finally {
    setProviderStore("loading", false);
  }
}

/** Store an API key for `provider`, then refresh status. Returns an error
 * string on failure, else null. */
async function saveProviderKey(provider: string, key: string): Promise<string | null> {
  try {
    const { invoke } = await import("@tauri-apps/api/core");
    await invoke("set_provider_key", { provider, key });
    await loadProviderStatus();
    return null;
  } catch (e) {
    return String(e);
  }
}

/** Remove a provider's stored key, then refresh status. */
async function removeProviderKey(provider: string): Promise<string | null> {
  try {
    const { invoke } = await import("@tauri-apps/api/core");
    await invoke("remove_provider_key", { provider });
    await loadProviderStatus();
    return null;
  } catch (e) {
    return String(e);
  }
}

/** Models offered by CONNECTED providers, as `{value:"provider|model", label}`
 * for the model picker. Reads the store, so it is reactive inside JSX. */
function connectedModels(): { value: string; label: string }[] {
  const out: { value: string; label: string }[] = [];
  for (const p of providerStore.providers) {
    if (!p.connected) continue;
    for (const m of p.models) {
      out.push({ value: `${p.id}|${m.id}`, label: m.display_name });
    }
  }
  return out;
}

/** True when at least one provider is connected (has a key / is reachable). */
function anyConnected(): boolean {
  return providerStore.providers.some((p) => p.connected);
}

export {
  providerStore,
  setProviderStore,
  loadProviderStatus,
  saveProviderKey,
  removeProviderKey,
  connectedModels,
  anyConnected,
};
