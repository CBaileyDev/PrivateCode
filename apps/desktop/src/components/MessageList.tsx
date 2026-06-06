/**
 * MessageList — Virtualized message timeline with auto-scroll.
 * Uses a simple overflow-y: auto container with bottom-anchoring.
 * For production, replace with virtua/solid <Virtualizer>.
 */
import { For, Show, createSignal, createEffect, onMount } from "solid-js";
import { messageStore } from "../stores/messages";
import MessageItem from "./MessageItem";
import StreamingText from "./StreamingText";

export default function MessageList() {
  let containerRef: HTMLDivElement | undefined;
  const [isAtBottom, setIsAtBottom] = createSignal(true);
  const [showScrollBtn, setShowScrollBtn] = createSignal(false);

  const scrollToBottom = () => {
    if (containerRef) {
      containerRef.scrollTop = containerRef.scrollHeight;
      setIsAtBottom(true);
      setShowScrollBtn(false);
    }
  };

  const handleScroll = () => {
    if (!containerRef) return;
    const threshold = 100;
    const atBottom =
      containerRef.scrollHeight - containerRef.scrollTop - containerRef.clientHeight < threshold;
    setIsAtBottom(atBottom);
    setShowScrollBtn(!atBottom);
  };

  // Auto-scroll when new messages arrive and we're at bottom
  createEffect(() => {
    const _msgs = messageStore.messages.length;
    const _streaming = messageStore.streamingText;
    if (isAtBottom()) {
      requestAnimationFrame(() => scrollToBottom());
    }
  });

  onMount(() => {
    scrollToBottom();
  });

  return (
    <div class="message-container" style={{ position: "relative" }}>
      <div
        ref={containerRef}
        style={{ height: "100%", "overflow-y": "auto" }}
        onScroll={handleScroll}
        id="message-scroll-container"
      >
        <div class="message-list">
          <For each={messageStore.messages}>
            {(msg) => <MessageItem message={msg} />}
          </For>

          {/* Streaming message (currently being generated) */}
          <Show when={messageStore.isStreaming}>
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

                {/* Reasoning block */}
                <Show when={messageStore.streamingReasoning}>
                  <StreamingText
                    text={messageStore.streamingReasoning}
                    isReasoning={true}
                  />
                </Show>

                {/* Text content */}
                <Show when={messageStore.streamingText}>
                  <StreamingText
                    text={messageStore.streamingText}
                    isReasoning={false}
                  />
                </Show>

                {/* Active tool calls */}
                <Show when={messageStore.streamingToolCalls.size > 0}>
                  <For each={[...messageStore.streamingToolCalls.entries()]}>
                    {([id, tc]) => (
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

                <Show
                  when={!messageStore.streamingText && !messageStore.streamingReasoning}
                >
                  <span class="streaming-cursor" />
                </Show>
              </div>
            </div>
          </Show>
        </div>
      </div>

      {/* Scroll-to-bottom FAB */}
      <Show when={showScrollBtn()}>
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
