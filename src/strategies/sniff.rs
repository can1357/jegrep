//! Sniff: a content-first triage funnel.
//!
//! Directories are judged by name + peek exactly as in the baseline. Files are
//! NOT sent for a full 32 KB content check on the strength of their name alone.
//! Instead candidates get a cheap *sniff*: the first ~1.5 KB of many files packed
//! into one request, one Noul per file ("does the opening — imports, header
//! comment, first definitions — indicate this file contains what the search
//! describes?"). Only files whose opening passes get the full read + heatmap.
//!
//! Modes (env `JEGREP_SNIFF_MODE`):
//!   name  — files are name-judged first at a low bar (`name_frac` × τ), then sniffed   [default]
//!   all   — every listed file is sniffed directly; files never get a name judgment
//!
//! Knobs (env): `JEGREP_SNIFF_BYTES` (1024), `JEGREP_SNIFF_LINES` (30), `JEGREP_SNIFF_PER_REQ` (32),
//!   `JEGREP_SNIFF_NAME_FRAC` (0.6), `JEGREP_SNIFF_PASS_FRAC` (0.9), `JEGREP_SNIFF_FALLBACK` (3),
//!   `JEGREP_SNIFF_HEAD` (head | skeleton — skeleton = leading comment + column-0 declarations
//!   sampled from the first 32 KB; benchmarked worse than the literal head on TS: it drops
//!   the import block, which is the strongest signal).
//! Defaults are the measured best operating point (bench run 3/4).
//!
//! Fallback: if a round ends with no hits, the top-K sniff-scored files that were
//! never fully read are read anyway (openings do not always reveal purpose).

use super::Strategy;
use crate::ctx::Ctx;
use crate::pool::{Job, Outcome};
use crate::questions::{self, FileErr, heat_from, read_text};
use crate::tree::{Kind, State, human_size};
use std::collections::{HashMap, VecDeque};

