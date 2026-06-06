/**
 * Pure, offline Shiki highlighter.
 *
 * Uses Shiki's fine-grained core + the **JavaScript regex engine** (no
 * Oniguruma WASM) with a fixed set of statically-imported languages and one
 * theme — so everything is bundled and runs with no network and no WASM (a
 * privacy/local-first requirement). Output HTML escapes the code content, so it
 * is safe to inject via `innerHTML` (do NOT route it back through the markdown
 * sanitizer).
 *
 * This module is pure (no Worker, no DOM) so it can be unit-tested in node and
 * reused by the worker (`shiki-worker.ts`).
 */
import { createHighlighterCore, type HighlighterCore } from "shiki/core";
import { createJavaScriptRegexEngine } from "shiki/engine/javascript";
import nord from "@shikijs/themes/nord";
import bash from "@shikijs/langs/bash";
import javascript from "@shikijs/langs/javascript";
import json from "@shikijs/langs/json";
import python from "@shikijs/langs/python";
import rust from "@shikijs/langs/rust";
import typescript from "@shikijs/langs/typescript";

const THEME = "nord";

/** Canonical language ids we ship a grammar for. */
const LOADED = new Set(["typescript", "javascript", "python", "rust", "bash", "json"]);

/** Common aliases → canonical loaded language id. */
const ALIASES: Record<string, string> = {
  ts: "typescript",
  tsx: "typescript",
  typescript: "typescript",
  js: "javascript",
  jsx: "javascript",
  mjs: "javascript",
  cjs: "javascript",
  javascript: "javascript",
  py: "python",
  python: "python",
  rs: "rust",
  rust: "rust",
  sh: "bash",
  shell: "bash",
  zsh: "bash",
  bash: "bash",
  json: "json",
};

let highlighterPromise: Promise<HighlighterCore> | null = null;

function getHighlighter(): Promise<HighlighterCore> {
  if (!highlighterPromise) {
    highlighterPromise = createHighlighterCore({
      themes: [nord],
      langs: [typescript, javascript, python, rust, bash, json],
      engine: createJavaScriptRegexEngine(),
    });
  }
  return highlighterPromise;
}

/**
 * Highlight `code` to a safe HTML string. An unknown/unsupported language falls
 * back to plaintext (still wrapped in Shiki's `.line` markup) rather than
 * throwing.
 */
export async function highlight(code: string, language: string): Promise<string> {
  const canonical = ALIASES[(language ?? "").toLowerCase()];
  const lang = canonical && LOADED.has(canonical) ? canonical : "text";
  const hl = await getHighlighter();
  try {
    return hl.codeToHtml(code, { lang, theme: THEME });
  } catch {
    return hl.codeToHtml(code, { lang: "text", theme: THEME });
  }
}
