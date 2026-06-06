# API & Wire Protocol Specification

Private Code's daemon serves clients (TUI, editor extensions, and the GUI **in its loopback-attach mode**) over a local transport. The desktop GUI's **default** path is in-process Tauri Channels (no socket — see `plan.md §10`); this spec governs the network transport used by the TUI, editors, headless automation, and multi-device attach.

> **Corrections from the original spec:** (1) The original pushed one **JSON-RPC `event.delta` per token** and replayed from a bounded **last-1000 in-memory buffer**. That conflates a request/response RPC frame with a high-frequency stream and loses history across a daemon restart or a >1000-event turn. This spec **splits** the two channels and replays from a **durable cursor**. (2) Auth was required on REST but **not** the WS upgrade or SSE — fixed. (3) The permission reply was a boolean `approved` — it cannot express "always"; replaced with the `once|always|reject` enum.

---

## 1. Channels

| Direction | Channel | Framing |
|---|---|---|
| Client → Server (commands) | WS request/response **or** REST | JSON-RPC 2.0 request with `id`, or a REST call. Each is a discrete command (`session.prompt`, `session.abort`, `permission.reply`, …). |
| Server → Client (events) | WS or SSE | A **typed event stream** tagged by `type`, **not** JSON-RPC notifications. Each event is classified **Durable** or **Ephemeral** (§3). |

This separation matters: commands are low-frequency and want request/response semantics; events are high-frequency and want a stream with replay. Forcing token deltas through JSON-RPC notifications adds envelope overhead per token and offers no durability model.

### Authentication (all surfaces)
Every request — REST, **the WS upgrade**, and **the SSE stream** — must carry `Authorization: Bearer <daemon_token>`. The daemon additionally **validates `Origin`** against an allowlist and **rejects non-loopback `Host`** headers (WebSocket upgrades are not CORS-preflighted, so loopback binding alone is not a boundary — see `security.md` T5).

---

## 2. Client → Server commands

JSON-RPC 2.0 over WS (or the equivalent REST route). Examples:

```json
{ "jsonrpc": "2.0", "id": 1, "method": "session.prompt",
  "params": { "session_id": "sess_…", "prompt": "Build an HTTP router", "delivery": "steer" } }
```

`delivery` is `"steer" | "queue"` (steer promotes at the next safe boundary, including mid-drain continuation; queue is FIFO).

```json
{ "jsonrpc": "2.0", "id": 2, "method": "session.abort",  "params": { "session_id": "sess_…" } }
{ "jsonrpc": "2.0", "id": 3, "method": "permission.reply",
  "params": { "session_id": "sess_…", "permission_id": "perm_…", "reply": "always", "feedback": null } }
```

**Permission reply** carries the single `permission_id` minted in the `tool.permission_required` event and a three-value `reply`: `"once" | "always" | "reject"` (with optional `feedback` text on reject that is fed back to the model). A boolean cannot express "always" (save the rule), which both the permission engine and the GUI's "Always Allow" button require.

---

## 3. Server → Client events

Every event has `type`, `session_id`, a monotonically-increasing `seq` (the durable cursor; see §4), and a payload. Events are classified:

- **Ephemeral** — live-only deltas. **Coalesced server-side** (~8–16 ms or N tokens) before fan-out, **never persisted**, and they **do not advance the replay cursor**. A reconnecting client does not replay them; it gets the durable boundary instead.
- **Durable** — the replayable boundary. Persisted (or reconstructable from the DB) and assigned a durable `seq`.

