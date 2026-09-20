//! Paged: every open folder is a CURSOR over its entries; each wave shows one
//! fair-share page per open cursor (page = ⌊cap / K⌋ entries, K = open
//! cursors), one Noul per shown entry. After a page: hot files → content check,
//! hot subfolders → new cursors (K grows, pages shrink), and the cursor scrolls
//! to its next page. A cursor that reaches its end collapses with every score
//! cached.
//!
//! Knobs:
//! - `grep`: build a keyword grep index up front (pi's grep core) and order
//!   every cursor's entries by hit count, so page 1 is the most promising
//!   slice.
//! - `strikes`: collapse a cursor early after this many consecutive cold pages
//!   (its unseen remainder is parked and paged again in the next threshold
//!   round).
//! - `labels`: also show the hit counts to the model in each entry's label.

use std::collections::{HashMap, VecDeque};

use super::Strategy;
use crate::{
	ctx::Ctx,
	grep::{self, GrepIndex, grep_index},
	pool::{Job, Outcome},
	questions::{self, FileErr, heat_from},
	tree::{Kind, State},
};

pub struct Paged {
	pub grep:        bool,
	pub labels:      bool,
	pub strikes:     Option<u32>,
	cursors:         Vec<Cursor>,
	parked:          Vec<Cursor>,
	batches:         HashMap<u64, Batch>,
	ready_files:     VecDeque<Job>,
	in_flight:       usize,
	in_flight_dirs:  usize,
	next_id:         u64,
	round:           u8,
	tau:             f64,
	hits:            usize,
	index:           Option<GrepIndex>,
	waves:           u32,
	pages_done:      Vec<u32>,
	collapsed_early: u32,
}

struct Cursor {
	dir:         usize,
	remaining:   VecDeque<usize>,
	pages:       u32,
	hot:         u32,
	cold_streak: u32,
	prio:        u32,
}

struct Batch {
	entries: Vec<usize>,
	/// (cursor dir idx, number of entries from that cursor) in order.
	slices:  Vec<(usize, usize)>,
}

impl Paged {
	pub fn new(grep: bool, labels: bool, strikes: Option<u32>) -> Self {
		Self {
			grep,
			labels,
			strikes,
			cursors: Vec::new(),
			parked: Vec::new(),
			batches: HashMap::new(),
			ready_files: VecDeque::new(),
			in_flight: 0,
			in_flight_dirs: 0,
			next_id: 0,
			round: 0,
			tau: 0.0,
			hits: 0,
			index: None,
			waves: 0,
			pages_done: Vec::new(),
			collapsed_early: 0,
		}
	}
}

impl Strategy for Paged {
	fn run(&mut self, ctx: &mut Ctx) {
		if self.grep {
			let kws = grep::keywords(&ctx.opts.query, &ctx.opts.keywords);
			match grep_index(&ctx.tree.root, &kws, ctx.tree.include_hidden) {
				Ok(ix) => {
					ctx.ui.note(&ctx.ui.dim(&format!(
						"grep prior: {} keywords [{}] · {} files scanned · {} files match · {:.0} ms",
						ix.keywords.len(),
						ix.keywords.join(", "),
						ix.files_scanned,
						ix.per_file.len(),
						ix.elapsed.as_secs_f64() * 1000.0
					)));
					self.index = Some(ix);
				},
				Err(e) => ctx.ui.error(&format!("grep index failed: {e}")),
			}
		}

		// Root: its files are one cursor; every root-level folder opens as its
		// own cursor.
		let root_kids = ctx.tree.nodes[0].children.clone();
		let files: Vec<usize> = root_kids
			.iter()
			.copied()
			.filter(|&i| !ctx.tree.nodes[i].is_dir())
			.collect();
		if !files.is_empty() {
			self.push_cursor(ctx, 0, files);
		}
		let dirs: Vec<usize> = root_kids
			.iter()
			.copied()
			.filter(|&i| ctx.tree.nodes[i].is_dir())
			.collect();
		for d in dirs {
			self.open_cursor(ctx, d, "(root)");
		}

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

		let n = self.pages_done.len().max(1) as f64;
		let avg = self.pages_done.iter().sum::<u32>() as f64 / n;
		let max = self.pages_done.iter().copied().max().unwrap_or(0);
		let summary = format!(
			"paged: {} waves · {} cursors exhausted (avg {:.1} pages, max {}) · {} collapsed early · \
			 {} retries",
			self.waves,
			self.pages_done.len(),
			avg,
			max,
			self.collapsed_early,
			crate::jev::RETRIES.load(std::sync::atomic::Ordering::Relaxed)
		);
		ctx.ui.note(&ctx.ui.dim(&summary));
		if std::env::var_os("JEGREP_STATS").is_some() {
			crate::ui::diagnostic(&format!("  {summary}"));
		}
	}
}

