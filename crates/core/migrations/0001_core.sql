-- 0001_core.sql: Schema migration for core tables

CREATE TABLE project (
    id          TEXT PRIMARY KEY NOT NULL,
    name        TEXT NOT NULL,
    directory   TEXT NOT NULL,
    created_at  INTEGER NOT NULL
);

CREATE TABLE session (
    id                  TEXT PRIMARY KEY NOT NULL,
    project_id          TEXT NOT NULL,
    parent_id           TEXT,                          -- FK -> session.id for subagent sessions
    workspace_path      TEXT NOT NULL,
    active_directory    TEXT NOT NULL,
    title               TEXT NOT NULL,
    agent_id            TEXT NOT NULL DEFAULT 'build',
    model_config        TEXT NOT NULL,                 -- JSON: { provider_id, model_id, reasoning_effort?, temperature?, max_tokens? }
    seq_counter         INTEGER NOT NULL DEFAULT 0,    -- monotonic per-session event sequence
    cost                REAL NOT NULL DEFAULT 0.0,
    tokens_input        INTEGER NOT NULL DEFAULT 0,    -- non-cached input tokens
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

CREATE TABLE session_message (
    id          TEXT PRIMARY KEY NOT NULL,
    session_id  TEXT NOT NULL,
    seq         INTEGER NOT NULL,   -- drawn from session.seq_counter
    type        TEXT NOT NULL,      -- 'user' | 'assistant' | 'synthetic' | 'system' | 'agent_switched' | 'model_switched' | 'compaction'
    data        TEXT NOT NULL,      -- JSON: serde serialization of the protocol Message variant
    created_at  INTEGER NOT NULL,
    FOREIGN KEY(session_id) REFERENCES session(id) ON DELETE CASCADE
);

CREATE UNIQUE INDEX idx_session_message_seq             ON session_message(session_id, seq);
CREATE INDEX        idx_session_message_session_created ON session_message(session_id, created_at);

CREATE TABLE session_context_epoch (
    session_id      TEXT PRIMARY KEY NOT NULL,
    agent_id        TEXT NOT NULL DEFAULT 'build',
    baseline        TEXT NOT NULL,    -- the immutable baseline system context for this epoch
    snapshot        TEXT NOT NULL,    -- JSON: codec-encoded last-observed value per context source
    baseline_seq    INTEGER NOT NULL, -- seq at which this baseline took effect
    replacement_seq INTEGER,          -- seq at which a pending baseline replacement is scheduled, or NULL
    revision        INTEGER NOT NULL DEFAULT 0,  -- optimistic-concurrency fence (CAS target)
    FOREIGN KEY(session_id) REFERENCES session(id) ON DELETE CASCADE
);

CREATE TABLE session_input (
    id            TEXT PRIMARY KEY NOT NULL,
    session_id    TEXT NOT NULL,
    prompt        TEXT NOT NULL,
    delivery      TEXT NOT NULL,      -- 'steer' | 'queue'
    admitted_seq  INTEGER NOT NULL,   -- from session.seq_counter at admission
    promoted_seq  INTEGER,            -- from session.seq_counter when the runner promotes it
    created_at    INTEGER NOT NULL,
    FOREIGN KEY(session_id) REFERENCES session(id) ON DELETE CASCADE
);

CREATE UNIQUE INDEX idx_session_input_admitted ON session_input(session_id, admitted_seq);
CREATE UNIQUE INDEX idx_session_input_promoted ON session_input(session_id, promoted_seq);

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

CREATE TABLE permission_saved (
    project_id  TEXT NOT NULL,
    action      TEXT NOT NULL,
    resource    TEXT NOT NULL,
    created_at  INTEGER NOT NULL,
    PRIMARY KEY (project_id, action, resource),
    FOREIGN KEY(project_id) REFERENCES project(id) ON DELETE CASCADE
);
