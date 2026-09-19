//! Baseline: per-entry Noul over a filled frontier, fixed threshold rounds.
//!
//! - Fill: eagerly list unjudged folders (shallowest first) until the frontier
//!   holds ≥ N entries, then chunk into ≤ cap batches, one Noul per entry.
//! - Hot folder → expand (children join the frontier). Cold → collapse.
//! - Hot file → read first K bytes, relevance Noul + heatmap Choice.
//! - No hits → lower τ, reopen collapsed nodes from cached scores (no repeat asks).

use super::Strategy;
use crate::ctx::Ctx;
use crate::pool::{Job, Outcome};
use crate::questions::{self, FileErr, heat_from};
use crate::tree::{Kind, State};
use std::collections::{HashMap, VecDeque};

#[derive(Default)]
pub struct Baseline {
    frontier: Vec<usize>,
    ready_dirs: VecDeque<Job>,
    ready_files: VecDeque<Job>,
    batches: HashMap<u64, Vec<usize>>,
    in_flight: usize,
    next_id: u64,
    round: u8,
    tau: f64,
    hits: usize,
    /// Failed-request attempts per node; a batch or file job is re-queued once.
    attempts: HashMap<usize, u8>,
}

impl Strategy for Baseline {
    fn run(&mut self, ctx: &mut Ctx) {
        self.frontier = ctx.tree.nodes[0].children.clone();
        let thresholds = ctx.opts.thresholds.clone();
        for (r, &tau) in thresholds.iter().enumerate() {
            self.round = r as u8 + 1;
            self.tau = tau;
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

impl Baseline {
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

    /// Frontier too small for a worthwhile batch? Eagerly list the shallowest
    /// unjudged folders (no request needed) until it reaches the soft target.
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

    /// Chunk the frontier into batches when it is full, or whenever there are idle
    /// workers (a small batch beats an idle worker). Then feed the pool.
    fn dispatch(&mut self, ctx: &mut Ctx) {
        let idle =
            self.in_flight + self.ready_dirs.len() + self.ready_files.len() < ctx.opts.parallel;
        if !self.frontier.is_empty() && (self.frontier.len() >= ctx.opts.batch || idle) {
            let nodes = &ctx.tree.nodes;
            self.frontier
                .sort_by(|&a, &b| nodes[a].rel.cmp(&nodes[b].rel));
            let all = std::mem::take(&mut self.frontier);
            // Request latency grows superlinearly with entries (29 → 0.5 s, 162 → 0.7 s,
            // 255 → 1.7 s measured), so split evenly into ~N-sized requests rather than
            // packing to the cap; they run in parallel anyway.
            let parts = all.len().div_ceil(ctx.opts.batch).max(1);
            let size = all.len().div_ceil(parts).clamp(1, ctx.opts.max_batch);
            for chunk in all.chunks(size) {
                let entries = chunk.to_vec();
                for &i in &entries {
                    ctx.tree.nodes[i].state = State::Pending;
                }
                let (state, questions) = questions::dir_batch(&ctx.tree, &ctx.opts.query, &entries);
                self.next_id += 1;
                self.batches.insert(self.next_id, entries);
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
            if let Job::Ask { id, .. } = &job {
                ctx.ui
                    .batch(*id, self.batches[id].len(), self.in_flight + 1);
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
                let entries = self.batches.remove(&id).unwrap_or_default();
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
                        let mut requeued = 0;
                        for idx in entries {
                            let a = self.attempts.entry(idx).or_insert(0);
                            *a += 1;
                            if *a <= 1 {
                                ctx.tree.nodes[idx].state = State::Unk;
                                self.frontier.push(idx);
                                requeued += 1;
                            } else {
                                ctx.mark_skip(idx, format!("request failed: {e}"));
                            }
                        }
                        ctx.ui.error(&format!(
                            "directory batch failed ({e}); re-queued {requeued} entries"
                        ));
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
                            let a = self.attempts.entry(node).or_insert(0);
                            *a += 1;
                            if *a <= 1 {
                                ctx.ui.error(&format!(
                                    "{}: {e}; re-queued",
                                    ctx.tree.nodes[node].rel
                                ));
                                self.queue_file(ctx, node);
                                return;
                            }
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

    fn judge_entry(&mut self, ctx: &mut Ctx, idx: usize, p: f64) {
        ctx.stats.judged += 1;
        ctx.tree.nodes[idx].name_score = Some(p);
        let hot = p >= self.tau;
        match ctx.tree.nodes[idx].kind {
            Kind::Dir => {
                if hot {
                    self.expand_hot(ctx, idx, "");
                } else {
                    ctx.mark_fin(idx, self.round);
                    let n = &ctx.tree.nodes[idx];
                    ctx.ui.fin_dir(&n.rel, p, n.peek_count, self.round);
                }
            }
            Kind::File => {
                if hot {
                    self.queue_file(ctx, idx);
                } else {
                    ctx.mark_fin(idx, self.round);
                    ctx.ui.fin_file(&ctx.tree.nodes[idx].rel, p, self.round);
                }
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

    fn queue_file(&mut self, ctx: &mut Ctx, idx: usize) {
        let n = &mut ctx.tree.nodes[idx];
        n.state = State::Reading;
        ctx.ui.read(&n.rel, n.name_score.unwrap_or(0.0));
        self.ready_files.push_back(Job::File {
            id: idx as u64,
            path: n.path.clone(),
            rel: n.rel.clone(),
            size: n.size,
        });
    }

    /// New round, lower bar: revisit collapsed nodes using cached judgments.
    fn reopen(&mut self, ctx: &mut Ctx) {
        let tau = self.tau;
        let cands: Vec<usize> = (0..ctx.tree.nodes.len())
            .filter(|&i| matches!(ctx.tree.nodes[i].state, State::Fin(_)))
            .collect();
        for i in cands {
            let n = &ctx.tree.nodes[i];
            let name_p = n.name_score.unwrap_or(0.0);
            match n.kind {
                Kind::Dir => {
                    if name_p >= tau {
                        self.expand_hot(ctx, i, "(reopened)");
                    }
                }
                Kind::File => match n.content_score {
                    Some(c) => {
                        if c >= tau {
                            ctx.mark_hit(i, true);
                            self.hits += 1;
                        }
                    }
                    None => {
                        if name_p >= tau {
                            self.queue_file(ctx, i);
                        }
                    }
                },
            }
        }
    }
}
