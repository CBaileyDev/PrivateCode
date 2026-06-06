# Context Engine Specification

The Context Engine assembles the **System Context** — instructions (`AGENTS.md`), environment, date/time, the repo map — and keeps it fresh across a long session **without resending it every turn and without breaking the provider's prompt cache**. It does this with a **Context Epoch**: an immutable baseline plus delta updates. This is the single most cache-sensitive subsystem in the product, so its model mirrors the reference (`Reference/packages/core/src/system-context/`, `session/context-epoch.ts`, `CONTEXT.md`) precisely.

> **Corrections from the original spec:** (1) `ContextSource::load` returned `Result<Value, String>` with no way to distinguish *loaded-empty* from *transiently-unavailable*; restored to `Loaded | Unavailable`. (2) The reconcile enum `Unchanged | AppendUpdateMessage | TriggerCompaction` **mislabeled** an `AGENTS.md` change as "compaction" — an instruction change is an **Updated** (mid-conversation message, baseline preserved), not a baseline replacement and certainly not compaction. (3) Persistence is the **three-operation** model (see `database.md §2`), not one upsert. (4) The mid-conversation update lowers to a `{role:"system"}` message (2026 Anthropic beta), not inline `"System Update:"` text.

---

## 1. Terms

- **System Context** — the structured facts shown to the model as initial instructions + chronological updates.
- **Context Source** — one independently-observed typed value (`core/date`, `core/environment`, `core/instructions`, `code/repomap`) with a stable key, a **codec** (encode + equivalence), an **infallible-by-design loader** returning `Loaded | Unavailable`, and pure baseline/update/removal renderers.
- **Context Snapshot** — the codec-encoded last-observed value of every active source (model-hidden; persisted as the epoch `snapshot`).
- **Baseline System Context** — the full system context rendered at the start of an epoch. This is the **cached prefix**; keeping it byte-stable across turns is the entire point of the epoch.
- **Context Epoch** — the span during which the baseline is immutable. It ends only on a **Replacement** (agent switch, model switch, completed compaction, or a codec-incompatible source change) — **not** on an ordinary instruction/date change.
- **Mid-Conversation System Message** — a durable chronological `{role:"system"}` message stating the new effective state of a changed source. It updates the model *without* mutating the cached baseline prefix.

---

## 2. The ContextSource trait

```rust
/// A source either produced a value, or is transiently unavailable. The two are
/// NOT the same: a loaded-empty value renders a removal; an Unavailable source
/// retains the prior snapshot and blocks epoch replacement until it recovers.
pub enum SourceLoad {
    Loaded(serde_json::Value),
    Unavailable, // e.g. a file is locked / a probe failed — keep prior state, emit nothing
}

pub enum SourceCompare {
    Unchanged,
    Updated,       // value differs but is codec-compatible -> render an update
    Incompatible,  // value changed in a way the codec can't delta -> forces epoch replacement
}

#[async_trait]
pub trait ContextSource: Send + Sync {
    fn key(&self) -> &str;

    /// Infallible by design: failures map to Unavailable, never an error.
    async fn load(&self, location: &Location) -> SourceLoad;

    /// Codec equivalence over the source's own value type (NOT raw-JSON equality).
    fn compare(&self, previous: &serde_json::Value, current: &serde_json::Value) -> SourceCompare;

    /// Codec encode for the persisted snapshot.
    fn encode(&self, value: &serde_json::Value) -> serde_json::Value;

    fn render_baseline(&self, current: &serde_json::Value) -> String;
    fn render_update(&self, previous: &serde_json::Value, current: &serde_json::Value) -> String;
    fn render_removal(&self, previous: &serde_json::Value) -> Option<String>;
}
```

The snapshot value per source is `{ value, removed: bool }` — `removed` records a source that loaded-empty so a later re-appearance renders correctly.

---

## 3. Reconciliation

At a **safe provider-turn boundary**, the engine loads every source and compares against the stored snapshot. The per-session result is one of:

```rust
pub enum Reconcile {
    Unchanged,
    Updated   { text: String, snapshot: SystemContextSnapshot }, // mid-conv message; baseline preserved
    ReplacementReady { generation: Generation },                 // new baseline; ends the epoch
    ReplacementBlocked,                                          // an Unavailable source blocks replacement; retry next boundary
}
```

