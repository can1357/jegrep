//! Inline: files are expanded *inside* the same request as the tree.
//!
//! The frontier is listed and judged by name exactly like the baseline (one
//! Noul per entry). But a file whose name is hot is not given its own request:
//! its first bytes are read at once and put on a file-expand queue. Whenever a
//! request is formed, the queue is drained greedily into `state.files`
//! (line-numbered content after the tree listing) until a content budget is
//! hit. A file that does not fit is not shown at all and rides on the next
//! request — piggybacking on the next listing batch, or a files-only request
//! when no listing is pending.
//!
//! Heat modes: one Choice per expanded file over its line ranges (`PerFile`),
//! or the user's literal suggestion of one shared Choice whose options are
//! every expanded file's ranges (`Shared`). Relevance is always a per-file
//! Noul.

use std::collections::{BTreeMap, HashMap, VecDeque};

use serde_json::{Map, Value, json};

use super::Strategy;
use crate::{
	ctx::Ctx,
	jev::{NoulCriteria, Question},
	pool::{Job, Outcome},
	questions::{self, entry_key, line_ranges, range_key, read_text, tag_lines},
	tree::{HeatRange, Kind, State},
};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HeatMode {
	PerFile,
	Shared,
}

// Token-estimate budgeting (measured on jev-1.13: a 255-entry listing ≈ 43.8k
// input tokens; line-prefixed code ≈ 1.8 bytes/token). Hard limits are 32k
// tokens for state + longest question and 64k per request; keep a margin.
const BYTES_PER_TOKEN: f64 = 1.8;
const STATE_LIMIT_TOK: f64 = 29_000.0;
const TOTAL_LIMIT_TOK: f64 = 58_000.0;
const ENTRY_STATE_TOK: f64 = 45.0;
const ENTRY_TOTAL_TOK: f64 = 172.0;
const FILE_Q_TOK: f64 = 320.0;
const BASE_TOK: f64 = 300.0;

/// Would a request with `n_entries` listing entries and `n_files` inlined files
/// totalling `content_bytes` stay under both token limits?
fn fits(n_entries: usize, content_bytes: usize, n_files: usize) -> bool {
	let content = content_bytes as f64 / BYTES_PER_TOKEN;
	let state = ENTRY_STATE_TOK.mul_add(n_entries as f64, BASE_TOK) + content;
	let total = FILE_Q_TOK
		.mul_add(n_files as f64, ENTRY_TOTAL_TOK.mul_add(n_entries as f64, BASE_TOK) + content);
	state <= STATE_LIMIT_TOK && total <= TOTAL_LIMIT_TOK
}
/// Line ranges per expanded file (per-file heat mode).
const RANGES_PER_FILE: usize = 12;

const TASK: &str = "Semantic grep over a source tree. `entries` lists unexplored paths to judge \
                    by name and location; `files` holds the first bytes of candidate files (every \
                    line prefixed with its number, e.g. L0042|) to judge by actual content.";

pub struct Inline {
	heat:         HeatMode,
	/// Per-file content cap override (bytes); None = opts.bytes.
	cap_override: Option<usize>,
	/// Attach queued files to listing requests (true) or always send them
	/// files-only (false).
	piggyback:    bool,
	frontier:     Vec<usize>,
	queue:        VecDeque<ReadyFile>,
	ready:        VecDeque<Job>,
	batches:      HashMap<u64, Batch>,
	in_flight:    usize,
	next_id:      u64,
	round:        u8,
	tau:          f64,
	hits:         usize,
	/// Depth (dependent round trips) of the request that exposed/judged a node.
	via_depth:    HashMap<usize, u32>,
	max_depth:    u32,
	file_cap:     usize,
	/// How many failed requests a node has already been part of (re-queued up to
	/// `MAX_REQUEUE`).
	failures:     HashMap<usize, u8>,
}

/// A node rides on at most this many failed requests before it is skipped.
const MAX_REQUEUE: u8 = 2;

struct ReadyFile {
	node:      usize,
	text:      String,
	bytes:     usize,
	truncated: bool,
	depth:     u32,
}

struct InlinedFile {
	node:      usize,
	key:       String,
	ranges:    Vec<(usize, usize, String)>,
	lines:     usize,
	truncated: bool,
	bytes:     usize,
}

struct Batch {
	entries: Vec<usize>,
	files:   Vec<InlinedFile>,
}

impl Inline {
	pub fn new(heat: HeatMode, piggyback: bool) -> Self {
		Self::with_cap(heat, piggyback, None)
	}

