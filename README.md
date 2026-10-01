# DS

A lightweight native disk space utility written in Rust. DS reports volume
capacity from the platform API, and separately walks the filesystem itself to
show **what is actually using the space**.

```
╭──────────────────────────────────────────┬─────────┬────────╮
│ Path                                     │ Size    │ Files  │
├──────────────────────────────────────────┼─────────┼────────╤
│ D:\Personal File                         │ 13.9 GB │ 4,182  │
│ D:\.npm-cache                            │ 10.4 GB │ 13,476 │
╰──────────────────────────────────────────┴─────────┴────────╯
```

## Two separate jobs

| Concern | Source |
| --- | --- |
| Which drives exist; total and free capacity | Windows API (`GetLogicalDrives`, `GetDiskFreeSpaceExW`) |
| Which files and directories use the space | DS's own recursive metadata scan |

DS never shells out to `du`, `fsutil`, `wmic`, `dir`, WizTree, TreeSize, or any
other disk analyzer, and it never reads file contents. A 20 GB ISO is measured
with one `stat`.

## Usage

```
ds                          List volume capacity for every drive
ds C:                       Volume row, then a scan of C:
ds C:\Users                 Scan only C:\Users
ds --ascii                  ASCII-only table borders
ds --json                   Machine-readable JSON
ds --sort free              Volume list: drive|free|used|total|usage
ds C: --sort size           Directory list: size|files
ds --watch                  Live refresh on the alternate screen
```

### Scan options

| Option | Default | Description |
| --- | --- | --- |
| `--threads <N>` | `16` | Concurrent workers, 1–256 |
| `--dirs <N>` | `15` | Largest directories to report |
| `--largest <N>` | `10` | Largest files to report |
| `--depth <N>` | `0` | Max recursion depth; `0` is unlimited |
| `--ext` | off | Add a per-extension size breakdown |
| `--no-scan` | off | Volume capacity only, no filesystem walk |
| `--quiet` | off | Suppress the live progress line |

### Other options

`--style <rounded|sharp|ascii>`, `--bytes`, `--precision <N>`, `-r/--reverse`,
`--warn-above <PCT>`, `--plain`, `--interval <SECS>`.

Exit codes: `0` success, `1` runtime failure, `2` bad arguments. A closed pipe
(`ds | head`) exits `0`.

## Scanner design

```
                  Scanner
                    │
          WorkQueue (bounded, self-closing)
                    │
    ┌──────────┬────┴────┬──────────┐
    ▼          ▼         ▼          ▼
 Worker 1  Worker 2  Worker 3  Worker 16
    │  own      │         │          │
    ▼  agg      ▼         ▼          ▼
 per-worker aggregation — no shared lock at all
                    │
                    ▼  merged once at the end
               ScanOutcome
```

* **Exactly `--threads` threads.** A worker that finds a subdirectory pushes it
  back onto the shared queue, so any worker can take any branch. No thread is
  created per directory.
* **Bounded queue.** At most a few thousand directories are queued. A worker that
  cannot enqueue handles the directory itself instead of blocking, so a small
  queue never deadlocks and never loses a subtree.
* **Self-closing.** The queue tracks queued *plus in-flight* directories and
  closes when the last one finishes, so no worker idles forever and no producer
  is cut off mid-tree.
* **Metadata only.** `DirEntry::file_type()` and `DirEntry::metadata()` are
  stats. File contents are never opened.
* **No shared lock on the hot path.** Each worker owns an `Aggregator`; the
  coordinator merges them once at the end. Batching amortises bookkeeping.
* **Symlinks are measured, never followed.** Directory symlinks and Windows
  reparse points contribute their own size and are not descended, so cycles such
  as `A/B/link → A` are impossible.
* **Errors are data.** Files deleted mid-scan, access-denied directories, and
  broken links are counted and sampled, never fatal. The report says how many
  paths failed.
* **Cancellation.** A watchdog thread aborts the queue when the flag flips, so
  workers blocked waiting for work wake up. All workers are joined before
  `scan` returns, so no thread outlives the call.

