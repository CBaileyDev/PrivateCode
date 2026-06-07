/**
 * Toast — fixed stack of transient notifications (errors, info, success). Makes
 * failures visible instead of silently logged. Driven by the toast store.
 */
import { For, Show } from "solid-js";
import { toastStore, dismissToast } from "../stores/toast";

const icon = (kind: string) =>
  kind === "error" ? "⚠" : kind === "success" ? "✓" : "ℹ";

export default function Toast() {
  return (
    <Show when={toastStore.toasts.length > 0}>
      <div class="toast-container" id="toast-container">
        <For each={toastStore.toasts}>
          {(t) => (
            <div class={`toast toast-${t.kind}`} role="alert">
              <span class="toast-icon">{icon(t.kind)}</span>
              <span class="toast-message">{t.message}</span>
              <button
                class="toast-dismiss"
                onClick={() => dismissToast(t.id)}
                aria-label="Dismiss notification"
              >
                ✕
              </button>
            </div>
          )}
        </For>
      </div>
    </Show>
  );
}
