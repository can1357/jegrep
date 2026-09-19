//! Deep: fewer sequential waves via speculative subtree listing.
//!
//! Where the baseline judges one tree level per wave (judge → expand → judge
//! children → ...), `deep` collapses the dependency chain:
//!
//! - **Guarded fill.** While the frontier is smaller than N, eagerly list unjudged
//!   folders (shallowest first) — but never one whose child count would push the
//!   frontier past 2N. A giant cold folder (`test/` with 741 entries) then costs one
//!   Noul instead of 741, and is only opened if it scores hot.
//! - **Deep expand.** A hot folder is listed *recursively* (BFS, capped at
//!   `DEEP_CAP` entries) and every file under it is judged in the same wave. Sub-
//!   folders inside the cap are opened speculatively rather than judged; folders
//!   beyond the cap stay as single judgeable entries.
//! - **Small-first reads.** A name-hot file is read at `small_first` bytes first
//!   (default 8 KB). Only a borderline result on a truncated file is upgraded to
//!   the full read. Misses (the common case) never pay for 32 KB.
//! - **Lexical speculation (opt-in).** Query tokens vs. path tokens is a free
//!   local signal. Folders whose name matches a rare query token are opened before
//!   their judgment returns (their files join the same wave); the best-matching
//!   files start a small read right away. Jev still judges every one of them on
//!   content — this only removes a round trip when the name says what the query
//!   says. Measured: saves ≤1 wave on some cases, costs extra reads on all; off.
//! - Same threshold rounds as the baseline; reopening uses cached scores. If the
//!   lowest configured bar finds nothing, one extra round runs at half of it.
//!
//! Waves (dependent round trips) are tracked per job generation and reported in
//! `ctx.stats.waves`, which is noise-free unlike wall time.
//!
//! Env knobs (for experiments without CLI changes):
//!   `JEGREP_SMALL`=<bytes>   first-read size; 0 disables small-first (default 8192)
//!   `JEGREP_OUTLINE=1`       add a depth-≤2 folder outline of the repo to batch state
//!   `JEGREP_DEEP_CAP`=<n>    entries listed under one hot folder (default 400)
//!   `JEGREP_SPEC=1`          enable lexical speculation (off by default: on the bench it
//!                         cost +5–10 requests per case for at most one saved wave)

use super::Strategy;
use crate::ctx::Ctx;
use crate::pool::{Job, Outcome};
use crate::questions::{self, FileErr, heat_from};
use crate::tree::{Kind, State};
use serde_json::Value;
use std::collections::{HashMap, HashSet, VecDeque};

const DEFAULT_DEEP_CAP: usize = 400;
const DEFAULT_SMALL: usize = 8 * 1024;
/// A small read below τ is upgraded to a full read when the file was truncated
/// and the head already looked faintly relevant. (A name-based rule was tried and
/// dropped: it fired on many 0.04–0.09 heads and never produced a hit.)
const UPGRADE_FLOOR: f64 = 0.10;
/// In a later (lower-τ) round, a small-only miss is re-read in full when its name
/// score was at least this — it looked promising but the head didn't show it.
const UPGRADE_NAME_REOPEN: f64 = 0.40;
/// Lexical speculation caps per dispatch: files read and folders opened on a
/// query-token match before their name judgment returns.
const SPEC_FILES: usize = 4;
const SPEC_DIRS: usize = 4;
/// Files at most this many times the small size are read in full right away
/// (a possible upgrade would cost a round trip for little saving).
const SMALL_DIRECT_FACTOR: usize = 2;
/// A file is a hit only when its content clears max(τ, this). Exploration may
/// loosen τ to find hidden folders; that should not lower the bar for hits.
const CONTENT_FLOOR: f64 = 0.30;
/// Speculation ignores query tokens that match more than this share of listed
/// files (or more than `SPEC_DF_MAX` files): "client", "config", "export" are not
/// clues; "sql", "stdio", "tts" are. (12 / 2% let "export"/"html" read 21 files
/// for the html case where 9 sufficed; tightened.)
const SPEC_DF_SHARE: f64 = 0.004;
const SPEC_DF_MAX: usize = 4;