#[derive(Clone, Copy, PartialEq, Debug)]
enum Mode {
    Name,
    All,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum HeadKind {
    /// Literal opening lines.
    Head,
    /// Leading comment + column-0 declarations sampled from the first 32 KB (`JEGREP_SNIFF_HEAD=skeleton`).
    Skeleton,
}

#[derive(Debug)]
struct Knobs {
    mode: Mode,
    head: HeadKind,
    sniff_bytes: usize,
    sniff_lines: usize,
    per_req: usize,
    name_frac: f64,
    pass_frac: f64,
    fallback: usize,
}

impl Knobs {
    fn from_env() -> Self {
        fn num<T: std::str::FromStr>(k: &str, d: T) -> T {
            std::env::var(k)
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(d)
        }
        Self {
            mode: match std::env::var("JEGREP_SNIFF_MODE").as_deref() {
                Ok("all") => Mode::All,
                _ => Mode::Name,
            },
            head: match std::env::var("JEGREP_SNIFF_HEAD").as_deref() {
                Ok("skeleton") => HeadKind::Skeleton,
                _ => HeadKind::Head,
            },
            sniff_bytes: num("JEGREP_SNIFF_BYTES", 1024usize).max(128),
            sniff_lines: num("JEGREP_SNIFF_LINES", 30usize).max(3),
            per_req: num("JEGREP_SNIFF_PER_REQ", 32usize).clamp(1, 200),
            name_frac: num("JEGREP_SNIFF_NAME_FRAC", 0.6f64).clamp(0.0, 1.0),
            pass_frac: num("JEGREP_SNIFF_PASS_FRAC", 0.9f64).clamp(0.0, 1.0),
            fallback: num("JEGREP_SNIFF_FALLBACK", 3usize),
        }
    }
}

pub struct Sniff {
    k: Knobs,
    frontier: Vec<usize>,
    /// Ask jobs (directory batches and sniff batches), dispatched before file jobs.
    ready: VecDeque<Job>,
    ready_files: VecDeque<Job>,
    dir_batches: HashMap<u64, Vec<usize>>,
    sniff_batches: HashMap<u64, Vec<usize>>,
    /// node -> (head text, bytes) awaiting or used by a sniff request
    heads: HashMap<usize, (String, usize)>,
    sniff_score: HashMap<usize, f64>,
    /// nodes with heads read, not yet packed into a sniff request
    sniff_ready: Vec<usize>,
    in_flight: usize,
    next_id: u64,
    round: u8,
    tau: f64,
    hits: usize,
    fallback_used: bool,
}

impl Default for Sniff {
    fn default() -> Self {
        Self {
            k: Knobs::from_env(),
            frontier: Vec::new(),
            ready: VecDeque::new(),
            ready_files: VecDeque::new(),
            dir_batches: HashMap::new(),
            sniff_batches: HashMap::new(),
            heads: HashMap::new(),
            sniff_score: HashMap::new(),
            sniff_ready: Vec::new(),
            in_flight: 0,
            next_id: 0,
            round: 0,
            tau: 0.0,
            hits: 0,
            fallback_used: false,
        }
    }
}

impl Strategy for Sniff {
    fn run(&mut self, ctx: &mut Ctx) {
        if ctx.ui.verbose {
            ctx.ui.note(&format!("  sniff knobs: {:?}", self.k));
        }
        self.frontier = ctx.tree.nodes[0].children.clone();
        let thresholds = ctx.opts.thresholds.clone();
        for (r, &tau) in thresholds.iter().enumerate() {
            self.round = r as u8 + 1;
            self.tau = tau;
            self.fallback_used = false;
            ctx.rounds = self.round;
            ctx.tau = tau;
            ctx.ui.round(self.round, tau);
            if r > 0 {
                self.reopen(ctx);
            }
            self.run_round(ctx);
            let next = thresholds.get(r + 1).copied();
            ctx.ui.round_end(self.round, tau, self.hits, next);
            if self.hits >= ctx.opts.min_hits {
                break;
            }
        }
    }
}

impl Sniff {
    fn name_bar(&self) -> f64 {
        self.tau * self.k.name_frac
    }
    fn pass_bar(&self) -> f64 {
        self.tau * self.k.pass_frac
    }

    fn drained(&self) -> bool {
        self.in_flight == 0
            && self.ready.is_empty()
            && self.ready_files.is_empty()
            && self.frontier.is_empty()
            && self.sniff_ready.is_empty()
    }

    fn run_round(&mut self, ctx: &mut Ctx) {
        loop {
            self.fill(ctx);
            self.dispatch(ctx);
            if self.drained() {
                if self.hits == 0 && !self.fallback_used && self.k.fallback > 0 {
                    self.fallback_used = true;
                    if self.queue_fallback(ctx) {
                        continue;
                    }
                }
                break;
            }
            let out = ctx.pool.recv();
            self.in_flight -= 1;
            self.apply(ctx, out);
        }
    }

    /// Same eager fill as the baseline: list shallowest unjudged folders until the
    /// frontier reaches the soft batch target.
    fn fill(&mut self, ctx: &mut Ctx) {
        while self.frontier.len() < ctx.opts.batch {
            let pick = self
                .frontier
                .iter()
                .enumerate()
                .filter(|(_, i)| ctx.tree.nodes[**i].is_dir())
                .min_by_key(|(_, i)| (ctx.tree.nodes[**i].depth, **i))
                .map(|(pos, _)| pos);
            let Some(pos) = pick else { break };
            let idx = self.frontier.remove(pos);
            let kids = ctx.tree.expand(idx);
            ctx.stats.expanded += 1;
            ctx.ui.fill(&ctx.tree.nodes[idx].rel, kids.len());
            self.frontier.extend(kids);
        }
    }

