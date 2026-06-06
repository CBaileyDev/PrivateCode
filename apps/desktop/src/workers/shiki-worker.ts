/**
 * Shiki syntax-highlighting Web Worker.
 *
 * Runs the real (offline, no-WASM) Shiki highlighter off the main thread.
 * Receives `{id, code, language}` and replies `{id, html}` (or `{id, error}`
 * on failure so the caller can fall back to plain text). All the real logic
 * lives in the pure `highlight` module (unit-tested in node); this file is just
 * the message pump.
 */
import { highlight } from "../lib/highlight";

self.onmessage = async (e: MessageEvent) => {
  const { id, code, language } = e.data ?? {};
  try {
    const html = await highlight(code, language);
    self.postMessage({ id, html });
  } catch (err) {
    self.postMessage({ id, error: String(err) });
  }
};