struct Batch {
    entries: Vec<usize>,
    wave: u32,
}

enum Stage {
    /// Local small read in progress (no request).
    SmallRead,
    /// Small content check in flight; we built the request ourselves.
    SmallAsk {
        ranges: Vec<(usize, usize, String)>,
        total_lines: usize,
        bytes: usize,
        truncated: bool,
    },
    /// Full-size content check in flight (pool builds the request).
    Full,
}

struct FileMeta {
    node: usize,
    wave: u32,
    stage: Stage,
}

pub struct Deep {
    frontier: Vec<usize>,
    /// Highest generation among entries currently in the frontier.
    frontier_gen: u32,
    ready_dirs: VecDeque<Job>,
    ready_files: VecDeque<Job>,
    batches: HashMap<u64, Batch>,
    files: HashMap<u64, FileMeta>,
    /// Nodes whose cached content judgment came from a small read only.
    small_only: HashSet<usize>,
    in_flight: usize,
    next_id: u64,
    round: u8,
    /// Bar for reading a file by name and for accepting content (with `CONTENT_FLOOR`).
    tau: f64,
    /// Bar for opening a folder by name. Equal to `tau` except in the fallback round.
    dir_tau: f64,
    hits: usize,
    /// Generation for jobs created at round start (initial frontier, reopen).
    gen_floor: u32,
    small_first: usize,
    deep_cap: usize,
    outline: bool,
    speculate: bool,
    query_tokens: Vec<String>,
    /// Speculative reads that turned into hits, and how many were issued.
    spec_reads: usize,
    spec_hits: usize,
    spec_dirs: usize,
}

fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(default)
}

impl Default for Deep {
    fn default() -> Self {
        Self {
            frontier: Vec::new(),
            frontier_gen: 1,
            ready_dirs: VecDeque::new(),
            ready_files: VecDeque::new(),
            batches: HashMap::new(),
            files: HashMap::new(),
            small_only: HashSet::new(),
            in_flight: 0,
            next_id: 0,
            round: 0,
            tau: 1.0,
            dir_tau: 1.0,
            hits: 0,
            gen_floor: 1,
            small_first: env_usize("JEGREP_SMALL", DEFAULT_SMALL),
            deep_cap: env_usize("JEGREP_DEEP_CAP", DEFAULT_DEEP_CAP),
            outline: std::env::var("JEGREP_OUTLINE")
                .is_ok_and(|v| v == "1"),
            speculate: std::env::var("JEGREP_SPEC")
                .is_ok_and(|v| v == "1"),
            query_tokens: Vec::new(),
            spec_reads: 0,
            spec_hits: 0,
            spec_dirs: 0,
        }
    }
}

impl Strategy for Deep {
    fn run(&mut self, ctx: &mut Ctx) {
        self.frontier = ctx.tree.nodes[0].children.clone();
        self.frontier_gen = 1;
        self.query_tokens = tokens(&ctx.opts.query);
        let mut rounds: Vec<(f64, f64)> = ctx.opts.thresholds.iter().map(|&t| (t, t)).collect();
        let mut r = 0;
        while r < rounds.len() {
            let (dir_tau, tau) = rounds[r];
            self.round = r as u8 + 1;
            self.tau = tau;
            self.dir_tau = dir_tau;
            ctx.rounds = self.round;
            ctx.tau = dir_tau;
            ctx.ui.round(self.round, dir_tau);
            if r > 0 {
                self.gen_floor = ctx.stats.waves + 1;
                self.reopen(ctx);
            }
            self.run_round(ctx);
            // Nothing at the lowest configured bar: one extra round that only loosens
            // the folder-open bar (hidden folders are the usual failure), keeping the
            // file-read bar where it was.
            if r + 1 == rounds.len() && self.hits == 0 && dir_tau > 0.05 && r < 3 {
                rounds.push((dir_tau / 2.0, tau));
            }
            let next = rounds.get(r + 1).map(|x| x.0);
            ctx.ui.round_end(self.round, dir_tau, self.hits, next);
            if self.hits >= ctx.opts.min_hits {
                break;
            }
            r += 1;
        }
        if ctx.ui.verbose || self.spec_reads > 0 {
            ctx.ui.note_entry(
                "spec",
                "lexical speculation",
                None,
                &format!(
                    "{} folders opened · {} files read early · {} of those became hits",
                    self.spec_dirs, self.spec_reads, self.spec_hits
                ),
            );
        }
    }
}

