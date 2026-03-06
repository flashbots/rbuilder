#!/usr/bin/env python3
"""Create plots comparing V2 vs v_experimental cache-warm benchmark results."""

from __future__ import annotations

import argparse
import csv
from collections import defaultdict
from pathlib import Path
from statistics import mean

def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Generate V2 vs v_experimental plots from cache-warm-combined.csv."
    )
    parser.add_argument(
        "--input",
        type=Path,
        default=Path("cache-warm-combined.csv"),
        help="Input CSV path (default: cache-warm-combined.csv)",
    )
    parser.add_argument(
        "--output-dir",
        type=Path,
        default=Path("plots"),
        help="Output directory for generated PNGs (default: plots)",
    )
    parser.add_argument(
        "--source",
        default="",
        help=(
            "Optional source_file filter, e.g. cache-warm-1001blocks.csv. "
            "Leave empty to use all rows."
        ),
    )
    parser.add_argument(
        "--exclude-100",
        action="store_true",
        help="Exclude rows where warm_pct is 100.",
    )
    parser.add_argument(
        "--moving-window",
        type=int,
        default=50,
        help="Window size for moving-average block plots (default: 50).",
    )
    return parser.parse_args()


def read_rows(
    path: Path,
    source_filter: str,
    exclude_100: bool,
) -> list[dict[str, str]]:
    with path.open(newline="", encoding="utf-8") as f:
        rows = list(csv.DictReader(f))
    if source_filter:
        rows = [r for r in rows if r.get("source_file", "") == source_filter]
    if exclude_100:
        rows = [r for r in rows if to_int(r, "warm_pct") != 100]
    if not rows:
        raise ValueError("No rows found after applying filters.")
    return rows


def load_plt():
    try:
        import matplotlib

        matplotlib.use("Agg")
        import matplotlib.pyplot as plt
    except ModuleNotFoundError as exc:
        raise SystemExit(
            "matplotlib is required. Install with: python3 -m pip install matplotlib"
        ) from exc
    return plt


def to_float(row: dict[str, str], key: str) -> float:
    return float(row[key])

def to_float_any(row: dict[str, str], keys: tuple[str, ...]) -> float:
    for key in keys:
        value = row.get(key)
        if value not in (None, ""):
            return float(value)
    raise KeyError(f"missing expected columns: {', '.join(keys)}")


def to_float_v_experimental(row: dict[str, str], metric: str) -> float:
    return to_float_any(
        row,
        (
            f"v_experimental_{metric}_ms",
            f"v3_{metric}_ms",
        ),
    )


def to_int(row: dict[str, str], key: str) -> int:
    return int(float(row[key]))


def aggregate_by_warm(rows: list[dict[str, str]]) -> dict[int, dict[str, float]]:
    bucket: dict[int, dict[str, list[float]]] = defaultdict(
        lambda: defaultdict(list)
    )
    for row in rows:
        warm = to_int(row, "warm_pct")
        bucket[warm]["v2_p50"].append(to_float(row, "v2_p50_ms"))
        bucket[warm]["v_experimental_p50"].append(to_float_v_experimental(row, "p50"))
        bucket[warm]["v2_p99"].append(to_float(row, "v2_p99_ms"))
        bucket[warm]["v_experimental_p99"].append(to_float_v_experimental(row, "p99"))
        bucket[warm]["v2_mean"].append(to_float(row, "v2_mean_ms"))
        bucket[warm]["v_experimental_mean"].append(to_float_v_experimental(row, "mean"))

    out: dict[int, dict[str, float]] = {}
    for warm, values in bucket.items():
        out[warm] = {
            "v2_p50": mean(values["v2_p50"]),
            "v_experimental_p50": mean(values["v_experimental_p50"]),
            "v2_p99": mean(values["v2_p99"]),
            "v_experimental_p99": mean(values["v_experimental_p99"]),
            "v2_mean": mean(values["v2_mean"]),
            "v_experimental_mean": mean(values["v_experimental_mean"]),
        }
    return out


def save_speedup_by_warm_plot(
    plt, by_warm: dict[int, dict[str, float]], output_dir: Path
) -> None:
    warms = sorted(by_warm.keys())
    p50 = [by_warm[w]["v2_p50"] / by_warm[w]["v_experimental_p50"] for w in warms]
    p99 = [by_warm[w]["v2_p99"] / by_warm[w]["v_experimental_p99"] for w in warms]
    means = [by_warm[w]["v2_mean"] / by_warm[w]["v_experimental_mean"] for w in warms]

    x = range(len(warms))
    width = 0.25

    plt.figure(figsize=(11, 6))
    plt.bar([i - width for i in x], p50, width=width, label="p50 speedup")
    plt.bar(x, p99, width=width, label="p99 speedup")
    plt.bar([i + width for i in x], means, width=width, label="mean speedup")
    plt.xticks(list(x), [str(w) for w in warms])
    plt.xlabel("Warm %")
    plt.ylabel("Speedup (V2 / v_experimental)")
    plt.title("v_experimental Speedup by Warm %")
    plt.grid(axis="y", alpha=0.3)
    plt.legend()
    plt.tight_layout()
    plt.savefig(output_dir / "speedup_by_warm_pct.png", dpi=180)
    plt.close()