    fn dispatch(&mut self, ctx: &mut Ctx) {
        // In `all` mode files never wait for a name judgment: sniff them now.
        if self.k.mode == Mode::All {
            let (files, dirs): (Vec<usize>, Vec<usize>) = self
                .frontier
                .drain(..)
                .partition(|&i| !ctx.tree.nodes[i].is_dir());
            self.frontier = dirs;
            for i in files {
                self.sniff_read(ctx, i);
            }
        }

        let idle = self.in_flight + self.ready.len() + self.ready_files.len() < ctx.opts.parallel;

        // Frontier → directory batches.
        if !self.frontier.is_empty() && (self.frontier.len() >= ctx.opts.batch || idle) {
            let nodes = &ctx.tree.nodes;
            self.frontier
                .sort_by(|&a, &b| nodes[a].rel.cmp(&nodes[b].rel));
            let all = std::mem::take(&mut self.frontier);
            for chunk in all.chunks(ctx.opts.max_batch) {
                let entries = chunk.to_vec();
                for &i in &entries {
                    ctx.tree.nodes[i].state = State::Pending;
                }
                let (state, questions) = questions::dir_batch(&ctx.tree, &ctx.opts.query, &entries);
                self.next_id += 1;
                self.dir_batches.insert(self.next_id, entries);
                self.ready.push_back(Job::Ask {
                    id: self.next_id,
                    state,
                    questions,
                });
            }
        }

        // Heads → sniff batches. Full batches always; partial ones when workers idle.
        if !self.sniff_ready.is_empty() && (self.sniff_ready.len() >= self.k.per_req || idle) {
            let nodes = &ctx.tree.nodes;
            self.sniff_ready
                .sort_by(|&a, &b| nodes[a].rel.cmp(&nodes[b].rel));
            let all = std::mem::take(&mut self.sniff_ready);
            for chunk in all.chunks(self.k.per_req) {
                let files: Vec<(String, String, String)> = chunk
                    .iter()
                    .map(|&i| {
                        let n = &ctx.tree.nodes[i];
                        let head = self.heads.get(&i).map(|h| h.0.clone()).unwrap_or_default();
                        (n.rel.clone(), human_size(n.size), head)
                    })
                    .collect();
                let (state, questions) = questions::sniff_batch(&ctx.opts.query, &files);
                self.next_id += 1;
                self.sniff_batches.insert(self.next_id, chunk.to_vec());
                self.ready.push_back(Job::Ask {
                    id: self.next_id,
                    state,
                    questions,
                });
            }
        }

        while self.in_flight < ctx.opts.parallel {
            let job = self
                .ready
                .pop_front()
                .or_else(|| self.ready_files.pop_front());
            let Some(job) = job else { break };
            if let Job::Ask { id, .. } = &job {
                if let Some(e) = self.dir_batches.get(id) {
                    ctx.ui.batch(*id, e.len(), self.in_flight + 1);
                } else if let Some(e) = self.sniff_batches.get(id) {
                    ctx.ui.sniff_batch(*id, e.len(), self.in_flight + 1);
                }
            }
            ctx.pool.submit(job);
            self.in_flight += 1;
        }
    }

