/**
 * PermissionDialog — Modal for tool permission requests.
 * Shows tool name, action, resources, and preview of what will happen.
 * Keyboard shortcuts: y = once, a = always, n = reject.
 */
import { createSignal, onMount, onCleanup } from "solid-js";
import { permissionStore, replyPermission } from "../stores/permissions";

export default function PermissionDialog() {
  const [feedback, setFeedback] = createSignal("");

  const handleKeyDown = (e: KeyboardEvent) => {
    // Don't let the single-key shortcuts fire while the user is typing a denial
    // reason (otherwise the letters "n"/"y"/"a" would trigger a decision).
    const tag = (e.target as HTMLElement | null)?.tagName;
    if (tag === "INPUT" || tag === "TEXTAREA") {
      if (e.key === "Escape") {
        e.preventDefault();
        replyPermission("reject", feedback());
      }
      return;
    }
    if (e.key === "y") {
      e.preventDefault();
      replyPermission("once");
    } else if (e.key === "a") {
      e.preventDefault();
      replyPermission("always");
    } else if (e.key === "n" || e.key === "Escape") {
      e.preventDefault();
      replyPermission("reject", feedback());
    }
  };

  onMount(() => {
    document.addEventListener("keydown", handleKeyDown);
  });

  onCleanup(() => {
    document.removeEventListener("keydown", handleKeyDown);
  });

  const perm = () => permissionStore.pending;

  return (
    <div class="permission-overlay" id="permission-overlay">
      <div class="permission-dialog" id="permission-dialog">
        <div class="permission-dialog-header">
          <div class="perm-icon">🔐</div>
          <h3>Permission Required</h3>
        </div>

        <div class="permission-dialog-body">
          <div class="permission-detail">
            <div class="detail-label">Tool</div>
            <div class="detail-value">{perm()?.toolName}</div>
          </div>

          <div class="permission-detail">
            <div class="detail-label">Action</div>
            <div class="detail-value">{perm()?.action}</div>
          </div>

          <div class="permission-detail">
            <div class="detail-label">Resources</div>
            <div class="detail-value">
              {perm()?.resources.join("\n")}
            </div>
          </div>

          <div class="permission-detail">
            <div class="detail-label">Preview</div>
            <div class="detail-value">{perm()?.preview}</div>
          </div>

          <div class="permission-detail">
            <div class="detail-label">Reason (optional)</div>
            <input
              class="detail-value perm-feedback-input"
              type="text"
              placeholder="Sent to the model when you reject…"
              value={feedback()}
              onInput={(e) => setFeedback(e.currentTarget.value)}
              id="perm-feedback-input"
            />
          </div>
        </div>

        <div class="permission-dialog-footer">
          <button
            class="perm-btn reject"
            onClick={() => replyPermission("reject", feedback())}
            id="perm-reject-btn"
          >
            Reject <span class="kbd">n</span>
          </button>
          <button
            class="perm-btn once"
            onClick={() => replyPermission("once")}
            id="perm-once-btn"
          >
            Allow Once <span class="kbd">y</span>
          </button>
          <button
            class="perm-btn always"
            onClick={() => replyPermission("always")}
            id="perm-always-btn"
          >
            Always Allow <span class="kbd">a</span>
          </button>
        </div>
      </div>
    </div>
  );
}
