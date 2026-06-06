import { describe, expect, it } from "vitest";
import { escapeHtml, renderInline, safeUrl } from "./sanitize";

describe("escapeHtml", () => {
  it("escapes all five HTML-significant characters", () => {
    expect(escapeHtml(`<a href="x" foo='y'>&`)).toBe(
      "&lt;a href=&quot;x&quot; foo=&#39;y&#39;&gt;&amp;",
    );
  });

  it("escapes & before the other entities (no double-escaping)", () => {
    // If `&` were not replaced first, `<` → `&lt;` then the `&` would become
    // `&amp;lt;`. Correct output keeps a single entity per character.
    expect(escapeHtml("<")).toBe("&lt;");
    expect(escapeHtml("&lt;")).toBe("&amp;lt;");
  });
});

describe("safeUrl", () => {
  it("allows http/https/mailto and relative references", () => {
    expect(safeUrl("https://example.com")).toBe("https://example.com");
    expect(safeUrl("http://example.com")).toBe("http://example.com");
    expect(safeUrl("mailto:a@b.com")).toBe("mailto:a@b.com");
    expect(safeUrl("/local/path")).toBe("/local/path");
    expect(safeUrl("./rel")).toBe("./rel");
    expect(safeUrl("#anchor")).toBe("#anchor");
  });

  it("rejects javascript/data/vbscript and other schemes", () => {
    expect(safeUrl("javascript:alert(1)")).toBe("#");
    expect(safeUrl("  javascript:alert(1)")).toBe("#");
    expect(safeUrl("JaVaScRiPt:alert(1)")).toBe("#");
    expect(safeUrl("data:text/html,<script>1</script>")).toBe("#");
    expect(safeUrl("vbscript:msgbox(1)")).toBe("#");
  });
});

describe("renderInline (XSS)", () => {
  it("escapes raw HTML so injected tags/handlers cannot execute", () => {
    const out = renderInline(`<img src=x onerror="alert(1)">`);
    expect(out).not.toContain("<img");
    expect(out).toContain("&lt;img");
    expect(out).not.toContain("onerror=\"alert");
  });

  it("neutralizes a javascript: link to href=\"#\"", () => {
    const out = renderInline("[click](javascript:alert(1))");
    expect(out).toContain('href="#"');
    expect(out).not.toContain("javascript:");
  });

  it("an attribute-breakout attempt cannot escape the href attribute", () => {
    // The `"` is escaped to &quot; before the link regex runs, so the crafted
    // onmouseover cannot become a real attribute.
    const out = renderInline('[x](https://e.com" onmouseover="alert(1))');
    expect(out).not.toContain('onmouseover="alert');
    expect(out).toContain("&quot;");
  });

  it("still renders the safe markdown subset", () => {
    expect(renderInline("**bold**")).toBe('<strong class="md-bold">bold</strong>');
    expect(renderInline("`code`")).toBe('<code class="code-inline">code</code>');
    expect(renderInline("[ok](https://e.com)")).toContain(
      'href="https://e.com"',
    );
  });
});
