# Benchmark and optimization results — 2026-09-19

**Current default: `cascade-balanced40`.** The three creative agents started in
independent worktrees at `f5b0231`. Cascade's final binary was then independently
validated on all ten Postgres queries, all ten CPython queries, and ten unseen
Kubernetes queries. The implementation is now integrated into the main checkout,
registered as `cascade`, and selected by default by both `jegrep` and the folder
benchmark runner. [recommended.json](recommended.json) pins the tested settings.
The existing strategies, including `window`, remain explicitly selectable.

Cascade judges small source sketches before spending its full-source budget. It
uses 128 lexical candidates, 20 selected files, up to 24 candidate 8 KiB passages
per file, 384-byte sketches, a 0.45 routing cutoff, and **at most 40 full passages
across the search**, with parallel 16 and filename batch 64/cap 128. Sketch scores
never generate hits or heat; only full original passages do.

## Result index

| Study | Results | Configuration / durable evidence |
|---|---|---|
| Current default: Cascade40 vs fresh controls, Postgres/CPython/Kubernetes | [Independent comparison](#current-independent-comparison) | [Recommended preset](recommended.json), [audited evidence](comparison-evidence.json) |
| Cascade40 vs Cascade24, development runs and comprehensiveness | [Cascade development](#cascade-development-results) | [All Cascade evidence](cascade-evidence.json), [24-passage preset](cascade-lean24.json) |
| Sieve local ranking/sketches and Structure source/call-graph experiments | [Creative approaches and negative results](#creative-approaches-and-rejected-settings) | [Sieve evidence](sieve-evidence.json), [Structure evidence](structure-results.json) |
| Compact schema, scouts, 12/16-file budgets | [Cost and quality](#cost-and-quality), [rejected settings](#alternatives-and-rejected-settings) | [Configurations](cost-variants.json), [conservative](cost-conservative.json), [prefilter16](cost-prefilter16.json), [evidence](cost-evidence.json) |
| Vertex integration and macOS spelling Pi queries | [Pi query results](#the-users-two-pi-queries) | [Cases](cost-pi-cases.json), [evidence](cost-evidence.json) |
| Original repeated Postgres finalists and window promotion | [Historical validation](#historical-window-experiments) | [Finalists](finalists.json), [refinement](cost-refinement.json) |
| 1. Batch size and parallelism | [Full report](01_batch_parallel.md) | [Configurations](01_batch_parallel.json) |
| 2. Beam recall | [Full report](02_beam_recall.md) | [Configurations](02_beam_recall.json) |
| 3. Budget stopping | [Full report](03_budget_tuning.md) | [Configurations](03_budget_tuning.json) |
| 4. Whole-file window retrieval | [Full report](04_window_strategy.md) | [Configurations](04_window_strategy.json) |
| 5. Request splitting | [Full report](05_request_split.md) | [Configurations](05_request_split.json) |
| 6. Sniff and deep reads | [Full report](06_sniff_deep.md) | [Configurations](06_sniff_deep.json) |
| 7. Inline and paged retrieval | [Full report](07_inline_paged.md) | [Configurations](07_inline_paged.json) |
| 8. Hybrid discovery and window breadth | [Full report](08_hybrid_finalist.md) | [Configurations](08_hybrid_finalist.json) |
| Main-tree promotion and installed CLI | [Validation](#main-tree-promotion-validation) | Smoke rows and binary identity |
| Every saved run, including failed/rejected trials | [Run index](#recorded-run-index) | Local summaries, manifests and full rows under `bench/runs/` |

## Current independent comparison

Each row is one complete, error-free ten-query suite. Files and regions count
individual targets; lines are macro annotated-line coverage. Costs are estimated
per query. All four macro/micro file/region recall measures exceed 90% for Cascade.

| Suite | Configuration | Files | Regions | Lines | File precision ≥ | Est. USD/query | Median |
|---|---|---:|---:|---:|---:|---:|---:|
| Postgres | Previous window default | 16/16 | 27/29 | 95.9% | 19.8% | $0.009204 | 2.15s |
| Postgres | **Cascade40** | **16/16** | **29/29** | **98.3%** | **37.9%** | **$0.005057** | **1.87s** |
| CPython | Previous window default | 15/16 | 30/31 | 90.7% | 20.5% | $0.006377 | 1.95s |
| CPython | **Cascade40** | **16/16** | **31/31** | **91.1%** | **30.3%** | **$0.004927** | **1.87s** |
| Kubernetes, held out | Previous window default | 17/17 | 34/34 | 99.7% | 18.6% | $0.007635 | 3.52s |
| Kubernetes, held out | **Cascade40** | **17/17** | **33/34** | **94.8%** | **29.9%** | **$0.004526** | **3.30s** |

Across these 30 queries, Cascade found **49/49 files and 93/94 regions** for
$0.145097 versus $0.232162: **37.5% less estimated API cost**. Cost reductions by
suite are 45.1%, 22.7%, and 40.7%. Small latency differences remain subject to
service variation. The one Kubernetes miss is the per-endpoint chain generation
region in kube-proxy. Region recall requires any overlap in the top three ranges;
it does not imply complete coverage. The large CPython bytecode case has about
10.9% annotated-line coverage despite finding its files and regions.

Sieve's local ranking plus source sketches passed Postgres/CPython recall gates
but fell to 80% macro recall on unseen Kubernetes queries. Structure's grouped
source bodies passed the development gates without improving the overall tradeoff;
its optional call graph recovered a WAL helper but reduced CPython line coverage
to 82.5%. These remain worktree experiments. Only Cascade was integrated.
A cheaper Cascade24 preset passed development file/region gates but reduced
CPython line coverage to 85.9%; the default keeps 40 passages for comprehensiveness.

The gold labels are positive-only. Additional hits are unjudged, so precision
lower bounds are reported; these measurements cannot establish conventional
false-positive rates. This is a measured winner, not proof of a global optimum.

- [Full comparison, method, caveats and worktree commits](#creative-approaches-and-rejected-settings)
- [Cascade design and development results](#cascade-development-results)
- [Audited per-query evidence and manifests](comparison-evidence.json)
- [Recommended preset](recommended.json), [previous controls](creative-controls.json), [cheaper development-only preset](cascade-lean24.json)

```sh
cargo build --release
python3 bench/run.py benches/postgres --out bench/runs/cascade-default
# Select the previous strategy explicitly:
python3 bench/run.py benches/postgres --strategies window --out bench/runs/window-comparison
```

## Main-tree promotion validation

Cascade40 is the default in the CLI, the folder runner, and the installed
`~/.cargo/bin/jegrep`. The existing terminal-UI work is integrated: local scan,
filename submission/completion, and queued/running/scored source passages update
the live display. Sketch scores never appear as verified source scores.

The main build passed **32 Rust tests and 12 Python runner tests**. Release build,
strategy registration, default help text, JSON configurations, local report links,
and historical rescoring were checked. Only Cascade was added as a strategy.
The two checks below validate integration and defaults; they do not replace the
full-suite experiments above.

| Promotion check | Files | Regions | Lines | Estimated USD | Seconds | Errors |
|---|---:|---:|---:|---:|---:|---:|
| [cascade-default-promotion-postgres](../runs/cascade-default-promotion-postgres/summary.md) | 2/2 | 4/4 | 99.6% | $0.005708 | 2.08 | 0 |
| [cascade-default-promotion-cli](../runs/cascade-default-promotion-cli/summary.md) | 3/3 | 3/3 | 10.4% | $0.005523 | 2.34 | 0 |

The Postgres checkpoint check uses the folder runner without strategy/config/tuning
options. The CPython bytecode check invokes the installed CLI directly without
strategy, model, or tuning flags. Both recover every expected file and region;
the bytecode case retains its known low line coverage. Installed binary SHA-256:

`b8554715bf07d87cdcc76225d406f21f3062de1ebc2ad8a824a8c334db08ea76`

## Creative approaches and rejected settings

**Cascade — semantic maps before expensive reads.** Build a broad query-derived candidate pool, judge filenames, then submit tiny verbatim sketches of candidate passages from across each file. Pack sketches from multiple files into shared requests with compact identifiers. Only the most promising passages receive full source reads and semantic judgments. The global budget is 40 full 8 KiB passages, instead of paying for many windows in every plausible file. Generated implementation remains eligible. A sketch can guide selection but cannot create result ranges or coverage.

**Sieve — replace the filename-model stage with local retrieval.** Fuse query-concept coverage and term-frequency rankings, produce 1.5 KiB source sketches for 96 files, semantically select up to eight files, then judge their full passages. This reduced dependent model stages and was the fastest CPython candidate. Its purely local shortlist failed to generalize to two held-out Kubernetes cases.

**Structure — retrieve source bodies and follow their calls.** Partition files around functions/methods and neighboring documentation; pack requests by actual source bytes. An optional query-seeded call graph boosts helpers referenced by promising bodies. The graph recovered a missing WAL helper, but under a fixed budget displaced relevant CPython code. The 96 KiB grouped-context variant is its most balanced preset.

All strategies return only genuinely submitted source ranges and preserve original coordinates. The parent reviewed these paths for benchmark-specific rules, oracle use, and invented coverage. Benchmark runner and labels were unchanged.

## Other finalists and negative results

These development-suite rows are the agents' frozen complete-suite measurements, independently rescored by the parent. They are not additional parent reruns. Kubernetes is an independent parent run of the frozen Sieve candidate.

| Suite | Candidate | Files | Regions | Lines | File precision ≥ | Est. USD/query | Median |
|---|---|---:|---:|---:|---:|---:|---:|
| Postgres | Sieve fused | 16/16 | 28/29 | 97.3% | 31.7% | $0.008625 | 2.22s |
| CPython | Sieve fused | 15/16 | 29/31 | 89.5% | 41.7% | $0.006926 | 1.61s |
| Kubernetes, held out | Sieve fused | 14/17 | 28/34 | 80.0% | 30.5% | $0.005125 | 3.21s |
| Postgres | Structure context96 | 16/16 | 27/29 | 90.1% | 20.0% | $0.008909 | 2.32s |
| CPython | Structure context96 | 15/16 | 30/31 | 90.8% | 24.1% | $0.006399 | 1.93s |
| Postgres | Structure graph96 | 16/16 | 28/29 | 93.3% | 20.1% | $0.008919 | 2.46s |
| CPython | Structure graph96 | 15/16 | 28/31 | 82.5% | 21.8% | $0.006367 | 1.91s |

Both structural variants exceed 90% macro/micro file/region recall on both development suites; the graph's 82.5% CPython line coverage is nevertheless a substantial comprehensiveness regression. Neither justified further paid held-out testing after the Cascade result. Sieve qualifies on the development suites but fails the held-out recall floor: macro file/region recall is 80%, micro is 82.35%. Optimistic concurrency and informer-reflector queries failed completely. No tuning followed held-out feedback.

The Cascade agent also tested a **24-passage** budget: 100% file/region recall on both development suites, costing $0.004116 / $0.003976 per query at 1.78s / 1.80s. Line coverage fell to 96.6% / 85.9%, including partial dictionary and exception-unwinding coverage. It is the cheaper development-only option when file/region recall alone defines acceptance. It was not independently validated on Kubernetes. The 40-passage recommendation was frozen before the parent revealed held-out results.

Rejected one-wave Sieve variants were exceptionally fast on narrow subsets but missed large fractions of annotated regions. Narrow local file pools also passed small subsets while failing full suites. Their negative evidence is preserved in [sieve-evidence.json](sieve-evidence.json) and the recorded-run index below.

## Meaning and limits of these scores

- Eligibility requires **strictly greater than 90%** macro and micro file recall and annotated-region recall in each suite, with complete runs, no errors, and complete reported cost. Cost and speed stay separate objectives; line coverage and precision lower bounds remain visible. No arbitrary blended score hides a recall failure.
- A region counts as found when a returned top-three range overlaps it. This is discovery, not full comprehension. Annotated-line coverage measures how much of the gold code is actually covered. Cascade's generated CPython bytecode case has only about 10.9% line coverage on the final parent run despite finding the annotated regions; the very large annotation is not fully read.
- Cascade's one held-out miss is `pkg/proxy/iptables/proxier.go:1215–1243`, the per-endpoint chain generation region in `syncProxyRules`. The file was found, but that region was not returned. Kubernetes line coverage fell 4.9 points relative to the current control; this is the observed price of its 40.7% cost reduction there.
- Labels are positive-only, so extra hits are **unjudged**, not established false positives. File precision lower bounds improved, but conventional precision, false-positive rate, and F-scores cannot be established from this gold set. The evidence retains unjudged counts and missed targets.
- Ten cases per suite are a small sample. The Cascade agent's full development runs and the parent's final-binary reruns agree on file/region recall, but they do not establish a universal guarantee. Separate run order and service variation make the small latency differences less certain than the token-cost differences.
- Every timed query starts a new process and includes local scanning/preparation and all model calls. There is no persistent application index. OS filesystem caches were not flushed. Builds, queue waiting on the shared benchmark lock, and report generation are outside query latency.
- API costs are the repository's estimates from reported input usage, not invoice totals. Output tokens are recorded separately under that pricing model. All valid comparisons use `jev-latest`; the service rejected the explicit `jev-1.13.0` model ID, so an immutable version was unavailable.


## Cascade development results

The existing runner and ground truth are unchanged. Each configuration ran all ten cases once per suite; earlier four-case development subsets are reported separately in the evidence JSON. Model selection was the production alias `jev-latest`, because the parent confirmed the explicit version string was rejected by the provider. All requests used the runner's global request lock. No `--allow-concurrent`, supplemental oracle keywords, warm application index, or hidden preprocessing was used.

| Suite | Configuration | File macro / micro | Region macro / micro | Line macro | File P≥ | Line P≥ | Cost/query | Median |
|---|---|---:|---:|---:|---:|---:|---:|---:|
| postgres | high-recall-compact12 | 100.0% / 100.0% | 100.0% / 100.0% | 99.00% | 22.8% | 8.7% | $0.011307 | 2.60s |
| postgres | current-prefilter16 † | 100.0% / 100.0% | 100.0% / 100.0% | 99.00% | 20.2% | 8.9% | $0.008811 | 2.33s |
| postgres | cascade40 | 100.0% / 100.0% | 100.0% / 100.0% | 98.26% | 39.9% | 13.8% | $0.005053 | 2.09s |
| postgres | cascade24 | 100.0% / 100.0% | 100.0% / 100.0% | 96.64% | 37.9% | 16.0% | $0.004116 | 1.78s |
| cpython | high-recall-compact12 | 96.7% / 93.8% | 96.7% / 96.8% | 90.90% | 20.5% | 7.7% | $0.009647 | 2.21s |
| cpython | current-prefilter16 | 96.7% / 93.8% | 96.7% / 96.8% | 90.65% | 20.5% | 7.3% | $0.006377 | 1.95s |
| cpython | cascade40 | 100.0% / 100.0% | 100.0% / 100.0% | 90.99% | 29.6% | 10.1% | $0.004862 | 1.88s |
| cpython | cascade24 | 100.0% / 100.0% | 100.0% / 100.0% | 85.92% | 35.3% | 12.8% | $0.003976 | 1.80s |

† The parent's current Postgres control had one API-error/retry run, making its cost incomplete and excluding it from Pareto claims. Its figures remain visible, but the error-free high-recall control is the primary comparison. Controls came from the parent's separate worktree; this agent did not rerun them. Separate experiment order and service variation limit latency causal claims. Costs estimate reported input usage at the repository's configured rate, not invoice amounts.

All labels are positive-only. Precision lower bounds count known relevant hits among all returned hits; extra hits are unjudged, not proven false positives. No conventional false-positive rate is established.

## Comprehensiveness and failure modes

Both cascade budgets discover the enormous generated-bytecode region, which the current and high-recall CPython controls miss, but finding a region is not equivalent to reading it completely. In the full cascade40 run, the bytecode query covered only 9.9% of its 20,555 annotated lines; other CPython queries achieved full annotated-line coverage. This single query drives the 91.0% macro line result. There is no fabricated heat over the unexamined generated source.

Cascade24 reduces additional cost by roughly 18–19% relative to 40 but cuts CPython dictionary line coverage to 70.7% and exception-unwinding coverage to 78.5%; Postgres hash-join coverage falls to 83.8%. These are real comprehensiveness losses despite unchanged file/region recall.

Retrieval remains bounded to the query-derived 128-file pool and 20 semantic/lexical selections. It can miss a relevant file with no lexical bridge, or a needed passage omitted from the 24-window local map. It has not proved an optimum or a universal >90% guarantee. Filename/model judgments vary between calls, and this full-suite comparison is one repeat per variant; parent validation should check variance and unseen repositories.

## Cold preparation and cost accounting

Every measured search is a new process. The strategy has no persistent index or application cache; all local scanning, reading, sketch construction, and model calls occur inside the timed query. OS filesystem caches were not flushed, so this is an application-cold measurement, not a claim of cold disk I/O. Offline preprocessing cost and persistent-cache storage are zero.

| Suite | Mean local preparation | First-query preparation | Mean filename tokens | Mean map tokens | Mean full-source tokens | Mean mapped / fully judged / pruned passages |
|---|---:|---:|---:|---:|---:|---:|
| postgres | 368.6 ms | 366 ms | 12,550 | 40,482 | 67,289 | 238.2 / 31.3 / 206.9 |
| cpython | 324.6 ms | 315 ms | 12,629 | 38,569 | 64,554 | 219.6 / 32.5 / 187.1 |

`cascade_prepare_ms` reports local scan/map preparation. Final source additionally counts full-request packing in that timing. `cascade_map_tokens` separates routing cost from `window_name_tokens` and `window_content_tokens`. The final source also counts every file whose sketch was sent and counts sketch bytes in total transmitted file bytes; the v2 measurements predate those reporting-only corrections. Input-token cost, latency, and scored search results are unchanged by those accounting corrections.


Development measurements below precede promotion; the independent final-binary comparison above is the current recommendation.

## Compact schema and prefilter results

This section preserves the earlier cost study; its “new default” means the window preset selected at that time.

**Historical recommendation: `window-prefilter16` (superseded by Cascade).** Compact source passages, up to 16
candidate files, and two scout windows for files whose filename score is below
0.5. A scout scoring at least 0.5 unlocks the remaining 16-window budget. The
user accepted recall above 90%; this configuration cleared that target for
both file and annotated-region recall on Postgres and CPython, including when
counting individual targets rather than averaging percentages across queries.

This remains the explicit `window` strategy preset, saved in
[cost-prefilter16.json](cost-prefilter16.json). The application default is now Cascade.
It retains 128 lexical candidates, 8 KiB windows, parallel 16, filename batches
of 64, and the ordinary final passage-relevance threshold of 0.2. Source code,
comments, indentation, and original output coordinates are preserved.

## Cost and quality

Costs below are **per query**, averaged over the measured searches. The original
default was measured twice on Postgres and once on CPython; the new preset was
measured once on Postgres and twice on CPython. Each suite contains ten queries.
These are bounded comparisons, not proof of an optimal setting or a guarantee
that every individual query exceeds 90% recall.

| Suite / configuration | Searches | File recall, macro | File targets found | Region recall, macro | Region targets found | Line recall, macro | Est. USD / query | Median seconds |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| Postgres, original | 20 | 100.0% | 32/32 (100%) | 100.0% | 58/58 (100%) | 99.0% | $0.018006 | 2.36 |
| Postgres, compact only | 20 | 100.0% | 32/32 (100%) | 100.0% | 58/58 (100%) | 99.0% | $0.011368 | 2.22 |
| **Postgres, new default** | **10** | **100.0%** | **16/16 (100%)** | **95.0%** | **27/29 (93.1%)** | **95.9%** | **$0.008782** | **2.08** |
| CPython, original | 10 | 96.7% | 15/16 (93.8%) | 96.7% | 30/31 (96.8%) | 90.8% | $0.014870 | 2.06 |
| CPython, compact only | 10 | 96.7% | 15/16 (93.8%) | 96.7% | 30/31 (96.8%) | 90.9% | $0.009842 | 2.06 |
| **CPython, new default** | **20** | **96.7%** | **30/32 (93.8%)** | **95.0%** | **59/62 (95.2%)** | **90.1%** | **$0.006422** | **1.78** |

The new default reduced average cost by **51.2% on Postgres** and **56.8% on
CPython**, with median latency lower by approximately 12% and 13%, respectively.
The additional savings over compact-only requests come with lower coverage of
some deep functions. Postgres missed the annotated `BufferSync` and
`CheckPointBuffers` regions in its checkpoint query while still returning
`bufmgr.c`. CPython still missed `Python/generated_cases.c.h` for the bytecode
loop query, as the original default did, and one dictionary region was omitted
in one repeat. Both repeated CPython searches recovered `Python/bytecodes.c`
for exception unwinding with the 16-file preset.

### The user's two Pi queries

Both configurations were measured twice per query against the current `/work/pi`
checkout. The known-positive checks require `google-vertex.ts` and
`google-shared.ts` for the first query, and `spelling.rs` and `macos-spelling.ts`
for the second. All four targets were found in both repeats. These checks are
not exhaustive annotations and do not establish complete recall on Pi.

| Query | Original tokens | New tokens | Original USD | New USD | Cost reduction | Original / new median seconds |
|---|---:|---:|---:|---:|---:|---:|
| `vertex api integration` | 207,564 | 171,946 | $0.008718 | $0.007222 | 17.2% | 1.86 / 1.99 |
| `osx typo correction stuff` | 285,945 | 184,018 | $0.012010 | $0.007729 | 35.6% | 1.82 / 1.85 |

The wider 16-file candidate budget costs a little more on Pi than the narrower
12-file prefilter. It recovered the additional CPython target needed to keep
target-count recall above 90%. There is no measured speed gain on the two Pi
queries; the improvements there are in token cost. The user's pasted runs were
$0.0088 and $0.0120; the table uses fresh measured controls instead.

## What changed

1. **Compact passage schema.** The model receives a map from short passage IDs
   to unmodified source text, with shared search criteria. It no longer receives
   an `L1234|` label on every source line, redundant start/end metadata, and
   repeated location instructions. The caller retains all coordinates locally.
   Window boundaries and lexical rankings are unchanged by compact mode.
2. **Content prefilter.** Files with a filename score below the scout cutoff get
   their two strongest lexical windows first, wherever those occur in the file.
   A strong scout unlocks the rest; weak name and content evidence stop further
   paid reads. Strong filenames and prior strategy hits retain the full budget.
   Missing filename judgments, absent lexical matches, failed scout requests,
   and incomplete scout responses do not justify pruning.
3. **Broader inexpensive discovery.** Raising the file ceiling from 12 to 16
   recovered exception bytecode implementation that the narrower prefilter runs
   missed. Weak additional candidates pay for scouts before a full read.
4. **Cost accounting.** JSON stats separate `window_name_tokens` and
   `window_content_tokens`, and include `windows_judged` / `windows_pruned`.
   The benchmark retains these fields in each complete result row.
5. **Recall accounting.** Runner JSON/CSV summaries now include
   `micro_file_recall`, `micro_span_recall`, and region target counts, alongside
   per-query macro averages. Both forms contribute to the Pareto comparison.

This does not minify code, remove comments, summarize functions, or depend on
ground-truth keywords. All candidate and window ranking uses the user's query.
In the user's original-format controls, content judgments accounted for roughly
95–96% of input tokens; filename prompts were a much smaller part of the bill.

## Alternatives and rejected settings

The compact-only mode preserved the observed file and region recall on both
annotated suites and saved 36.9% on Postgres and 33.8% on CPython. It remains the
choice for a stricter coverage objective:

```sh
JEGREP_WINDOW_ADAPTIVE=0 JEGREP_WINDOW_FILES=12 jegrep -s window "your query" /path/to/repo
```

The 12-file prefilter at cutoff 0.5 was cheaper but recovered only 14/16 CPython
file targets (87.5%), despite its 93.3% macro file-recall score. Lowering both
scout gates to 0.2 restored Postgres region coverage, but that separate CPython
run also found 14/16 targets. Because filename judgments and the selected file
set vary between API calls, the missing file cannot be attributed solely to
content pruning. The 16-file preset was selected based on its measured results
under both macro and target-count recall, rather than a claim about that cause.

| Setting | Postgres cost / query | Postgres macro region recall | CPython cost / query | CPython file targets |
|---|---:|---:|---:|---:|
| Compact only, 12 files | $0.011368 | 100% | $0.009842 | 15/16 |
| Scout 0.2, 12 files | $0.010189 | 100% | $0.007222 | 14/16 |
| Scout 0.5, 12 files | $0.007788 | 95% | $0.005557 | 14/16 |
| **Scout 0.5, 16 files** | **$0.008782** | **95%** | **$0.006422** | **30/32** |

`JEGREP_WINDOW_SCOUT_THRESHOLD=0.2` lowers the gates for either budget.
`JEGREP_WINDOW_ADAPTIVE=0` disables scouting. `JEGREP_WINDOW_COMPACT=0` restores
the original request format. To reproduce the original default, also disable
scouting and set `JEGREP_WINDOW_FILES=12`. Increasing the scout threshold may
reduce cost further at the expense of relevant files or deeper functions.

## Precision and reproducibility

All suites have positive labels only. Extra results remain unjudged, not proven
false positives. The new preset's file-precision lower bound was 19.5% on
Postgres and 20.7% on CPython; the original controls were 27.6% and 23.2%.
The wider pool and changed judgments return more extra hits. This optimization
does not demonstrate improved precision.

Postgres and CPython used the clean revisions recorded in their `tag.json`
files. Pi used the user's dirty working checkout at commit
`04588ba502fe2d0403dec4ae4f7acb0ac0b10ab0`; its starting diff hash is recorded in
each manifest. Configurations, target revisions, model selection, binary hashes,
and scoring settings are saved with every run. Model selection was `jev-latest`;
the remote alias is not an immutable model version. Filename scores and selected
files can vary across repeated calls.

API traffic used the runner's shared lock. Within the original comparisons,
configuration order rotated between queries and repeats. Subsequent prefilter
refinements were separate experiments, so latency comparisons can include
service variation. Costs are estimates from reported input tokens at the
configured **$0.042 per million input tokens**, consistent with the
[TypeSafe model documentation](https://docs.typesafe.ai/models). They are not
invoice totals.

- [Committed derived measurements](cost-evidence.json), including target-count
  recall, phase token totals, pruned windows, and per-query cost.
- Original three-way trials: [Postgres](../runs/cost-postgres-trial/summary.md),
  [CPython](../runs/cost-cpython-trial/summary.md),
  [Pi](../runs/cost-pi-trial/summary.md).
- Conservative 12-file trials: [Postgres](../runs/cost-postgres-conservative/summary.md),
  [CPython](../runs/cost-cpython-conservative/summary.md),
  [Pi](../runs/cost-pi-conservative/summary.md).
- Chosen 16-file preset: [Postgres](../runs/cost-postgres-prefilter16/summary.md),
  [CPython](../runs/cost-cpython-prefilter16/summary.md),
  [Pi](../runs/cost-pi-prefilter16/summary.md).

The comparison phase comprised **170 searches, estimated $1.694782**, with no
API errors or retries. Each run directory retains complete rows, manifests, and
summaries locally; `bench/runs` is ignored by Git. Initial trials used the frozen
`window-cost-v1` binary, the 0.2 scouts used `window-cost-v3`, and the chosen
16-file preset used `window-cost-v4`. The current configuration files explicitly
pin measured settings; their cutoff overrides describe earlier hardcoded gates.

## Historical window validation and installation

Eighteen Rust tests and twelve Python runner tests passed, including source/line
preservation, scout budget selection, failure fallback, follow-up job accounting,
and the distinction between macro and target-count recall. JSON configurations
and whitespace checks passed.

The shared checkout had concurrent unfinished terminal-UI changes referencing a
not-yet-created `src/ui/live.rs`. The cost changes were therefore built and tested
in a separate source snapshot using the last complete staged UI. Shared UI edits
were left intact. At that historical promotion, `/Users/can/.cargo/bin/jegrep` contained the compact
schema and 16-file prefilter defaults, with that complete log UI. Its SHA-256 is
`cbc442667c89b308ad2d48475d352c85303904eb3bc329fe1dfc3b838b08777d`.

A final [installed-default smoke test](../runs/cost-prefilter-default-pi/summary.md)
used no strategy or tuning flags. Both Pi queries retained both known-positive
files, read 16 files, and successfully pruned 2 and 38 windows, respectively.
Reported cost was $0.007316 and $0.007662, with no API errors. That historical validation predates the current Cascade integration and terminal UI.

## Historical window experiments

The remainder preserves the earlier eight-agent experiments and window validation.
The later [cost follow-up](#compact-schema-and-prefilter-results) introduced compact requests and scouts.
Those recommendations are superseded by Cascade; saved configurations retain the
settings that produced their measurements.

**Earlier recommendation (superseded): `window-balanced16`**, a query-only `window` preset with 128 lexical candidates, 12 content files, 16 passages per file, 8 KiB passages, parallel 16, and filename batches of 64. The requested packing setting is 4, bounded internally to 3 passages / 24 KiB of passage content per request. Search does not receive benchmark keywords, symbols, or expected paths.

It recovered all 16 expected file targets and all 29 annotated regions in each of two runs over the ten-query suite: **32/32 file targets and 58/58 regions**. It covered 99.0% of annotated lines on average. This is the best measured cost among the full-suite configurations that recovered every file and region; it is not a proof of global optimality or performance on unseen repositories.

```sh
cargo build --release
python3 bench/run.py benches/postgres --configs bench/experiments/cost-refinement.json --repeat 2 --out bench/runs/my-postgres
```

## Full-suite validation

Each row contains 20 searches: the same ten queries repeated twice. Quality percentages are macro averages across query/repeat pairs. Estimated cost is the total for all 20 searches, and latency is per search. Target: clean Postgres commit `e73841ffbceea314cf9fa3f64a5eae9f87a46449`.

| Configuration | File recall | Region recall | Line recall | File P≥ | Median s | p95 s | Est. USD / 20 |
|---|---:|---:|---:|---:|---:|---:|---:|
| window-balanced16 | 100.0% | 100.0% | 99.0% | 28.3% | 2.43 | 2.66 | $0.35986 |
| window-breadth | 100.0% | 100.0% | 99.1% | 27.3% | 2.46 | 2.93 | $0.42746 |
| window-lean | 85.0% | 70.0% | 60.2% | 28.7% | 1.55 | 1.85 | $0.10270 |
| hybrid-window-lean | 95.0% | 95.0% | 97.3% | 27.8% | 4.43 | 6.80 | $0.49840 |
| baseline | 85.0% | 39.6% | 6.7% | 47.3% | 2.21 | 3.39 | $0.23726 |
| beam | 77.5% | 35.8% | 6.9% | 60.8% | 2.09 | 5.73 | $0.14135 |

The 16-passage preset costs **15.8% less** than the 24-passage preset with the same observed file/region recall, giving up about **0.14 percentage points of line coverage**. Their median times differ by only 0.04 seconds; this is too small to claim a dependable speed advantage from two repeats. The lean preset is cheaper/faster but missed two entire queries across the repeats. The tested hybrid used a smaller file/candidate budget than window-breadth, so this experiment does not establish that composition itself causes its misses.

Baseline and beam each scored **20/20 query success** under the old “found any acceptable file” metric, even though they missed 6 and 9 expected file targets respectively. A single success score hides both missing files and unvisited implementation regions.

## Precision and completeness limits

Postgres annotations contain positive labels only. The recommended preset returned 85 additional file hits across 20 searches. These are **unjudged**, not established false positives. Its macro file-precision lower bound is 28.3%; the micro ratio across all returned files is 32/117 = 27.4%. Its line-precision lower bound is 11.3%, reflecting broad passages and partially labeled relevant code. Better recall therefore comes with more material for the reader to inspect. The 24-window preset has a 10.3% line-precision lower bound; the reference baseline and beam have about 24.9% and 31.1%.

Region recall means at least one annotated line is covered by the top three reported ranges in that file. It does not imply the entire function was returned. Line recall and mean span coverage make that distinction explicit. To call every extra file a false positive, run with `--closed-world` or provide exhaustive/negative labels. True-negative counts and conventional false-positive rates cannot be inferred from the current labels.

## Eight agent experiments

Agents ran in waves of up to three due to the session concurrency limit. Their API experiments used a common runner lock; separate agents did not overlap measured HTTP workloads. Frozen binaries keep concurrent source edits out of measured configurations. The first-stage comparison subset was checkpoint creation, hash-join execution, and deadlock detection.

| Experiment | Finding | Evidence |
|---|---|---|
| 1. Batch / parallel | Batch 32 recovered an extra file at higher cost; doubling workers did not improve recall. Baseline max_batch is inert when it is already ≥ batch. | [Report](01_batch_parallel.md) |
| 2. Beam recall | Larger prefixes hit context limits at 64–512 KiB; more bytes is not a reliable way to reach distant functions. | [Report](02_beam_recall.md) |
| 3. Budget stopping | Disabling confident-hit settling and widening exploration improved file recall, at substantially greater cost. CLI thresholds are ignored internally. | [Report](03_budget_tuning.md) |
| 4. Window strategy | Whole-file local ranking plus bounded semantic passages recovered deep implementation regions cheaply. | [Report](04_window_strategy.md) |
| 5. Request splitting | Independent Noul splitting gave no recall gain, +4.8% cost, and mixed timing; remains opt-in. Choice probabilities remain intact. | [Report](05_request_split.md) |
| 6. Sniff / deep | Both remain limited by prefixes; deep accepts relevant short reads without upgrading them for completeness. | [Report](06_sniff_deep.md) |
| 7. Inline / paged | Paged was cheaper/faster; inline found one additional region. Neither reached checkpoint implementations. | [Report](07_inline_paged.md) |
| 8. Hybrid / breadth | More bounded windows recovered the missed hash-probe helper; window-only beat the tested beam-seeded version on cost/speed. | [Report](08_hybrid_finalist.md) |

After the eight agents, full-suite validation compared lean, broad, and hybrid candidates. A final controlled cost refinement changed only the broad preset’s per-file passage budget from 24 to 16, then repeated all ten queries twice.

## Reproducibility and validation

At the time of these historical measurements, the recommended window configuration
was promoted for normal searches and the folder runner; Cascade now supersedes it. A post-promotion smoke check over checkpoint creation and
hash-join execution, using no strategy or tuning flags, recovered every annotated
file and region without errors. Its evidence is in
[default-promotion](../runs/default-promotion/summary.md). Saved experiment
configurations explicitly pin all measured knobs, retaining their historical
settings after the default change.

- [Recommended preset](recommended.json); [finalist configurations](finalists.json); [cost refinement](cost-refinement.json).
- [Reference runs](../runs/reference/summary.md), [finalists](../runs/finalists/summary.md), [cost refinement](../runs/cost-refinement/summary.md). Each directory contains its manifest, full rows, CSV, and JSON summaries.
- Reference runs used the original frozen binary; the final window/hybrid runs used `final-jegrep-v2`. These measurements preceded the subsequent default promotion; all measured parameters are retained in each manifest. Full-suite runs had no API errors or retries.
- Fourteen Rust tests and eleven Python tests passed, including mock HTTP tests for splitting/failover and an end-to-end runner test covering folder discovery, all strategies, repeats, resume, and configuration mismatch rejection.
- Before default promotion, across smoke tests, agent sweeps, references, finalists, and cost refinement: **177 measured search invocations, $2.84685 in reported cost estimates**. Three early oversized-prefix runs contained API errors; their failed-request charges may be absent from reported usage. All failed evidence is retained.
- Following validation, the user selected `window-balanced16` as the default: normal searches and the runner then used window, parallel 16, filename batch 64 / cap 128, 128 candidates, 12 files, and 16 passages per file. Question splitting remains opt-in.
- [Runner usage and scoring](../README.md).

## Reproduction, provenance and spending

The creative experiments started at `f5b0231ee510f3581a298576d6d91bb5281b14e4`.
Cascade source/evidence commit: `2bacc95d1b41ab157bd287a011c858cfa6ae19bd`;
Sieve: `a68117089dfdbe0aa95f83d056443d53d11540af`;
Structure: `bfc8bfe150b90c6f59eacc139a9db129751f8f55`.
Only Cascade is integrated as a main-tree strategy. Other candidates remain
reproducible from their original worktrees/commits; their configurations and
measurements here are historical records, not registered main-tree strategies.

The original frozen Cascade binary SHA-256 is
`2e532775fa6a3af23215db59211b3a477890e97af19c1ecb260820e55029a237`.
The exact-base control binary is
`c5cd01c907cdaece7b2d5858d80f6e49fb54dd9ec5399b48c4f2c0ecb0b04910`.
Targets: Postgres `e73841ffbceea314cf9fa3f64a5eae9f87a46449`,
CPython `aa5407091f60a68187f3b82a4d954b03904af4d4`,
Kubernetes `96b5e4e3ae3f8d7aa50d360cf57f01eaba61f855`.
All valid creative comparisons used `jev-latest`; the provider rejected the
explicit version string `jev-1.13.0`. The first current-window Postgres control
had an API error and incomplete cost; a complete error-free repeat supplied the
control in the current comparison. Failed evidence is retained.

[comparison-evidence.json](comparison-evidence.json) contains canonical cases,
per-query scores/stats, summaries, configurations, manifests and hashes.
[cascade-evidence.json](cascade-evidence.json), [sieve-evidence.json](sieve-evidence.json),
and [structure-results.json](structure-results.json) preserve development trials.
[creative-controls.json](creative-controls.json) pins both fresh controls.
The original manifest paths remain provenance; copied raw runs are now available
locally under `bench/runs/` and indexed below. That directory is ignored by Git.

[audit_results.py](audit_results.py) recomputes scores from raw hits, verifies the
runner hash, target revision, canonical labels, case counts, query-only inputs,
serialized traffic and all four >90% recall gates. For historical creative runs,
select the exact historical runner so the new CLI default does not invalidate its
hash; this does not relax any score checks:

```sh
python3 bench/experiments/audit_results.py --runner-ref f5b0231 benches/kubernetes bench/runs/creative-final-cascade40-kubernetes
```

The creative study recorded 345 search attempts (324 error-free, 20 rejected
explicit-model attempts, and one API-error control) and **$2.188864** estimated
usage: Cascade $0.266121, Sieve $0.508257, Structure $0.635346, parent $0.779139.
Failed calls may incur unreported charges. The earlier eight-agent phase recorded
177 searches/$2.84685, and the compact-schema cost study 170 searches/$1.694782.
Those are separate historical study totals, not per-query production costs.

## Recorded run index

Every saved summary is indexed here, including subsets, failed calls, rejected variants,
controls, repeats, held-out tests and promotion checks. Each linked folder also retains
its manifest and full rows. A small-subset success is not a full-suite result.

| Run | Configurations | Completed / planned rows | Error rows |
|---|---|---:|---:|
| [01-batch-parallel](../runs/01-batch-parallel/summary.md) | `baseline-b32-p20`, `baseline-b128-p20`, `baseline-b32-p40` | 9/9 | 0 |
| [02-beam-recall](../runs/02-beam-recall/summary.md) | `beam-default`, `beam-recall-512k`, `beam-recall-128k` | 3/9 | 2 |
| [02-beam-recall-safe](../runs/02-beam-recall-safe/summary.md) | `beam-default`, `beam-recall-64k` | 5/6 | 1 |
| [03-budget-tuning](../runs/03-budget-tuning/summary.md) | `budget-unsettled-wide`, `budget-unsettled`, `budget-default-explicit` | 9/9 | 0 |
| [04-window-common3](../runs/04-window-common3/summary.md) | `window-wide`, `window-lean` | 6/6 | 0 |
| [05-request-split](../runs/05-request-split/summary.md) | `baseline-unsplit`, `baseline-question-chunk64` | 6/6 | 0 |
| [06-sniff-deep](../runs/06-sniff-deep/summary.md) | `deep-small16-recall`, `sniff-recall-head` | 6/6 | 0 |
| [07-inline-paged](../runs/07-inline-paged/summary.md) | `inline16-packed-recall`, `paged-grep-fast-recall` | 6/6 | 0 |
| [08-hybrid-common3](../runs/08-hybrid-common3/summary.md) | `window-breadth`, `hybrid-window-lean` | 6/6 | 0 |
| [cascade-default-promotion-cli](../runs/cascade-default-promotion-cli/summary.md) | `cascade-cli-default` | 1/1 | 0 |
| [cascade-default-promotion-postgres](../runs/cascade-default-promotion-postgres/summary.md) | `cascade` | 1/1 | 0 |
| [cascade-final-default-smoke](../runs/cascade-final-default-smoke/summary.md) | `cascade` | 1/1 | 0 |
| [cascade-lean24-cpython-full](../runs/cascade-lean24-cpython-full/summary.md) | `cascade-lean24` | 10/10 | 0 |
| [cascade-lean24-pg-dev](../runs/cascade-lean24-pg-dev/summary.md) | `cascade-lean24` | 2/2 | 0 |
| [cascade-lean24-postgres-full](../runs/cascade-lean24-postgres-full/summary.md) | `cascade-lean24` | 10/10 | 0 |
| [cascade-lean24-py-dev](../runs/cascade-lean24-py-dev/summary.md) | `cascade-lean24` | 2/2 | 0 |
| [cascade-stable-default-smoke](../runs/cascade-stable-default-smoke/summary.md) | `cascade` | 1/1 | 0 |
| [cascade-v1-pg-dev](../runs/cascade-v1-pg-dev/summary.md) | `cascade-v1` | 2/2 | 0 |
| [cascade-v1-py-dev](../runs/cascade-v1-py-dev/summary.md) | `cascade-v1` | 2/2 | 0 |
| [cascade-v2-cpython-full](../runs/cascade-v2-cpython-full/summary.md) | `cascade-v2` | 10/10 | 0 |
| [cascade-v2-pg-dev](../runs/cascade-v2-pg-dev/summary.md) | `cascade-v2` | 2/2 | 0 |
| [cascade-v2-postgres-full](../runs/cascade-v2-postgres-full/summary.md) | `cascade-v2` | 10/10 | 0 |
| [cascade-v2-py-dev](../runs/cascade-v2-py-dev/summary.md) | `cascade-v2` | 2/2 | 0 |
| [cost-cpython-conservative](../runs/cost-cpython-conservative/summary.md) | `window-conservative` | 10/10 | 0 |
| [cost-cpython-prefilter16](../runs/cost-cpython-prefilter16/summary.md) | `window-prefilter16` | 20/20 | 0 |
| [cost-cpython-trial](../runs/cost-cpython-trial/summary.md) | `window-compact`, `window-current`, `window-adaptive` | 30/30 | 0 |
| [cost-default-pi](../runs/cost-default-pi/summary.md) | `window` | 2/2 | 0 |
| [cost-pi-conservative](../runs/cost-pi-conservative/summary.md) | `window-conservative` | 4/4 | 0 |
| [cost-pi-prefilter16](../runs/cost-pi-prefilter16/summary.md) | `window-prefilter16` | 4/4 | 0 |
| [cost-pi-trial](../runs/cost-pi-trial/summary.md) | `window-adaptive`, `window-compact`, `window-current` | 12/12 | 0 |
| [cost-postgres-conservative](../runs/cost-postgres-conservative/summary.md) | `window-conservative` | 20/20 | 0 |
| [cost-postgres-prefilter16](../runs/cost-postgres-prefilter16/summary.md) | `window-prefilter16` | 10/10 | 0 |
| [cost-postgres-trial](../runs/cost-postgres-trial/summary.md) | `window-compact`, `window-current`, `window-adaptive` | 60/60 | 0 |
| [cost-prefilter-default-pi](../runs/cost-prefilter-default-pi/summary.md) | `window` | 2/2 | 0 |
| [cost-refinement](../runs/cost-refinement/summary.md) | `window-balanced16` | 20/20 | 0 |
| [creative-control-postgres](../runs/creative-control-postgres/summary.md) | `current-prefilter16`, `high-recall-compact12` | 20/20 | 20 |
| [creative-controls-cpython](../runs/creative-controls-cpython/summary.md) | `high-recall-compact12`, `current-prefilter16` | 20/20 | 0 |
| [creative-controls-postgres](../runs/creative-controls-postgres/summary.md) | `high-recall-compact12`, `current-prefilter16` | 20/20 | 1 |
| [creative-current-postgres-repeat](../runs/creative-current-postgres-repeat/summary.md) | `current-prefilter16` | 10/10 | 0 |
| [creative-final-cascade40-cpython](../runs/creative-final-cascade40-cpython/summary.md) | `cascade-v2` | 10/10 | 0 |
| [creative-final-cascade40-kubernetes](../runs/creative-final-cascade40-kubernetes/summary.md) | `cascade-v2` | 10/10 | 0 |
| [creative-final-cascade40-postgres](../runs/creative-final-cascade40-postgres/summary.md) | `cascade-v2` | 10/10 | 0 |
| [creative-heldout-cascade40](../runs/creative-heldout-cascade40/summary.md) | `cascade-v2` | 10/10 | 0 |
| [creative-heldout-current](../runs/creative-heldout-current/summary.md) | `current-prefilter16` | 10/10 | 0 |
| [creative-heldout-sieve](../runs/creative-heldout-sieve/summary.md) | `sieve-fused-sketch` | 10/10 | 0 |
| [creative-model-check](../runs/creative-model-check/summary.md) | `window` | 1/1 | 0 |
| [default-promotion](../runs/default-promotion/summary.md) | `window` | 2/2 | 0 |
| [finalists](../runs/finalists/summary.md) | `window-breadth`, `hybrid-window-lean`, `window-lean` | 60/60 | 0 |
| [reference](../runs/reference/summary.md) | `baseline`, `beam` | 40/40 | 0 |
| [sieve-dev-pg-v1](../runs/sieve-dev-pg-v1/summary.md) | `sieve-96x4k` | 2/2 | 0 |
| [sieve-dev-pg-v2](../runs/sieve-dev-pg-v2/summary.md) | `sieve-content-first` | 2/2 | 0 |
| [sieve-dev-pg-v3](../runs/sieve-dev-pg-v3/summary.md) | `sieve-sketch` | 2/2 | 0 |
| [sieve-dev-py-v1](../runs/sieve-dev-py-v1/summary.md) | `sieve-96x4k` | 2/2 | 0 |
| [sieve-dev-py-v2](../runs/sieve-dev-py-v2/summary.md) | `sieve-content-first` | 2/2 | 0 |
| [sieve-dev-py-v3](../runs/sieve-dev-py-v3/summary.md) | `sieve-sketch` | 2/2 | 0 |
| [sieve-full-cpython](../runs/sieve-full-cpython/summary.md) | `sieve-sketch` | 10/10 | 0 |
| [sieve-full-postgres](../runs/sieve-full-postgres/summary.md) | `sieve-sketch` | 10/10 | 0 |
| [sieve-fused-full-cpython](../runs/sieve-fused-full-cpython/summary.md) | `sieve-fused-sketch` | 10/10 | 0 |
| [sieve-fused-full-postgres](../runs/sieve-fused-full-postgres/summary.md) | `sieve-fused-sketch` | 10/10 | 0 |
| [sieve-wide-dev-cpython](../runs/sieve-wide-dev-cpython/summary.md) | `sieve-wide-sketch` | 2/2 | 0 |
| [sieve-wide-dev-postgres](../runs/sieve-wide-dev-postgres/summary.md) | `sieve-wide-sketch` | 2/2 | 0 |
| [sieve-wide-full-cpython](../runs/sieve-wide-full-cpython/summary.md) | `sieve-wide-sketch` | 10/10 | 0 |
| [sieve-wide-full-postgres](../runs/sieve-wide-full-postgres/summary.md) | `sieve-wide-sketch` | 10/10 | 0 |
| [smoke](../runs/smoke/summary.md) | `beam` | 1/1 | 0 |
| [structure-context-dev-pg](../runs/structure-context-dev-pg/summary.md) | `structure-context` | 3/3 | 0 |
| [structure-context-dev-py](../runs/structure-context-dev-py/summary.md) | `structure-context` | 3/3 | 0 |
| [structure-dev-pg](../runs/structure-dev-pg/summary.md) | `structure32` | 3/3 | 0 |
| [structure-dev-py](../runs/structure-dev-py/summary.md) | `structure32` | 3/3 | 0 |
| [structure-full-pg](../runs/structure-full-pg/summary.md) | `structure-context128`, `structure-context96` | 20/20 | 0 |
| [structure-full-py](../runs/structure-full-py/summary.md) | `structure-context128`, `structure-context96` | 20/20 | 0 |
| [structure-graph-dev-pg](../runs/structure-graph-dev-pg/summary.md) | `structure-graph96` | 2/2 | 0 |
| [structure-graph-dev-py](../runs/structure-graph-dev-py/summary.md) | `structure-graph96` | 2/2 | 0 |
| [structure-graph-full-pg](../runs/structure-graph-full-pg/summary.md) | `structure-graph96` | 10/10 | 0 |
| [structure-graph-full-py](../runs/structure-graph-full-py/summary.md) | `structure-graph96` | 10/10 | 0 |
| [structure-scout-dev-pg](../runs/structure-scout-dev-pg/summary.md) | `structure-scout-context96`, `structure-scout-recall` | 4/4 | 0 |
| [structure-scout-dev-py](../runs/structure-scout-dev-py/summary.md) | `structure-scout-context96`, `structure-scout-recall` | 4/4 | 0 |
