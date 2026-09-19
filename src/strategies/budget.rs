//! Budget: rank-based selection with a sliding read window, an adaptive cut,
//! and early stop. No fixed 0.4/0.2 thresholds.
//!
//! - Names are judged exactly like the baseline (one Noul per frontier entry,
//!   eager fill), but every judged entry lands in a ranked BACKLOG instead of
//!   being accepted/rejected on the spot. Nothing is ever re-asked.
//! - Folders: expand the top-D of the backlog per pass, above a low floor.
//!   Listing is cheap (~50 tokens/entry); reading is expensive (~10k/file), so
//!   folders get a lower floor than files and exploration runs ahead of reading.
//! - Files: at most K reads outstanding at any time (sliding window), at most
//!   CAP reads per query. A file with a very high name score (≥ HI) is read at
//!   once; everything else waits until exploration is quiescent, then the top of
//!   the backlog is read if it clears the adaptive cut (gap / ratio rule over
//!   the whole distribution of name scores seen so far). While a clearly
//!   stronger candidate is being read (score > GATE × ours), weaker ones wait.
//! - Settling instead of a hard early stop: once a CONFIDENT hit exists (content
//!   ≥ 0.7), only folders and files whose name score is ≥ that hit's own rank
//!   score are still explored (they might hold a better answer); a weak hit
//!   counts but lets the current tier finish. Any hit caps further reads at K.
//!   When that drains, the search ends. No hits → the floors relax tier by tier
//!   and cached content scores are re-examined, like the baseline.
//! - Folder expansion is rank-relative at tier 0 (≥ `DIR_RATIO` × best folder
//!   seen) and giant folders (> GIANT × batch children) are judged by name
//!   before being listed, instead of being eagerly dumped into the frontier.
//! - Look before you leap: lukewarm folders (above `PEEK_FLOOR`, below the
//!   expansion cut) get their FULL child-name list judged with one Noul each,
//!   many folders per request (~20× cheaper than listing + judging every child).
//!   A folder that lights up is re-scored and expands normally.
//!
//! Knobs (env): `JEGREP_K=8` `JEGREP_CAP=40` `JEGREP_D=8` `JEGREP_HI=0.7` `JEGREP_GATE=0.9`
//! `JEGREP_CONFIDENT=0.7` `JEGREP_TEST_DEMOTE=0.1` `JEGREP_DIR_RATIO=0.5` `JEGREP_GIANT=2`
//! `JEGREP_PEEK_FLOOR=0.05` `JEGREP_RULE=gap|ratio|none`.

use super::Strategy;
use crate::ctx::Ctx;
use crate::pool::{Job, Outcome};
use crate::questions::{self, FileErr, heat_from};
use crate::tree::{Kind, State};
use std::collections::{HashMap, HashSet, VecDeque};

/// Per relaxation tier: (folder-expansion floor, file-read floor, content hit bar).
/// Expanding a folder costs ~50 tokens per child; reading a file ~10k tokens, so
/// folders get a much lower bar than files.
const TIERS: &[(f64, f64, f64)] = &[(0.20, 0.40, 0.50), (0.10, 0.25, 0.35), (0.05, 0.10, 0.25)];

#[derive(Clone, Copy, PartialEq, Debug)]
enum Rule {
    Gap,
    Ratio,
    None,
}

pub struct Budget {
    k: usize,
    cap: usize,
    d: usize,
    hi: f64,
    /// Hold a candidate whose name score is below `gate` × the best outstanding read.
    gate: f64,
    /// Content score at which a hit is trusted enough to start settling.
    confident: f64,
    /// Subtract this from the ranking score of test files (a prior: a search for
    /// "where X happens" usually wants the implementation, not its tests).
    test_demote: f64,
    /// Tier 0: only expand folders scoring ≥ this × the best folder seen so far.
    dir_ratio: f64,
    /// Folders with more than `giant` × batch children are not eagerly listed
    /// during fill; they get judged by name first (0 disables).
    giant: usize,
    /// Lukewarm folders at or above this (and below the expansion cut) get a
    /// full-listing judgment before being written off.
    peek_floor: f64,
    rule: Rule,

