#!/usr/bin/env bash
set -euo pipefail

# Usage:
#   scripts/run_correct_and_bench_thread_sweep.sh [OUTDIR]
#
# Optional environment overrides:
#   THREADS="1 2 4 8"
#   BIN=/home/robert/rbuilder/target/release/correct_and_bench
#   RETH_BIN=/usr/local/bin/reth
#   DATADIR=/mnt/reth-full
#   CHAIN=mainnet
#   BLOCKS=25
#   CACHE_WARM_ITERS=25
#   CACHE_WARM_KEYS=10000
#   CACHE_WARM_PERCENTAGES=70,80,90,100
#   STATIC_FILE_BLOCKS_PER_FILE=10000

THREADS="${THREADS:-1 2 4 8}"
BIN="${BIN:-/home/robert/rbuilder/target/release/correct_and_bench}"
RETH_BIN="${RETH_BIN:-/usr/local/bin/reth}"
DATADIR="${DATADIR:-/mnt/reth-full}"
CHAIN="${CHAIN:-mainnet}"
BLOCKS="${BLOCKS:-25}"
CACHE_WARM_ITERS="${CACHE_WARM_ITERS:-25}"
CACHE_WARM_KEYS="${CACHE_WARM_KEYS:-10000}"
CACHE_WARM_PERCENTAGES="${CACHE_WARM_PERCENTAGES:-70,80,90,100}"
# /mnt/reth-full commonly uses 10k static-file windows near the tip.
STATIC_FILE_BLOCKS_PER_FILE="${STATIC_FILE_BLOCKS_PER_FILE:-10000}"

OUTDIR="${1:-${OUTDIR:-bench-results/correct-and-bench-mainnet-${BLOCKS}blocks-$(date +%F-%H%M%S)}}"
mkdir -p "$OUTDIR"
echo "output dir: $OUTDIR"

for t in $THREADS; do
  echo "running threads=$t ..."
  args=(
    --cache-warm-iters "$CACHE_WARM_ITERS"
    --cache-warm-keys "$CACHE_WARM_KEYS"
    --cache-warm-percentages "$CACHE_WARM_PERCENTAGES"
    --cache-warm-out-csv "$OUTDIR/correct-and-bench-threads-$t.csv"
    --full
    --blocks "$BLOCKS"
    --reth-bin "$RETH_BIN"
    --datadir "$DATADIR"
    --chain "$CHAIN"
  )
  if [[ -n "$STATIC_FILE_BLOCKS_PER_FILE" ]]; then
    args+=(--static-file-blocks-per-file "$STATIC_FILE_BLOCKS_PER_FILE")
  fi

  if ! RAYON_NUM_THREADS="$t" "$BIN" "${args[@]}" \
    > "$OUTDIR/correct-and-bench-threads-$t.log" 2>&1; then
    echo "failed for threads=$t"
    echo "log: $OUTDIR/correct-and-bench-threads-$t.log"
    tail -n 80 "$OUTDIR/correct-and-bench-threads-$t.log" || true
    exit 1
  fi
  echo "completed threads=$t"
done

{
  echo "threads,iteration,v_experimental_root_correct,warm_pct,v2_p50_ms,v_experimental_p50_ms,v2_p99_ms,v_experimental_p99_ms,v2_mean_ms,v_experimental_mean_ms,speedup"
  for t in $THREADS; do
    tail -n +2 "$OUTDIR/correct-and-bench-threads-$t.csv" | sed "s/^/$t,/"
  done
} > "$OUTDIR/correct-and-bench-thread-sweep-combined.csv"

echo "done: $OUTDIR"
