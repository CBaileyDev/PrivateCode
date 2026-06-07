/**
 * Toast store — surfaces transient notifications (errors, info, success) so
 * failures are VISIBLE instead of swallowed by `console.error`. Used by the
 * stores' catch blocks and by the engine `error` protocol event.
 */
import { createStore } from "solid-js/store";

export type ToastKind = "error" | "info" | "success";

export interface Toast {
  id: number;
  kind: ToastKind;
  message: string;
}

interface ToastState {
  toasts: Toast[];
}

const [toastStore, setToastStore] = createStore<ToastState>({ toasts: [] });

// Monotonic id (Date.now/Math.random avoided for determinism in tests).
let nextId = 1;

/** Show a toast. Returns its id. `ttlMs <= 0` keeps it until dismissed. */
function showToast(message: string, kind: ToastKind = "error", ttlMs = 6000): number {
  const id = nextId++;
  setToastStore("toasts", (t) => [...t, { id, kind, message }]);
  if (ttlMs > 0) {
    setTimeout(() => dismissToast(id), ttlMs);
  }
  return id;
}

function dismissToast(id: number) {
  setToastStore("toasts", (t) => t.filter((x) => x.id !== id));
}

function clearToasts() {
  setToastStore("toasts", []);
}

export { toastStore, showToast, dismissToast, clearToasts };