### Memory

Aggregates are stored per **directory**, never per file:

| Structure | Bound |
| --- | --- |
| `dirs: HashMap<PathBuf, DirTotals>` | one entry per directory (the only scaling cost) |
| `exts: HashMap<String, ExtTotals>` | one entry per distinct extension |
| largest files | hard top-N heap, only `--largest` entries kept |
| error samples | hard cap, count still exact |

Measured on `C:\` (1,247,269 files, 247,684 directories): **115 MB at 1 worker,
118 MB at 32 workers.** Memory tracks directory count and is essentially
independent of both file count and worker count.

### Progress

The scanner publishes counters to relaxed atomics; a separate renderer thread
repaints at most every 200 ms. The scanner performs no terminal I/O, and progress
is suppressed entirely when stdout is not a terminal.

## Benchmarks

Real measurements on this machine: **NVMe SSD, Windows 11, Rust 1.98,
release build (`opt-level="z"`, LTO)**. No invented numbers below.

### Worker scaling — `C:\`, 1,247,269 files / 247,684 directories

Median of 3 runs. This is the big, metadata-heavy workload.

| Workers | Median | Files/sec | vs 1 worker |
| ---: | ---: | ---: | ---: |
| 1 | 16.5 s | 75,309 | 1.0× |
| 8 | 6.7 s | 186,160 | 2.5× |
| **16 (default)** | **6.0 s** | **207,878** | **2.8×** |
| 32 | 7.3 s | 170,859 | 2.3× |
| 64 | 7.6 s | 164,114 | 2.1× |

**16 workers is the default because it measured fastest here.** Concurrency
helps sharply (1 → 16 is 2.8×), then plateaus and drifts slightly backwards as
workers are added. The optimum depends on the drive, the tree shape, and the
machine, so this is a starting point rather than a law — re-run the benchmark
above on your own hardware before assuming it transfers, and raise `--threads`
for much larger or slower storage.

### Worker scaling — `D:\`, 43,895 files / 15,673 directories

Median of 7 runs, warm cache.

| Workers | Median | Files/sec |
| ---: | ---: | ---: |
| 1 | 81 ms | 541,914 |
| 8 | 82 ms | 535,305 |
| 16 (default) | 97 ms | 452,526 |
| 32 | 84 ms | 522,560 |
| 64 | 88 ms | 498,807 |

On a small, cache-warm tree the worker count makes no measurable difference:
everything is already in memory, so the bottleneck is not filesystem latency.
This is the case where `--threads` is irrelevant and the default is fine.

## Architecture

| Module | Responsibility |
| --- | --- |
| `cli` | Clap definitions and derived settings |
| `disk` | Drive enumeration, volume capacity, path normalisation |
| `queue` | Bounded work queue with self-closing lifetime |
| `scan` | Worker pool, traversal, metadata collection, cancellation |
| `aggregate` | Directory rollups, extension totals, bounded top-N, merge |
| `progress` | Periodic progress rendering, decoupled from the scanner |
| `format` | Byte, count, percentage, and duration formatting |
| `table` | `tabled` presentation, border styles, colour |
| `output` | Drives or scan results to tables or JSON |
| `watch` | Crossterm live-refresh loop and terminal state |
| `main` | Argument handling and exit codes |

Directory rollups are built during the scan: when a worker finishes
`C:\Games\Minecraft`, its totals are added to that directory and to each ancestor
up to the scan root, so parents include descendants without a second pass.

## Development

```sh
cargo test                  # 195 unit tests
cargo clippy --all-targets  # clean
cargo fmt --check
cargo build --release       # ~0.7 MB stripped binary
```

Tests use scratch directories under the system temp path and remove them first,
so repeat runs are deterministic. They never drive the real terminal.

## Stack

`clap`, `tabled`, `crossterm`, `serde`/`serde_json`, plus `windows-sys` (or
`libc` on Unix) for volume capacity. No async runtime, no TUI framework, no
scanner dependencies.