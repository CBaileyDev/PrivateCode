/**
 * Main-thread client for the Shiki worker.
 *
 * A SINGLE shared worker with ONE `onmessage` dispatcher keyed by request id —
 * NOT a listener per CodeBlock. This matters because the virtualized message
 * list mounts/unmounts CodeBlocks constantly; per-component worker listeners
 * would accumulate and fire for dead components. Callers get a Promise and are
 * responsible for ignoring a late resolve if they unmounted (a cheap `cancelled`
 * flag); the pending map entry is always cleaned up here.
 */
type Pending = { resolve: (html: string) => void; reject: (err: unknown) => void };

let worker: Worker | null = null;
const pending = new Map<number, Pending>();
let nextId = 1;

function getWorker(): Worker {
  if (!worker) {
    worker = new Worker(new URL("../workers/shiki-worker.ts", import.meta.url), {
      type: "module",
    });
    worker.onmessage = (e: MessageEvent) => {
      const { id, html, error } = e.data ?? {};
      const p = pending.get(id);
      if (!p) return;
      pending.delete(id);
      if (typeof html === "string") p.resolve(html);
      else p.reject(new Error(error ?? "highlight failed"));
    };
    worker.onerror = (e) => {
      // Worker crashed — fail every in-flight request so callers fall back.
      for (const p of pending.values()) p.reject(e);
      pending.clear();
    };
  }
  return worker;
}

/** Highlight `code` off the main thread. Rejects on worker error (caller falls
 * back to plain text). */
export function highlightInWorker(code: string, language: string): Promise<string> {
  const w = getWorker();
  const id = nextId++;
  return new Promise<string>((resolve, reject) => {
    pending.set(id, { resolve, reject });
    w.postMessage({ id, code, language });
  });
}
