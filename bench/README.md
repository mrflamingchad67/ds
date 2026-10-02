# Benchmark harness

Compares DS against other disk-space scanners, with the failure modes of the
earlier ad-hoc benchmarks designed out.

## Why this exists

An earlier benchmark reported Sysinternals DU scanning a 1.2M-file volume in
**29 milliseconds**. That was not DU being fast. It was PowerShell pipeline
truncation:

```powershell
du -nobanner -c C:\ | Select-Object -First 1   # measures nothing useful
```

`Select-Object -First N` terminates the upstream process once N lines are
available. The scanner was killed before it finished walking, then timed on the
fraction of the work it managed to do.

The effect is easy to demonstrate:

| | Elapsed |
| --- | ---: |
| slow command piped to `Select-Object -First 1` | 505 ms |
| same command, untruncated | 2061 ms |

DS is subject to the same trap, which is why a fair harness has to be careful
about DS as well:

| | `.rustup`, 187k files |
| --- | ---: |
| DS, piped to `Select-Object -First 1` | 0.065 s |
| DS, untruncated | 0.366 s |

Truncation cut a 0.366 s scan to 0.065 s, a 5.6x speedup that never happened.

## Usage

```powershell
# Minimum trustworthy run: one warm-up, one measured pass
.\bench\compare.ps1

# A subtree instead of the whole volume
.\bench\compare.ps1 -Target 'C:\Users\User\.rustup' -Tools ds -Reps 3

# Everything installed on this machine
.\bench\compare.ps1 -Tools all -Reps 3
```

Every invocation runs a self-test first and **aborts if the harness cannot
verify itself**.

## Rules the harness enforces

1. **No truncation.** Tools run via `Start-Process -Wait` with output
   redirected to files. There is no pipeline for a filter to cut short, and the
   stopwatch wraps the whole process.
2. **Self-test before results.** A slow helper command is timed directly and
   then through the harness. If the two disagree by more than 30%, the run is
   aborted with a non-zero exit code. A harness that cannot prove its own
   correctness is just another way to get wrong numbers.
3. **Totals must agree.** Every tool's file and byte counts are captured and
   compared. If two tools report different file counts for the same tree, the
   timings are reported but explicitly marked as **not a speed comparison**,
   because a different file count means a different workload.
4. **One benchmark at a time.** The harness refuses to start if `ds`,
   `diskusage`, `du64`, `gdu`, `dust`, or `dua` is already running.
5. **Minimum repetitions.** Default is one warm-up plus one measured pass.
   Raise `-Reps` only when a result genuinely needs confirmation.

## Why the default is one repetition

Full-volume scans are expensive and this filesystem is noisy. Observed spread on
the same target at one thread ranged from 16.5 s to 77.4 s, which is background
activity, not DS.

That noise has already produced one false conclusion: an optimization appeared
5.5% faster in one round of A/B testing and turned out to be nothing, once the
per-rep ranges were compared and found to overlap.

More repetitions reduce noise but cost SSD wear and CPU time. They are not a
default.

## Interpreting results

- Compare **totals first**, timings second. If file counts differ, stop.
- Prefer **minimum** over median when a run was cut short by interference; the
  minimum is the closest figure to the tool's real capability.
- A tool that reports fewer files than DS has not been given credit for being
  faster, because it did not do the same work.
- Size accounting differs between tools by design. DS reports logical file size.
  GNU `du` reports allocated blocks, and Windows tools may report on-disk size.
  Large discrepancies are expected; file counts are the fairer comparison.

## The self-test

Every run proves the harness measures whole processes. A helper that streams
one line every 400 ms for ~5 s is timed three ways:

| | Elapsed |
| --- | ---: |
| direct, output redirected to a file | 5.44 s |
| through `Measure-Tool` | 5.34 s |
| piped to `Select-Object -First 1` | **0.80 s** |

The third row is the bug. It understates the same work by **6.6x**, and a run
whose measured time falls outside 70–160% of the direct measurement aborts with
a non-zero exit code.

A self-test built on a command that emits no output stream, such as `cmd.exe`,
passes for the wrong reason and proves nothing. The helper has to stream output
for truncation to be possible at all.

## Tool notes

| Tool | Status | Notes |
| --- | --- | --- |
| `ds` | this repo | `--plain --ascii --threads 16` |
| `diskusage` | ships with Windows | **Requires administrator privileges.** Without elevation it prints `The DiskUsage utility requires that you have administrative privileges.` and exits immediately, which is why its timings look artificially fast. Run the harness elevated, or skip it. Output is `SizeOnDisk,Files,Directory path`, one row per directory, followed by a `<volume> in use` summary line that is not part of the tree. |
| `du-sysinternals` | optional | `-nobanner -c`. Its default is one level of detail; the root row's totals are still recursive. `-n` disables recursion and would not be comparable. |
| `gdu` | `dundee/gdu`, Go, not crates.io | Needs `--depth 1` to be comparable. See below. |
| `dust`, `dua` | optional | Installed on demand; not currently present. |

### gdu needs `--depth 1`, or it is not doing the same work

gdu's own documentation states that in non-interactive mode, without `--top` or
`--depth`, it uses a memory-efficient analyzer that keeps only top-level
directory totals and never builds the full tree. DS always builds the full tree.

Confirmed on a 24-file tree:

| Command | Root total | Files listed |
| --- | --- | --- |
| `gdu -np` | *absent* | 8 of 24 |
| `gdu -np --depth 1` | 56048 bytes | 24 |
| `ds` | 56013 bytes | 24 |

So `gdu -np` on its own omits the root total and most of the tree, and timing it
against DS would compare less work against more. The harness therefore runs
`gdu -npa --depth 1 --no-prefix`:

- `--depth 1` forces the full tree, making it work-equivalent to DS
- `-a` selects apparent size, because DS reports logical file size and gdu's
  default is on-disk size, which differ substantially over a large tree
- `--no-prefix` emits raw bytes instead of `54.7 KiB`, so totals can be parsed

A second entry, `gdu-toponly`, runs plain `gdu -np` deliberately, to document
that difference. Its totals fail to parse, because the root row is missing, and
the harness reports the run as **not a valid comparison** rather than presenting
the time as a result. That is intended: if a future gdu release changes this
behaviour, the warning disappears and the discrepancy has genuinely closed.

## Adding a tool

Add an entry to the `$registry` table in the script and a matching case in
`Read-ToolOutput`. The parser must leave `Recognised` false for anything it
does not understand, so an unrecognised format fails loudly instead of reporting
zeros that look like a fast scan.