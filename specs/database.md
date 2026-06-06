# Database Specification: SQLite Storage

This document defines the SQLite schema, indexes, queries, and migration discipline used by the **Private Code** daemon. It is the corrected version of the original spec; the changes that matter most are the **three-operation context-epoch model** (§1.D / §2), the **race-free sequence allocation** (§2), and the **split, forward-only migrations** (§3).

Storage engine: **sqlx + SQLite in WAL mode** (North Star §15 decision 4). All writes for a given session are serialized through that session's coordinator (one writer per session); a dedicated writer task owns the write connection, readers use a separate pool. Persist on message/tool-result **completion**, never per streaming delta (avoids WAL-fsync storms).

---

## 1. Table Definitions

### A. `project`
```sql
CREATE TABLE project (
    id          TEXT PRIMARY KEY NOT NULL,
    name        TEXT NOT NULL,
    directory   TEXT NOT NULL,
    created_at  INTEGER NOT NULL
);
```

### B. `session`
Adds a single monotonic `seq_counter` per session — the source of truth for ordering (see §2, the seq-race fix). All per-session ordinals (message `seq`, input `admitted_seq`/`promoted_seq`, epoch `baseline_seq`/`replacement_seq`) are drawn from this one counter so they are mutually comparable.

```sql
CREATE TABLE session (
    id                  TEXT PRIMARY KEY NOT NULL,
    project_id          TEXT NOT NULL,
    parent_id           TEXT,                          -- FK -> session.id for subagent sessions
    workspace_path      TEXT NOT NULL,
    active_directory    TEXT NOT NULL,
    title               TEXT NOT NULL,
    agent_id            TEXT NOT NULL DEFAULT 'build',
    model_config        TEXT NOT NULL,                 -- JSON: { provider_id, model_id, reasoning_effort?, temperature?, max_tokens? }
    seq_counter         INTEGER NOT NULL DEFAULT 0,    -- monotonic per-session event sequence (see §2)
    cost                REAL NOT NULL DEFAULT 0.0,
    tokens_input        INTEGER NOT NULL DEFAULT 0,    -- non-cached input tokens (see cost model note)
    tokens_output       INTEGER NOT NULL DEFAULT 0,
    tokens_reasoning    INTEGER NOT NULL DEFAULT 0,
    tokens_cache_read   INTEGER NOT NULL DEFAULT 0,
    tokens_cache_write  INTEGER NOT NULL DEFAULT 0,
    revert              TEXT,                          -- JSON: { message_id, tree_hash } or null
    permission          TEXT,                          -- JSON: per-session permission ruleset override or null
    created_at          INTEGER NOT NULL,
    updated_at          INTEGER NOT NULL,
    FOREIGN KEY(project_id) REFERENCES project(id) ON DELETE CASCADE,
    FOREIGN KEY(parent_id)  REFERENCES session(id) ON DELETE SET NULL
);

CREATE INDEX idx_session_project ON session(project_id);
CREATE INDEX idx_session_parent  ON session(parent_id);
```

> **Cost model note:** the token columns are a **non-overlapping** breakdown. `tokens_input` is the *non-cached* input; total prompt input = `tokens_input + tokens_cache_read + tokens_cache_write`. Each category has its own price (cache read ≈ 0.1×, cache write ≈ 1.25× base input), summed independently — never subtracted. `ModelInfo` carries `cache_read_cost` **and** `cache_write_cost`.