impl Paged {
	fn run_round(&mut self, ctx: &mut Ctx) {
		loop {
			if self.in_flight_dirs == 0 {
				self.dispatch_wave(ctx);
			}
			while self.in_flight < ctx.opts.parallel {
				let Some(job) = self.ready_files.pop_front() else {
					break;
				};
				ctx.pool.submit(job);
				self.in_flight += 1;
			}
			let cursors_left = self.cursors.iter().any(|c| !c.remaining.is_empty());
			if self.in_flight == 0 && self.ready_files.is_empty() && !cursors_left {
				break;
			}
			let out = ctx.pool.recv();
			self.in_flight -= 1;
			self.apply(ctx, out);
		}
	}

	/// One page per eligible cursor, fair share of the request cap. Cursors
	/// beyond the cap round-robin into further requests (all dispatched
	/// together).
	fn dispatch_wave(&mut self, ctx: &mut Ctx) {
		let cap = ctx.opts.max_batch.max(1);
		let mut order: Vec<usize> = (0..self.cursors.len())
			.filter(|&c| !self.cursors[c].remaining.is_empty())
			.collect();
		if order.is_empty() {
			return;
		}
		order.sort_by(|&a, &b| {
			let ca = &self.cursors[a];
			let cb = &self.cursors[b];
			cb.hot
				.cmp(&ca.hot)
				.then(cb.prio.cmp(&ca.prio))
				.then(ca.pages.cmp(&cb.pages))
				.then(ca.dir.cmp(&cb.dir))
		});
		self.waves += 1;
		let groups: Vec<Vec<usize>> = order.chunks(cap).map(|g| g.to_vec()).collect();
		for group in groups {
			let n = (cap / group.len()).max(1);
			let mut entries = Vec::new();
			let mut slices = Vec::new();
			for &c in &group {
				let cur = &mut self.cursors[c];
				let take = n.min(cur.remaining.len());
				for _ in 0..take {
					entries.push(cur.remaining.pop_front().unwrap());
				}
				cur.pages += 1;
				slices.push((cur.dir, take));
			}
			for &i in &entries {
				ctx.tree.nodes[i].state = State::Pending;
			}
			let (mut state, questions) = questions::dir_batch(&ctx.tree, &ctx.opts.query, &entries);
			if self.labels {
				// Overlay keyword counts onto the listing without touching
				// dir_batch itself.
				if let Some(map) = state.get_mut("entries").and_then(|v| v.as_object_mut()) {
					for (i, &idx) in entries.iter().enumerate() {
						map.insert(
							questions::entry_key(i),
							serde_json::Value::String(self.label(ctx, idx)),
						);
					}
				}
			}
			self.next_id += 1;
			ctx.ui
				.batch(self.next_id, entries.len(), self.in_flight + 1);
			ctx.ui.note(&ctx.ui.dim(&format!(
                    "    wave {} · {} cursors · page size {} · {}",
                    self.waves,
                    group.len(),
                    n,
                    slices
                        .iter()
                        .map(|(d, k)| format!("{}:{k}", short(&ctx.tree.nodes[*d].rel)))
                        .collect::<Vec<_>>()
                        .join(" ")
                )));
			self.batches.insert(self.next_id, Batch { entries, slices });
			ctx.pool
				.submit(Job::Ask { id: self.next_id, state, questions });
			self.in_flight += 1;
			self.in_flight_dirs += 1;
		}
	}

	fn label(&self, ctx: &Ctx, idx: usize) -> String {
		let base = ctx.tree.label(idx);
		let Some(ix) = &self.index else { return base };
		let n = &ctx.tree.nodes[idx];
		if n.is_dir() {
			let (lines, files) = ix.dir_hits(&n.rel);
			if lines == 0 {
				format!("{base}; keyword hits: none")
			} else {
				format!("{base}; keyword hits: {lines} lines in {files} files")
			}
		} else {
			let lines = ix.file_hits(&n.rel);
			if lines == 0 {
				format!("{base}; keyword hits: none")
			} else {
				format!("{base}; keyword hits: {lines} lines ({})", ix.breakdown(&n.rel))
			}
		}
	}

