/**
 * Settings — model-provider connection panel. Paste a BYOK API key per provider
 * (stored in the OS keychain via the backend) and see which providers are
 * connected, including auto-detected local Ollama / LM Studio.
 */
import { createSignal, For, Show, onMount } from "solid-js";
import {
  providerStore,
  loadProviderStatus,
  saveProviderKey,
  removeProviderKey,
  type ProviderStatus,
} from "../stores/providers";
import { showToast } from "../stores/toast";
import { closeSettings } from "../stores/ui";

export default function Settings() {
  const [keys, setKeys] = createSignal<Record<string, string>>({});
  const [busy, setBusy] = createSignal<string | null>(null);

  onMount(() => {
    void loadProviderStatus();
  });

  const setKey = (id: string, v: string) => setKeys({ ...keys(), [id]: v });

  const save = async (p: ProviderStatus) => {
    const key = (keys()[p.id] || "").trim();
    if (!key) return;
    setBusy(p.id);
    const err = await saveProviderKey(p.id, key);
    setBusy(null);
    if (err) showToast(`Could not save ${p.display_name} key: ${err}`, "error");
    else {
      showToast(`${p.display_name} connected.`, "success");
      setKey(p.id, "");
    }
  };

  const remove = async (p: ProviderStatus) => {
    setBusy(p.id);
    const err = await removeProviderKey(p.id);
    setBusy(null);
    if (err) showToast(`Could not remove ${p.display_name} key: ${err}`, "error");
    else showToast(`${p.display_name} key removed.`, "info");
  };

  return (
    <div
      class="permission-overlay"
      id="settings-overlay"
      onClick={(e) => {
        if (e.target === e.currentTarget) closeSettings();
      }}
    >
      <div class="settings-dialog" id="settings-dialog">
        <div class="settings-header">
          <h3>⚙ Model Providers</h3>
          <button class="btn icon-only" onClick={closeSettings} aria-label="Close settings">
            ✕
          </button>
        </div>

        <div class="settings-body">
          <p class="settings-hint">
            Paste an API key to connect a provider. Keys are stored in your OS keychain
            and never leave this machine. Local providers are auto-detected.
          </p>

          <Show
            when={providerStore.providers.length > 0}
            fallback={<p class="settings-hint">Loading providers…</p>}
          >
            <For each={providerStore.providers}>
              {(p) => (
                <div class="provider-row" id={`provider-${p.id}`}>
                  <div class="provider-head">
                    <span class={`connection-dot ${p.connected ? "connected" : "offline"}`} />
                    <span class="provider-name">{p.display_name}</span>
                    <span class="provider-status-text">
                      {p.connected ? "Connected" : p.local ? "Not running" : "Not connected"}
                    </span>
                  </div>

                  <Show
                    when={p.requires_key}
                    fallback={
                      <div class="provider-local-note">
                        Local provider — auto-detected when running ({p.models.length} model
                        {p.models.length === 1 ? "" : "s"}).
                      </div>
                    }
                  >
                    <div class="provider-key-row">
                      <input
                        class="input-field"
                        type="password"
                        autocomplete="off"
                        placeholder={
                          p.connected ? "Key set — paste to replace" : `${p.display_name} API key`
                        }
                        value={keys()[p.id] || ""}
                        onInput={(e) => setKey(p.id, e.currentTarget.value)}
                        onKeyDown={(e) => e.key === "Enter" && save(p)}
                        id={`provider-key-${p.id}`}
                      />
                      <button
                        class="btn primary"
                        disabled={busy() === p.id || !(keys()[p.id] || "").trim()}
                        onClick={() => save(p)}
                        id={`provider-save-${p.id}`}
                      >
                        Save
                      </button>
                      <Show when={p.connected}>
                        <button
                          class="btn danger"
                          disabled={busy() === p.id}
                          onClick={() => remove(p)}
                          id={`provider-remove-${p.id}`}
                        >
                          Remove
                        </button>
                      </Show>
                    </div>
                  </Show>
                </div>
              )}
            </For>
          </Show>
        </div>

        <div class="settings-footer">
          <button class="btn primary" onClick={closeSettings}>
            Done
          </button>
        </div>
      </div>
    </div>
  );
}
