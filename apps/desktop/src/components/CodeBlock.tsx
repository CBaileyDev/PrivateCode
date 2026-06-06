/**
 * CodeBlock — Syntax-highlighted code block with language label,
 * line numbers, and copy button. Highlights via a simple CSS-based
 * approach; the Shiki web worker is available for richer highlighting.
 */
import { createSignal, createMemo, Show } from "solid-js";

interface Props {
  code: string;
  language: string;
}

export default function CodeBlock(props: Props) {
  const [copied, setCopied] = createSignal(false);

  const handleCopy = async () => {
    try {
      await navigator.clipboard.writeText(props.code);
      setCopied(true);
      setTimeout(() => setCopied(false), 2000);
    } catch (e) {
      console.error("Copy failed:", e);
    }
  };

  const lines = createMemo(() => props.code.split("\n"));

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
      <pre>
        <code>
          {lines().map((line, i) => (
            <div
              style={{
                display: "flex",
                "min-height": "20px",
              }}
            >
              <span
                style={{
                  width: "40px",
                  "text-align": "right",
                  "padding-right": "12px",
                  color: "var(--text-muted)",
                  "user-select": "none",
                  "flex-shrink": "0",
                  "font-size": "var(--text-xs)",
                  opacity: "0.5",
                }}
              >
                {i + 1}
              </span>
              <span style={{ flex: "1" }}>{line || " "}</span>
            </div>
          ))}
        </code>
      </pre>
    </div>
  );
}
