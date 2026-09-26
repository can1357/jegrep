<div align="center">
  <a href="https://github.com/can1357/jegrep">
    <img src="assets/logo.png" alt="jegrep" width="128" height="128" />
  </a>
  <h1>jegrep</h1>
  <p><em>Jevantic grep: describe it, find it.</em></p>
  <a href="https://github.com/can1357/jegrep/actions/workflows/ci.yml"><img src="https://github.com/can1357/jegrep/actions/workflows/ci.yml/badge.svg" alt="CI" /></a>
  <a href="https://github.com/can1357/jegrep/releases/latest"><img src="https://img.shields.io/github/v/release/can1357/jegrep" alt="GitHub release" /></a>
  <a href="https://opensource.org/licenses/MIT"><img src="https://img.shields.io/badge/License-MIT-blue.svg" alt="License" /></a>
</div>

Natural-language search that works like `grep`. No embeddings, no index, no daemon.

- **Semantic:** Finds concepts ("where do we verify JWT tokens?"), not just strings.
- **No Index:** Searches the live tree on every run. Nothing to build, refresh, or go stale.
- **Calibrated:** Every path gets an absolute yes/no probability, so thresholds mean something and batches stay comparable.
- **Precise:** Returns files *and* line ranges as `dirname/filename:first-last`
  references, ready to paste into an editor, with original line numbers and
  merged adjacent passages.
- **Cheap:** Jev bills $0.042 per million input tokens, output free. A typical search over a few thousand files runs **$0.01–0.03**.
- **Agent-Ready:** `--json` for scripts and coding agents, `--compact` for a token-lean digest, plus a benchmark harness for regressions.

## Quick Start