// ── lexical speculation helpers ──────────────────────────────────────────────

const STOP: &[&str] = &[
    "the",
    "and",
    "for",
    "with",
    "that",
    "this",
    "from",
    "into",
    "when",
    "where",
    "how",
    "what",
    "which",
    "are",
    "its",
    "over",
    "via",
    "like",
    "code",
    "file",
    "files",
    "function",
    "class",
    "module",
    "logic",
    "gets",
    "get",
    "used",
    "use",
    "using",
    "uses",
    "does",
    "not",
    "any",
    "all",
    "some",
    "one",
    "our",
    "their",
    "there",
    "than",
    "then",
    "also",
    "just",
    "very",
    "user",
    "users",
    "facing",
    "concrete",
    "actual",
    "actually",
    "src",
    "test",
    "tests",
    "index",
    "main",
    "lib",
    "utils",
    "util",
    "types",
    "type",
    "spec",
    "impl",
    "implementation",
    "source",
    "part",
    "place",
    "way",
    "thing",
    "things",
    "after",
    "before",
    "being",
    "been",
    "has",
    "have",
    "had",
    "was",
    "were",
    "will",
    "would",
    "should",
    "can",
    "could",
    "may",
    "might",
    "must",
    "run",
    "runs",
    "running",
    "ran",
    "make",
    "makes",
    "made",
    "new",
    "old",
    "set",
    "sets",
    "each",
];

/// Lowercase alphanumeric runs (camelCase split), ≥3 chars, minus stopwords.
fn tokens(s: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut prev_lower = false;
    let flush = |cur: &mut String, out: &mut Vec<String>| {
        if cur.len() >= 3 && !STOP.contains(&cur.as_str()) && !out.contains(cur) {
            out.push(cur.clone());
        }
        cur.clear();
    };
    for ch in s.chars() {
        if ch.is_alphanumeric() {
            if ch.is_uppercase() && prev_lower {
                flush(&mut cur, &mut out);
            }
            cur.extend(ch.to_lowercase());
            prev_lower = ch.is_lowercase();
        } else {
            flush(&mut cur, &mut out);
            prev_lower = false;
        }
    }
    flush(&mut cur, &mut out);
    out
}

/// Crude stem: the first five characters of longer tokens (resolver ~ resolved, download ~ downloader).
fn stem(t: &str) -> &str {
    if t.chars().count() >= 6 {
        &t[..t.char_indices().nth(5).map_or(t.len(), |(i, _)| i)]
    } else {
        t
    }
}

fn lexical_score(query_tokens: &[String], name: &str) -> usize {
    let nt = tokens(name);
    nt.iter()
        .filter(|n| {
            query_tokens
                .iter()
                .any(|q| q == *n || (q.len() >= 6 && n.len() >= 6 && stem(q) == stem(n)))
        })
        .count()
}

impl Deep {
    fn accept(&self, content_score: f64) -> bool {
        content_score >= self.tau.max(CONTENT_FLOOR)
    }

    fn run_round(&mut self, ctx: &mut Ctx) {
        loop {
            self.fill(ctx);
            self.dispatch(ctx);
            if self.in_flight == 0
                && self.ready_dirs.is_empty()
                && self.ready_files.is_empty()
                && self.frontier.is_empty()
            {
                break;
            }
            let out = ctx.pool.recv();
            self.in_flight -= 1;
            self.apply(ctx, out);
        }
    }

