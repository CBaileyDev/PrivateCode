/**
 * StreamingText — rAF-batched text renderer for streaming content.
 * Buffers incoming text changes and flushes once per animation frame
 * to maintain 60 FPS during rapid token streaming.
 */
import { createSignal, createEffect, onCleanup } from "solid-js";
import MarkdownRenderer from "./MarkdownRenderer";

interface Props {
  text: string;
  isReasoning: boolean;
}

export default function StreamingText(props: Props) {
  const [displayedText, setDisplayedText] = createSignal("");
  let pendingText = "";
  let rafId: number | null = null;

  const flushFrame = () => {
    if (pendingText !== displayedText()) {
      setDisplayedText(pendingText);
    }
    rafId = null;
  };

  createEffect(() => {
    pendingText = props.text;
    if (rafId === null) {
      rafId = requestAnimationFrame(flushFrame);
    }
  });

  onCleanup(() => {
    if (rafId !== null) {
      cancelAnimationFrame(rafId);
    }
  });

  return (
    <div class={props.isReasoning ? "reasoning-block" : ""}>
      {props.isReasoning ? (
        <div class="reasoning-content">
          {displayedText()}
          <span class="streaming-cursor" />
        </div>
      ) : (
        <div class="message-content">
          <MarkdownRenderer text={displayedText()} />
          <span class="streaming-cursor" />
        </div>
      )}
    </div>
  );
}