	fn apply(&mut self, ctx: &mut Ctx, out: Outcome) {
		match out {
			Outcome::Ask { id, result, elapsed } => {
				self.in_flight_dirs -= 1;
				let Some(batch) = self.batches.remove(&id) else {
					return;
				};
				match result {
					Ok(resp) => {
						ctx.stats.record(resp.usage, elapsed);
						let mut pos = 0;
						for (dir, take) in batch.slices {
							let mut hot = 0u32;
							for i in pos..pos + take {
								let idx = batch.entries[i];
								let p = resp.noul(&questions::entry_key(i)).unwrap_or(0.0);
								if self.judge_entry(ctx, idx, p) {
									hot += 1;
								}
							}
							pos += take;
							self.after_page(ctx, dir, hot);
						}
					},
					Err(e) => {
						ctx.stats.record_error(elapsed);
						ctx.ui
							.error(&format!("page batch of {} failed: {e}", batch.entries.len()));
						for idx in batch.entries {
							ctx.mark_skip(idx, format!("request failed: {e}"));
						}
						for (dir, _) in batch.slices {
							self.after_page(ctx, dir, 0);
						}
					},
				}
			},
			Outcome::File { id, result, elapsed } => {
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
					},
					Err(e) => {
						if matches!(e, FileErr::Api(_)) {
							ctx.stats.record_error(elapsed);
							ctx.ui.error(&format!("{}: {e}", ctx.tree.nodes[node].rel));
						} else {
							ctx.ui.skip(&ctx.tree.nodes[node].rel, &e.to_string());
						}
						ctx.mark_skip(node, e.to_string());
					},
				}
			},
			Outcome::Read { .. } => {},
		}
	}

	/// Bookkeeping after one page of a cursor came back: scroll, collapse, or
	/// park.
	fn after_page(&mut self, ctx: &mut Ctx, dir: usize, hot: u32) {
		let Some(ci) = self.cursors.iter().position(|c| c.dir == dir) else {
			return;
		};
		let cur = &mut self.cursors[ci];
		cur.hot += hot;
		cur.cold_streak = if hot == 0 { cur.cold_streak + 1 } else { 0 };
		if cur.remaining.is_empty() {
			let cur = self.cursors.remove(ci);
			self.pages_done.push(cur.pages);
			if dir != 0 {
				let n = &mut ctx.tree.nodes[dir];
				if cur.hot == 0 {
					n.state = State::Fin(self.round);
					let (rel, count) = (n.rel.clone(), n.peek_count);
					ctx.ui
						.fin_dir(&rel, n.name_score.unwrap_or(0.0), count, self.round);
				} else {
					n.state = State::Exp;
				}
			}
			return;
		}
		if let Some(limit) = self.strikes
			&& cur.cold_streak >= limit
			&& cur.hot == 0
			&& dir != 0
		{
			let cur = self.cursors.remove(ci);
			self.collapsed_early += 1;
			let n = &mut ctx.tree.nodes[dir];
			n.state = State::Fin(self.round);
			let (rel, left) = (n.rel.clone(), cur.remaining.len());
			ctx.ui.note(&format!(
				"  {} {:<5} {:<52} {}",
				ctx.ui.dim("▹"),
				format!("park{}", self.round),
				crate::ui::fit(&rel, 52),
				ctx.ui
					.dim(&format!("{} cold pages → collapsed, {left} entries unseen", cur.pages))
			));
			self.parked.push(cur);
		}
	}

	/// Apply a name judgment. Returns whether the entry was hot.
	fn judge_entry(&mut self, ctx: &mut Ctx, idx: usize, p: f64) -> bool {
		ctx.stats.judged += 1;
		ctx.tree.nodes[idx].name_score = Some(p);
		let hot = p >= self.tau;
		match ctx.tree.nodes[idx].kind {
			Kind::Dir => {
				if hot {
					self.open_cursor(ctx, idx, "");
				} else {
					ctx.mark_fin(idx, self.round);
					let n = &ctx.tree.nodes[idx];
					ctx.ui.fin_dir(&n.rel, p, n.peek_count, self.round);
				}
			},
			Kind::File => {
				if hot {
					self.queue_file(ctx, idx);
				} else {
					ctx.mark_fin(idx, self.round);
					ctx.ui.fin_file(&ctx.tree.nodes[idx].rel, p, self.round);
				}
			},
		}
		hot
	}

	/// Expand a folder (once) and open a cursor over its unjudged children.
	fn open_cursor(&mut self, ctx: &mut Ctx, dir: usize, why: &str) {
		let kids = if ctx.tree.nodes[dir].children.is_empty() {
			let k = ctx.tree.expand(dir);
			ctx.stats.expanded += 1;
			k
		} else {
			ctx.tree.nodes[dir].children.clone()
		};
		let kids: Vec<usize> = kids
			.into_iter()
			.filter(|&k| ctx.tree.nodes[k].state == State::Unk)
			.collect();
		ctx.tree.nodes[dir].state = State::Exp;
		let p = ctx.tree.nodes[dir].name_score;
		match p {
			Some(p) => ctx.ui.exp(&ctx.tree.nodes[dir].rel, p, kids.len(), why),
			None => ctx.ui.fill(&ctx.tree.nodes[dir].rel, kids.len()),
		}
		if kids.is_empty() {
			return;
		}
		self.push_cursor(ctx, dir, kids);
	}

	fn push_cursor(&mut self, ctx: &Ctx, dir: usize, mut kids: Vec<usize>) {
		let mut prio = 0;
		if let Some(ix) = &self.index {
			let hits = |i: usize| ix.hits(&ctx.tree.nodes[i].rel);
			kids.sort_by(|&a, &b| hits(b).cmp(&hits(a)).then(a.cmp(&b)));
			prio = ix.dir_hits(&ctx.tree.nodes[dir].rel).0;
		}
		self.cursors.push(Cursor {
			dir,
			remaining: kids.into(),
			pages: 0,
			hot: 0,
			cold_streak: 0,
			prio,
		});
	}

	fn queue_file(&mut self, ctx: &mut Ctx, idx: usize) {
		let n = &mut ctx.tree.nodes[idx];
		n.state = State::Reading;
		ctx.ui.read(&n.rel, n.name_score.unwrap_or(0.0));
		self.ready_files.push_back(Job::File {
			id:   idx as u64,
			path: n.path.clone(),
			rel:  n.rel.clone(),
			size: n.size,
		});
	}

	/// New round, lower bar: parked cursors resume, collapsed folders whose
	/// cached score now clears the bar open, cached file scores resolve without
	/// re-asking.
	fn reopen(&mut self, ctx: &mut Ctx) {
		let tau = self.tau;
		for mut cur in std::mem::take(&mut self.parked) {
			cur.cold_streak = 0;
			ctx.tree.nodes[cur.dir].state = State::Exp;
			ctx.ui.exp(
				&ctx.tree.nodes[cur.dir].rel,
				ctx.tree.nodes[cur.dir].name_score.unwrap_or(0.0),
				cur.remaining.len(),
				"(resumed)",
			);
			self.cursors.push(cur);
		}
		let cands: Vec<usize> = (0..ctx.tree.nodes.len())
			.filter(|&i| matches!(ctx.tree.nodes[i].state, State::Fin(_)))
			.collect();
		for i in cands {
			let n = &ctx.tree.nodes[i];
			let name_p = n.name_score.unwrap_or(0.0);
			match n.kind {
				Kind::Dir => {
					if !n.children.is_empty() {
						// exhausted cursor: its children carry their own cached
						// scores
						ctx.tree.nodes[i].state = State::Exp;
					} else if name_p >= tau {
						self.open_cursor(ctx, i, "(reopened)");
					}
				},
				Kind::File => match n.content_score {
					Some(c) => {
						if c >= tau {
							ctx.mark_hit(i, true);
							self.hits += 1;
						}
					},
					None => {
						if name_p >= tau {
							self.queue_file(ctx, i);
						}
					},
				},
			}
		}
		self.grep_reopen(ctx);
	}
}