    fn apply(&mut self, ctx: &mut Ctx, out: Outcome) {
        match out {
            Outcome::Ask {
                id,
                result,
                elapsed,
            } => {
                if let Some(entries) = self.dir_batches.remove(&id) {
                    match result {
                        Ok(resp) => {
                            ctx.stats.record(resp.usage, elapsed);
                            for (i, &idx) in entries.iter().enumerate() {
                                let p = resp.noul(&questions::entry_key(i)).unwrap_or(0.0);
                                self.judge_entry(ctx, idx, p);
                            }
                        }
                        Err(e) => {
                            ctx.stats.record_error(elapsed);
                            ctx.ui.error(&format!(
                                "directory batch of {} failed: {e}",
                                entries.len()
                            ));
                            for idx in entries {
                                ctx.mark_skip(idx, format!("request failed: {e}"));
                            }
                        }
                    }
                } else if let Some(files) = self.sniff_batches.remove(&id) {
                    match result {
                        Ok(resp) => {
                            ctx.stats.record(resp.usage, elapsed);
                            for (i, &idx) in files.iter().enumerate() {
                                let p = resp.noul(&questions::sniff_key(i)).unwrap_or(0.0);
                                self.judge_sniff(ctx, idx, p);
                            }
                        }
                        Err(e) => {
                            ctx.stats.record_error(elapsed);
                            ctx.ui
                                .error(&format!("sniff batch of {} failed: {e}", files.len()));
                            for idx in files {
                                ctx.mark_skip(idx, format!("request failed: {e}"));
                            }
                        }
                    }
                }
            }
            Outcome::File {
                id,
                result,
                elapsed,
            } => {
                let node = id as usize;
                match result {
                    Ok((prep, resp)) => {
                        ctx.stats.record(resp.usage, elapsed);
                        let c = resp.noul("relevant").unwrap_or(0.0);
                        let (heat, conf) = heat_from(&resp, "where", &prep.ranges);
                        ctx.record_content(
                            node,
                            c,
                            heat,
                            conf,
                            (prep.total_lines, prep.truncated),
                            prep.bytes_used,
                        );
                        if c >= self.tau {
                            ctx.mark_hit(node, false);
                            self.hits += 1;
                        } else {
                            ctx.mark_fin(node, self.round);
                            ctx.ui.miss(&ctx.tree.nodes[node].rel, c, self.round);
                        }
                    }
                    Err(e) => {
                        if matches!(e, FileErr::Api(_)) {
                            ctx.stats.record_error(elapsed);
                            ctx.ui.error(&format!("{}: {e}", ctx.tree.nodes[node].rel));
                        } else {
                            ctx.ui.skip(&ctx.tree.nodes[node].rel, &e.to_string());
                        }
                        ctx.mark_skip(node, e.to_string());
                    }
                }
            }
            Outcome::Read { .. } => {}
        }
    }

    /// Name/path judgment (dirs always; files only in `name` mode).
    fn judge_entry(&mut self, ctx: &mut Ctx, idx: usize, p: f64) {
        ctx.stats.judged += 1;
        ctx.tree.nodes[idx].name_score = Some(p);
        match ctx.tree.nodes[idx].kind {
            Kind::Dir => {
                if p >= self.tau {
                    self.expand_hot(ctx, idx, "");
                } else {
                    ctx.mark_fin(idx, self.round);
                    let n = &ctx.tree.nodes[idx];
                    ctx.ui.fin_dir(&n.rel, p, n.peek_count, self.round);
                }
            }
            Kind::File => {
                if p >= self.name_bar() {
                    self.sniff_read(ctx, idx);
                } else {
                    ctx.mark_fin(idx, self.round);
                    ctx.ui.fin_file(&ctx.tree.nodes[idx].rel, p, self.round);
                }
            }
        }
    }

    /// Sniff judgment: pass → full content check; fail → collapse.
    fn judge_sniff(&mut self, ctx: &mut Ctx, idx: usize, p: f64) {
        self.sniff_score.insert(idx, p);
        ctx.tree.nodes[idx].note = Some(format!("sniff {p:.2}"));
        let rel = ctx.tree.nodes[idx].rel.clone();
        if p >= self.pass_bar() {
            ctx.ui.sniff(&rel, p, true);
            self.queue_file(ctx, idx, "opening looks relevant → full read");
        } else {
            ctx.ui.sniff(&rel, p, false);
            ctx.mark_fin(idx, self.round);
        }
    }

