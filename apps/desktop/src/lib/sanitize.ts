/**
 * HTML sanitization for the markdown renderer.
 *
 * `renderInline` produces an HTML string that the renderer injects via
 * `innerHTML`. The input is untrusted (model output, tool results, user text,
 * web-fetched content), and the Tauri webview has IPC access — so an unescaped
 * `<img onerror=invoke(...)>` would be a real XSS→RCE. We therefore HTML-escape
 * the text FIRST, then layer only known-safe markdown tags on top, and
 * scheme-allowlist link hrefs (a denylist would miss `javascript:`/`data:`
 * variants).
 */

/** Escape the five HTML-significant characters. `&` MUST be replaced first. */
export function escapeHtml(s: string): string {
  return s
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;")
    .replace(/'/g, "&#39;");
}

/**
 * Return `url` if it uses an allowlisted scheme (http/https/mailto) or is a
 * relative reference (`/`, `.`, `#`, `?`); otherwise return `"#"`. Reject
 * `javascript:`, `data:`, `vbscript:`, etc. Input is assumed already
 * HTML-escaped (so it is safe to interpolate into an attribute value).
 */
export function safeUrl(url: string): string {
  const trimmed = url.trim();
  // Reject protocol-relative `//host` (it inherits the page scheme and points
  // off-origin) before the relative-reference allowance below.
  if (trimmed.startsWith("//")) return "#";
  if (/^(https?:|mailto:)/i.test(trimmed) || /^[/.#?]/.test(trimmed)) {
    return trimmed;
  }
  return "#";
}

/**
 * Render inline markdown (bold, italic, code, links) to a SAFE HTML string.
 * The raw text is escaped before any markup is applied, so the only HTML that
 * survives is the small set of tags inserted here.
 */
export function renderInline(text: string): string {
  return escapeHtml(text)
    // Bold **text** or __text__
    .replace(/\*\*(.+?)\*\*/g, '<strong class="md-bold">$1</strong>')
    .replace(/__(.+?)__/g, '<strong class="md-bold">$1</strong>')
    // Italic *text* or _text_
    .replace(/(?<!\*)\*(?!\*)(.+?)(?<!\*)\*(?!\*)/g, '<em class="md-italic">$1</em>')
    .replace(/(?<!_)_(?!_)(.+?)(?<!_)_(?!_)/g, '<em class="md-italic">$1</em>')
    // Inline code `code`
    .replace(/`([^`]+)`/g, '<code class="code-inline">$1</code>')
    // Links [text](url) — href is scheme-allowlisted; text is already escaped.
    .replace(
      /\[([^\]]+)\]\(([^)]+)\)/g,
      (_m, label: string, url: string) =>
        `<a class="md-link" href="${safeUrl(url)}" target="_blank" rel="noopener noreferrer">${label}</a>`,
    );
}
