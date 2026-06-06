/**
 * UsagePanel — Real-time usage and cost dashboard.
 * Shows token counts (input/output/reasoning/cache), estimated cost,
 * and active model/provider information.
 */
import { Show } from "solid-js";
import { usageStore, formatTokens, formatCost } from "../stores/usage";
import { sessionStore } from "../stores/session";

export default function UsagePanel() {
  return (
    <>
      <div class="right-panel-header">
        <h3>Details</h3>
      </div>

      <div class="right-panel-content">
        <Show
          when={sessionStore.activeSession}
          fallback={
            <div style={{ color: "var(--text-muted)", "font-size": "var(--text-sm)" }}>
              Select a session to view details
            </div>
          }
        >
          {/* Model Info */}
          <div class="usage-section">
            <div class="usage-section-title">Active Model</div>
            <div class="usage-model-info">
              <div class="model-name">{usageStore.model}</div>
              <div class="provider-name">{usageStore.provider}</div>
            </div>
          </div>

          {/* Session Usage */}
          <div class="usage-section">
            <div class="usage-section-title">Session Usage</div>
            <div class="usage-grid">
              <div class="usage-stat">
                <div class="stat-label">Input</div>
                <div class="stat-value">
                  {formatTokens(usageStore.session.input_tokens)}
                </div>
              </div>
              <div class="usage-stat">
                <div class="stat-label">Output</div>
                <div class="stat-value">
                  {formatTokens(usageStore.session.output_tokens)}
                </div>
              </div>
              <div class="usage-stat">
                <div class="stat-label">Reasoning</div>
                <div class="stat-value">
                  {formatTokens(usageStore.session.reasoning_tokens)}
                </div>
              </div>
              <div class="usage-stat">
                <div class="stat-label">Cache Read</div>
                <div class="stat-value">
                  {formatTokens(usageStore.session.cache_read_tokens)}
                </div>
              </div>
              <div class="usage-stat">
                <div class="stat-label">Cache Write</div>
                <div class="stat-value">
                  {formatTokens(usageStore.session.cache_write_tokens)}
                </div>
              </div>
              <div class="usage-stat">
                <div class="stat-label">Total Cost</div>
                <div class="stat-value cost">
                  {formatCost(usageStore.session.cost)}
                </div>
              </div>
            </div>
          </div>

          {/* Session Info */}
          <div class="usage-section">
            <div class="usage-section-title">Session Info</div>
            <div
              style={{
                "font-size": "var(--text-sm)",
                color: "var(--text-secondary)",
                display: "flex",
                "flex-direction": "column",
                gap: "var(--space-2)",
              }}
            >
              <div style={{ display: "flex", "justify-content": "space-between" }}>
                <span style={{ color: "var(--text-muted)" }}>Agent</span>
                <span style={{ "font-family": "var(--font-mono)" }}>
                  {sessionStore.activeSession?.agent_id}
                </span>
              </div>
              <div style={{ display: "flex", "justify-content": "space-between" }}>
                <span style={{ color: "var(--text-muted)" }}>Session ID</span>
                <span
                  style={{
                    "font-family": "var(--font-mono)",
                    "font-size": "var(--text-xs)",
                    "max-width": "140px",
                    overflow: "hidden",
                    "text-overflow": "ellipsis",
                  }}
                >
                  {sessionStore.activeSession?.id}
                </span>
              </div>
            </div>
          </div>

          {/* Keyboard Shortcuts */}
          <div class="usage-section">
            <div class="usage-section-title">Keyboard Shortcuts</div>
            <div
              style={{
                "font-size": "var(--text-xs)",
                color: "var(--text-muted)",
                display: "flex",
                "flex-direction": "column",
                gap: "var(--space-1)",
              }}
            >
              <ShortcutRow keys="⌘B" label="Toggle sidebar" />
              <ShortcutRow keys="⌘E" label="Toggle details" />
              <ShortcutRow keys="⌘K" label="Command palette" />
              <ShortcutRow keys="⌘↵" label="Send message" />
              <ShortcutRow keys="Esc" label="Abort / close" />
            </div>
          </div>
        </Show>
      </div>
    </>
  );
}

function ShortcutRow(props: { keys: string; label: string }) {
  return (
    <div style={{ display: "flex", "justify-content": "space-between", "align-items": "center" }}>
      <span>{props.label}</span>
      <span
        style={{
          "font-family": "var(--font-mono)",
          background: "var(--bg-surface)",
          padding: "1px 6px",
          "border-radius": "var(--radius-sm)",
          "font-size": "var(--text-xs)",
        }}
      >
        {props.keys}
      </span>
    </div>
  );
}