    /// Read a file's head on the main thread (tiny local I/O) and stage it for a sniff batch.
    fn sniff_read(&mut self, ctx: &mut Ctx, idx: usize) {
        let path = ctx.tree.nodes[idx].path.clone();
        // Skeleton scans the same span a full read would (local I/O only) but sends
        // ≤ sniff_bytes of it; the bytes counted are the bytes sent.
        let scan = match self.k.head {
            HeadKind::Head => self.k.sniff_bytes,
            HeadKind::Skeleton => ctx.opts.bytes,
        };
        match read_text(&path, scan) {
            Ok(rt) => {
                let head = match self.k.head {
                    HeadKind::Head => questions::head_excerpt(&rt.text, self.k.sniff_lines),
                    HeadKind::Skeleton => questions::skeleton_excerpt(
                        &rt.text,
                        self.k.sniff_lines,
                        self.k.sniff_bytes,
                    ),
                };
                let sent = head.len();
                ctx.stats.sniffed += 1;
                ctx.stats.sniff_bytes += sent as u64;
                ctx.stats.file_bytes += sent as u64;
                self.heads.insert(idx, (head, sent));
                ctx.tree.nodes[idx].state = State::Pending;
                self.sniff_ready.push(idx);
            }
            Err(e) => {
                ctx.ui.skip(&ctx.tree.nodes[idx].rel, &e.to_string());
                ctx.mark_skip(idx, e.to_string());
            }
        }
    }

    fn expand_hot(&mut self, ctx: &mut Ctx, idx: usize, why: &str) {
        let p = ctx.tree.nodes[idx].name_score.unwrap_or(0.0);
        let kids = ctx.tree.expand(idx);
        ctx.stats.expanded += 1;
        ctx.ui.exp(&ctx.tree.nodes[idx].rel, p, kids.len(), why);
        self.frontier.extend(kids);
    }

    fn queue_file(&mut self, ctx: &mut Ctx, idx: usize, why: &str) {
        let p = self.sniff_score.get(&idx).copied();
        let n = &mut ctx.tree.nodes[idx];
        n.state = State::Reading;
        let p = p.or(n.name_score).unwrap_or(0.0);
        ctx.ui.read_why(&n.rel, p, why);
        self.ready_files.push_back(Job::File {
            id: idx as u64,
            path: n.path.clone(),
            rel: n.rel.clone(),
            size: n.size,
        });
    }

    /// No hits this round: fully read the best-sniffed files that were never read.
    fn queue_fallback(&mut self, ctx: &mut Ctx) -> bool {
        let mut cands: Vec<(usize, f64)> = self
            .sniff_score
            .iter()
            .filter(|(i, _)| {
                let n = &ctx.tree.nodes[**i];
                n.content_score.is_none() && matches!(n.state, State::Fin(_))
            })
            .map(|(i, p)| (*i, *p))
            .collect();
        if cands.is_empty() {
            return false;
        }
        cands.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        for (i, _) in cands.into_iter().take(self.k.fallback) {
            self.queue_file(ctx, i, "fallback: best sniff score, nothing found yet");
        }
        true
    }

    /// New round, lower bar: revisit collapsed nodes using cached judgments only.
    fn reopen(&mut self, ctx: &mut Ctx) {
        let tau = self.tau;
        let cands: Vec<usize> = (0..ctx.tree.nodes.len())
            .filter(|&i| matches!(ctx.tree.nodes[i].state, State::Fin(_)))
            .collect();
        for i in cands {
            let (kind, name_p, content) = {
                let n = &ctx.tree.nodes[i];
                (n.kind, n.name_score.unwrap_or(0.0), n.content_score)
            };
            match kind {
                Kind::Dir => {
                    if name_p >= tau {
                        self.expand_hot(ctx, i, "(reopened)");
                    }
                }
                Kind::File => {
                    if let Some(c) = content {
                        if c >= tau {
                            ctx.mark_hit(i, true);
                            self.hits += 1;
                        }
                    } else if let Some(&s) = self.sniff_score.get(&i) {
                        if s >= self.pass_bar() {
                            self.queue_file(ctx, i, "reopened: sniff score clears the lower bar");
                        }
                    } else if name_p >= self.name_bar() {
                        self.sniff_read(ctx, i);
                    }
                }
            }
        }
    }
}
