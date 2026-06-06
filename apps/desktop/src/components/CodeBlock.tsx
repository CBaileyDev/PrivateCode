/**
 * CodeBlock — Syntax-highlighted code block with a language label and copy
 * button. Highlighting runs in the Shiki worker (off the main thread); until it
 * resolves — and if it ever fails — we render escaped plain text, so the block
 * always degrades gracefully. Shiki output escapes the code content, so the
 * `innerHTML` here is safe (do NOT route it through the markdown sanitizer).
 */
import { createSignal, onCleanup, Show } from "solid-js";
import { highlightInWorker } from "../lib/shiki-client";

interface Props {
  code: string;
  language: string;
}

export default function CodeBlock(props: Props) {
  const [copied, setCopied] = createSignal(false);
  const [html, setHtml] = createSignal<string | null>(null);

  // Highlight once on mount. A virtua unmount mid-flight is handled by the
  // `cancelled` flag (a late resolve is dropped); the worker client owns the
  // pending-map cleanup, so there is no per-component listener leak.
  let cancelled = false;
  onCleanup(() => {
    cancelled = true;
  });
  highlightInWorker(props.code, props.language)
    .then((h) => {
      if (!cancelled) setHtml(h);
    })
    .catch(() => {
      /* keep the plain-text fallback */
    });

  const handleCopy = async () => {
    try {
      await navigator.clipboard.writeText(props.code);
      setCopied(true);
      setTimeout(() => setCopied(false), 2000);
    } catch (e) {
      console.error("Copy failed:", e);
    }
  };

  return (
    <div class="code-block" id={`code-block-${props.language}`}>
      <div class="code-block-header">
        <span class="language-label">{props.language}</span>
        <button
          class={`copy-button ${copied() ? "copied" : ""}`}
          onClick={handleCopy}
        >
          {copied() ? "✓ Copied" : "Copy"}
        </button>
      </div>
      <Show
        when={html()}
        fallback={
          <pre class="code-fallback">
            <code>{props.code}</code>
          </pre>
        }
      >
        {/* Shiki output is trusted, escaped HTML. */}
        <div class="shiki-host" innerHTML={html()!} />
      </Show>
    </div>
  );
}