    /// Guarded fill: list shallowest unjudged folders until the frontier reaches N,
    /// skipping any folder that would push it past 2N.
    fn fill(&mut self, ctx: &mut Ctx) {
        let soft = ctx.opts.batch;
        let hard = 2 * ctx.opts.batch;
        while self.frontier.len() < soft {
            let mut cands: Vec<(usize, usize)> = self
                .frontier
                .iter()
                .enumerate()
                .filter(|(_, i)| ctx.tree.nodes[**i].is_dir())
                .map(|(pos, i)| (pos, *i))
                .collect();
            cands.sort_by_key(|&(_, i)| (ctx.tree.nodes[i].depth, i));
            let pick = cands
                .into_iter()
                .find(|&(_, i)| self.frontier.len() - 1 + ctx.tree.nodes[i].peek_count <= hard);
            let Some((pos, idx)) = pick else { break };
            self.frontier.remove(pos);
            let kids = ctx.tree.expand(idx);
            ctx.stats.expanded += 1;
            ctx.ui.fill(&ctx.tree.nodes[idx].rel, kids.len());
            self.frontier.extend(kids);
        }
    }

    fn dispatch(&mut self, ctx: &mut Ctx) {
        let idle =
            self.in_flight + self.ready_dirs.len() + self.ready_files.len() < ctx.opts.parallel;
        if !self.frontier.is_empty() && (self.frontier.len() >= ctx.opts.batch || idle) {
            let nodes = &ctx.tree.nodes;
            self.frontier
                .sort_by(|&a, &b| nodes[a].rel.cmp(&nodes[b].rel));
            let mut all = std::mem::take(&mut self.frontier);
            let wave = self.frontier_gen;
            if self.speculate && !self.query_tokens.is_empty() {
                self.speculate(ctx, &mut all, wave);
            }
            let nodes = &ctx.tree.nodes;
            all.sort_by(|&a, &b| nodes[a].rel.cmp(&nodes[b].rel));
            let outline = if self.outline {
                Some(self.outline(ctx))
            } else {
                None
            };
            // Request latency grows superlinearly with entry count (measured: 29→0.5s,
            // 162→0.7s, 255→1.6s), so aim for the soft target N per request and split
            // evenly rather than filling to the hard cap.
            let target = ctx.opts.batch.min(ctx.opts.max_batch).max(1);
            let n_chunks = all.len().div_ceil(target).max(1);
            let size = all.len().div_ceil(n_chunks).max(1);
            for chunk in all.chunks(size) {
                let entries = chunk.to_vec();
                for &i in &entries {
                    if ctx.tree.nodes[i].state == State::Unk {
                        ctx.tree.nodes[i].state = State::Pending;
                    }
                }
                let (mut state, questions) =
                    questions::dir_batch(&ctx.tree, &ctx.opts.query, &entries);
                if let (Some(o), Some(obj)) = (&outline, state.as_object_mut()) {
                    obj.insert("folder_outline".into(), o.clone());
                }
                self.next_id += 1;
                self.batches.insert(self.next_id, Batch { entries, wave });
                self.ready_dirs.push_back(Job::Ask {
                    id: self.next_id,
                    state,
                    questions,
                });
            }
        }
        while self.in_flight < ctx.opts.parallel {
            let job = self
                .ready_dirs
                .pop_front()
                .or_else(|| self.ready_files.pop_front());
            let Some(job) = job else { break };
            let wave = match &job {
                Job::Ask { id, .. } => {
                    if let Some(b) = self.batches.get(id) {
                        ctx.ui.batch(*id, b.entries.len(), self.in_flight + 1);
                        b.wave
                    } else {
                        self.files.get(id).map_or(0, |m| m.wave)
                    }
                }
                Job::File { id, .. } | Job::Read { id, .. } => {
                    self.files.get(id).map_or(0, |m| m.wave)
                }
            };
            // A local read is not a round trip; only requests count as waves.
            if !matches!(job, Job::Read { .. }) {
                ctx.stats.waves = ctx.stats.waves.max(wave);
            }
            ctx.pool.submit(job);
            self.in_flight += 1;
        }
    }

