//! Beam: hierarchical Choice beam search over the directory tree.
//!
//! Per directory ("unit") two questions instead of one Noul per entry:
//!   - `which`: a Choice over the directory's entries (+ `none_of_these`); its
//!     distribution is the edge probability to each child.
//!   - `any`: a Noul gate — does this folder hold a match at any depth.
//! Several units ride in one request (state = the listings; Choice criteria are
//! null so both questions read the same listing). A child's score is the
//! geometric mean of the edge probabilities along its path (cookbook style)
//! times its parent's gate. Each wave expands the top-K unexpanded dirs (with a
//! cheap one-level lookahead packed into the same request) and content-checks
//! the top-M unread files. No hits → widen K/M, lower the bar, reuse every
//! cached distribution; nothing is asked twice.

use super::Strategy;
use crate::ctx::Ctx;
use crate::jev::Question;
use crate::pool::{Job, Outcome};
use crate::questions::{self, FileErr, TASK, dir_criteria, heat_from};
use crate::tree::{Kind, State};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};

/// Options per Choice, leaving room for `none_of_these` under the 255 cap.
const CHOICE_MAX: usize = 254;
/// Soft cap on listed entries per request (~20 tokens each → well under 32k state).
const REQ_MAX_OPTIONS: usize = 600;
const REQ_MAX_UNITS: usize = 24;
/// Entries we are willing to list speculatively (child dirs of an expanded dir)
/// so that a wave covers two levels when the subtree is small.
const LOOKAHEAD_OPTIONS: usize = 200;
const MAX_WAVES: usize = 8;
const GATE_FLOOR: f64 = 0.1;
const BEAM_K: usize = 4;
const READ_M: usize = 4;
const S_MIN: [f64; 3] = [0.05, 0.02, 0.0];
/// After the first hit, still read pool files at least this strong (p × gate).
const STRONG_FILE: f64 = 0.5;

struct Unit {
    dir: usize,
    chunk: Vec<usize>,
    part: usize,
    parts: usize,
}

#[derive(Default)]
pub struct Beam {
    /// Groups of units; a group (a dir + its lookahead) stays within one request.
    groups_next: Vec<Vec<Unit>>,
    ready_asks: VecDeque<Job>,
    ready_files: VecDeque<Job>,
    asks: HashMap<u64, Vec<Unit>>,
    /// Rescue batches (flat per-entry Nouls over the pool): request id → entries.
    rescues: HashMap<u64, Vec<usize>>,
    rescued: HashSet<usize>,
    closing: bool,
    /// Files read by the post-hit strong sweep (bounded by M).
    swept: usize,
    in_flight: usize,
    next_id: u64,
    /// Node → (sum of ln edge probabilities, number of decisions) along its path.
    path: HashMap<usize, (f64, u32)>,
    /// Dir → `any` gate probability.
    gate: HashMap<usize, f64>,
    /// Node → ranking score.
    score: HashMap<usize, f64>,
    dir_pool: Vec<usize>,
    file_pool: Vec<usize>,
    hits: usize,
    round: u8,
    tau: f64,
    k: usize,
    m: usize,
    s_min: f64,
    /// Separate (lower) bar for expanding folders in rescue rounds.
    s_min_dir: Option<f64>,
}

