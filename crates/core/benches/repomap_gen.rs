//! Phase-4 verification bench (Step 4.14 target): repo-map generation for a
//! ~10k-file project should complete in < 2s. Builds a 10k-file in-memory index
//! once (not timed), then times `repomap::generate` — the full path: read every
//! symbol from SQLite, group by file, rank by richness, and render within the
//! default token budget.

use criterion::{Criterion, criterion_group, criterion_main};
use private_code_codeintel::Symbol;
use private_code_core::db::{connect_db, run_migrations};
use private_code_core::repomap::{DEFAULT_REPOMAP_TOKEN_BUDGET, generate};
use private_code_core::symbols::index_file;
use std::hint::black_box;

const FILES: usize = 10_000;
const PER_FILE: usize = 3; // 30k symbols across 10k files

fn bench_repomap_generate(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();

    // One-time setup: a 10k-file index (not timed).
    let pool = rt.block_on(async {
        let pool = connect_db("sqlite::memory:").await.unwrap();
        run_migrations(&pool).await.unwrap();
        for f in 0..FILES {
            let path = format!("src/mod_{f}.rs");
            let syms: Vec<Symbol> = (0..PER_FILE)
                .map(|i| {
                    let name = format!("item_{f}_{i}");
                    let line = i as u32 + 1;
                    Symbol {
                        symbol_uid: Symbol::make_uid(&path, line, 0, &name),
                        filepath: path.clone(),
                        signature: format!("pub fn {name}(arg: u32) -> u32"),
                        name,
                        kind: "function".into(),
                        start_line: line,
                        start_column: 0,
                        end_line: line + 1,
                        end_column: 0,
                        parent_scope: None,
                    }
                })
                .collect();
            index_file(&pool, &path, &syms).await.unwrap();
        }
        pool
    });

    c.bench_function("repomap_generate/10k_files", |b| {
        b.iter(|| {
            let map = rt
                .block_on(generate(&pool, black_box(DEFAULT_REPOMAP_TOKEN_BUDGET)))
                .unwrap();
            black_box(map);
        });
    });
}

criterion_group!(benches, bench_repomap_generate);
criterion_main!(benches);