def save_latency_by_warm_plot(
    plt, by_warm: dict[int, dict[str, float]], output_dir: Path
) -> None:
    warms = sorted(by_warm.keys())
    v2_p50 = [by_warm[w]["v2_p50"] for w in warms]
    v_experimental_p50 = [by_warm[w]["v_experimental_p50"] for w in warms]
    v2_p99 = [by_warm[w]["v2_p99"] for w in warms]
    v_experimental_p99 = [by_warm[w]["v_experimental_p99"] for w in warms]

    fig, (ax1, ax2) = plt.subplots(1, 2, figsize=(14, 5))
    width = 0.35
    x = range(len(warms))

    ax1.bar([i - width / 2 for i in x], v2_p50, width=width, label="V2")
    ax1.bar(
        [i + width / 2 for i in x],
        v_experimental_p50,
        width=width,
        label="v_experimental",
    )
    ax1.set_title("p50 Latency by Warm %")
    ax1.set_xticks(list(x))
    ax1.set_xticklabels([str(w) for w in warms])
    ax1.set_xlabel("Warm %")
    ax1.set_ylabel("Milliseconds")
    ax1.grid(axis="y", alpha=0.3)
    ax1.legend()

    ax2.bar([i - width / 2 for i in x], v2_p99, width=width, label="V2")
    ax2.bar(
        [i + width / 2 for i in x],
        v_experimental_p99,
        width=width,
        label="v_experimental",
    )
    ax2.set_title("p99 Latency by Warm %")
    ax2.set_xticks(list(x))
    ax2.set_xticklabels([str(w) for w in warms])
    ax2.set_xlabel("Warm %")
    ax2.set_ylabel("Milliseconds")
    ax2.grid(axis="y", alpha=0.3)
    ax2.legend()

    fig.suptitle("V2 vs v_experimental Latency by Warm %")
    fig.tight_layout()
    fig.savefig(output_dir / "latency_by_warm_pct.png", dpi=180)
    plt.close(fig)


def save_speedup_over_blocks_plot(
    plt, rows: list[dict[str, str]], output_dir: Path
) -> None:
    points: dict[int, list[tuple[int, float]]] = defaultdict(list)
    for row in rows:
        if row.get("source_type") != "interleaved":
            continue
        warm = to_int(row, "warm_pct")
        block = to_int(row, "iteration")
        speedup = to_float(row, "v2_p50_ms") / to_float_v_experimental(row, "p50")
        points[warm].append((block, speedup))

    if not points:
        return

    plt.figure(figsize=(12, 6))
    for warm in sorted(points):
        series = sorted(points[warm], key=lambda x: x[0])
        x = [p[0] for p in series]
        y = [p[1] for p in series]
        plt.plot(x, y, linewidth=1.2, label=f"warm={warm}%")

    plt.xlabel("Block")
    plt.ylabel("p50 Speedup (V2 / v_experimental)")
    plt.title("p50 Speedup Over Blocks (Interleaved Runs)")
    plt.grid(alpha=0.3)
    plt.legend()
    plt.tight_layout()
    plt.savefig(output_dir / "p50_speedup_over_blocks.png", dpi=180)
    plt.close()


def save_p99_regression_plot(
    plt, rows: list[dict[str, str]], output_dir: Path
) -> None:
    x: list[int] = []
    y: list[float] = []
    c: list[int] = []

    for row in rows:
        if row.get("source_type") != "interleaved":
            continue
        v2_p99 = to_float(row, "v2_p99_ms")
        v_experimental_p99 = to_float_v_experimental(row, "p99")
        if v_experimental_p99 < v2_p99:
            continue
        x.append(to_int(row, "iteration"))
        y.append(v_experimental_p99 - v2_p99)
        c.append(to_int(row, "warm_pct"))

    if not x:
        return

    plt.figure(figsize=(12, 6))
    sc = plt.scatter(x, y, c=c, cmap="viridis", s=30, alpha=0.9)
    cb = plt.colorbar(sc)
    cb.set_label("Warm %")
    plt.xlabel("Block")
    plt.ylabel("p99 Regression (v_experimental - V2, ms)")
    plt.title("p99 Regressions Where v_experimental >= V2 (Interleaved Runs)")
    plt.grid(alpha=0.3)
    plt.tight_layout()
    plt.savefig(output_dir / "p99_regressions.png", dpi=180)
    plt.close()