    /// Lexical speculation over a frontier about to be judged: folders whose name
    /// shares a token with the query are deep-expanded now (their files join this
    /// same wave), and the best-matching files start a small read immediately.
    /// Jev still judges every one of them; this only removes a round trip.
    fn speculate(&mut self, ctx: &mut Ctx, all: &mut Vec<usize>, wave: u32) {
        // Keep only query tokens that are rare among listed file names.
        let files_listed: Vec<usize> = (1..ctx.tree.nodes.len())
            .filter(|&i| !ctx.tree.nodes[i].is_dir())
            .collect();
        let df_cap = SPEC_DF_MAX.max((files_listed.len() as f64 * SPEC_DF_SHARE) as usize);
        let qt: Vec<String> = self
            .query_tokens
            .iter()
            .filter(|q| {
                let one = vec![(*q).clone()];
                files_listed
                    .iter()
                    .filter(|&&i| lexical_score(&one, ctx.tree.nodes[i].name()) > 0)
                    .count()
                    <= df_cap
            })
            .cloned()
            .collect();
        if qt.is_empty() {
            return;
        }
        let score = |ctx: &Ctx, i: usize| lexical_score(&qt, ctx.tree.nodes[i].name());
        // folders
        let mut dirs: Vec<(usize, usize)> = all
            .iter()
            .copied()
            .filter(|&i| ctx.tree.nodes[i].is_dir() && ctx.tree.nodes[i].state == State::Unk)
            .map(|i| (score(ctx, i), i))
            .filter(|(s, _)| *s > 0)
            .collect();
        dirs.sort_by_key(|&(s, i)| {
            (
                std::cmp::Reverse(s),
                ctx.tree.nodes[i].depth,
                ctx.tree.nodes[i].rel.len(),
            )
        });
        for (_, d) in dirs.into_iter().take(SPEC_DIRS) {
            let kids = self.deep_list(ctx, d);
            ctx.ui.note_entry(
                "spec",
                &ctx.tree.nodes[d].rel,
                None,
                &format!(
                    "name matches the query → opened early, {} entries",
                    kids.len()
                ),
            );
            self.spec_dirs += 1;
            all.extend(kids);
        }
        // files (including those just exposed)
        let mut files: Vec<(usize, usize)> = all
            .iter()
            .copied()
            .filter(|&i| !ctx.tree.nodes[i].is_dir() && ctx.tree.nodes[i].state == State::Unk)
            .map(|i| (score(ctx, i), i))
            .filter(|(s, _)| *s > 0)
            .collect();
        files.sort_by_key(|&(s, i)| {
            (
                std::cmp::Reverse(s),
                ctx.tree.nodes[i].depth,
                ctx.tree.nodes[i].rel.len(),
            )
        });
        for (_, f) in files.into_iter().take(SPEC_FILES) {
            ctx.ui.note_entry(
                "spec",
                &ctx.tree.nodes[f].rel,
                None,
                "name matches the query → reading early",
            );
            self.spec_reads += 1;
            self.queue_file(ctx, f, wave);
        }
    }

    /// Depth-≤2 folder names already known to the tree, as a compact outline.
    fn outline(&self, ctx: &Ctx) -> Value {
        let mut names: Vec<&str> = ctx
            .tree
            .nodes
            .iter()
            .skip(1)
            .filter(|n| n.is_dir() && n.depth <= 2 && n.state != State::Skip)
            .map(|n| n.rel.as_str())
            .collect();
        names.sort_unstable();
        names.truncate(80);
        Value::Array(
            names
                .into_iter()
                .map(|s| Value::String(s.to_string()))
                .collect(),
        )
    }