impl Strategy for Beam {
    fn run(&mut self, ctx: &mut Ctx) {
        self.path.insert(0, (0.0, 0));
        let group = self.expand_group(ctx, 0, "(root)");
        self.groups_next.push(group);

        let thresholds = ctx.opts.thresholds.clone();
        'rounds: for (r, &tau) in thresholds.iter().enumerate() {
            self.round = r as u8 + 1;
            self.tau = tau;
            self.k = BEAM_K << r;
            self.m = READ_M << r;
            self.s_min = S_MIN[r.min(S_MIN.len() - 1)];
            ctx.rounds = self.round;
            ctx.tau = tau;
            ctx.ui.round(self.round, tau);
            if r > 0 {
                self.reopen(ctx);
                // The beam stalled: judge everything listed-but-unjudged with flat
                // per-entry Nouls (absolute scores), then resume the beam at this bar.
                self.rescue(ctx);
                // Files must clear τ (a read costs ~10k tokens); listing a folder is
                // cheap, so folders may descend at half the bar.
                self.s_min = self.s_min.max(tau);
                self.s_min_dir = Some(tau / 2.0);
            }
            if self.run_waves(ctx) {
                break 'rounds;
            }
            let next = thresholds.get(r + 1).copied();
            ctx.ui.round_end(self.round, tau, self.hits, next);
            if self.hits >= ctx.opts.min_hits {
                break;
            }
        }
        if !self.closing && self.hits < ctx.opts.min_hits {
            // Name-based descent failed twice. Last resort for recall: list every
            // unread file (free, local) and judge each name once, flat, at the final bar.
            let tau = thresholds.last().copied().unwrap_or(0.2);
            self.round += 1;
            self.tau = tau;
            self.s_min = tau;
            self.s_min_dir = Some(f64::INFINITY);
            self.k = 0;
            self.m = READ_M * 2;
            ctx.rounds = self.round;
            ctx.ui.round(self.round, tau);
            self.last_resort(ctx);
            self.run_waves(ctx);
            ctx.ui.round_end(self.round, tau, self.hits, None);
        }
        // Drain in-flight work so every response is tallied.
        while self.in_flight > 0 {
            let out = ctx.pool.recv();
            self.in_flight -= 1;
            self.apply(ctx, out);
        }
    }
}

impl Beam {
    /// Run waves until hits close the search (returns true) or nothing worth
    /// selecting remains / the wave budget is spent (returns false).
    fn run_waves(&mut self, ctx: &mut Ctx) -> bool {
        let mut waves = 0;
        loop {
            if self.hits >= ctx.opts.min_hits && !self.closing {
                self.closing = true;
                self.groups_next.clear();
                self.sweep_strong(ctx);
            }
            let idle = self.groups_next.is_empty()
                && self.ready_asks.is_empty()
                && self.ready_files.is_empty()
                && self.in_flight == 0;
            if self.closing {
                if idle {
                    return true;
                }
            } else if idle {
                if waves >= MAX_WAVES || !self.select_wave(ctx) {
                    return false;
                }
                waves += 1;
            }
            self.dispatch(ctx);
            if self.in_flight == 0 {
                continue;
            }
            let out = ctx.pool.recv();
            self.in_flight -= 1;
            self.apply(ctx, out);
            if self.closing {
                // A listing that landed after the first hit may reveal a strong file.
                self.sweep_strong(ctx);
            }
        }
    }

    /// List the whole remaining tree and queue flat name judgments for every unread file.
    fn last_resort(&mut self, ctx: &mut Ctx) {
        let mut stack: Vec<usize> = (0..ctx.tree.nodes.len())
            .filter(|&i| {
                ctx.tree.nodes[i].kind == Kind::Dir && ctx.tree.nodes[i].state == State::Unk
            })
            .collect();
        while let Some(d) = stack.pop() {
            let kids = ctx.tree.expand(d);
            ctx.stats.expanded += 1;
            stack.extend(
                kids.into_iter()
                    .filter(|&k| ctx.tree.nodes[k].kind == Kind::Dir),
            );
        }
        let mut files: Vec<usize> = (0..ctx.tree.nodes.len())
            .filter(|&i| {
                ctx.tree.nodes[i].kind == Kind::File && ctx.tree.nodes[i].state == State::Unk
            })
            .collect();
        let nodes = &ctx.tree.nodes;
        files.sort_by(|&a, &b| nodes[a].rel.cmp(&nodes[b].rel));
        let seen: HashSet<usize> = self.file_pool.iter().copied().collect();
        self.file_pool
            .extend(files.iter().copied().filter(|f| !seen.contains(f)));
        let unjudged: Vec<usize> = files
            .iter()
            .copied()
            .filter(|f| !self.rescued.contains(f))
            .collect();
        for chunk in unjudged.chunks(ctx.opts.max_batch) {
            let (state, questions) = questions::dir_batch(&ctx.tree, &ctx.opts.query, chunk);
            self.next_id += 1;
            self.rescues.insert(self.next_id, chunk.to_vec());
            self.rescued.extend(chunk.iter().copied());
            self.ready_asks.push_back(Job::Ask {
                id: self.next_id,
                state,
                questions,
            });
        }
        ctx.ui.exp(
            "(every file)",
            0.0,
            unjudged.len(),
            "(last resort: flat Noul over every unread file name)",
        );
    }

