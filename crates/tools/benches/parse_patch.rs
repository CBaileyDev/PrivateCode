//! Microbenchmark for the `*** Begin Patch` apply-patch parser.
//!
//! `parse_patch` runs on every `apply_patch` tool call the model makes — the
//! primary multi-file edit path — and the model emits these eagerly during a
//! turn, so its decode cost sits on the agent's critical loop. This bench parses
//! a representative patch exercising all three hunk kinds (Add / Update with
//! context+`@@`+`±` lines / Delete) so the per-line classification, the chunk
//! state machine, and the move-target branch are all measured, not just a
//! trivial single-add.

use criterion::{Criterion, criterion_group, criterion_main};
use private_code_tools::file_tools::parse_patch;
use std::hint::black_box;

const REPRESENTATIVE_PATCH: &str = r#"*** Begin Patch
*** Add File: src/new_module.rs
+pub fn added() -> u32 {
+    // a freshly added function
+    42
+}
*** Update File: src/lib.rs
*** Move to: src/core.rs
@@ pub mod things
 use std::collections::HashMap;
-let old = compute_old_value();
+let new = compute_new_value();
+let extra = derive_more(new);
 do_something_with(new);
@@ fn second_chunk
-removed_line();
+replacement_line();
*** Delete File: src/legacy.rs
*** End Patch
"#;

fn bench_parse_patch(c: &mut Criterion) {
    c.bench_function("parse_patch/add_update_delete", |b| {
        b.iter(|| {
            let hunks = parse_patch(black_box(REPRESENTATIVE_PATCH));
            black_box(hunks.ok());
        });
    });
}

criterion_group!(benches, bench_parse_patch);
criterion_main!(benches);