1. **Install**

   With [Rust and Cargo](https://rustup.rs) installed:

   ```bash
   cargo install jegrep
   ```

   Or download the archive for your platform from
   [GitHub Releases](https://github.com/can1357/jegrep/releases/latest), extract it,
   and put `jegrep` (`jegrep.exe` on Windows) on your `PATH`. Releases include
   Linux and macOS binaries for x86-64 and ARM64, Windows x86-64 binaries, and
   `SHA256SUMS` checksums. Linux binaries require glibc 2.39 or newer.

   Or build from source with [rustup](https://rustup.rs):

   ```bash
   git clone https://github.com/can1357/jegrep
   cd jegrep
   cargo build --release
   ```

2. **Setup (Recommended)**

   jegrep judges code with [Jev](https://docs.typesafe.ai) via OpenRouter or TypeSafe directly.
   Set at least one key, in the process environment or `~/.env`:

   ```bash
   export OPENROUTER_API_KEY=...
   # and/or
   export TYPESAFE_API_KEY=...
   ```

   By default OpenRouter is preferred when its key exists, otherwise TypeSafe is used.
   Order them with `--endpoint openrouter` / `--endpoint typesafe`. With both keys set,
   auth/credit failures (401/402/403), timeouts (408), rate limits (429), server
   errors (5xx), transport failures, and invalid responses automatically fail over
   to the other provider. Other request errors (e.g. 400/422) are returned as-is.

   `--endpoint` is a preference, not a guarantee: the other account can still be
   billed after a failover. To refuse that, use `--only typesafe` /
   `--only openrouter`: the named key must resolve and a failing request is an
   error. Every run names its providers in the banner (`jev-latest via openrouter
   → typesafe`), and the footer and `--json` `stats.provider` report which one
   actually served it.

3. **Search**

   ```bash
   cd my-repo
   jegrep "where do we handle authentication?"
   ```

## Coding Agent Integration

`--json` emits the full result (scores, ranges, costs) best-first for callers:

```bash
jegrep "how is the database connection pooled?" --json | jq .
```

`--compact` emits a token-lean digest instead: one row per hit, no color,
indentation, grouping, or snippets.

```bash
jegrep "how are request retries counted and reported?" --compact
```

```
"how are request retries counted and reported?" → 3 hit(s) · τ 0.20 · round 1
benches/run.py 0.72 294-385
src/report.rs 0.73 1-209
src/jev.rs 0.96 1-458
# root /home/user/computing/terminal/jegrep · judged 90 · read 20 files · $0.0015 · 1.9s
```

Each row is tab-separated `path`, `score`, `spans` (so paths with spaces
split cleanly): `spans` holds up to three `start-end` line ranges, strongest
first, or `?` when the hit carries no localized range. Rows are ordered like
the terminal report — weakest first, strongest last — and the `#` trailer
carries the root the paths are relative to plus what the search touched.
`--compact` conflicts with `--json` and `--tree`.

## Output

```bash
jegrep "how are request retries counted and reported?"
```

```
 3 hit(s) for "how are request retries counted and reported?"  · round 1 · τ = 0.20

   src/report.rs  0.70 · whole file · 209 lines

   benches/run.py  0.72 · 92 lines shown
     benches/run.py:294-385  0.72  …                      failures=errors, query_success=mean("query_success"),

   src/jev.rs  0.97 · 458 lines shown
     src/jev.rs:1-458  0.97  //! Minimal Jev (`TypeSafe` System One) HTTP client over ureq.

listed 102 · judged 90 · expanded 11 dirs · read 20 files (69.4 KB) · 9 requests · 36.2k tokens · $0.0015 · 2.1s wall / 4.8s api
```

Every hit row opens with the root-relative `dirname/filename`, and every
localized passage under it opens with `dirname/filename:first-last`, so one
selection pastes straight into an editor. A hit shows its top three ranges by
relevance, strongest last, and hits are ordered weakest first, so the best
result sits next to your prompt. Directory runs are separated by a
blank line rather than a directory header, which would only repeat the path.

## Commands

### `jegrep [query] [path]`

The default command. Searches `path` (default `.`) for what `query` describes.

```bash
jegrep "how is the database connection pooled?"
```

**Options:**
| Flag | Description | Default |
| --- | --- | --- |
| `-s`, `--strategy <name>` | Exploration strategy (see `--list-strategies`) | `cascade` |
| `-p`, `--parallel <n>` | Requests in flight | `16` |
| `-n`, `--batch <n>` | Soft frontier target per batch | `64` |
| `--max-batch <n>` | Hard cap of entries per request (≤ 255) | `128` |
| `-t`, `--thresholds <list>` | Relevance thresholds, one per round | `0.4,0.2` |
| `--bytes <n>` | Bytes of each file sent for the content check (whole-file strategies¹) | `32768` |
| `--ranges <n>` | Heatmap line ranges per file (whole-file strategies¹) | `16` |
| `--min-hits <n>` | Stop lowering thresholds once this many hits exist (whole-file strategies¹) | `1` |
| `-k`, `--keywords <list>` | Extra keywords for the lexical scan (`cascade`, `window`, `paged-grep*`) | derived |
| `--endpoint <provider>` | Preferred provider: `openrouter` \| `typesafe` \| `local`; fails over between hosted keys | auto |
| `--only <provider>` | Exactly this provider: error if its key is absent or a request fails, never fail over | unset |
| `--model <id>` | Jev model id or alias | `jev-latest` |
| `--hidden` | Include dot-files and dot-folders | `false` |
| `--allow-secrets` | Send files whose content looks like credential material instead of withholding them | `false` |
| `--tree` | Print the annotated exploration tree | `false` |
| `--json` | JSON output format | `false` |
| `--compact` | Token-lean digest for LLM readers (see Output) | `false` |
| `--progress <mode>` | `live` \| `log` (terminal-aware fallback) | `live` |
| `-v`, `--verbose` | Log every judgment | `false` |
| `-q`, `--quiet` | Suppress progress on stderr | `false` |

¹ `cascade` (the default) and `window` budget by passage, not by whole-file
read, and run a single pass: they ignore `--bytes`, `--ranges` and
`--min-hits` (a warning says so) and use only the last `-t` threshold. Their
per-passage budgets are the `JEGREP_CASCADE_*` / `JEGREP_WINDOW_*` variables
below.

**Examples:**

```bash
# General concept search
jegrep "API rate limiting logic" .

# Previous strategy
jegrep "error handling" -s window .

# JSON for scripting
jegrep "config parsing" --json .

# Original scrolling log instead of the live view
jegrep --progress log "where are model aliases resolved?" .
```

### Strategies

| Name | Idea |
| --- | --- |
| `cascade` (default) | Sketch-routed global budget of verified full-source passages |
| `baseline` | Eager fill to N, one Noul per entry, fixed τ rounds, 32 KB content check |
| `beam` | Per-folder Choice + gate Noul; beam of top-K paths; flat Noul rescue pass |
| `sniff` | 1 KB heads of 32 files per request; survivors get the 32 KB check |
| `budget` | Rank-budgeted reads with a gap-rule cut instead of a fixed τ |
| `deep` | Recursive hot-folder listing; 8 KB-first reads upgraded to 32 KB if warm |
| `paged`, `paged-grep`, `paged-grep-fast`, `paged-grep-labels` | Per-folder cursors ordered by keyword hits (grep variants) |
| `inline`, `inline-16k`, `inline-shared`, `inline-solo` | Hot-file content inlined into the next request under a token budget |
| `window` | Ranked lexical candidates scored as bounded passages through selected files |
| `hybrid-window` | Beam discovery followed by window refinement |

Exploration policy is pluggable (`src/strategies/`). Each strategy drives the same
primitives — the lazily-listed `Tree`, the worker `Pool`, the question builders in
`questions.rs`, and the `jev::Client`. To add one: implement `Strategy` in
`src/strategies/<name>.rs` and register it in `strategies/mod.rs`.

## Configuration

There is no config file. Everything is CLI flags plus environment variables.

| Variable | Description | Default |
| --- | --- | --- |
| `OPENROUTER_API_KEY` / `TYPESAFE_API_KEY` | Provider keys (env or `~/.env`) | unset |
| `JEGREP_ENDPOINT_URL` | URL used by `--endpoint local` (env or `~/.env`) | `http://127.0.0.1:8756/` |
| `JEGREP_CASCADE_CANDIDATES` / `FILES` / `WINDOWS` / `BYTES` | Cascade candidate/file/passage/byte budgets | `128` / `20` / `24` / `8192` |
| `JEGREP_CASCADE_SKETCH_BYTES` / `FULL_LIMIT` / `CUTOFF` | Sketch size, full-passage cap, sketch cutoff | `384` / `40` / `0.45` |
| `JEGREP_WINDOW_CANDIDATES` / `FILES` / `PER_FILE` / `BYTES` / `PACK` | Window strategy budgets | — |
| `JEGREP_WINDOW_SCOUT_THRESHOLD` / `JEGREP_WINDOW_ADAPTIVE` / `JEGREP_WINDOW_COMPACT` | Window prefilter tuning | `0.5` / on / on |
| `JEGREP_QUESTION_CHUNK` | Split Noul batches (opt-in request splitting) | off |

### Local endpoints

`--endpoint local` posts to any HTTP server that speaks the hosted providers'
request shape, at `JEGREP_ENDPOINT_URL` (default `http://127.0.0.1:8756/`; an
empty value counts as unset). No API key is read, no `Authorization` header is
sent, and no hosted failover is added. Failed requests are logged and counted
in the footer like any other request error; the run still completes with
whatever was judged.

```jsonc
// POST <JEGREP_ENDPOINT_URL>
{ "state": <any JSON>, "model": "jev-latest",
  "questions": { "q0": { "type": "noul", "instructions": "…" } } }
// -> 200
{ "model": "my-local-judge", "answers": { "q0": { "type": "noul", "noul": 0.87 } },
  "usage": { "input_tokens": 0, "output_tokens": 0 } }
```

`questions` carry `noul` (yes/no probability) or `choice` (distribution over
named options, with `criteria`) entries; answers use the same keys and the
`noul` / `choice` shapes. `usage` may be all zeros for a judge that does not
bill per token (jegrep prints `$0.0000`). Fit the server's context window
yourself with the `JEGREP_CASCADE_*` budgets (`-n`/`--max-batch` bound only the
filename batches; see the option table).

```bash
JEGREP_ENDPOINT_URL=http://127.0.0.1:8010/v1/systemone jegrep "…" --endpoint local
```

### Ignoring Files

jegrep respects `.gitignore` (nested files honored, also in directory peeks)
and skips lockfiles, build outputs (`node_modules`, `target`, `dist`, …), and
binary extensions. Dot-files are excluded unless `--hidden` is passed.

Credential files are never listed or read, with or without `--hidden`: `.env`
and `.env.*` (except `.env.example`-style templates), `.netrc`, `.npmrc`,
`.pypirc`, `.git-credentials`, `credentials.json`, `id_rsa`-style SSH keys, and
anything ending in `.pem`, `.key`, `.p12`, `.pfx`, `.jks`, `.ppk`, `.kdbx`,
`.gpg`, `.crt`, `.tfvars` or `.tfstate` (full list: `SECRET_FILES` /
`SECRET_EXT` in `src/tree.rs`).

Names are only a first line: a Google service-account key downloaded as
`my-project-4f3a1c.json` or an AWS profile in `deploy/aws_credentials.txt` has
no listed name. So every file is also checked by *content* before its bytes
leave the machine (`src/secrets.rs`): PEM private-key blocks, service-account
JSON, `aws_secret_access_key` / `AKIA…` pairs, kubeconfig `client-key-data`,
and `ghp_` / `xoxb-` / `sk-` style tokens. Matching files are skipped, named
in the log (`withheld: private key`), and tallied in the footer and in `--json`
`stats.secrets_withheld`. The markers are shaped so parsers and docs that
merely mention a format pass; fixtures with real key material are withheld,
and `--allow-secrets` sends them anyway. This is still a safety net, not a
secret scanner — an unfamiliar token pasted into `config.yaml` is ordinary
text.

## Troubleshooting

- **Nothing found?** Thresholds lower automatically across rounds and cached
  judgments reopen — but you can also pass an explicit `-t 0.3,0.1`.
- **Weird results?** Re-run with `--verbose` and `--tree` to see every judgment.
- **Auth errors?** Check the right key is set (`OPENROUTER_API_KEY` /
  `TYPESAFE_API_KEY`) or pin `--only` to the provider you meant.
- **Billed on the wrong account?** `--endpoint` only orders providers; the
  footer's `via …` shows who served the run. Use `--only` to forbid failover.
- **Slow or pricey?** Lower `-n`/`--max-batch`, raise `-t`, or try `-s beam`/`budget`.

## Building from Source

```bash
git clone https://github.com/can1357/jegrep
cd jegrep
cargo build --release

# Run tests
cargo test
```

The native file walker is vendored in `src/walker/` from `pi-walker` 18.2.6
([upstream revision](https://github.com/can1357/oh-my-pi/tree/836048d81e088b4cddcd023780d6d769920e8525/crates/pi-walker)).
It is compiled into jegrep; publishing does not require a separate `pi-walker`
release. All Cargo dependencies resolve from crates.io.

To verify the publishable package without uploading it:

```bash
cargo publish --dry-run
```

## CI and Releases

GitHub Actions checks formatting, the configured Clippy lints, and the Python
benchmark-runner tests. It runs Rust tests and builds and smoke-tests release
binaries on all five supported platforms for pull requests and pushes to `main`.

To publish a release, update the version in `Cargo.toml` and `Cargo.lock`, commit
the change, then push a matching tag:

```bash
git tag -a v0.1.1 -m "jegrep 0.1.1"
git push origin main v0.1.1
```

The same checks gate tag builds. Tags must match the package version. Only after
every job passes does CI publish a GitHub Release with archives, licenses, and
checksums; versions with a prerelease suffix are marked as prereleases.
Publishing uses the repository's automatic `GITHUB_TOKEN`; no release secret is
required. crates.io publishing is separate and is not enabled by this workflow.

## License

Licensed under the MIT License.
See [LICENSE](LICENSE) for details. The vendored walker retains its
[upstream MIT license](src/walker/LICENSE).
