//! Phase-4 verification bench (Step 4.3 / 4.14 target): FTS5 symbol search over
//! ~200k symbols should return first results in well under 50ms. Builds an
//! in-memory index of 200k synthetic symbols once, then times a representative
//! multi-token prefix search.

use criterion::{Criterion, criterion_group, criterion_main};
use private_code_codeintel::Symbol;
use private_code_core::db::{connect_db, run_migrations};
use private_code_core::symbols::{index_file, search_symbols};
use std::hint::black_box;

const FILES: usize = 2_000;
const PER_FILE: usize = 100; // 2_000 * 100 = 200_000 symbols

fn bench_symbol_search(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();

    // One-time setup: a 200k-symbol in-memory index (not timed).
    let pool = rt.block_on(async {
        let pool = connect_db("sqlite::memory:").await.unwrap();
        run_migrations(&pool).await.unwrap();
        for f in 0..FILES {
            let path = format!("src/mod_{f}.rs");
            let syms: Vec<Symbol> = (0..PER_FILE)
                .map(|i| {
                    let name = format!("symbol_{f}_{i}");
                    let line = i as u32 + 1;
                    Symbol {
                        symbol_uid: Symbol::make_uid(&path, line, 0, &name),
                        filepath: path.clone(),
                        signature: format!("fn {name}()"),
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

    c.bench_function("search_symbols/200k", |b| {
        b.iter(|| {
            // Multi-token prefix query (the sanitizer splits on '_'): exercises a
            // realistic AND-of-prefixes MATCH narrowing 200k to a small result set.
            let hits = rt
                .block_on(search_symbols(&pool, black_box("symbol_1500"), 20))
                .unwrap();
            black_box(hits);
        });
    });
}

criterion_group!(benches, bench_symbol_search);
criterion_main!(benches);