    frontier: Vec<usize>,
    /// Best folder name score seen so far.
    best_dir: f64,
    /// Folders already judged from their full listing; ask id → folders in flight.
    peeked: HashSet<usize>,
    peeks: HashMap<u64, Vec<usize>>,
    /// (node, name score) of reads currently outstanding.
    outstanding: Vec<(usize, f64)>,
    ready_asks: VecDeque<Job>,
    ready_reads: VecDeque<Job>,
    batches: HashMap<u64, Vec<usize>>,
    next_id: u64,
    asks_out: usize,
    reads_out: usize,
    /// Backlogs, sorted by score descending.
    files: Vec<(f64, usize)>,
    dirs: Vec<(f64, usize)>,
    /// Every file name score seen (for the adaptive cut).
    scores: Vec<f64>,
    reads_total: usize,
    hits: usize,
    /// Highest name score among hits so far. Once set, only candidates at least
    /// this promising by name are still explored ("settling"), then we stop.
    bar: Option<f64>,
    /// Reads spent when the first hit landed; settling may add at most K more.
    reads_at_hit: usize,
    stop: bool,
    tier: usize,
}

fn env_or<T: std::str::FromStr>(key: &str, default: T) -> T {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

impl Default for Budget {
    fn default() -> Self {
        let rule = match std::env::var("JEGREP_RULE").as_deref() {
            Ok("ratio") => Rule::Ratio,
            Ok("none") => Rule::None,
            _ => Rule::Gap,
        };
        Self {
            k: env_or("JEGREP_K", 8),
            cap: env_or("JEGREP_CAP", 40),
            d: env_or("JEGREP_D", 8),
            hi: env_or("JEGREP_HI", 0.7),
            gate: env_or("JEGREP_GATE", 0.9),
            confident: env_or("JEGREP_CONFIDENT", 0.7),
            test_demote: env_or("JEGREP_TEST_DEMOTE", 0.1),
            dir_ratio: env_or("JEGREP_DIR_RATIO", 0.5),
            giant: env_or("JEGREP_GIANT", 2),
            peek_floor: env_or("JEGREP_PEEK_FLOOR", 0.05),
            rule,
            frontier: Vec::new(),
            best_dir: 0.0,
            peeked: HashSet::new(),
            peeks: HashMap::new(),
            outstanding: Vec::new(),
            ready_asks: VecDeque::new(),
            ready_reads: VecDeque::new(),
            batches: HashMap::new(),
            next_id: 0,
            asks_out: 0,
            reads_out: 0,
            files: Vec::new(),
            dirs: Vec::new(),
            scores: Vec::new(),
            reads_total: 0,
            hits: 0,
            bar: None,
            reads_at_hit: 0,
            stop: false,
            tier: 0,
        }
    }
}

fn insert_sorted(v: &mut Vec<(f64, usize)>, p: f64, idx: usize) {
    let pos = v.partition_point(|x| x.0 > p);
    v.insert(pos, (p, idx));
}

fn is_test_path(rel: &str) -> bool {
    let lower = rel.to_ascii_lowercase();
    lower.starts_with("test/")
        || lower.starts_with("tests/")
        || lower.contains("/test/")
        || lower.contains("/tests/")
        || lower.contains("__tests__/")
        || lower.contains(".test.")
        || lower.contains(".spec.")
        || lower.contains("_test.")
}

impl Strategy for Budget {
    fn run(&mut self, ctx: &mut Ctx) {
        self.frontier = ctx.tree.nodes[0].children.clone();
        self.enter_tier(ctx, 0);
        loop {
            if !self.stop {
                self.fill(ctx);
                self.dispatch_names(ctx);
                self.select(ctx);
            }
            self.feed(ctx);
            let busy = self.asks_out + self.reads_out > 0;
            if !busy {
                if self.stop || self.hits >= ctx.opts.min_hits {
                    break;
                }
                if !self.frontier.is_empty() {
                    continue; // select() just expanded folders; go list/judge them
                }
                // Quiescent with nothing above the floor: relax, or give up.
                let more =
                    !self.files.is_empty() || !self.dirs.is_empty() || self.has_cached_content(ctx);
                if self.tier + 1 < TIERS.len() && more && self.reads_total < self.cap {
                    self.enter_tier(ctx, self.tier + 1);
                    continue;
                }
                break;
            }
            let out = ctx.pool.recv();
            self.apply(ctx, out);
        }
        self.finish(ctx);
    }
}

impl Budget {
    const fn dir_floor(&self) -> f64 {
        TIERS[self.tier].0
    }
    const fn file_floor(&self) -> f64 {
        TIERS[self.tier].1
    }
    const fn hit_bar(&self) -> f64 {
        TIERS[self.tier].2
    }

    fn enter_tier(&mut self, ctx: &mut Ctx, tier: usize) {
        self.tier = tier;
        ctx.rounds = tier as u8 + 1;
        ctx.tau = self.file_floor();
        ctx.ui.round(ctx.rounds, self.file_floor());
        if tier > 0 {
            // Cached content judgments that clear the lower bar become hits for free.
            let bar = self.hit_bar();
            let cached: Vec<usize> = (0..ctx.tree.nodes.len())
                .filter(|&i| {
                    let n = &ctx.tree.nodes[i];
                    matches!(n.state, State::Fin(_)) && n.content_score.is_some_and(|c| c >= bar)
                })
                .collect();
            for i in cached {
                ctx.mark_hit(i, true);
                let c = ctx.tree.nodes[i].content_score.unwrap_or(0.0);
                self.on_hit(ctx, i, c);
            }
        }
    }

    /// Ranking score of a file: its name judgment, demoted a little for tests.
    fn rank_score(&self, ctx: &Ctx, idx: usize) -> f64 {
        let n = &ctx.tree.nodes[idx];
        let p = n.name_score.unwrap_or(0.0);
        if is_test_path(&n.rel) {
            (p - self.test_demote).max(0.0)
        } else {
            p
        }
    }

    /// Record a hit. A confident hit (content ≥ `confident`) enters settling:
    /// the bar rises to its rank score and only stronger names are still
    /// explored. A weak hit counts and caps further reads at K, but does not
    /// stop exploration of the current tier — a lukewarm wrong file must not
    /// hide the right folder.
    fn on_hit(&mut self, ctx: &Ctx, idx: usize, content: f64) {
        if self.hits == 0 {
            self.reads_at_hit = self.reads_total;
        }
        self.hits += 1;
        if content >= self.confident {
            let r = self.rank_score(ctx, idx);
            self.bar = Some(self.bar.map_or(r, |b| b.max(r)));
        }
    }

    fn has_cached_content(&self, ctx: &Ctx) -> bool {
        let next_bar = TIERS.get(self.tier + 1).map_or(0.0, |t| t.2);
        ctx.tree.nodes.iter().any(|n| {
            matches!(n.state, State::Fin(_)) && n.content_score.is_some_and(|c| c >= next_bar)
        })
    }

    /// Same eager fill as the baseline: list the shallowest unjudged folders
    /// until the frontier reaches the soft batch size.
    fn fill(&mut self, ctx: &mut Ctx) {
        let giant_at = if self.giant == 0 {
            usize::MAX
        } else {
            self.giant * ctx.opts.batch
        };
        while self.frontier.len() < ctx.opts.batch {
            let pick = self
                .frontier
                .iter()
                .enumerate()
                .filter(|(_, i)| {
                    let n = &ctx.tree.nodes[**i];
                    n.is_dir() && n.peek_count <= giant_at
                })
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

    fn dispatch_names(&mut self, ctx: &mut Ctx) {
        let idle = self.asks_out + self.reads_out < ctx.opts.parallel;
        if self.frontier.is_empty() || !(self.frontier.len() >= ctx.opts.batch || idle) {
            return;
        }
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
            self.batches.insert(self.next_id, entries);
            self.ready_asks.push_back(Job::Ask {
                id: self.next_id,
                state,
                questions,
            });
            self.asks_out += 1;
        }
    }

    /// Adaptive cut over all file name scores seen so far (tier 0 only).
    fn cut_value(&self) -> f64 {
        if self.tier > 0 || self.scores.len() < 2 {
            return 0.0;
        }
        let mut s = self.scores.clone();
        s.sort_by(|a, b| b.partial_cmp(a).unwrap_or(std::cmp::Ordering::Equal));
        match self.rule {
            Rule::None => 0.0,
            Rule::Ratio => s[0] * 0.5,
            Rule::Gap => {
                let m = s.len().min(2 * self.k);
                let mut best = (0.0f64, 0usize);
                for i in 1..m {
                    let d = s[i - 1] - s[i];
                    if d > best.0 {
                        best = (d, i);
                    }
                }
                if best.0 >= 0.10 {
                    s[best.1] + 1e-9
                } else {
                    0.0
                }
            }
        }
    }

    /// Spend the budgets: top-D folders above the floor; top files within the
    /// read window if they are obviously hot, or if exploration has settled and
    /// they clear the cut.
    fn select(&mut self, ctx: &mut Ctx) {
        // Tier 0: rank-relative to the best folder seen. While settling, only names
        // at least as promising as the best hit qualify.
        let mut dir_floor = self.dir_floor();
        if self.tier == 0 {
            dir_floor = dir_floor.max(self.dir_ratio * self.best_dir);
        }
        if let Some(b) = self.bar {
            dir_floor = dir_floor.max(b);
        }
        let mut taken = 0;
        while taken < self.d {
            match self.dirs.first() {
                Some(&(s, idx)) if s >= dir_floor => {
                    self.dirs.remove(0);
                    self.expand_hot(ctx, idx);
                    taken += 1;
                }
                _ => break,
            }
        }
        // Look before you leap: lukewarm folders get their full listing judged in
        // one cheap batched request instead of being expanded or written off.
        if self.bar.is_none() {
            let cands: Vec<usize> = self
                .dirs
                .iter()
                .filter(|(s, i)| {
                    *s >= self.peek_floor && *s < dir_floor && !self.peeked.contains(i)
                })
                .map(|x| x.1)
                .take(24)
                .collect();
            if !cands.is_empty() {
                for &i in &cands {
                    self.peeked.insert(i);
                }
                let (state, questions) =
                    questions::dir_peek_batch(&ctx.tree, &ctx.opts.query, &cands, 120);
                self.next_id += 1;
                self.peeks.insert(self.next_id, cands);
                self.ready_asks.push_back(Job::Ask {
                    id: self.next_id,
                    state,
                    questions,
                });
                self.asks_out += 1;
            }
        }
        let exploring = self.asks_out > 0
            || !self.frontier.is_empty()
            || self.dirs.first().is_some_and(|d| d.0 >= dir_floor);
        let file_floor = self.file_floor();
        let cut = self.cut_value();
        let read_cap = match self.bar {
            Some(_) => self.cap.min(self.reads_at_hit + self.k),
            None => self.cap,
        };
        while !self.stop && self.reads_out < self.k && self.reads_total < read_cap {
            let Some(&(s, idx)) = self.files.first() else {
                break;
            };
            // A clearly stronger candidate is already being read: wait for its verdict
            // before spending reads on weaker names.
            let best_out = self.outstanding.iter().map(|o| o.1).fold(0.0, f64::max);
            if s < self.gate * best_out {
                break;
            }
            let take = match self.bar {
                Some(b) => s >= b,
                None => s >= self.hi || (!exploring && s >= file_floor && s >= cut),
            };
            if !take {
                break;
            }
            self.files.remove(0);
            self.queue_read(ctx, idx);
        }
    }

    fn feed(&mut self, ctx: &mut Ctx) {
        // `*_out` already count queued jobs; only respect the total in-flight cap.
        let mut in_flight =
            (self.asks_out - self.ready_asks.len()) + (self.reads_out - self.ready_reads.len());
        while in_flight < ctx.opts.parallel {
            let job = self
                .ready_asks
                .pop_front()
                .or_else(|| self.ready_reads.pop_front());
            let Some(job) = job else { break };
            if let Job::Ask { id, .. } = &job {
                let n = self
                    .batches
                    .get(id)
                    .map(|b| b.len())
                    .or_else(|| self.peeks.get(id).map(|p| p.len()))
                    .unwrap_or(0);
                ctx.ui.batch(*id, n, in_flight + 1);
            }
            ctx.pool.submit(job);
            in_flight += 1;
        }
    }

    fn apply(&mut self, ctx: &mut Ctx, out: Outcome) {
        match out {
            Outcome::Ask {
                id,
                result,
                elapsed,
            } => {
                self.asks_out -= 1;
                if let Some(folders) = self.peeks.remove(&id) {
                    match result {
                        Ok(resp) => {
                            ctx.stats.record(resp.usage, elapsed);
                            for (i, &idx) in folders.iter().enumerate() {
                                let p = resp.noul(&questions::folder_key(i)).unwrap_or(0.0);
                                let old = ctx.tree.nodes[idx].name_score.unwrap_or(0.0);
                                if p > old {
                                    ctx.tree.nodes[idx].name_score = Some(p);
                                    self.best_dir = self.best_dir.max(p);
                                    self.dirs.retain(|d| d.1 != idx);
                                    insert_sorted(&mut self.dirs, p, idx);
                                    if ctx.ui.verbose {
                                        ctx.ui.fill(&ctx.tree.nodes[idx].rel, 0);
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            ctx.stats.record_error(elapsed);
                            ctx.ui
                                .error(&format!("folder peek of {} failed: {e}", folders.len()));
                        }
                    }
                    return;
                }
                let entries = self.batches.remove(&id).unwrap_or_default();
                match result {
                    Ok(resp) => {
                        ctx.stats.record(resp.usage, elapsed);
                        for (i, &idx) in entries.iter().enumerate() {
                            let p = resp.noul(&questions::entry_key(i)).unwrap_or(0.0);
                            ctx.stats.judged += 1;
                            let n = &mut ctx.tree.nodes[idx];
                            n.name_score = Some(p);
                            n.state = State::Unk; // judged, parked in the backlog
                            match n.kind {
                                Kind::Dir => {
                                    self.best_dir = self.best_dir.max(p);
                                    insert_sorted(&mut self.dirs, p, idx);
                                }
                                Kind::File => {
                                    // Ranking score: demote tests a little; the raw
                                    // judgment stays on the node for display.
                                    let r = if is_test_path(&n.rel) {
                                        (p - self.test_demote).max(0.0)
                                    } else {
                                        p
                                    };
                                    self.scores.push(r);
                                    insert_sorted(&mut self.files, r, idx);
                                }
                            }
                        }
                    }
                    Err(e) => {
                        ctx.stats.record_error(elapsed);
                        ctx.ui
                            .error(&format!("directory batch of {} failed: {e}", entries.len()));
                        for idx in entries {
                            ctx.mark_skip(idx, format!("request failed: {e}"));
                        }
                    }
                }
            }
            Outcome::File {
                id,
                result,
                elapsed,
            } => {
                self.reads_out -= 1;
                let node = id as usize;
                self.outstanding.retain(|o| o.0 != node);
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
                        if c >= self.hit_bar() {
                            ctx.mark_hit(node, false);
                            self.on_hit(ctx, node, c);
                            if self.reads_total >= self.cap {
                                self.stop = true;
                            }
                        } else {
                            ctx.mark_fin(node, self.tier as u8 + 1);
                            ctx.ui
                                .miss(&ctx.tree.nodes[node].rel, c, self.tier as u8 + 1);
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

    fn expand_hot(&mut self, ctx: &mut Ctx, idx: usize) {
        let p = ctx.tree.nodes[idx].name_score.unwrap_or(0.0);
        let kids = ctx.tree.expand(idx);
        ctx.stats.expanded += 1;
        ctx.ui.exp(&ctx.tree.nodes[idx].rel, p, kids.len(), "");
        self.frontier.extend(kids);
    }

    fn queue_read(&mut self, ctx: &mut Ctx, idx: usize) {
        let n = &mut ctx.tree.nodes[idx];
        n.state = State::Reading;
        ctx.ui.read(&n.rel, n.name_score.unwrap_or(0.0));
        self.outstanding.push((idx, n.name_score.unwrap_or(0.0)));
        self.ready_reads.push_back(Job::File {
            id: idx as u64,
            path: n.path.clone(),
            rel: n.rel.clone(),
            size: n.size,
        });
        self.reads_out += 1;
        self.reads_total += 1;
    }

    /// Park whatever is left in the backlogs as collapsed, for the tree view.
    fn finish(&mut self, ctx: &mut Ctx) {
        let round = self.tier as u8 + 1;
        for &(p, idx) in &self.dirs {
            ctx.mark_fin(idx, round);
            let n = &ctx.tree.nodes[idx];
            ctx.ui.fin_dir(&n.rel, p, n.peek_count, round);
        }
        for &(p, idx) in &self.files {
            ctx.mark_fin(idx, round);
            ctx.ui.fin_file(&ctx.tree.nodes[idx].rel, p, round);
        }
        ctx.ui.round_end(round, self.file_floor(), self.hits, None);
    }
}