	pub fn with_cap(heat: HeatMode, piggyback: bool, cap_override: Option<usize>) -> Self {
		Self {
			heat,
			cap_override,
			piggyback,
			frontier: Vec::new(),
			queue: VecDeque::new(),
			ready: VecDeque::new(),
			batches: HashMap::new(),
			in_flight: 0,
			next_id: 0,
			round: 0,
			tau: 0.0,
			hits: 0,
			via_depth: HashMap::new(),
			max_depth: 0,
			file_cap: 32 * 1024,
			failures: HashMap::new(),
		}
	}
}

impl Strategy for Inline {
	fn run(&mut self, ctx: &mut Ctx) {
		self.file_cap = self
			.cap_override
			.map_or(ctx.opts.bytes, |c| c.min(ctx.opts.bytes));
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
		ctx.stats.waves = self.max_depth;
	}
}

impl Inline {
	fn run_round(&mut self, ctx: &mut Ctx) {
		loop {
			self.fill(ctx);
			self.dispatch(ctx);
			if self.in_flight == 0
				&& self.ready.is_empty()
				&& self.frontier.is_empty()
				&& self.queue.is_empty()
			{
				break;
			}
			let out = ctx.pool.recv();
			self.in_flight -= 1;
			self.apply(ctx, out);
		}
	}

