/**
 * MarkdownRenderer — Incremental streaming-friendly markdown parser.
 * Handles: headings, bold, italic, inline code, code blocks, links, lists, blockquotes.
 * Code blocks are delegated to <CodeBlock> for syntax highlighting.
 */
import { createMemo, For, Show } from "solid-js";
import CodeBlock from "./CodeBlock";
import { renderInline } from "../lib/sanitize";

interface Props {
  text: string;
}

interface RenderedBlock {
  type: "paragraph" | "heading" | "code" | "list" | "blockquote";
  level?: number;        // heading level
  language?: string;     // code block language
  content: string;
  items?: string[];      // list items
}

function parseMarkdown(text: string): RenderedBlock[] {
  const blocks: RenderedBlock[] = [];
  const lines = text.split("\n");
  let i = 0;
  let currentParagraph = "";

  const flushParagraph = () => {
    if (currentParagraph.trim()) {
      blocks.push({ type: "paragraph", content: currentParagraph.trim() });
      currentParagraph = "";
    }
  };

  while (i < lines.length) {
    const line = lines[i];

    // Code blocks (``` delimited)
    if (line.startsWith("```")) {
      flushParagraph();
      const language = line.slice(3).trim();
      const codeLines: string[] = [];
      i++;
      while (i < lines.length && !lines[i].startsWith("```")) {
        codeLines.push(lines[i]);
        i++;
      }
      blocks.push({
        type: "code",
        language: language || "text",
        content: codeLines.join("\n"),
      });
      i++; // skip closing ```
      continue;
    }

    // Headings
    const headingMatch = line.match(/^(#{1,6})\s+(.+)/);
    if (headingMatch) {
      flushParagraph();
      blocks.push({
        type: "heading",
        level: headingMatch[1].length,
        content: headingMatch[2],
      });
      i++;
      continue;
    }

    // Blockquotes
    if (line.startsWith("> ")) {
      flushParagraph();
      const quoteLines: string[] = [];
      while (i < lines.length && lines[i].startsWith("> ")) {
        quoteLines.push(lines[i].slice(2));
        i++;
      }
      blocks.push({ type: "blockquote", content: quoteLines.join("\n") });
      continue;
    }

    // List items (- or * or numbered)
    if (/^[-*]\s+/.test(line) || /^\d+\.\s+/.test(line)) {
      flushParagraph();
      const items: string[] = [];
      while (
        i < lines.length &&
        (/^[-*]\s+/.test(lines[i]) || /^\d+\.\s+/.test(lines[i]))
      ) {
        items.push(lines[i].replace(/^[-*]\s+/, "").replace(/^\d+\.\s+/, ""));
        i++;
      }
      blocks.push({ type: "list", content: "", items });
      continue;
    }

    // Empty line — flush paragraph
    if (!line.trim()) {
      flushParagraph();
      i++;
      continue;
    }

    // Regular text — accumulate into paragraph
    currentParagraph += (currentParagraph ? "\n" : "") + line;
    i++;
  }

  flushParagraph();
  return blocks;
}

export default function MarkdownRenderer(props: Props) {
  const blocks = createMemo(() => parseMarkdown(props.text));

  return (
    <div>
      <For each={blocks()}>
        {(block) => (
          <>
            <Show when={block.type === "paragraph"}>
              <p innerHTML={renderInline(block.content)} />
            </Show>

            <Show when={block.type === "heading"}>
              <div
                class={`md-heading md-h${block.level}`}
                innerHTML={renderInline(block.content)}
              />
            </Show>

            <Show when={block.type === "code"}>
              <CodeBlock
                code={block.content}
                language={block.language || "text"}
              />
            </Show>

            <Show when={block.type === "list" && block.items}>
              <ul class="md-list">
                <For each={block.items}>
                  {(item) => (
                    <li
                      class="md-list-item"
                      innerHTML={renderInline(item)}
                    />
                  )}
                </For>
              </ul>
            </Show>

            <Show when={block.type === "blockquote"}>
              <blockquote
                class="md-blockquote"
                innerHTML={renderInline(block.content)}
              />
            </Show>
          </>
        )}
      </For>
    </div>
  );
}