| Event `type` | Class | Notes |
|---|---|---|
| `session.created` | Durable | new session row |
| `message.delta` | **Ephemeral** | text / reasoning / tool-input token deltas (coalesced) |
| `message.completed` | Durable | final message persisted + usage (the replayable boundary for a turn) |
| `tool.requested` | Durable | a tool call was parsed (name, args, `tool_call_id`) |
| `tool.permission_required` | Durable | mints `permission_id`; carries action/resource/preview |
| `tool.output` | Durable | tool result (or a managed-output pointer) |
| `checkpoint.created` | Durable | `{ tree_hash, tool_name, kind }` (see `checkpointing.md`) |
| `usage.updated` | Durable | running token/cost totals (its own event, not folded into a delta) |
| `error` | Durable | `{ code, message, retryable }` |

Example durable + ephemeral pair:

```json
{ "type": "message.delta", "session_id": "sess_…",
  "delta": { "kind": "text", "text": "Sure, starting with Cargo.toml" } }      // Ephemeral, coalesced, no seq advance

{ "type": "tool.permission_required", "session_id": "sess_…", "seq": 412,
  "permission_id": "perm_…", "tool_name": "write_file", "action": "write_file",
  "resources": ["Cargo.toml"], "preview": "…diff…" }                            // Durable

{ "type": "message.completed", "session_id": "sess_…", "seq": 418,
  "message_id": "msg_…", "usage": { "input_tokens": 1234, "output_tokens": 567,
  "cache_read_tokens": 890, "cache_write_tokens": 0, "reasoning_tokens": 0, "cost": 0.0042 } }
```

> The `usage` breakdown is **non-overlapping** (`input_tokens` is non-cached); each category is priced independently. See `database.md §1.B`.

---

## 4. Attach & replay (durable cursor, not a buffer)

On (re)connect a client sends its last-seen durable `seq`; the daemon replays **durable** events `after=seq` from persistent state, then tails the live stream. Ephemeral deltas emitted during the gap are **not** replayed — the client receives the durable boundary (the completed message / current tool state) instead, which is sufficient to render correct state. This survives a daemon restart and turns with >1000 events, which the original last-1000 in-memory buffer did not.

If a client attaches mid-turn, it also receives any **pending** `tool.permission_required` (so a deadlock — client gone while a permission is pending — is recoverable) and the current `usage.updated`.

---

## 5. REST surface

All under `Authorization: Bearer <token>`. Mirrors the reference routing.

```
GET    /project                                              -> Project[]
POST   /project/init                                         -> Project

GET    /project/:projectID/session                          -> Session[]
GET    /project/:projectID/session/:sessionID               -> Session
POST   /project/:projectID/session                          -> Session (create)
DELETE /project/:projectID/session/:sessionID               -> void

POST   /project/:projectID/session/:sessionID/abort         -> void
POST   /project/:projectID/session/:sessionID/compact       -> void
POST   /project/:projectID/session/:sessionID/revert        -> Session
POST   /project/:projectID/session/:sessionID/unrevert      -> Session

GET    /project/:projectID/session/:sessionID/message       -> Message[]
POST   /project/:projectID/session/:sessionID/message       -> Message (prompt; body carries delivery: steer|queue)

POST   /project/:projectID/session/:sessionID/permission/:permissionID  -> void   (body: { reply: once|always|reject, feedback? })

GET    /project/:projectID/session/:sessionID/file/status   -> FileStatus[]
GET    /project/:projectID/session/:sessionID/checkpoint    -> Checkpoint[]

GET    /provider                                            -> Provider[]
GET    /config                                              -> Config
```

The WS command `method` names and these REST routes are 1:1 where they overlap (e.g. `permission.reply` ↔ `POST …/permission/:permissionID`), using the **same** `permission_id` identity on both surfaces.

---

## 6. SSE fallback

For clients that can't hold a WebSocket (curl scripts, simple HTTP):

- **Path:** `GET /project/:projectID/session/:sessionID/events?after=<seq>`  (Bearer token required)
- **Content-Type:** `text/event-stream`
- Emits the **Durable** event class only (deltas are WS-only), `data: <json>\n\n`, with the same durable-cursor replay semantics as §4. The client polls/streams and reconstructs live text from `message.completed` boundaries.