fn short(rel: &str) -> String {
	if rel.is_empty() {
		"./".into()
	} else {
		rel.trim_end_matches('/')
			.rsplit('/')
			.next()
			.unwrap_or(rel)
			.to_string()
			+ "/"
	}
}

const GREP_BOOST: usize = 3;

impl Paged {
	/// Grep as a complementary signal, owned by code: when lowering the bar,
	/// also open the few still-collapsed folders / unread files with the most
	/// keyword hits, regardless of their (borderline, context-dependent) name
	/// score.
	fn grep_reopen(&mut self, ctx: &mut Ctx) {
		let Some(ix) = &self.index else { return };
		let mut dirs: Vec<(u32, usize)> = (0..ctx.tree.nodes.len())
			.filter(|&i| {
				let n = &ctx.tree.nodes[i];
				n.is_dir() && matches!(n.state, State::Fin(_)) && n.children.is_empty()
			})
			.map(|i| (ix.dir_hits(&ctx.tree.nodes[i].rel).0, i))
			.filter(|(h, _)| *h > 0)
			.collect();
		dirs.sort_by(|a, b| b.0.cmp(&a.0));
		let mut files: Vec<(u32, usize)> = (0..ctx.tree.nodes.len())
			.filter(|&i| {
				let n = &ctx.tree.nodes[i];
				!n.is_dir() && matches!(n.state, State::Fin(_)) && n.content_score.is_none()
			})
			.map(|i| (ix.file_hits(&ctx.tree.nodes[i].rel), i))
			.filter(|(h, _)| *h > 0)
			.collect();
		files.sort_by(|a, b| b.0.cmp(&a.0));
		let picked_dirs: Vec<usize> = dirs.iter().take(GREP_BOOST).map(|&(_, i)| i).collect();
		let picked_files: Vec<usize> = files.iter().take(GREP_BOOST).map(|&(_, i)| i).collect();
		for i in picked_dirs {
			self.open_cursor(ctx, i, "(grep boost)");
		}
		for i in picked_files {
			ctx.ui.note(
				&ctx
					.ui
					.dim(&format!("    grep boost → reading {}", ctx.tree.nodes[i].rel)),
			);
			self.queue_file(ctx, i);
		}
	}
}
