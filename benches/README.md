# Benchmark runner

```sh
cargo build --release
python3 bench/run.py benches/postgres --out bench/runs/default
python3 bench/run.py benches/postgres --strategies baseline beam budget --out bench/runs/comparison
python3 bench/run.py benches/postgres --strategies all --out bench/runs/all
python3 bench/run.py benches/postgres --strategies beam --repeat 3 --out bench/runs/beam-repeat
```

The default is the validated `cascade` preset: parallel 16, filename batch 64,
cap 128, 128 lexical candidates, up to 20 files, and 24 candidate passages of
8 KiB per file. Small 384-byte sketches route the global budget of at most 40
full-source passages. Only full-source judgments contribute result ranges.
The sketch cutoff is 0.45; final passage relevance uses the last CLI threshold
(default 0.2). Explicit strategies and named configurations override the defaults.

The [current results](experiments/RESULTS.md) and
[full creative comparison](experiments/RESULTS.md#creative-approaches-and-rejected-settings) report independent
Postgres, CPython, and held-out Kubernetes measurements. JSON stats separate
filename, sketch, and source tokens and count judged/pruned passages. Use
`--strategies window` for the previous adaptive window strategy; its historical
[cost follow-up](experiments/RESULTS.md#compact-schema-and-prefilter-results) and pinned configurations remain available.
Saved experiment configurations pin their measured settings, so changing the
application defaults does not silently change earlier comparisons.

The suite's `tag.json` supplies the target checkout and pinned commit. Override
its location with `--root /path/to/postgres`. The target must be clean at the
pinned revision; `--allow-revision-mismatch` explicitly permits and records a
different checkout. All annotated paths and line numbers are validated before
API calls. `--dry-run` validates and prints the plan without API requests. Legacy
arrays of `{name, query, expect: [...]}` work with `--root`; the old `jegrep --bench`
interface remains available.

Each experiment directory contains:

- `manifest.json`: queries, labels, knobs, strategy environment, model, binary and
  runner hashes, target revision, and scoring policy. No API keys are stored.
- `rows.jsonl`: one durable row per case/configuration/repeat, including every hit
  and heatmap, timings, errors, individual scores, and missed labels.
- `summary.md`, `summary.csv`, `summary.json`: comparisons updated after each
  query. JSON/CSV include more metrics than the compact Markdown table.

Each search runs in a fresh process. Wall time includes startup, scanning,
workers, retries, and shutdown. Configuration order rotates between cases and
repeats. Separate runners share a request lock so optimization agents can work
concurrently without overlapping API traffic. Lock wait is excluded from timing.
`--allow-concurrent` opts out and is recorded. Calls made outside this runner are
outside the lock.

## Scoring

Prefer recall and completeness, then compare cost and speed among acceptable
configurations. The runner keeps multiple objectives instead of one opaque score.

| Metric | Meaning |
|---|---|
| File recall | Fraction of distinct expected files returned |
| Micro file / region recall | Fraction of all required targets recovered, weighting queries by their number of targets; included in JSON/CSV as `micro_file_recall` / `micro_span_recall` |
| Query success | At least one correct hit, corresponding to the old 8/8 score |
| Complete files | Fraction of cases returning every expected file |
| TP / FN | Returned relevant files / missing expected files |
| FP | Explicitly irrelevant returned files; all extras only with exhaustive labels |
| Unjudged (`?`) | Returned files absent from both positive and negative labels |
| Precision lower bound (`P≥`) | Known relevant files / all returned files |
| Precision / F2 | Available when no returned hits are unjudged; F2 favors recall |
| Region recall | Fraction of annotated code regions touched by reported ranges |
| Mean span coverage | Mean covered fraction of each annotated region |
| Line recall | Union of covered annotated lines / union of annotated lines |
| Line precision lower bound | Annotated overlapping lines / all reported lines |
| Reciprocal rank | 1 / rank of first relevant hit, available per query |
| USD / tokens | jegrep's configured cost estimate / reported input tokens |
| Median / p95 seconds | End-to-end latency across cases and repeats |

Region scoring uses the **top 3 positive-probability ranges per returned file**
(`--top-ranges`). Returning a file does not automatically earn credit for every
function inside it. Inclusive intervals are unioned to avoid double counting.
Region recall rewards finding functions; line recall and precision expose ranges
that are too narrow or broad. Missing heatmaps receive no region credit. Scoring
settings remain fixed across configurations in an experiment.

Query files may include `irrelevant: ["path/to/file"]` and `exhaustive: true`.
Exhaustiveness may also be declared in `tag.json` or assumed explicitly with
`--closed-world`. Postgres has positive labels only; extras are unjudged by
default. These labels cannot establish a conventional false-positive rate or a
true-negative count. Legacy `expect` entries each count as one required target
(a trailing slash matches a folder prefix); these were originally acceptable
alternatives, so use query success for that historical interpretation.

★ identifies the Pareto frontier over macro/micro file and region recall, line recall,
file/line precision lower bounds, estimated cost, and median latency. No eligible alternative
improves one objective without worsening another. This is a candidate set, not
proof of an optimum. Failed/incomplete experiments cannot be Pareto candidates.
Failures still contribute misses and latency; their cost may be underreported.
Repeats are retained, never replaced by the latest result. Quality percentages
are macro averages across cases/repeats. Retest finalists on repeated runs and
new queries to check variance and overfitting.

## Configuration and grids

Normal knobs are runner flags: `--batch`, `--max-batch`, `--parallel`, `--bytes`,
`--ranges`, `--thresholds`, `--min-hits`. Invalid values fail validation rather
than being silently clamped. `--model`, `--endpoint`, `--hidden` apply to all
configurations. Answer-key `keywords` are never passed into searches unless
`--use-keywords` is set; those runs are labeled **oracle-assisted** and should be
compared separately.

`--configs variants.json` reads named variants:

```json
[
  {"name": "beam-default", "strategy": "beam"},
  {"name": "budget-wide", "strategy": "budget", "parallel": 12,
   "batch": 64, "max_batch": 128, "min_hits": 4,
   "env": {"JEGREP_CAP": "80", "JEGREP_K": "12"}}
]
```

`--grid grid.json` takes a Cartesian product over every selected configuration:

```json
{"batch": [32, 64, 128], "parallel": [8, 16, 32], "ranges": [16, 64]}
```

Threshold grids contain arrays of arrays, e.g. `{"thresholds": [[0.4, 0.2],
[0.3, 0.1]]}`. Per-configuration `env` may set strategy-specific `JEGREP_*` knobs;
ambient `JEGREP_*` settings are also recorded. Existing strategies sometimes
internally override or ignore knobs; inspect implementation before interpreting
a sweep. `--cases query_hash_join_execution query_checkpoint_creation` selects
a recorded subset for preliminary sweeps; use the full suite for finalists.

Request splitting is opt-in via `JEGREP_QUESTION_CHUNK`, for example `64` in a
configuration's `env`. Independent Noul questions share the same state but run
in chunks with at most two in flight per logical request; thus `--parallel P`
can permit up to `2P` HTTP requests. Choice and mixed batches stay intact.
Splitting may increase repeated-state tokens. `requests` counts logical jobs;
`http_attempts` counts physical attempts, including chunks/retries/failover
(available in newer binaries). Both appear in JSON/CSV. Partial failed chunks
can incur cost that the logical error result cannot report.

`--timeout 180` bounds each query. `--max-usd 1` stops **between queries** after
recorded cost reaches $1; it is not a hard billing cap. The last query and failed
requests can exceed it. Interrupted runs preserve completed rows. Resume with
the same command plus `--resume`. Changes to binary, knobs, labels, scoring, or
runner code require a fresh output directory. Failed rows are preserved as
evidence; retry them in a new experiment.

The new `window` strategy uses `JEGREP_WINDOW_*` variables for candidate count,
content-file count, passages per file, bytes per passage, and passages per
request. Its `--parallel`, `--batch`, and `--max-batch` affect scheduling and
filename judgments; the last `--thresholds` value gates passage relevance.
`--bytes`, `--ranges`, `--min-hits`, and supplemental keywords do not control its
window selection. `hybrid-window` applies the usual knobs to its beam phase,
then the window knobs to refinement. Presets and measured results are in
[`experiments/04_window_strategy.md`](experiments/04_window_strategy.md) and
[`experiments/08_hybrid_finalist.md`](experiments/08_hybrid_finalist.md).

```sh
python3 -m unittest discover -s bench -p 'test_*.py'
```