def moving_average(values: list[float], window: int) -> list[float]:
    if window <= 1:
        return values[:]
    out: list[float] = []
    acc = 0.0
    for i, value in enumerate(values):
        acc += value
        if i >= window:
            acc -= values[i - window]
        count = min(i + 1, window)
        out.append(acc / count)
    return out


def save_latency_moving_average_plot(
    plt,
    rows: list[dict[str, str]],
    output_dir: Path,
    window: int,
) -> None:
    by_iter: dict[int, dict[str, list[float]]] = defaultdict(
        lambda: defaultdict(list)
    )
    for row in rows:
        if row.get("source_type") != "interleaved":
            continue
        block = to_int(row, "iteration")
        by_iter[block]["v2_p50"].append(to_float(row, "v2_p50_ms"))
        by_iter[block]["v_experimental_p50"].append(to_float_v_experimental(row, "p50"))
        by_iter[block]["v2_p99"].append(to_float(row, "v2_p99_ms"))
        by_iter[block]["v_experimental_p99"].append(to_float_v_experimental(row, "p99"))

    if not by_iter:
        return

    blocks = sorted(by_iter.keys())
    v2_p50 = [mean(by_iter[i]["v2_p50"]) for i in blocks]
    v_experimental_p50 = [mean(by_iter[i]["v_experimental_p50"]) for i in blocks]
    v2_p99 = [mean(by_iter[i]["v2_p99"]) for i in blocks]
    v_experimental_p99 = [mean(by_iter[i]["v_experimental_p99"]) for i in blocks]

    v2_p50_ma = moving_average(v2_p50, window)
    v_experimental_p50_ma = moving_average(v_experimental_p50, window)
    v2_p99_ma = moving_average(v2_p99, window)
    v_experimental_p99_ma = moving_average(v_experimental_p99, window)

    fig, (ax1, ax2) = plt.subplots(2, 1, figsize=(12, 9), sharex=True)
    ax1.plot(blocks, v2_p50_ma, linewidth=1.8, label="V2 p50 (moving avg)")
    ax1.plot(
        blocks,
        v_experimental_p50_ma,
        linewidth=1.8,
        label="v_experimental p50 (moving avg)",
    )
    ax1.set_ylabel("Milliseconds")
    ax1.set_title(f"Moving Average p50 (window={window} blocks)")
    ax1.grid(alpha=0.3)
    ax1.legend()

    ax2.plot(blocks, v2_p99_ma, linewidth=1.8, label="V2 p99 (moving avg)")
    ax2.plot(
        blocks,
        v_experimental_p99_ma,
        linewidth=1.8,
        label="v_experimental p99 (moving avg)",
    )
    ax2.set_xlabel("Block")
    ax2.set_ylabel("Milliseconds")
    ax2.set_title(f"Moving Average p99 (window={window} blocks)")
    ax2.grid(alpha=0.3)
    ax2.legend()

    fig.suptitle("V2 vs v_experimental Latency Moving Averages")
    fig.tight_layout()
    fig.savefig(output_dir / "latency_moving_average_over_blocks.png", dpi=180)
    plt.close(fig)


def main() -> None:
    args = parse_args()
    plt = load_plt()
    if args.moving_window < 1:
        raise SystemExit("--moving-window must be >= 1")

    rows = read_rows(args.input, args.source, args.exclude_100)
    args.output_dir.mkdir(parents=True, exist_ok=True)

    by_warm = aggregate_by_warm(rows)
    save_speedup_by_warm_plot(plt, by_warm, args.output_dir)
    save_latency_by_warm_plot(plt, by_warm, args.output_dir)
    save_speedup_over_blocks_plot(plt, rows, args.output_dir)
    save_p99_regression_plot(plt, rows, args.output_dir)
    save_latency_moving_average_plot(plt, rows, args.output_dir, args.moving_window)

    print(f"Generated plots in: {args.output_dir}")
    if args.exclude_100:
        print("Filter: excluding warm_pct=100 rows")
    for name in [
        "speedup_by_warm_pct.png",
        "latency_by_warm_pct.png",
        "p50_speedup_over_blocks.png",
        "p99_regressions.png",
        "latency_moving_average_over_blocks.png",
    ]:
        path = args.output_dir / name
        if path.exists():
            print(f" - {path}")


if __name__ == "__main__":
    main()