Decision (mirrors the reference `prepare`):

```
load all sources  +  load stored epoch
├─ no stored epoch ───────────────► initialize: insert(baseline, snapshot, revision 0)
└─ stored epoch:
   replacingAgent = (stored.agent != current.agent)
   if  replacement_seq IS NULL  and not replacingAgent → reconcile(value, snapshot)
   else                                                 → replace(value, snapshot)   // forced
   then:
     ReplacementBlocked & replacingAgent → fence(revision); return AgentReplacementBlocked
     Unchanged | ReplacementBlocked      → fence(revision); keep current baseline
     ReplacementReady{gen}               → replace(baseline, baseline_seq, revision); new epoch
     Updated{text, snapshot}             → publish ContextUpdated(text) AND advance(snapshot) in ONE txn;
                                           baseline UNCHANGED (cache prefix preserved)
```

- **`reconcile`** returns `Updated` when any source changed compatibly (date ticked, env changed, `AGENTS.md` edited), `ReplacementReady` when a source changed `Incompatible`-ly, `ReplacementBlocked` when a needed source is `Unavailable`, else `Unchanged`.
- **`replace`** (the forced path, taken when an agent/model switch is pending) always rebuilds the baseline.
- The three DB writes (`insert` / `advance` / `replace`) and the `requestReplacement`/`fence` CAS forms are specified in `database.md §2`. The **Updated** path commits the durable mid-conversation message and the snapshot advance **atomically** — they must not diverge.

---

## 4. Built-in Context Sources

### A. `core/environment`
Working directory, workspace root, OS/platform, VCS availability + dirty state.
- **Baseline:**
  ```
  Useful info about the running environment:
    Working directory: /Users/carter/Downloads/PrivateCode
    Workspace root: /Users/carter/Downloads/PrivateCode
    VCS: Git (dirty)
    Platform: macOS
  ```
- **Update:** `Working directory changed to: <new>` (the bare effective state, no meta-prefix).

### B. `core/date`
- **Update render:** `Today's date is now: 2026-06-06.` — the **bare effective state**, not `"System Update: the date is …"`. (The reference renders the new state, not a diff narration.)

### C. `core/instructions` (`AGENTS.md`)
Scans upward from the active directory for the nearest `AGENTS.md`.
- **Baseline:** `Instructions from /path/AGENTS.md:\n<content>`
- **Update:** `These instructions replace all previously loaded ambient instructions:\n<new_content>` — routed through **Updated** (mid-conversation message, baseline preserved), **never** treated as compaction.
- **Removal:** `Previously loaded instructions no longer apply.`
- **Trust:** `AGENTS.md` content (especially from a cloned repo) is instruction *context* but **never** widens the permission ruleset — see `security.md` T1.

### D. `code/repomap` (Phase 4)
A compact structural overview from the code-intel index, capped to a token budget, injected as a source so its updates ride the same epoch machinery.

---

## 5. Lowering to the provider (cache discipline)

- The **baseline** is sent as the top-level `system` block with the cache breakpoint on it. It is byte-stable for the life of the epoch.
- A **Mid-Conversation System Message** is appended to `messages[]` as a `{role:"system"}` entry (2026 Anthropic beta `mid-conversation-system-2026-04-07`); on providers without it, fall back to a wrapped text block in a user turn. Appending a chronological message — instead of editing the cached `system` prefix — is exactly what preserves the prompt cache (any byte change in the prefix invalidates the whole cache).
- A **Replacement** starts a new epoch: a fresh baseline (new cached prefix) is written, and the prior mid-conversation messages remain in the durable transcript.

---

## 6. Compaction (distinct from a plain Replacement)

Compaction is *summarize-and-replace*: when the projected request would exceed the budget, the engine summarizes older complete turns into a rolling summary and starts a **Replacement** epoch whose history drops the provider-native assistant/reasoning/tool messages across the boundary (avoiding signature/encrypted-reasoning replay failures). The full transcript stays durable; only the *active model representation* is compacted. `/compact` is the manual entry onto this same machinery. See `plan.md §6D` for the budget trigger and the overflow-400 fallback.
