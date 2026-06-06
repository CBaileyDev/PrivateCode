//! Phase-4 verification bench (Step 4.14 target): incremental re-index of a
//! single changed file should complete in < 100ms. Times `watcher::reindex_path`
//! — read + tree-sitter parse + extract + the atomic delete-then-insert into
//! FTS5 — on one realistic source file against a warm in-memory index.

use criterion::{Criterion, criterion_group, criterion_main};
use private_code_core::db::{connect_db, run_migrations};
use private_code_core::watcher::reindex_path;
use std::hint::black_box;
use tempfile::TempDir;

/// A realistic ~40-symbol Rust file (functions + a struct/impl), regenerated each
/// iteration so the body differs and the delete-then-insert does real work.
fn source(rev: usize) -> String {
    let mut s = String::from("pub struct Widget { pub id: u64 }\nimpl Widget {\n");
    for i in 0..20 {
        s.push_str(&format!(
            "    pub fn method_{i}_{rev}(&self, arg: u32) -> u32 {{ arg + {i} }}\n"
        ));
    }
    s.push_str("}\n");
    for i in 0..20 {
        s.push_str(&format!("pub fn free_fn_{i}_{rev}() -> usize {{ {i} }}\n"));
    }
    s
}

fn bench_reindex(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().to_path_buf();
    std::fs::create_dir_all(root.join("src")).unwrap();
    let file = root.join("src/widget.rs");

    let pool = rt.block_on(async {
        let pool = connect_db("sqlite::memory:").await.unwrap();
        run_migrations(&pool).await.unwrap();
        pool
    });

    let mut rev = 0usize;
    c.bench_function("reindex_path/single_file", |b| {
        b.iter(|| {
            rev += 1;
            std::fs::write(&file, source(rev)).unwrap();
            rt.block_on(reindex_path(&pool, &root, black_box(&file)))
                .unwrap();
        });
    });
}

criterion_group!(benches, bench_reindex);
criterion_main!(benches);