    fn apply(&mut self, ctx: &mut Ctx, out: Outcome) {
        match out {
            Outcome::Ask {
                id,
                result,
                elapsed,
            } => {
                if let Some(b) = self.batches.remove(&id) {
                    match result {
                        Ok(resp) => {
                            ctx.stats.record(resp.usage, elapsed);
                            if ctx.ui.verbose {
                                ctx.ui.note_entry(
                                    "done",
                                    &format!("batch #{id}"),
                                    None,
                                    &format!(
                                        "{} entries · {} tokens · {:.2}s · wave {}",
                                        b.entries.len(),
                                        resp.usage.input_tokens,
                                        elapsed.as_secs_f64(),
                                        b.wave
                                    ),
                                );
                            }
                            for (i, &idx) in b.entries.iter().enumerate() {
                                let p = resp.noul(&questions::entry_key(i)).unwrap_or(0.0);
                                self.judge_entry(ctx, idx, p, b.wave + 1);
                            }
                        }
                        Err(e) => {
                            ctx.stats.record_error(elapsed);
                            ctx.ui.error(&format!(
                                "directory batch of {} failed: {e}",
                                b.entries.len()
                            ));
                            for idx in b.entries {
                                ctx.mark_skip(idx, format!("request failed: {e}"));
                            }
                        }
                    }
                } else if let Some(m) = self.files.remove(&id) {
                    let Stage::SmallAsk {
                        ranges,
                        total_lines,
                        bytes,
                        truncated,
                    } = m.stage
                    else {
                        return;
                    };
                    match result {
                        Ok(resp) => {
                            ctx.stats.record(resp.usage, elapsed);
                            if ctx.ui.verbose {
                                ctx.ui.note_entry(
                                    "done",
                                    &ctx.tree.nodes[m.node].rel,
                                    None,
                                    &format!(
                                        "small read {bytes} B · {} tokens · {:.2}s · wave {}",
                                        resp.usage.input_tokens,
                                        elapsed.as_secs_f64(),
                                        m.wave
                                    ),
                                );
                            }
                            let c = resp.noul("relevant").unwrap_or(0.0);
                            let (heat, conf) = heat_from(&resp, "where", &ranges);
                            ctx.record_content(
                                m.node,
                                c,
                                heat,
                                conf,
                                (total_lines, truncated),
                                bytes,
                            );
                            if self.accept(c) {
                                if ctx.tree.nodes[m.node].name_score.is_none() {
                                    self.spec_hits += 1;
                                }
                                ctx.mark_hit(m.node, false);
                                self.hits += 1;
                            } else if truncated && c >= UPGRADE_FLOOR {
                                ctx.ui.note_entry(
                                    "more",
                                    &ctx.tree.nodes[m.node].rel,
                                    Some(c),
                                    "borderline on the head → reading full size",
                                );
                                self.queue_full(ctx, m.node, m.wave + 1);
                            } else {
                                if truncated {
                                    self.small_only.insert(m.node);
                                }
                                ctx.mark_fin(m.node, self.round);
                                ctx.ui.miss(&ctx.tree.nodes[m.node].rel, c, self.round);
                            }
                        }
                        Err(e) => {
                            ctx.stats.record_error(elapsed);
                            ctx.ui
                                .error(&format!("{}: {e}", ctx.tree.nodes[m.node].rel));
                            ctx.mark_skip(m.node, e.to_string());
                        }
                    }
                }
            }
            Outcome::File {
                id,
                result,
                elapsed,
            } => {
                let Some(m) = self.files.remove(&id) else {
                    return;
                };
                self.small_only.remove(&m.node);
                match result {
                    Ok((prep, resp)) => {
                        ctx.stats.record(resp.usage, elapsed);
                        if ctx.ui.verbose {
                            ctx.ui.note_entry(
                                "done",
                                &ctx.tree.nodes[m.node].rel,
                                None,
                                &format!(
                                    "full read {} B · {} tokens · {:.2}s · wave {}",
                                    prep.bytes_used,
                                    resp.usage.input_tokens,
                                    elapsed.as_secs_f64(),
                                    m.wave
                                ),
                            );
                        }
                        let c = resp.noul("relevant").unwrap_or(0.0);
                        let (heat, conf) = heat_from(&resp, "where", &prep.ranges);
                        ctx.record_content(
                            m.node,
                            c,
                            heat,
                            conf,
                            (prep.total_lines, prep.truncated),
                            prep.bytes_used,
                        );
                        if self.accept(c) {
                            if ctx.tree.nodes[m.node].name_score.is_none() {
                                self.spec_hits += 1;
                            }
                            ctx.mark_hit(m.node, false);
                            self.hits += 1;
                        } else {
                            ctx.mark_fin(m.node, self.round);
                            ctx.ui.miss(&ctx.tree.nodes[m.node].rel, c, self.round);
                        }
                    }
                    Err(e) => {
                        if matches!(e, FileErr::Api(_)) {
                            ctx.stats.record_error(elapsed);
                            ctx.ui
                                .error(&format!("{}: {e}", ctx.tree.nodes[m.node].rel));
                        } else {
                            ctx.ui.skip(&ctx.tree.nodes[m.node].rel, &e.to_string());
                        }
                        ctx.mark_skip(m.node, e.to_string());
                    }
                }
            }
            Outcome::Read { id, result, .. } => {
                let Some(m) = self.files.remove(&id) else {
                    return;
                };
                match result {
                    Ok(rt) => {
                        let n = &ctx.tree.nodes[m.node];
                        let lines: Vec<&str> = rt.text.lines().collect();
                        let tagged = questions::tag_lines(&lines);
                        let (ranges, criteria) = questions::line_ranges(&lines, ctx.opts.ranges);
                        let note = if rt.truncated {
                            format!("only the first {} of {} bytes are shown", rt.bytes, n.size)
                        } else {
                            "complete file".to_string()
                        };
                        let state = questions::file_state(&ctx.opts.query, &n.rel, &note, &tagged);
                        let mut qs = std::collections::BTreeMap::new();
                        qs.insert(
                            "relevant".to_string(),
                            questions::relevant_noul(&n.rel, &ctx.opts.query),
                        );
                        if ranges.len() >= 2 {
                            qs.insert(
                                "where".to_string(),
                                questions::where_choice(&ctx.opts.query, criteria),
                            );
                        }
                        self.next_id += 1;
                        self.files.insert(
                            self.next_id,
                            FileMeta {
                                node: m.node,
                                wave: m.wave,
                                stage: Stage::SmallAsk {
                                    ranges,
                                    total_lines: lines.len(),
                                    bytes: rt.bytes,
                                    truncated: rt.truncated,
                                },
                            },
                        );
                        // Continuation of work already started: front of the queue.
                        self.ready_files.push_front(Job::Ask {
                            id: self.next_id,
                            state,
                            questions: qs,
                        });
                    }
                    Err(e) => {
                        ctx.ui.skip(&ctx.tree.nodes[m.node].rel, &e.to_string());
                        ctx.mark_skip(m.node, e.to_string());
                    }
                }
            }
        }
    }