    fn units_for(dir: usize, kids: &[usize]) -> Vec<Unit> {
        let parts = kids.len().div_ceil(CHOICE_MAX).max(1);
        kids.chunks(CHOICE_MAX)
            .enumerate()
            .map(|(part, chunk)| Unit {
                dir,
                chunk: chunk.to_vec(),
                part,
                parts,
            })
            .collect()
    }

    /// List `dir` (root is already listed) and, while cheap, its child dirs too,
    /// so the wave's request covers two levels. Returns one group of units.
    fn expand_group(&mut self, ctx: &mut Ctx, dir: usize, why: &str) -> Vec<Unit> {
        let kids = if dir == 0 {
            ctx.tree.nodes[0].children.clone()
        } else {
            let k = ctx.tree.expand(dir);
            ctx.stats.expanded += 1;
            k
        };
        let score = self.score.get(&dir).copied().unwrap_or(1.0);
        ctx.ui.exp(&ctx.tree.nodes[dir].rel, score, kids.len(), why);
        let mut group = Self::units_for(dir, &kids);
        let mut listed = kids.len();
        let mut budget = LOOKAHEAD_OPTIONS;
        for &c in &kids {
            let n = &ctx.tree.nodes[c];
            if n.kind != Kind::Dir
                || n.peek_count == 0
                || n.peek_count > budget
                || listed + n.peek_count > REQ_MAX_OPTIONS
            {
                continue;
            }
            let ck = ctx.tree.expand(c);
            ctx.stats.expanded += 1;
            if ck.is_empty() {
                continue;
            }
            ctx.ui.fill(&ctx.tree.nodes[c].rel, ck.len());
            budget -= ck.len();
            listed += ck.len();
            group.extend(Self::units_for(c, &ck));
        }
        group
    }

    /// Pick the next wave from the cached pools: top-K unexpanded dirs, top-M unread files.
    fn select_wave(&mut self, ctx: &mut Ctx) -> bool {
        let score = &self.score;
        let by_score = |a: &usize, b: &usize| {
            score
                .get(b)
                .copied()
                .unwrap_or(0.0)
                .partial_cmp(&score.get(a).copied().unwrap_or(0.0))
                .unwrap_or(std::cmp::Ordering::Equal)
        };
        self.dir_pool
            .retain(|&d| ctx.tree.nodes[d].state == State::Unk);
        self.file_pool
            .retain(|&f| ctx.tree.nodes[f].state == State::Unk);
        self.dir_pool.sort_by(by_score);
        self.file_pool.sort_by(by_score);
        let s_min = self.s_min;
        let s_min_dir = self.s_min_dir.unwrap_or(s_min);
        let take_dirs: Vec<usize> = self
            .dir_pool
            .iter()
            .copied()
            .take_while(|d| self.score.get(d).copied().unwrap_or(0.0) >= s_min_dir)
            .take(self.k)
            .collect();
        let take_files: Vec<usize> = self
            .file_pool
            .iter()
            .copied()
            .take_while(|f| self.score.get(f).copied().unwrap_or(0.0) >= s_min)
            .take(self.m)
            .collect();
        if take_dirs.is_empty() && take_files.is_empty() {
            return false;
        }
        self.dir_pool.retain(|d| !take_dirs.contains(d));
        self.file_pool.retain(|f| !take_files.contains(f));
        for d in take_dirs {
            let group = self.expand_group(ctx, d, &format!("(beam k={})", self.k));
            if !group.is_empty() {
                self.groups_next.push(group);
            }
        }
        for f in take_files {
            let s = self.score.get(&f).copied().unwrap_or(0.0);
            let n = &mut ctx.tree.nodes[f];
            n.state = State::Reading;
            ctx.ui.read(&n.rel, s);
            self.ready_files.push_back(Job::File {
                id: f as u64,
                path: n.path.clone(),
                rel: n.rel.clone(),
                size: n.size,
            });
        }
        true
    }