### C. `session_message`
The coarse `type` is a discriminant; the rich content (the reference's tagged-union part model) is serialized into `data`.

```sql
CREATE TABLE session_message (
    id          TEXT PRIMARY KEY NOT NULL,
    session_id  TEXT NOT NULL,
    seq         INTEGER NOT NULL,   -- drawn from session.seq_counter (see §2)
    type        TEXT NOT NULL,      -- 'user' | 'assistant' | 'synthetic' | 'system' | 'agent_switched' | 'model_switched' | 'compaction'
    data        TEXT NOT NULL,      -- JSON: serde serialization of the protocol Message variant (parts: text | reasoning | tool{state})
    created_at  INTEGER NOT NULL,
    FOREIGN KEY(session_id) REFERENCES session(id) ON DELETE CASCADE
);

CREATE UNIQUE INDEX idx_session_message_seq             ON session_message(session_id, seq);
CREATE INDEX        idx_session_message_session_created ON session_message(session_id, created_at);
```

`data` is **one** serde path shared with the protocol crate's `Message` type — not a parallel hand-written schema. Assistant content parts are `text | reasoning | tool{state: pending|running|completed|error}`; `synthetic`/`system` carry context-update text; `agent_switched`/`model_switched`/`compaction` are durable transition records.

### D. `session_context_epoch`
One **current** epoch per session (PK = `session_id`; this is intentional — the full transcript and prior mid-conversation system messages remain durable in `session_message`). The columns support **three distinct write operations** (§2); the single ON-CONFLICT upsert in the original spec is removed because it overwrote the baseline on every change and made the snapshot-advance path unrepresentable, defeating prompt-cache stability.

```sql
CREATE TABLE session_context_epoch (
    session_id      TEXT PRIMARY KEY NOT NULL,
    agent_id        TEXT NOT NULL DEFAULT 'build',
    baseline        TEXT NOT NULL,    -- the immutable baseline system context for this epoch (the cached prefix)
    snapshot        TEXT NOT NULL,    -- JSON: codec-encoded last-observed value per context source
    baseline_seq    INTEGER NOT NULL, -- seq at which this baseline took effect
    replacement_seq INTEGER,          -- seq at which a pending baseline replacement is scheduled, or NULL
    revision        INTEGER NOT NULL DEFAULT 0,  -- optimistic-concurrency fence (CAS target)
    FOREIGN KEY(session_id) REFERENCES session(id) ON DELETE CASCADE
);
```

### E. `session_input`
Idempotent admission. `delivery` uses the reference's **`steer` / `queue`** semantics (not `direct`/`queued`): `steer` promotes at the next safe boundary, including a continuation inside the current drain; `queue` is FIFO, opening one new activity at a time.

```sql
CREATE TABLE session_input (
    id            TEXT PRIMARY KEY NOT NULL,
    session_id    TEXT NOT NULL,
    prompt        TEXT NOT NULL,
    delivery      TEXT NOT NULL,      -- 'steer' | 'queue'
    admitted_seq  INTEGER NOT NULL,   -- from session.seq_counter at admission
    promoted_seq  INTEGER,            -- from session.seq_counter when the runner promotes it to a visible turn
    created_at    INTEGER NOT NULL,
    FOREIGN KEY(session_id) REFERENCES session(id) ON DELETE CASCADE
);

CREATE UNIQUE INDEX idx_session_input_admitted ON session_input(session_id, admitted_seq);
CREATE UNIQUE INDEX idx_session_input_promoted ON session_input(session_id, promoted_seq);
```

### F. `checkpoint`
Renamed `commit_hash` → `tree_hash` (these are git **tree** objects from `write-tree`, not commits) and adds `kind`. See [`checkpointing.md`](checkpointing.md).

```sql
CREATE TABLE checkpoint (
    id          TEXT PRIMARY KEY NOT NULL,
    session_id  TEXT NOT NULL,
    message_id  TEXT NOT NULL,
    tree_hash   TEXT NOT NULL,    -- git tree object id (NOT a commit)
    tool_name   TEXT NOT NULL,
    kind        TEXT NOT NULL,    -- 'turn_start' | 'pre_step' | 'post_step'
    created_at  INTEGER NOT NULL,
    FOREIGN KEY(session_id) REFERENCES session(id) ON DELETE CASCADE
);

CREATE INDEX idx_checkpoint_session ON checkpoint(session_id);
```

### G. `permission_saved`
Persistent "Always allow" rules. Saved rules are always interpreted as `effect = allow` (see the permission spec); the column stores no effect because there is no saved-deny.

```sql
CREATE TABLE permission_saved (
    project_id  TEXT NOT NULL,
    action      TEXT NOT NULL,
    resource    TEXT NOT NULL,
    created_at  INTEGER NOT NULL,
    PRIMARY KEY (project_id, action, resource),
    FOREIGN KEY(project_id) REFERENCES project(id) ON DELETE CASCADE
);
```

### H. `symbols` (Code Intelligence — Phase 4, separate migration)
Created in a **later** numbered migration (not `0001`), gated to Phase 4. `id` is `INTEGER PRIMARY KEY` (a rowid **alias**) so the FTS5 external-content `rowid` is stable across `VACUUM`. A separate `symbol_uid TEXT UNIQUE` carries the logical id.

```sql
CREATE TABLE symbols (
    id            INTEGER PRIMARY KEY,        -- rowid alias: stable under VACUUM (required for FTS5 external content)
    symbol_uid    TEXT NOT NULL UNIQUE,
    filepath      TEXT NOT NULL,
    name          TEXT NOT NULL,
    kind          TEXT NOT NULL,              -- 'function' | 'struct' | 'trait' | 'impl' | 'enum' | 'type_alias' | 'const' | ...
    start_line    INTEGER NOT NULL,
    start_column  INTEGER NOT NULL,
    end_line      INTEGER NOT NULL,
    end_column    INTEGER NOT NULL,
    signature     TEXT NOT NULL,
    parent_scope  TEXT
);

CREATE INDEX idx_symbols_filepath ON symbols(filepath);
CREATE INDEX idx_symbols_kind     ON symbols(kind);

-- FTS5 external-content index (content rows live in `symbols`)
CREATE VIRTUAL TABLE symbols_fts USING fts5(
    name, signature, filepath,
    content='symbols', content_rowid='id'
);

-- Canonical external-content sync triggers
CREATE TRIGGER symbols_ai AFTER INSERT ON symbols BEGIN
  INSERT INTO symbols_fts(rowid, name, signature, filepath)
  VALUES (new.id, new.name, new.signature, new.filepath);
END;
CREATE TRIGGER symbols_ad AFTER DELETE ON symbols BEGIN
  INSERT INTO symbols_fts(symbols_fts, rowid, name, signature, filepath)
  VALUES ('delete', old.id, old.name, old.signature, old.filepath);
END;
CREATE TRIGGER symbols_au AFTER UPDATE ON symbols BEGIN
  INSERT INTO symbols_fts(symbols_fts, rowid, name, signature, filepath)
  VALUES ('delete', old.id, old.name, old.signature, old.filepath);
  INSERT INTO symbols_fts(rowid, name, signature, filepath)
  VALUES (new.id, new.name, new.signature, new.filepath);
END;
```

> If a `VACUUM` is ever run without the `INTEGER PRIMARY KEY` alias, the non-alias rowid renumbers and silently desyncs the external-content index — hence the alias. Document a `rebuild` (`INSERT INTO symbols_fts(symbols_fts) VALUES('rebuild')`) as the recovery path.

---

## 2. Sequence allocation, epoch operations, and core queries

### Race-free sequence allocation (the seq fix)
**Never** compute `seq` with `(SELECT COALESCE(MAX(seq),0)+1 …)` in a deferred transaction — two concurrent writers (e.g. API-side input admission interleaving with the runner) read the same MAX and collide on the UNIQUE index, dropping a write. Allocate from the per-session counter inside `BEGIN IMMEDIATE`, on the single per-session writer:

```sql
-- BEGIN IMMEDIATE;  (acquires the write lock up front)
UPDATE session
SET seq_counter = seq_counter + 1, updated_at = strftime('%s','now')
WHERE id = ?1
RETURNING seq_counter;                       -- := :seq

INSERT INTO session_message (id, session_id, seq, type, data, created_at)
VALUES (?2, ?1, :seq, ?3, ?4, strftime('%s','now'));
-- COMMIT;
```

The same `:seq` source feeds `session_input.admitted_seq`/`promoted_seq`, so epoch `baseline_seq`/`replacement_seq` are comparable to message/input ordinals. *(Event-sourcing with a per-owner fence is the V2 direction; do not rearchitect `session_message` now.)*

### Context-epoch operations (the three-op model)
Each runs inside `BEGIN IMMEDIATE`; the UPDATE forms are **compare-and-swap on `revision`**. Zero affected rows ⇒ a concurrent writer advanced the epoch ⇒ retry at the next boundary (the `RevisionMismatch`/`ObservationBlocked` path).

**(a) Initialize** — first epoch for a session (`Unchanged`/first turn):
```sql
INSERT INTO session_context_epoch (session_id, agent_id, baseline, snapshot, baseline_seq, revision)
VALUES (?1, ?2, ?3, ?4, ?5, 0)
ON CONFLICT(session_id) DO NOTHING;
-- 0 rows ⇒ another writer created it ⇒ fall through to prepare/reconcile
```

**(b) Advance** — snapshot-only, baseline **unchanged** (the common "a date/env value changed → emit one mid-conversation system message → keep the cached prefix" path). Committed in the **same transaction** as the durable mid-conversation `system` message:
```sql
UPDATE session_context_epoch
SET snapshot = ?2, revision = revision + 1
WHERE session_id = ?1
  AND revision = ?3            -- expected_revision (CAS)
  AND replacement_seq IS NULL;
-- 0 rows ⇒ RevisionMismatch ⇒ retry
```

**(c) Replace** — new baseline (agent switch, model switch, completed compaction, or a codec-incompatible source change):
```sql
UPDATE session_context_epoch
SET baseline = ?2, agent_id = ?3, snapshot = ?4,
    baseline_seq = ?5, replacement_seq = NULL, revision = revision + 1
WHERE session_id = ?1
  AND revision = ?6;          -- expected_revision (CAS)
-- 0 rows ⇒ RevisionMismatch ⇒ retry
```

**(d) Request replacement** — schedule a baseline swap to happen at a given input seq (e.g. the user switched agent/model mid-turn; the swap lands at the next safe boundary):
```sql
UPDATE session_context_epoch
SET replacement_seq = ?2, revision = revision + 1
WHERE session_id = ?1
  AND baseline_seq < ?2
  AND (replacement_seq IS NULL OR replacement_seq < ?2);
```

**Load active epoch:**
```sql
SELECT agent_id, baseline, snapshot, baseline_seq, replacement_seq, revision
FROM session_context_epoch
WHERE session_id = ?1;
```

### Other core queries

**Create session:**
```sql
INSERT INTO session (id, project_id, workspace_path, active_directory, title, agent_id, model_config, created_at, updated_at)
VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, strftime('%s','now'), strftime('%s','now'));
```

**Update usage after a turn** (non-overlapping categories; no subtraction):
```sql
UPDATE session
SET cost = cost + ?2,
    tokens_input        = tokens_input        + ?3,   -- non-cached input
    tokens_output       = tokens_output       + ?4,
    tokens_reasoning    = tokens_reasoning    + ?5,
    tokens_cache_read   = tokens_cache_read   + ?6,
    tokens_cache_write  = tokens_cache_write  + ?7,
    updated_at = strftime('%s','now')
WHERE id = ?1;
```

**List sessions / Get checkpoints / Search symbols / Saved permissions:** (unchanged in shape; `checkpoint` now selects `tree_hash`, `kind`)
```sql
SELECT id, title, agent_id, cost, tokens_input, tokens_output, created_at, updated_at
FROM session WHERE project_id = ?1 ORDER BY updated_at DESC;

SELECT id, message_id, tree_hash, tool_name, kind, created_at
FROM checkpoint WHERE session_id = ?1 ORDER BY created_at DESC;

SELECT s.symbol_uid, s.filepath, s.name, s.kind, s.start_line, s.end_line, s.signature,
       bm25(symbols_fts) AS rank
FROM symbols_fts JOIN symbols s ON symbols_fts.rowid = s.id
WHERE symbols_fts MATCH ?1 ORDER BY rank LIMIT ?2;

INSERT OR IGNORE INTO permission_saved (project_id, action, resource, created_at)
VALUES (?1, ?2, ?3, strftime('%s','now'));

SELECT action, resource, created_at FROM permission_saved
WHERE project_id = ?1 ORDER BY created_at DESC;
```

---

## 3. Migration discipline

- **Forward-only, checksummed.** Use sqlx's `_sqlx_migrations` tracking; the runner **fails fast** on a checksum mismatch (an applied migration was edited) — applied migrations are immutable.
- **Split by phase.** `0001_core.sql` ships the session/message/epoch/input/checkpoint/permission tables (Phases 1–2). The `symbols` / `symbols_fts` code-intelligence objects ship in a **later** numbered migration gated to Phase 4 — they are not part of `0001`.
- **WAL + pragmas** are applied on connect (`journal_mode=WAL`, `foreign_keys=ON`, `busy_timeout`, `synchronous=NORMAL`).
- Never edit an applied migration; add a new one.
