/**
 * ConnectionStatus — Visual indicator for engine connection state.
 * Shows connected (green), reconnecting (pulsing amber), or offline (red).
 */
import { sessionStore } from "../stores/session";

export default function ConnectionStatus() {
  return (
    <div class="connection-status" id="connection-status">
      <span class={`connection-dot ${sessionStore.connectionStatus}`} />
      <span>
        {sessionStore.connectionStatus === "connected"
          ? "In-Process Engine"
          : sessionStore.connectionStatus === "reconnecting"
          ? "Reconnecting..."
          : "Offline"}
      </span>
    </div>
  );
}
