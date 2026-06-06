/**
 * Permission store — manages pending permission requests.
 * When the engine needs user approval for a tool execution,
 * the permission dialog is shown and the response is sent back.
 */
import { createStore } from "solid-js/store";

export interface PendingPermission {
  permissionId: string;
  toolName: string;
  action: string;
  resources: string[];
  preview: string;
  sessionId: string;
}

interface PermissionState {
  pending: PendingPermission | null;
}

const [permissionStore, setPermissionStore] = createStore<PermissionState>({
  pending: null,
});

function addPendingPermission(perm: PendingPermission) {
  setPermissionStore("pending", perm);
}

async function replyPermission(reply: "once" | "always" | "reject") {
  const perm = permissionStore.pending;
  if (!perm) return;

  try {
    const { invoke } = await import("@tauri-apps/api/core");
    await invoke("reply_permission", {
      sessionId: perm.sessionId,
      permissionId: perm.permissionId,
      reply,
    });
  } catch (e) {
    console.error("Failed to reply permission:", e);
  }

  setPermissionStore("pending", null);
}

function clearPendingPermission() {
  setPermissionStore("pending", null);
}

export {
  permissionStore,
  setPermissionStore,
  addPendingPermission,
  replyPermission,
  clearPendingPermission,
};