    fn judge_entry(&mut self, ctx: &mut Ctx, idx: usize, p: f64, wave: u32) {
        ctx.stats.judged += 1;
        ctx.tree.nodes[idx].name_score = Some(p);
        let hot = match ctx.tree.nodes[idx].kind {
            Kind::Dir => p >= self.dir_tau,
            Kind::File => p >= self.tau,
        };
        match ctx.tree.nodes[idx].state {
            // Speculatively opened / started already: the judgment is recorded, nothing else to do.
            State::Exp | State::Skip => return,
            State::Reading | State::Hit | State::Fin(_) => return,
            _ => {}
        }
        match ctx.tree.nodes[idx].kind {
            Kind::Dir => {
                if hot {
                    self.deep_expand(ctx, idx, wave, "");
                } else {
                    ctx.mark_fin(idx, self.round);
                    let n = &ctx.tree.nodes[idx];
                    ctx.ui.fin_dir(&n.rel, p, n.peek_count, self.round);
                }
            }
            Kind::File => {
                if hot {
                    self.queue_file(ctx, idx, wave);
                } else {
                    ctx.mark_fin(idx, self.round);
                    ctx.ui.fin_file(&ctx.tree.nodes[idx].rel, p, self.round);
                }
            }
        }
    }

    /// List a hot folder's whole subtree (BFS) up to `deep_cap` entries. Files and
    /// over-cap folders join the frontier; in-cap folders are opened speculatively.
    fn deep_expand(&mut self, ctx: &mut Ctx, idx: usize, wave: u32, why: &str) {
        let p = ctx.tree.nodes[idx].name_score.unwrap_or(0.0);
        let before = ctx.stats.expanded;
        let added = self.deep_list(ctx, idx);
        ctx.ui.exp(
            &ctx.tree.nodes[idx].rel,
            p,
            added.len(),
            &format!(
                "{why}(deep: {} folders listed)",
                ctx.stats.expanded - before
            ),
        );
        self.frontier.extend(added);
        self.frontier_gen = self.frontier_gen.max(wave);
    }