    fn flush(&mut self, ctx: &Ctx, units: Vec<Unit>) {
        if units.is_empty() {
            return;
        }
        let (state, questions) = build_request(ctx, &units, &ctx.opts.query);
        self.next_id += 1;
        self.asks.insert(self.next_id, units);
        self.ready_asks.push_back(Job::Ask {
            id: self.next_id,
            state,
            questions,
        });
    }

    /// Pack groups into requests (≤ `REQ_MAX_OPTIONS` entries, ≤ `REQ_MAX_UNITS` units), then feed the pool.
    fn dispatch(&mut self, ctx: &mut Ctx) {
        let groups = std::mem::take(&mut self.groups_next);
        let mut cur: Vec<Unit> = Vec::new();
        let mut cur_opts = 0usize;
        for group in groups {
            let g_opts: usize = group.iter().map(|u| u.chunk.len()).sum();
            if g_opts > REQ_MAX_OPTIONS {
                // A giant flat dir: its chunks are independent, one request each.
                let pending = std::mem::take(&mut cur);
                self.flush(ctx, pending);
                cur_opts = 0;
                for u in group {
                    self.flush(ctx, vec![u]);
                }
                continue;
            }
            if cur_opts + g_opts > REQ_MAX_OPTIONS || cur.len() + group.len() > REQ_MAX_UNITS {
                let pending = std::mem::take(&mut cur);
                self.flush(ctx, pending);
                cur_opts = 0;
            }
            cur_opts += g_opts;
            cur.extend(group);
        }
        self.flush(ctx, cur);

        while self.in_flight < ctx.opts.parallel {
            let job = self
                .ready_asks
                .pop_front()
                .or_else(|| self.ready_files.pop_front());
            let Some(job) = job else { break };
            if let Job::Ask { id, .. } = &job {
                let n: usize = match self.asks.get(id) {
                    Some(units) => units.iter().map(|u| u.chunk.len()).sum(),
                    None => self.rescues.get(id).map_or(0, |e| e.len()),
                };
                ctx.ui.batch(*id, n, self.in_flight + 1);
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
            } if self.rescues.contains_key(&id) => {
                let entries = self.rescues.remove(&id).unwrap_or_default();
                match result {
                    Ok(resp) => {
                        ctx.stats.record(resp.usage, elapsed);
                        for (i, &idx) in entries.iter().enumerate() {
                            let p = resp.noul(&questions::entry_key(i)).unwrap_or(0.0);
                            ctx.stats.judged += 1;
                            ctx.tree.nodes[idx].name_score = Some(p);
                            // Absolute score replaces the squashed Choice-derived one, and
                            // the path restarts here so descendants are not dragged down
                            // by the edge that squashed it.
                            self.score.insert(idx, p);
                            self.path.insert(idx, (p.max(1e-6).ln(), 1));
                            if p >= 0.1 {
                                ctx.ui
                                    .skip(&ctx.tree.nodes[idx].rel, &format!("rescued p={p:.2}"));
                            }
                        }
                    }
                    Err(e) => {
                        ctx.stats.record_error(elapsed);
                        ctx.ui
                            .error(&format!("rescue batch of {} failed: {e}", entries.len()));
                    }
                }
            }
            Outcome::Ask {
                id,
                result,
                elapsed,
            } => {
                let units = self.asks.remove(&id).unwrap_or_default();
                match result {
                    Ok(resp) => {
                        ctx.stats.record(resp.usage, elapsed);
                        for (u, unit) in units.iter().enumerate() {
                            let uid = format!("d{u:02}");
                            let any = resp.noul(&format!("{uid}_any")).unwrap_or(0.0);
                            let g = self.gate.entry(unit.dir).or_insert(0.0);
                            *g = g.max(any);
                            let gate = (*g).max(GATE_FLOOR);
                            let (sum_log, dec) =
                                self.path.get(&unit.dir).copied().unwrap_or((0.0, 0));
                            let Some((probs, _conf)) = resp.choice(&format!("{uid}_which")) else {
                                continue;
                            };
                            let none_p = probs.get("none_of_these").copied().unwrap_or(0.0);
                            let mut best: Option<(f64, usize)> = None;
                            for (j, &c) in unit.chunk.iter().enumerate() {
                                let p = probs.get(&format!("c{j:03}")).copied().unwrap_or(0.0);
                                ctx.stats.judged += 1;
                                ctx.tree.nodes[c].name_score = Some(p);
                                let child = (sum_log + p.max(1e-6).ln(), dec + 1);
                                self.path.insert(c, child);
                                // Dirs: geometric mean along the path (descend decision).
                                // Files: own share of this dir's distribution — a strong
                                // parent edge must not carry a weak leaf into a 32 KB read.
                                let s = match ctx.tree.nodes[c].kind {
                                    Kind::Dir => (child.0 / child.1 as f64).exp() * gate,
                                    Kind::File => p * gate,
                                };
                                self.score.insert(c, s);
                                if best.is_none_or(|(bp, _)| p > bp) {
                                    best = Some((p, c));
                                }
                                match ctx.tree.nodes[c].kind {
                                    Kind::Dir if ctx.tree.nodes[c].state == State::Unk => {
                                        self.dir_pool.push(c);
                                    }
                                    Kind::File if ctx.tree.nodes[c].state == State::Unk => {
                                        self.file_pool.push(c);
                                    }
                                    _ => {}
                                }
                            }
                            if let Some((p, c)) = best {
                                let n = &ctx.tree.nodes[unit.dir];
                                let part = if unit.parts > 1 {
                                    format!(" [{}/{}]", unit.part + 1, unit.parts)
                                } else {
                                    String::new()
                                };
                                ctx.ui.skip(
                                    &format!("{}{part}", n.rel),
                                    &format!(
                                        "gate={any:.2} none={none_p:.2} → top pick {} p={p:.2}",
                                        ctx.tree.nodes[c].rel
                                    ),
                                );
                            }
                        }
                    }
                    Err(e) => {
                        ctx.stats.record_error(elapsed);
                        ctx.ui
                            .error(&format!("beam request of {} dirs failed: {e}", units.len()));
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

    /// Flat per-entry Nouls over the pool (everything listed but never judged on its
    /// own). Gives absolute scores to entries a wide Choice squashed — the beam's
    /// known failure mode when the path to the answer runs through an innocuous name.
    fn rescue(&mut self, ctx: &mut Ctx) {
        let mut entries: Vec<usize> = self
            .dir_pool
            .iter()
            .chain(self.file_pool.iter())
            .copied()
            .filter(|&i| ctx.tree.nodes[i].state == State::Unk && !self.rescued.contains(&i))
            .collect();
        if entries.is_empty() {
            return;
        }
        let nodes = &ctx.tree.nodes;
        entries.sort_by(|&a, &b| nodes[a].rel.cmp(&nodes[b].rel));
        for chunk in entries.chunks(ctx.opts.max_batch) {
            let (state, questions) = questions::dir_batch(&ctx.tree, &ctx.opts.query, chunk);
            self.next_id += 1;
            self.rescues.insert(self.next_id, chunk.to_vec());
            self.rescued.extend(chunk.iter().copied());
            self.ready_asks.push_back(Job::Ask {
                id: self.next_id,
                state,
                questions,
            });
        }
        ctx.ui.exp(
            "(pool)",
            0.0,
            entries.len(),
            "(rescue: flat Noul over everything listed but unjudged)",
        );
    }

    /// First hit landed: also read the pool files that are strong on their own, so a
    /// lucky early hit does not hide the obvious answer sitting one wave away.
    fn sweep_strong(&mut self, ctx: &mut Ctx) {
        let budget = self.m.saturating_sub(self.swept);
        if budget == 0 {
            return;
        }
        let score = &self.score;
        self.file_pool
            .retain(|&f| ctx.tree.nodes[f].state == State::Unk);
        self.file_pool.sort_by(|a, b| {
            score
                .get(b)
                .copied()
                .unwrap_or(0.0)
                .partial_cmp(&score.get(a).copied().unwrap_or(0.0))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        let take: Vec<usize> = self
            .file_pool
            .iter()
            .copied()
            .take_while(|f| self.score.get(f).copied().unwrap_or(0.0) >= STRONG_FILE)
            .take(budget)
            .collect();
        self.file_pool.retain(|f| !take.contains(f));
        self.swept += take.len();
        for f in take {
            let s = self.score.get(&f).copied().unwrap_or(0.0);
            let n = &mut ctx.tree.nodes[f];
            n.state = State::Reading;
            ctx.ui.read(&n.rel, s);
            self.ready_files.push_back(Job::File {
                id: f as u64,
                path: n.path.clone(),
                rel: n.rel.clone(),
                size: n.size,
            });
        }
    }

    /// Lower bar: cached content scores that now clear τ become hits, no re-asks.
    fn reopen(&mut self, ctx: &mut Ctx) {
        let tau = self.tau;
        let cands: Vec<usize> = (0..ctx.tree.nodes.len())
            .filter(|&i| {
                matches!(ctx.tree.nodes[i].state, State::Fin(_))
                    && ctx.tree.nodes[i].content_score.is_some_and(|c| c >= tau)
            })
            .collect();
        for i in cands {
            ctx.mark_hit(i, true);
            self.hits += 1;
        }
    }
}

/// State carries every unit's listing (`dirs.dNN.entries`); the Choice criteria are
/// null so both the `which` Choice and the `any` Noul read the same listing.
fn build_request(ctx: &Ctx, units: &[Unit], query: &str) -> (Value, BTreeMap<String, Question>) {
    let mut dirs = Map::new();
    let mut questions = BTreeMap::new();
    for (u, unit) in units.iter().enumerate() {
        let uid = format!("d{u:02}");
        let node = &ctx.tree.nodes[unit.dir];
        let path = if unit.dir == 0 {
            format!("{}/ (project root)", ctx.tree.name())
        } else {
            node.rel.clone()
        };
        let note = if unit.parts > 1 {
            format!(
                "{} entries in total; this is part {} of {} of the listing",
                node.children.len(),
                unit.part + 1,
                unit.parts
            )
        } else {
            format!("{} entries", unit.chunk.len())
        };
        let mut entries = Map::new();
        let mut criteria = BTreeMap::new();
        for (j, &c) in unit.chunk.iter().enumerate() {
            let key = format!("c{j:03}");
            entries.insert(key.clone(), Value::String(ctx.tree.label(c)));
            criteria.insert(key, Value::Null);
        }
        criteria.insert(
            "none_of_these".into(),
            Value::String(
                "No listed entry plausibly contains, or leads to, a file matching the search."
                    .into(),
            ),
        );
        dirs.insert(
            uid.clone(),
            json!({ "path": path, "note": note, "entries": Value::Object(entries) }),
        );
        questions.insert(
            format!("{uid}_which"),
            Question::Choice {
                instructions: Value::String(format!(
                    "Which entry listed in `dirs.{uid}.entries` (folder \"{path}\") is the most likely place to find what this search is looking for: \"{query}\"? For a file, judge whether the file itself would contain it; for a folder (name ends with /), judge whether it contains such a file at any depth. Choose none_of_these if no listed entry plausibly does."
                )),
                criteria,
            },
        );
        questions.insert(
            format!("{uid}_any"),
            Question::Noul {
                instructions: Value::String(format!(
                    "Judging by the folder \"{path}\" and its entries listed in `dirs.{uid}.entries`, does it contain at any depth at least one file relevant to this search: \"{query}\"?"
                )),
                criteria: Some(dir_criteria()),
            },
        );
    }
    let state = json!({
        "task": TASK,
        "search": query,
        "project": ctx.tree.name(),
        "dirs": Value::Object(dirs),
    });
    (state, questions)
}
