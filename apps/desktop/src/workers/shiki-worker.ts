/**
 * Shiki syntax highlighting Web Worker.
 *
 * Receives messages with `{code, language}` and returns `{html}`.
 * Runs highlighting off the main thread to maintain 60fps.
 *
 * In production, this would use the full Shiki library. For now,
 * it does basic tokenization with regex-based highlighting.
 */

// Cache for highlighted results keyed by content hash
const cache = new Map<string, string>();

function simpleHash(str: string): string {
  let hash = 0;
  for (let i = 0; i < str.length; i++) {
    const char = str.charCodeAt(i);
    hash = ((hash << 5) - hash + char) | 0;
  }
  return hash.toString(36);
}

// Basic keyword sets for common languages
const keywords: Record<string, Set<string>> = {
  rust: new Set([
    "fn", "let", "mut", "const", "pub", "use", "mod", "struct", "enum",
    "impl", "trait", "type", "where", "async", "await", "match", "if",
    "else", "for", "while", "loop", "return", "break", "continue",
    "self", "Self", "super", "crate", "true", "false", "as", "in",
    "ref", "move", "dyn", "static", "unsafe", "extern",
  ]),
  typescript: new Set([
    "const", "let", "var", "function", "class", "interface", "type",
    "enum", "import", "export", "from", "default", "return", "if",
    "else", "for", "while", "do", "switch", "case", "break", "continue",
    "try", "catch", "finally", "throw", "new", "this", "super",
    "extends", "implements", "async", "await", "yield", "true", "false",
    "null", "undefined", "typeof", "instanceof", "in", "of", "as",
  ]),
  javascript: new Set([
    "const", "let", "var", "function", "class", "import", "export",
    "from", "default", "return", "if", "else", "for", "while", "do",
    "switch", "case", "break", "continue", "try", "catch", "finally",
    "throw", "new", "this", "super", "extends", "async", "await",
    "yield", "true", "false", "null", "undefined", "typeof",
    "instanceof", "in", "of",
  ]),
  python: new Set([
    "def", "class", "import", "from", "as", "return", "if", "elif",
    "else", "for", "while", "break", "continue", "pass", "try",
    "except", "finally", "raise", "with", "yield", "lambda", "and",
    "or", "not", "in", "is", "True", "False", "None", "self", "async",
    "await",
  ]),
};

function escapeHtml(text: string): string {
  return text
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;");
}

function highlightCode(code: string, language: string): string {
  const lang = language.toLowerCase();
  const kws = keywords[lang] || keywords["typescript"] || new Set();

  // Basic tokenization
  const lines = code.split("\n");
  return lines
    .map((line) => {
      let result = "";
      let i = 0;

      while (i < line.length) {
        // String literals
        if (line[i] === '"' || line[i] === "'" || line[i] === "`") {
          const quote = line[i];
          let j = i + 1;
          while (j < line.length && line[j] !== quote) {
            if (line[j] === "\\") j++;
            j++;
          }
          result += `<span style="color:#a3be8c">${escapeHtml(line.slice(i, j + 1))}</span>`;
          i = j + 1;
          continue;
        }

        // Line comments
        if (line.slice(i, i + 2) === "//" || (lang === "python" && line[i] === "#")) {
          result += `<span style="color:#636f88">${escapeHtml(line.slice(i))}</span>`;
          break;
        }

        // Numbers
        if (/\d/.test(line[i]) && (i === 0 || /[\s(,=+\-*/]/.test(line[i - 1]))) {
          let j = i;
          while (j < line.length && /[\d._xXa-fA-F]/.test(line[j])) j++;
          result += `<span style="color:#b48ead">${escapeHtml(line.slice(i, j))}</span>`;
          i = j;
          continue;
        }

        // Words (keywords and identifiers)
        if (/[a-zA-Z_]/.test(line[i])) {
          let j = i;
          while (j < line.length && /[a-zA-Z0-9_]/.test(line[j])) j++;
          const word = line.slice(i, j);
          if (kws.has(word)) {
            result += `<span style="color:#81a1c1;font-weight:500">${escapeHtml(word)}</span>`;
          } else if (word[0] === word[0].toUpperCase() && word.length > 1) {
            result += `<span style="color:#8fbcbb">${escapeHtml(word)}</span>`;
          } else {
            result += escapeHtml(word);
          }
          i = j;
          continue;
        }

        // Operators
        if (/[=+\-*/<>!&|^~%]/.test(line[i])) {
          result += `<span style="color:#81a1c1">${escapeHtml(line[i])}</span>`;
          i++;
          continue;
        }

        // Everything else
        result += escapeHtml(line[i]);
        i++;
      }

      return result;
    })
    .join("\n");
}

// Worker message handler
self.onmessage = (e: MessageEvent) => {
  const { code, language, id } = e.data;
  const cacheKey = simpleHash(code + language);

  let html = cache.get(cacheKey);
  if (!html) {
    html = highlightCode(code, language);
    cache.set(cacheKey, html);
    // Keep cache bounded
    if (cache.size > 500) {
      const firstKey = cache.keys().next().value;
      if (firstKey) cache.delete(firstKey);
    }
  }

  self.postMessage({ html, id });
};
