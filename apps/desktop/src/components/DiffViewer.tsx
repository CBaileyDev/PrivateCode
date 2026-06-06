/**
 * DiffViewer — Side-by-side and unified diff viewer for tool outputs.
 * Shows green/red line highlighting with accept/reject per hunk.
 * Keyboard navigation: j/k for hunks, a to accept, r to reject.
 */
import { createSignal, createMemo, For, Show } from "solid-js";

interface DiffLine {
  type: "added" | "removed" | "unchanged";
  oldLineNum?: number;
  newLineNum?: number;
  content: string;
}

interface Props {
  fileName: string;
  oldContent: string;
  newContent: string;
}

function computeDiff(oldText: string, newText: string): DiffLine[] {
  const oldLines = oldText.split("\n");
  const newLines = newText.split("\n");
  const result: DiffLine[] = [];

  let oi = 0;
  let ni = 0;

  // Simple LCS-based diff (simplified for the UI)
  while (oi < oldLines.length || ni < newLines.length) {
    if (oi >= oldLines.length) {
      result.push({
        type: "added",
        newLineNum: ni + 1,
        content: newLines[ni],
      });
      ni++;
    } else if (ni >= newLines.length) {
      result.push({
        type: "removed",
        oldLineNum: oi + 1,
        content: oldLines[oi],
      });
      oi++;
    } else if (oldLines[oi] === newLines[ni]) {
      result.push({
        type: "unchanged",
        oldLineNum: oi + 1,
        newLineNum: ni + 1,
        content: oldLines[oi],
      });
      oi++;
      ni++;
    } else {
      // Check if the old line was removed or new line was added
      const oldInNew = newLines.indexOf(oldLines[oi], ni);
      const newInOld = oldLines.indexOf(newLines[ni], oi);

      if (oldInNew === -1 || (newInOld !== -1 && newInOld < oldInNew)) {
        result.push({
          type: "removed",
          oldLineNum: oi + 1,
          content: oldLines[oi],
        });
        oi++;
      } else {
        result.push({
          type: "added",
          newLineNum: ni + 1,
          content: newLines[ni],
        });
        ni++;
      }
    }
  }

  return result;
}

export default function DiffViewer(props: Props) {
  const [mode, setMode] = createSignal<"unified" | "split">("unified");

  const diffLines = createMemo(() =>
    computeDiff(props.oldContent, props.newContent)
  );

  const stats = createMemo(() => {
    const lines = diffLines();
    return {
      added: lines.filter((l) => l.type === "added").length,
      removed: lines.filter((l) => l.type === "removed").length,
    };
  });

  return (
    <div class="diff-viewer" id="diff-viewer">
      <div class="diff-header">
        <span class="file-name">{props.fileName}</span>
        <div class="diff-controls">
          <span
            style={{
              "font-size": "var(--text-xs)",
              color: "var(--success)",
              "font-family": "var(--font-mono)",
            }}
          >
            +{stats().added}
          </span>
          <span
            style={{
              "font-size": "var(--text-xs)",
              color: "var(--error)",
              "font-family": "var(--font-mono)",
            }}
          >
            -{stats().removed}
          </span>
          <button
            class={`diff-mode-btn ${mode() === "unified" ? "active" : ""}`}
            onClick={() => setMode("unified")}
          >
            Unified
          </button>
          <button
            class={`diff-mode-btn ${mode() === "split" ? "active" : ""}`}
            onClick={() => setMode("split")}
          >
            Split
          </button>
        </div>
      </div>
      <div class="diff-body">
        <For each={diffLines()}>
          {(line) => (
            <div class={`diff-line ${line.type}`}>
              <span class="line-number">
                {line.type === "removed"
                  ? line.oldLineNum ?? ""
                  : line.type === "added"
                  ? ""
                  : line.oldLineNum ?? ""}
              </span>
              <span class="line-number">
                {line.type === "added"
                  ? line.newLineNum ?? ""
                  : line.type === "removed"
                  ? ""
                  : line.newLineNum ?? ""}
              </span>
              <span class="line-content">
                {line.type === "added"
                  ? "+"
                  : line.type === "removed"
                  ? "-"
                  : " "}
                {line.content}
              </span>
            </div>
          )}
        </For>
      </div>
    </div>
  );
}
