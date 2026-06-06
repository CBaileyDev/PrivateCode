/**
 * MessageList — virtualized message timeline (virtua) with bottom-anchored
 * auto-scroll.
 *
 * Completed messages AND the in-flight streaming bubble are a single `data`
 * array fed to <VList>, so virtua handles dynamic heights (including the
 * streaming item growing token-by-token). Auto-scroll is deliberately dumb: on
 * any content change, if we're pinned to the bottom, scroll to the last row —
 * nothing cleverer (clever scroll-anchoring fights virtua's re-measurement).
 */
import { createMemo, createSignal, createEffect, For, Show } from "solid-js";
import { VList, type VListHandle } from "virtua/solid";
import { messageStore, type ParsedMessage } from "../stores/messages";
import MessageItem from "./MessageItem";
import StreamingText from "./StreamingText";

type Row = { kind: "msg"; msg: ParsedMessage } | { kind: "streaming" };

export default function MessageList() {
  let handle: VListHandle | undefined;
  const [atBottom, setAtBottom] = createSignal(true);

  const rows = createMemo<Row[]>(() => {
    const base: Row[] = messageStore.messages.map((msg) => ({ kind: "msg", msg }));
    if (messageStore.isStreaming) base.push({ kind: "streaming" });
    return base;
  });

  const scrollToBottom = () => {
    const n = rows().length;
    if (handle && n > 0) handle.scrollToIndex(n - 1, { align: "end" });
    setAtBottom(true);
  };

  // Stay pinned: re-run whenever the row count OR the streaming buffers change,
  // and if we were at the bottom, jump back to it after the DOM updates.
  createEffect(() => {
    rows().length;
    messageStore.streamingText;
    messageStore.streamingReasoning;
    messageStore.streamingToolCalls.size;
    if (atBottom()) requestAnimationFrame(scrollToBottom);
  });

  const handleScroll = () => {
    if (!handle) return;
    const distanceFromBottom =
      handle.scrollSize - (handle.scrollOffset + handle.viewportSize);
    setAtBottom(distanceFromBottom < 100);
  };

  return (
    <div class="message-container" style={{ position: "relative" }}>
      <VList
        ref={(h) => (handle = h)}
        data={rows()}
        onScroll={handleScroll}
        class="message-vlist"
        style={{ height: "100%" }}
        id="message-scroll-container"
      >
        {(row) => (
          <div class="message-row">
            <Show
              when={row.kind === "msg"}
              fallback={<StreamingBubble />}
            >
              <MessageItem
                message={(row as { kind: "msg"; msg: ParsedMessage }).msg}
              />
            </Show>
          </div>
        )}
      </VList>

      <Show when={!atBottom()}>
        <button
          class="scroll-to-bottom"
          onClick={scrollToBottom}
          id="scroll-to-bottom-btn"
        >
          ↓ Scroll to bottom
        </button>
      </Show>
    </div>
  );
}

/** The live, in-flight assistant message (text + reasoning + running tools). */
function StreamingBubble() {
  return (
    <div class="message-item">
      <div class="message-bubble assistant">
        <div class="message-role assistant">
          <span class="role-icon">A</span>
          <span>Assistant</span>
          <span
            style={{
              "font-size": "var(--text-xs)",
              color: "var(--text-muted)",
              "font-weight": 400,
              "font-style": "italic",
            }}
          >
            streaming...
          </span>
        </div>

        <Show when={messageStore.streamingReasoning}>
          <StreamingText text={messageStore.streamingReasoning} isReasoning={true} />
        </Show>

        <Show when={messageStore.streamingText}>
          <StreamingText text={messageStore.streamingText} isReasoning={false} />
        </Show>

        <Show when={messageStore.streamingToolCalls.size > 0}>
          <For each={[...messageStore.streamingToolCalls.entries()]}>
            {([, tc]) => (
              <div class="tool-call">
                <div class="tool-call-header">
                  <span class="tool-icon">⚡</span>
                  <span class="tool-name">{tc.name}</span>
                  <span class="tool-status running">running</span>
                </div>
                <Show when={tc.input}>
                  <div class="tool-call-body">{tc.input}</div>
                </Show>
              </div>
            )}
          </For>
        </Show>

        <Show when={!messageStore.streamingText && !messageStore.streamingReasoning}>
          <span class="streaming-cursor" />
        </Show>
      </div>
    </div>
  );
}
