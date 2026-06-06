-- Code Intelligence (Phase 4). Separate, forward-only migration — NOT part of
-- 0001. `id` is INTEGER PRIMARY KEY (a rowid alias) so the FTS5 external-content
-- rowid is stable across VACUUM (a TEXT PK is a non-alias rowid VACUUM can
-- renumber, silently desyncing the index). The logical id is `symbol_uid`.
-- See specs/database.md §1.H.

CREATE TABLE symbols (
    id            INTEGER PRIMARY KEY,        -- rowid alias: stable under VACUUM
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

-- FTS5 external-content index (content rows live in `symbols`).
CREATE VIRTUAL TABLE symbols_fts USING fts5(
    name, signature, filepath,
    content='symbols', content_rowid='id'
);

-- Canonical external-content sync triggers (all three: AFTER INSERT keeps the
-- index populated; AFTER DELETE and AFTER UPDATE must issue the 'delete' command
-- or the index keeps stale rows).
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
