/**
 * MessageItem — Renders a single message with content blocks.
 * Handles text, reasoning, tool_use, and tool_result blocks.
 */
import { Show, For, createSignal } from "solid-js";
import type { ParsedMessage, ContentBlock } from "../stores/messages";
import MarkdownRenderer from "./MarkdownRenderer";
import CodeBlock from "./CodeBlock";

interface Props {
  message: ParsedMessage;
}

export default function MessageItem(props: Props) {
  const roleLabel = () => {
    switch (props.message.role) {
      case "user": return "You";
      case "assistant": return "Assistant";
      case "system": return "System";
      default: return props.message.role;
    }
  };

  return (
    <div class="message-item" id={`msg-${props.message.id}`}>
      <div class={`message-bubble ${props.message.role}`}>
        <div class={`message-role ${props.message.role}`}>
          <span class="role-icon">
            {props.message.role === "user"
              ? "U"
              : props.message.role === "assistant"
              ? "A"
              : "S"}
          </span>
          <span>{roleLabel()}</span>
        </div>

        <div class="message-content">
          <For each={props.message.blocks}>
            {(block) => <BlockRenderer block={block} />}
          </For>
        </div>
      </div>
    </div>
  );
}

function BlockRenderer(props: { block: ContentBlock }) {
  const [reasoningOpen, setReasoningOpen] = createSignal(false);
  const [toolOpen, setToolOpen] = createSignal(true);

  return (
    <>
      <Show when={props.block.type === "text" && props.block.text}>
        <MarkdownRenderer text={props.block.text!} />
      </Show>

      <Show when={props.block.type === "reasoning" && props.block.reasoning}>
        <div class="reasoning-block">
          <div
            class="reasoning-toggle"
            onClick={() => setReasoningOpen(!reasoningOpen())}
          >
            <span class={`chevron ${reasoningOpen() ? "open" : ""}`}>▶</span>
            <span>💭 Thinking</span>
          </div>
          <Show when={reasoningOpen()}>
            <div class="reasoning-content">{props.block.reasoning}</div>
          </Show>
        </div>
      </Show>

      <Show when={props.block.type === "tool_use"}>
        <div class="tool-call">
          <div
            class="tool-call-header"
            onClick={() => setToolOpen(!toolOpen())}
            style={{ cursor: "pointer" }}
          >
            <span class="tool-icon">⚡</span>
            <span class="tool-name">{props.block.name}</span>
            <span class="tool-status complete">completed</span>
          </div>
          <Show when={toolOpen()}>
            <div class="tool-call-body">
              {typeof props.block.input === "string"
                ? props.block.input
                : JSON.stringify(props.block.input, null, 2)}
            </div>
          </Show>
        </div>
      </Show>

      <Show when={props.block.type === "tool_result"}>
        <div class="tool-call">
          <div class="tool-call-header">
            <span class="tool-icon">📤</span>
            <span>Tool Result</span>
            <span
              class={`tool-status ${props.block.is_error ? "error" : "complete"}`}
            >
              {props.block.is_error ? "error" : "success"}
            </span>
          </div>
          <div class="tool-call-body">
            {typeof props.block.content === "string"
              ? props.block.content
              : JSON.stringify(props.block.content, null, 2)}
          </div>
        </div>
      </Show>
    </>
  );
}
