import { describe, expect, it } from "vitest";
import { highlight } from "./highlight";

describe("highlight (Shiki, offline JS engine)", () => {
  it("produces Shiki line markup for a known language", async () => {
    const html = await highlight("const x = 1;", "typescript");
    expect(html).toContain("shiki");
    expect(html).toContain('class="line"');
  });

  it("resolves a language alias (ts → typescript)", async () => {
    const html = await highlight("const x = 1;", "ts");
    expect(html).toContain('class="line"');
  });

  it("falls back to plaintext for an unknown language without throwing", async () => {
    const html = await highlight("hello world", "brainfuck");
    expect(html).toContain('class="line"'); // wrapped, just no token colours
  });

  it("escapes code content so the innerHTML is XSS-safe", async () => {
    const html = await highlight("<script>alert(1)</script>", "text");
    // No raw executable tag; the `<` is HTML-escaped (Shiki uses the hex entity
    // &#x3C;, others use &lt; — accept either).
    expect(html).not.toContain("<script");
    expect(html).toMatch(/&(lt|#x3c|#60);script/i);
  });
});