    /// BFS-list a folder's subtree up to `deep_cap` entries. Returns the entries to
    /// judge: all files, plus folders that did not fit under the cap.
    fn deep_list(&mut self, ctx: &mut Ctx, idx: usize) -> Vec<usize> {
        let mut queue = VecDeque::from([idx]);
        let mut listed = 0usize;
        let mut added = Vec::new();
        while let Some(d) = queue.pop_front() {
            let kids = ctx.tree.expand(d);
            ctx.stats.expanded += 1;
            listed += kids.len();
            for k in kids {
                let n = &ctx.tree.nodes[k];
                if n.is_dir() && listed + n.peek_count <= self.deep_cap {
                    queue.push_back(k);
                } else {
                    added.push(k);
                }
            }
        }
        added
    }

    fn queue_file(&mut self, ctx: &mut Ctx, idx: usize, wave: u32) {
        let n = &mut ctx.tree.nodes[idx];
        n.state = State::Reading;
        ctx.ui.read(&n.rel, n.name_score.unwrap_or(0.0));
        let n = &ctx.tree.nodes[idx];
        self.next_id += 1;
        if self.small_first > 0 && n.size as usize > SMALL_DIRECT_FACTOR * self.small_first {
            self.files.insert(
                self.next_id,
                FileMeta {
                    node: idx,
                    wave,
                    stage: Stage::SmallRead,
                },
            );
            self.ready_files.push_back(Job::Read {
                id: self.next_id,
                path: n.path.clone(),
                max_bytes: self.small_first,
            });
        } else {
            self.files.insert(
                self.next_id,
                FileMeta {
                    node: idx,
                    wave,
                    stage: Stage::Full,
                },
            );
            self.ready_files.push_back(Job::File {
                id: self.next_id,
                path: n.path.clone(),
                rel: n.rel.clone(),
                size: n.size,
            });
        }
    }

    fn queue_full(&mut self, ctx: &mut Ctx, idx: usize, wave: u32) {
        let n = &mut ctx.tree.nodes[idx];
        n.state = State::Reading;
        let n = &ctx.tree.nodes[idx];
        self.next_id += 1;
        self.files.insert(
            self.next_id,
            FileMeta {
                node: idx,
                wave,
                stage: Stage::Full,
            },
        );
        self.ready_files.push_back(Job::File {
            id: self.next_id,
            path: n.path.clone(),
            rel: n.rel.clone(),
            size: n.size,
        });
    }

    /// New round, lower bar: reopen collapsed nodes from cached judgments.
    fn reopen(&mut self, ctx: &mut Ctx) {
        let tau = self.tau;
        let dir_tau = self.dir_tau;
        let wave = self.gen_floor;
        let cands: Vec<usize> = (0..ctx.tree.nodes.len())
            .filter(|&i| matches!(ctx.tree.nodes[i].state, State::Fin(_)))
            .collect();
        for i in cands {
            let n = &ctx.tree.nodes[i];
            let name_p = n.name_score.unwrap_or(0.0);
            match n.kind {
                Kind::Dir => {
                    if name_p >= dir_tau {
                        self.deep_expand(ctx, i, wave, "(reopened) ");
                    }
                }
                Kind::File => match n.content_score {
                    Some(c) => {
                        if self.accept(c) {
                            ctx.mark_hit(i, true);
                            self.hits += 1;
                        } else if self.small_only.contains(&i) && name_p >= UPGRADE_NAME_REOPEN {
                            ctx.ui.note_entry(
                                "more",
                                &ctx.tree.nodes[i].rel,
                                Some(c),
                                "small-only miss, name was promising → reading full size",
                            );
                            self.queue_full(ctx, i, wave);
                        }
                    }
                    None => {
                        if name_p >= tau {
                            self.queue_file(ctx, i, wave);
                        }
                    }
                },
            }
        }
    }
}
