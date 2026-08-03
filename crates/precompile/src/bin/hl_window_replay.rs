//! HL mainnet window replay against the ON-CHAIN perp engine (bench-util harness).
//!
//! Usage:
//!   hl_window_replay --book0 <book0.tsv> --ops <ops.tsv> [--warmup <warmup-ops.tsv>]
//!             [--max-ops N] [--envelope] [--block-rows N]
//!
//! Files must be plain TSV (zstd -d the dataset first). `--envelope` times through the full
//! precompile shell (`run_perp_dex_call`: selector dispatch + gas + ABI + revert encoding);
//! default times the engine entry points directly. `--block-rows N` simulates a block boundary
//! every N rows: take_perp_delta + chained commitment fold, merged into a canonical store.

use revm_precompile::bench_util::{hl_window_replay, HlReplayOpts};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let get = |flag: &str| -> Option<String> {
        args.iter()
            .position(|a| a == flag)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    let book0 = get("--book0").expect("--book0 <path> required");
    let ops = get("--ops").expect("--ops <path> required");
    let warmup = get("--warmup");
    let max_ops: usize = get("--max-ops").map(|v| v.parse().unwrap()).unwrap_or(usize::MAX);
    let block_rows: usize = get("--block-rows").map(|v| v.parse().unwrap()).unwrap_or(0);
    let via_envelope = args.iter().any(|a| a == "--envelope");

    eprintln!(
        "hl_window_replay | book0={book0} warmup={warmup:?} ops={ops} max_ops={max_ops} \
         mode={} block_rows={block_rows}",
        if via_envelope { "ENVELOPE" } else { "DIRECT" }
    );
    let report = hl_window_replay(
        &book0,
        warmup.as_deref(),
        &ops,
        HlReplayOpts { max_ops, via_envelope, block_rows },
    );
    println!("{report}");
}