	/// Same eager fill as the baseline: list the shallowest unjudged folders
	/// until the frontier reaches the soft batch target.
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
			let d = self.via_depth.get(&idx).copied().unwrap_or(0);
			let kids = ctx.tree.expand(idx);
			ctx.stats.expanded += 1;
			ctx.ui.fill(&ctx.tree.nodes[idx].rel, kids.len());
			for &k in &kids {
				self.via_depth.insert(k, d);
			}
			self.frontier.extend(kids);
		}
	}

	/// Pop queued files, in order, while the request (which already carries
	/// `n_entries` listing entries) stays under the token limits. A file that
	/// does not fit is left on the queue for the next request.
	fn take_files(&mut self, n_entries: usize) -> Vec<ReadyFile> {
		let mut out: Vec<ReadyFile> = Vec::new();
		let mut used = 0;
		while let Some(f) = self.queue.front() {
			let ok = fits(n_entries, used + f.bytes, out.len() + 1);
			// a lone file in a files-only request always goes (it is ≤ file_cap)
			if !ok && !(out.is_empty() && n_entries == 0) {
				break;
			}
			used += f.bytes;
			out.push(self.queue.pop_front().unwrap());
		}
		out
	}

	/// Does the queue already hold a full files-only request?
	fn queue_full(&self) -> bool {
		let mut used = 0;
		for (i, f) in self.queue.iter().enumerate() {
			used += f.bytes;
			if !fits(0, used, i + 1) {
				return true;
			}
		}
		false
	}

	fn dispatch(&mut self, ctx: &mut Ctx) {
		let parallel = ctx.opts.parallel;
		let idle = |s: &Self| s.in_flight + s.ready.len() < parallel;

		// Listing requests, with queued files riding along.
		if !self.frontier.is_empty() && (self.frontier.len() >= ctx.opts.batch || idle(self)) {
			let nodes = &ctx.tree.nodes;
			self
				.frontier
				.sort_by(|&a, &b| nodes[a].rel.cmp(&nodes[b].rel));
			let all = std::mem::take(&mut self.frontier);
			for chunk in all.chunks(ctx.opts.max_batch) {
				let entries = chunk.to_vec();
				for &i in &entries {
					ctx.tree.nodes[i].state = State::Pending;
				}
				let files = if self.piggyback {
					self.take_files(entries.len())
				} else {
					Vec::new()
				};
				self.build_request(ctx, entries, files);
			}
		}
		// Files-only requests: when the budget is full, or whenever a worker is
		// idle (everything on the queue arrived together with the last outcome,
		// so there is nothing to wait for).
		while !self.queue.is_empty() && (self.queue_full() || idle(self)) {
			let files = self.take_files(0);
			self.build_request(ctx, Vec::new(), files);
		}
		while self.in_flight < ctx.opts.parallel {
			let Some(job) = self.ready.pop_front() else {
				break;
			};
			if let Job::Ask { id, .. } = &job {
				let b = &self.batches[id];
				let kb = b.files.iter().map(|f| f.bytes).sum::<usize>() as f64 / 1024.0;
				ctx.ui
					.batch_inline(*id, b.entries.len(), b.files.len(), kb, self.in_flight + 1);
			}
			ctx.pool.submit(job);
			self.in_flight += 1;
		}
	}

	fn build_request(&mut self, ctx: &mut Ctx, entries: Vec<usize>, files: Vec<ReadyFile>) {
		let query = ctx.opts.query.clone();
		let mut state = Map::new();
		state.insert("task".into(), Value::String(TASK.into()));
		state.insert("search".into(), Value::String(query.clone()));
		state.insert("project".into(), Value::String(ctx.tree.name()));
		let mut questions: BTreeMap<String, Question> = BTreeMap::new();

		let mut depth = 0u32;
		if !entries.is_empty() {
			let (st, qs) = questions::dir_batch(&ctx.tree, &query, &entries);
			if let Some(e) = st.get("entries") {
				state.insert("entries".into(), e.clone());
			}
			questions.extend(qs);
			for &i in &entries {
				depth = depth.max(self.via_depth.get(&i).copied().unwrap_or(0) + 1);
			}
		}

		let mut inlined = Vec::new();
		if !files.is_empty() {
			let n_files = files.len();
			let per_file = match self.heat {
				HeatMode::PerFile => RANGES_PER_FILE,
				HeatMode::Shared => (255 / n_files).clamp(1, RANGES_PER_FILE),
			};
			let mut fmap = Map::new();
			let mut shared: BTreeMap<String, Value> = BTreeMap::new();
			for (k, f) in files.into_iter().enumerate() {
				depth = depth.max(f.depth + 1);
				let key = format!("f{k:02}");
				let node = &ctx.tree.nodes[f.node];
				let lines: Vec<&str> = f.text.lines().collect();
				let tagged = tag_lines(&lines);
				let (ranges, criteria) = line_ranges(&lines, per_file);
				let note = if f.truncated {
					format!("only the first {} of {} bytes are shown", f.bytes, node.size)
				} else {
					"complete file".to_string()
				};
				fmap.insert(key.clone(), json!({ "path": node.rel, "note": note, "content": tagged }));
				questions.insert(format!("{key}_relevant"), Question::Noul {
					instructions: Value::String(format!(
						"Does the content of `files.{key}.content` (file \"{}\") actually contain what \
						 this search is looking for: \"{query}\"?",
						node.rel
					)),
					criteria:     Some(NoulCriteria {
						yes: "The file contains code, text, or data that directly matches, implements, \
						      defines, or documents what the search describes."
							.into(),
						no:  "The file is unrelated, or only shares surface keywords with the search \
						      without containing the thing itself."
							.into(),
					}),
				});
				match self.heat {
					HeatMode::PerFile => {
						if ranges.len() >= 2 {
							questions.insert(format!("{key}_where"), Question::Choice {
								instructions: Value::String(format!(
									"Which range of lines in `files.{key}.content` (file \"{}\") best \
									 matches this search: \"{query}\"? Prefer the range where it is \
									 implemented or defined over ranges that merely import or reference it.",
									node.rel
								)),
								criteria,
							});
						}
					},
					HeatMode::Shared => {
						for (r, (s, e, _)) in ranges.iter().enumerate() {
							shared.insert(
								format!("{key}_{}", range_key(r)),
								Value::String(format!("`files.{key}` (\"{}\") lines {s}-{e}", node.rel)),
							);
						}
					},
				}
				inlined.push(InlinedFile {
					node: f.node,
					key,
					ranges,
					lines: lines.len(),
					truncated: f.truncated,
					bytes: f.bytes,
				});
			}
			if self.heat == HeatMode::Shared && shared.len() >= 2 {
				questions.insert("where".into(), Question::Choice {
					instructions: Value::String(format!(
						"Across all expanded files in `files`, which single range of lines best matches \
						 this search: \"{query}\"? Options name the file key and its line range. Prefer \
						 the range where it is implemented or defined over ranges that merely import or \
						 reference it."
					)),
					criteria:     shared,
				});
			}
			state.insert("files".into(), Value::Object(fmap));
		}

		self.max_depth = self.max_depth.max(depth);
		self.next_id += 1;
		let id = self.next_id;
		// remember the depth for nodes this request judges
		for &i in &entries {
			self.via_depth.insert(i, depth);
		}
		for f in &inlined {
			self.via_depth.insert(f.node, depth);
		}
		self.batches.insert(id, Batch { entries, files: inlined });
		self
			.ready
			.push_back(Job::Ask { id, state: Value::Object(state), questions });
	}

	fn apply(&mut self, ctx: &mut Ctx, out: Outcome) {
		let Outcome::Ask { id, result, elapsed } = out else {
			return;
		};
		let Some(batch) = self.batches.remove(&id) else {
			return;
		};
		match result {
			Ok(resp) => {
				ctx.stats.record(resp.usage, elapsed);
				if ctx.ui.verbose {
					ctx.ui.note(&format!(
						"    ↳ #{id}: {} entries + {} files ({} KB) → {} input tokens in {:.2}s",
						batch.entries.len(),
						batch.files.len(),
						batch.files.iter().map(|f| f.bytes).sum::<usize>() / 1024,
						resp.usage.input_tokens,
						elapsed.as_secs_f64()
					));
				}
				for (i, &idx) in batch.entries.iter().enumerate() {
					let p = resp.noul(&entry_key(i)).unwrap_or(0.0);
					self.judge_entry(ctx, idx, p);
				}
				let shared = if self.heat == HeatMode::Shared {
					resp.choice("where")
				} else {
					None
				};
				for f in &batch.files {
					let c = resp.noul(&format!("{}_relevant", f.key)).unwrap_or(0.0);
					let (heat, conf): (Vec<HeatRange>, Option<f64>) = match self.heat {
						HeatMode::PerFile => {
							questions::heat_from(&resp, &format!("{}_where", f.key), &f.ranges)
						},
						HeatMode::Shared => match shared {
							Some((probs, conf)) => (
								f.ranges
									.iter()
									.enumerate()
									.map(|(r, (s, e, snip))| HeatRange {
										start:   *s,
										end:     *e,
										p:       probs
											.get(&format!("{}_{}", f.key, range_key(r)))
											.copied()
											.unwrap_or(0.0),
										snippet: snip.clone(),
									})
									.collect(),
								Some(conf),
							),
							None => (Vec::new(), None),
						},
					};
					ctx.record_content(f.node, c, heat, conf, (f.lines, f.truncated), f.bytes);
					if c >= self.tau {
						ctx.mark_hit(f.node, false);
						self.hits += 1;
					} else {
						ctx.mark_fin(f.node, self.round);
						ctx.ui.miss(&ctx.tree.nodes[f.node].rel, c, self.round);
					}
				}
			},
			Err(e) => {
				ctx.stats.record_error(elapsed);
				ctx.ui.error(&format!(
					"request with {} entries + {} files failed: {e}",
					batch.entries.len(),
					batch.files.len()
				));
				// Transient failures (rate limits, connection resets) must not drop
				// whole subtrees: put the work back once or twice, then give up.
				for idx in batch.entries {
					let n = self.failures.entry(idx).or_insert(0);
					*n += 1;
					if *n <= MAX_REQUEUE {
						ctx.tree.nodes[idx].state = State::Unk;
						self.frontier.push(idx);
					} else {
						ctx.mark_skip(idx, format!("request failed: {e}"));
					}
				}
				for f in batch.files {
					let n = self.failures.entry(f.node).or_insert(0);
					*n += 1;
					if *n <= MAX_REQUEUE {
						self.read_file(ctx, f.node);
					} else {
						ctx.mark_skip(f.node, format!("request failed: {e}"));
					}
				}
			},
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
			},
			Kind::File => {
				if hot {
					self.read_file(ctx, idx);
				} else {
					ctx.mark_fin(idx, self.round);
					ctx.ui.fin_file(&ctx.tree.nodes[idx].rel, p, self.round);
				}
			},
		}
	}

	fn expand_hot(&mut self, ctx: &mut Ctx, idx: usize, why: &str) {
		let p = ctx.tree.nodes[idx].name_score.unwrap_or(0.0);
		let d = self.via_depth.get(&idx).copied().unwrap_or(0);
		let kids = ctx.tree.expand(idx);
		ctx.stats.expanded += 1;
		ctx.ui.exp(&ctx.tree.nodes[idx].rel, p, kids.len(), why);
		for &k in &kids {
			self.via_depth.insert(k, d);
		}
		self.frontier.extend(kids);
	}

	/// Read a hot file's head right now (page-cache fast) and queue it for
	/// inlining.
	fn read_file(&mut self, ctx: &mut Ctx, idx: usize) {
		let d = self.via_depth.get(&idx).copied().unwrap_or(0);
		let n = &mut ctx.tree.nodes[idx];
		n.state = State::Reading;
		ctx.ui.read(&n.rel, n.name_score.unwrap_or(0.0));
		match read_text(&n.path, self.file_cap) {
			Ok(rt) => self.queue.push_back(ReadyFile {
				node:      idx,
				text:      rt.text,
				bytes:     rt.bytes,
				truncated: rt.truncated,
				depth:     d,
			}),
			Err(e) => {
				let rel = ctx.tree.nodes[idx].rel.clone();
				ctx.ui.skip(&rel, &e.to_string());
				ctx.mark_skip(idx, e.to_string());
			},
		}
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
							self.read_file(ctx, i);
						}
					},
				},
			}
		}
	}
}
