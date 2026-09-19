//! Query-only lexical candidate rescue followed by semantic, bounded windows.
//!
//! Unlike prefix reads, local ranking examines the entire eligible text file.
//! Every submitted passage maps back to original line numbers; absolute
//! per-passage Nouls are comparable across requests and overlapping accepted
//! passages merge. No benchmark labels, symbols, or supplemental oracle
//! keywords are consulted.

use std::{
	collections::{BTreeMap, HashMap, HashSet, VecDeque},
	fmt::Write as _,
};

use serde_json::{Value, json};

use super::Strategy;
use crate::{
	ctx::Ctx,
	grep::{self, GrepIndex},
	jev::{NoulCriteria, Question},
	pool::{Job, Outcome},
	questions,
	tree::{HeatRange, Kind, State},
};

#[derive(Default)]
pub struct Window;

#[derive(Clone, Debug)]
pub(super) struct Passage {
	pub(super) start: usize,
	pub(super) end:   usize,
	pub(super) text:  String,
	pub(super) score: f64,
}

struct Check {
	node:        usize,
	total_lines: usize,
	truncated:   bool,
	passages:    Vec<Passage>,
}

fn enabled(name: &str, default: bool) -> bool {
	std::env::var(format!("JEGREP_WINDOW_{name}")).map_or(default, |v| v == "1")
}

fn scout_threshold(default: f64) -> f64 {
	std::env::var("JEGREP_WINDOW_SCOUT_THRESHOLD")
		.ok()
		.and_then(|v| v.parse::<f64>().ok())
		.filter(|v| v.is_finite() && (0.0..=1.0).contains(v))
		.unwrap_or(default)
}

/// Line coordinates belong to the caller. Noul passage judgments do not need a
/// repeated LNNN tag on every source line. Strip only our generated prefix.
pub(super) fn plain_content(passage: &Passage) -> String {
	let mut out = String::new();
	for (i, line) in passage.text.lines().enumerate() {
		let prefix = format!("L{}| ", passage.start + i);
		out.push_str(line.strip_prefix(&prefix).unwrap_or(line));
		out.push('\n');
	}
	out
}

pub(super) fn passage_request(
	query: &str,
	file: &str,
	passages: &[Passage],
	compact: bool,
) -> (Value, BTreeMap<String, Question>) {
	let criteria = NoulCriteria {
		yes: "This passage contains an implementation, definition, or substantive explanation of an \
		      important part of the search. A helper implementing one requested step counts even \
		      when other steps are elsewhere."
			.into(),
		no:  "This passage only mentions, calls, imports, tests, or configures the subject, or \
		      contains unrelated code sharing keywords."
			.into(),
	};
	let mut entries = serde_json::Map::new();
	let mut questions = BTreeMap::new();
	for (k, passage) in passages.iter().enumerate() {
		let key = format!("p{k:02}");
		let instructions = if compact {
			entries.insert(key.clone(), json!(plain_content(passage)));
			format!(
				"Does `passages.{key}` substantively implement, define, or explain part of \
				 \"{query}\"? Apply `criteria`."
			)
		} else {
			entries.insert(
				key.clone(),
				json!({"start_line": passage.start, "end_line": passage.end, "content": passage.text}),
			);
			format!(
				"Does passage `{key}` of file `{file}` substantively implement, define, or explain \
				 part of `search`? Judge only this passage, considering its original line numbers; \
				 apply `criteria`."
			)
		};
		questions.insert(key, Question::Noul {
			instructions: Value::String(instructions),
			criteria:     None,
		});
	}
	let mut state = json!({
		 "search": query, "file": file,
		 "criteria": {"yes": criteria.yes, "no": criteria.no}, "passages": entries,
	});
	if !compact {
		state["task"] =
			json!("Find substantive implementation passages matching a source-code search.");
		state["note"] = json!(
			"Passages retain original file line numbers. Omitted gaps are not evidence. Judge each \
			 passage independently, including separate helpers for distinct requested steps."
		);
	}
	(state, questions)
}

/// Weak filename candidates first receive two real (not summarized) windows.
/// Strong semantic candidates and prior hits keep the full recall budget.
fn scout_windows(
	mut passages: Vec<Passage>,
	name_score: f64,
	prior_hit: bool,
	adaptive: bool,
	threshold: f64,
) -> (Vec<Passage>, Vec<Passage>) {
	if !adaptive
		|| prior_hit
		|| name_score >= threshold
		|| passages.len() <= 2
		|| passages.iter().all(|p| p.score == 0.0)
	{
		return (passages, Vec::new());
	}
	passages.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.start.cmp(&b.start)));
	let mut deferred = passages.split_off(2);
	passages.sort_by_key(|p| p.start);
	deferred.sort_by_key(|p| p.start);
	(passages, deferred)
}

fn should_expand(score: f64, failed: bool, threshold: f64) -> bool {
	// An API failure is not negative evidence: never prune because of one.
	// Raising the scout cutoff trades deeper coverage for fewer requests.
	failed || score >= threshold
}

#[derive(Default)]
struct ContentWork {
	jobs:      VecDeque<Job>,
	checks:    HashMap<u64, Check>,
	remaining: HashMap<usize, usize>,
	next_id:   u64,
}

impl ContentWork {
	fn enqueue(&mut self, query: &str, file: &str, plan: Check, pack: usize, compact: bool) {
		for group in plan.passages.chunks(pack) {
			let (state, questions) = passage_request(query, file, group, compact);
			self.next_id += 1;
			self.checks.insert(self.next_id, Check {
				node:        plan.node,
				total_lines: plan.total_lines,
				truncated:   plan.truncated,
				passages:    group.to_vec(),
			});
			*self.remaining.entry(plan.node).or_default() += 1;
			self
				.jobs
				.push_back(Job::Ask { id: self.next_id, state, questions });
		}
	}
}

fn knob(name: &str, default: usize, low: usize, high: usize) -> usize {
	std::env::var(format!("JEGREP_WINDOW_{name}"))
		.ok()
		.and_then(|s| s.parse().ok())
		.unwrap_or(default)
		.clamp(low, high)
}

/// Rare query terms count more; logarithmic frequency prevents common words in
/// a giant file from overwhelming a compact implementation with several terms.
pub(super) fn file_score(counts: &[u32], weights: &[f64], path: &str, keywords: &[String]) -> f64 {
	let path = path.to_lowercase();
	counts
		.iter()
		.zip(weights)
		.zip(keywords)
		.map(|((&n, &w), keyword)| {
			let in_path = if path.contains(keyword) { 1.0 } else { 0.0 };
			w * 2.0f64.mul_add(in_path, (n as f64).ln_1p())
		})
		.sum()
}

pub(super) fn idf(index: &GrepIndex) -> Vec<f64> {
	index
		.keywords
		.iter()
		.enumerate()
		.map(|(k, _)| {
			let df = index
				.per_file_kw
				.values()
				.filter(|counts| counts.get(k).copied().unwrap_or(0) > 0)
				.count();
			// Cap rarity: a word occurring once in a test fixture should not beat
			// an implementation that contains several query concepts repeatedly.
			((index.files_scanned as f64 + 1.0) / (df as f64 + 1.0))
				.ln()
				.clamp(0.5, 6.0)
		})
		.collect()
}

/// Contiguous whole-line windows bounded by bytes, including the final line.
/// A single oversized line is UTF-8-safely clipped, retaining its real line id.
pub(super) fn windows(
	text: &str,
	bytes: usize,
	keywords: &[String],
	weights: &[f64],
) -> Vec<Passage> {
	let lines: Vec<&str> = text.lines().collect();
	let mut passages = Vec::new();
	let mut start = 0;
	while start < lines.len() {
		let mut end = start;
		let mut content = String::new();
		while end < lines.len() {
			let overhead = format!("L{}| ", end + 1).len() + 1;
			if end > start && content.len() + lines[end].len() + overhead > bytes {
				break;
			}
			let room = bytes.saturating_sub(content.len() + overhead);
			let line = lines[end];
			let mut cut = line.len().min(room);
			while !line.is_char_boundary(cut) {
				cut -= 1;
			}
			let _ = writeln!(content, "L{}| {}", end + 1, &line[..cut]);
			end += 1;
			if content.len() >= bytes {
				break;
			}
		}
		let lower = content.to_lowercase();
		let score = keywords
			.iter()
			.zip(weights)
			.map(|(kw, &w)| w * (lower.matches(kw).count() as f64).ln_1p())
			.sum();
		passages.push(Passage { start: start + 1, end, text: content, score });
		start = end;
	}
	passages
}

/// Keep best lexical windows and, when no words match, distribute the budget
/// through the file. This avoids always falling back to its opening bytes.
pub(super) fn select_windows(mut passages: Vec<Passage>, limit: usize) -> Vec<Passage> {
	if passages.len() > limit {
		if passages.iter().all(|p| p.score == 0.0) {
			let len = passages.len();
			passages = passages
				.into_iter()
				.enumerate()
				.filter_map(|(i, p)| {
					(0..limit)
						.any(|k| i == k * (len - 1) / (limit - 1).max(1))
						.then_some(p)
				})
				.collect();
		} else {
			passages.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.start.cmp(&b.start)));
			passages.truncate(limit);
		}
	}
	passages.sort_by_key(|p| p.start);
	passages
}

/// Union actually judged positive spans; never bridge an unjudged gap. Max is
/// used for ranking the merged span without rewarding repeated/overlapping
/// asks.
pub(super) fn merge_heat(mut heat: Vec<HeatRange>, threshold: f64) -> Vec<HeatRange> {
	heat.retain(|h| h.p >= threshold && h.p > 0.0 && h.start <= h.end);
	heat.sort_by_key(|h| (h.start, h.end));
	let mut merged: Vec<HeatRange> = Vec::new();
	for h in heat {
		if let Some(last) = merged.last_mut()
			&& h.start <= last.end.saturating_add(1)
		{
			last.end = last.end.max(h.end);
			if h.p > last.p {
				last.p = h.p;
			}
			continue;
		}
		merged.push(h);
	}
	merged.sort_by(|a, b| b.p.total_cmp(&a.p).then(a.start.cmp(&b.start)));
	merged
}

/// Existing Choice heat is already accepted evidence; its probabilities must
/// not be tested against an absolute Noul relevance threshold.
fn combine_heat(new: Vec<HeatRange>, prior: &[HeatRange], threshold: f64) -> Vec<HeatRange> {
	let mut heat = merge_heat(new, threshold);
	// Keep prior Choice bins separate: each positive probability is one part of
	// a distribution, not an independent assertion that the whole bin matches.
	heat.extend_from_slice(prior);
	heat.sort_by(|a, b| b.p.total_cmp(&a.p).then(a.start.cmp(&b.start)));
	heat
}

impl Strategy for Window {
	fn run(&mut self, ctx: &mut Ctx) {
		let compact = enabled("COMPACT", true);
		let adaptive = enabled("ADAPTIVE", true);
		let keywords = grep::keywords_from_query(&ctx.opts.query);
		ctx.ui.phase("lexical scan");
		let observer = ctx.ui.scan_observer();
		let index = match grep::grep_index_observed(
			&ctx.tree.root,
			&keywords,
			ctx.tree.include_hidden,
			observer.as_deref(),
		) {
			Ok(index) => index,
			Err(error) => {
				ctx.ui
					.error(&format!("window lexical scan failed: {error}"));
				return;
			},
		};
		let weights = idf(&index);
		let prior_hits = ctx.hits();
		let mut dirs: Vec<usize> = ctx
			.tree
			.nodes
			.iter()
			.enumerate()
			.filter(|(_, n)| n.kind == Kind::Dir && !matches!(n.state, State::Exp | State::Skip))
			.map(|(i, _)| i)
			.collect();
		while let Some(dir) = dirs.pop() {
			let children = ctx.tree.expand(dir);
			ctx.stats.expanded += 1;
			ctx.ui.fill(&ctx.tree.nodes[dir].rel, children.len());
			dirs.extend(
				children
					.into_iter()
					.filter(|&i| ctx.tree.nodes[i].kind == Kind::Dir),
			);
		}
		let candidates = knob("CANDIDATES", 128, 1, 512);
		let file_limit = knob("FILES", 16, 1, 64);
		let per_file = knob("PER_FILE", 16, 1, 64);
		let window_bytes = knob("BYTES", 8192, 1024, 12288);
		// At most 24 KB including line tags per state, safely below the service
		// state limit even for source that tokenizes poorly.
		let pack = knob("PACK", 4, 1, 16).min(24 * 1024 / window_bytes).max(1);
		let read_limit = knob("LOCAL_BYTES", 4 * 1024 * 1024, 8192, 16 * 1024 * 1024);
		let tau = ctx.opts.thresholds.last().copied().unwrap_or(0.2);
		let scout_tau = scout_threshold(tau.max(0.5));
		ctx.rounds = 1;
		ctx.tau = tau;
		ctx.ui.round(1, tau);
		ctx.ui.phase("filename ranking");
		let mut ranked: Vec<(usize, f64)> = ctx
			.tree
			.nodes
			.iter()
			.enumerate()
			.filter(|(_, n)| n.kind == Kind::File)
			.map(|(i, n)| {
				let counts = index
					.per_file_kw
					.get(&n.rel)
					.cloned()
					.unwrap_or_else(|| vec![0; keywords.len()]);
				(i, file_score(&counts, &weights, &n.rel, &keywords))
			})
			.collect();
		ranked.sort_by(|a, b| {
			b.1.total_cmp(&a.1)
				.then(ctx.tree.nodes[a.0].rel.cmp(&ctx.tree.nodes[b.0].rel))
		});
		ranked.truncate(candidates);
		// Existing semantic discoveries are eligible even outside the lexical
		// shortlist, allowing a previous strategy to seed this refinement.
		for &i in &prior_hits {
			if !ranked.iter().any(|&(node, _)| node == i) {
				ranked.push((i, 0.0));
			}
		}
		if ranked.is_empty() {
			return;
		}
		ctx.ui.note_entry(
			"window",
			"candidate scan",
			None,
			&format!(
				"{} local files; {} query-derived candidates; up to {file_limit} content files × \
				 {per_file} windows of {window_bytes} B{}",
				index.files_scanned,
				ranked.len(),
				if adaptive {
					format!("; 2-window scouts below {scout_tau:.2}")
				} else {
					String::new()
				}
			),
		);

		let mut jobs = VecDeque::new();
		let mut batches = HashMap::new();
		let mut next_id = 0;
		let ids: Vec<usize> = ranked.iter().map(|&(i, _)| i).collect();
		if ctx.ui.is_live() {
			for &i in &ids {
				ctx.ui.name_queued(&ctx.tree.nodes[i].rel);
			}
		}
		for batch in ids.chunks(ctx.opts.batch.min(ctx.opts.max_batch).clamp(1, 128)) {
			next_id += 1;
			let (state, questions) = questions::dir_batch(&ctx.tree, &ctx.opts.query, batch);
			batches.insert(next_id, batch.to_vec());
			jobs.push_back(Job::Ask { id: next_id, state, questions });
		}
		let mut in_flight = 0;
		let name_total = ids.len();
		let mut name_done = 0;
		ctx.ui.name_progress(name_done, name_total, in_flight);
		while !jobs.is_empty() || in_flight > 0 {
			while in_flight < ctx.opts.parallel.max(1) {
				let Some(job) = jobs.pop_front() else { break };
				if ctx.ui.is_live()
					&& let Job::Ask { id, .. } = &job
				{
					for &i in &batches[id] {
						ctx.ui.name_started(&ctx.tree.nodes[i].rel);
					}
				}
				ctx.pool.submit(job);
				in_flight += 1;
				ctx.ui.name_progress(name_done, name_total, in_flight);
			}
			let outcome = ctx.pool.recv();
			in_flight -= 1;
			if let Outcome::Ask { id, result, elapsed } = outcome {
				let Some(batch) = batches.remove(&id) else {
					ctx.ui.name_progress(name_done, name_total, in_flight);
					continue;
				};
				name_done += batch.len();
				ctx.ui.name_progress(name_done, name_total, in_flight);
				match result {
					Ok(resp) => {
						ctx.stats.window_name_tokens += resp.usage.input_tokens;
						ctx.stats.record(resp.usage, elapsed);
						let mut invalid = false;
						for (k, i) in batch.into_iter().enumerate() {
							let score = resp
								.noul(&questions::entry_key(k))
								.filter(|p| p.is_finite() && (0.0..=1.0).contains(p));
							ctx.tree.nodes[i].name_score = score;
							ctx.ui.name_scored(&ctx.tree.nodes[i].rel, score);
							if score.is_some() {
								ctx.stats.judged += 1;
							} else {
								invalid = true;
							}
						}
						if invalid {
							ctx.stats.errors += 1;
							ctx.ui
								.error("window candidate batch: missing or invalid filename judgment");
						}
					},
					Err(error) => {
						ctx.stats.record_error(elapsed);
						for i in batch {
							ctx.ui.name_scored(&ctx.tree.nodes[i].rel, None);
						}
						ctx.ui.error(&format!("window candidate batch: {error}"));
					},
				}
			}
		}
		// Semantic likelihood dominates; lexical rank breaks ties. Preserve a
		// small lexical rescue allowance in case independent filename judgments
		// fail to recognize a semantically relevant implementation filename.
		let rescue = knob("RESCUE", 2, 0, file_limit).min(ranked.len());
		let mut selected: Vec<usize> = prior_hits.iter().copied().take(file_limit).collect();
		for &(i, _) in ranked.iter().take(rescue) {
			if selected.len() < file_limit && !selected.contains(&i) {
				selected.push(i);
			}
		}
		ranked.sort_by(|a, b| {
			ctx.tree.nodes[b.0]
				.name_score
				.unwrap_or(0.0)
				.total_cmp(&ctx.tree.nodes[a.0].name_score.unwrap_or(0.0))
				.then(b.1.total_cmp(&a.1))
				.then(ctx.tree.nodes[a.0].rel.cmp(&ctx.tree.nodes[b.0].rel))
		});
		for (i, _) in ranked {
			if selected.len() >= file_limit {
				break;
			}
			if !selected.contains(&i) {
				selected.push(i);
			}
		}
		ctx.ui.phase("passage scoring");
		let mut work = ContentWork { next_id, ..ContentWork::default() };
		let mut results: HashMap<usize, (f64, Vec<HeatRange>, usize, bool, usize)> = HashMap::new();
		let mut deferred = HashMap::new();
		let mut failed = HashSet::new();
		for i in selected {
			let n = &ctx.tree.nodes[i];
			if ctx.ui.is_live() {
				ctx.ui.read(&n.rel, n.name_score.unwrap_or(0.0));
			}
			let read = match questions::read_text(&n.path, read_limit) {
				Ok(read) => read,
				Err(error) => {
					if prior_hits.contains(&i) {
						if ctx.ui.is_live() {
							let n = &ctx.tree.nodes[i];
							let top = n.heat.iter().max_by(|a, b| a.p.total_cmp(&b.p));
							ctx.ui
								.hit(&n.rel, n.content_score.unwrap_or(0.0), top, true);
						}
					} else {
						ctx.mark_skip(i, error.to_string());
					}
					continue;
				},
			};
			let total_lines = read.text.lines().count();
			let passages =
				select_windows(windows(&read.text, window_bytes, &keywords, &weights), per_file);
			if ctx.ui.is_live() {
				for passage in &passages {
					ctx.ui.range_queued(&n.rel, passage.start, passage.end);
				}
			}
			let chosen_lines: usize = passages.iter().map(|p| p.end - p.start + 1).sum();
			let truncated = read.truncated || chosen_lines < total_lines;
			// Unknown filename scores (e.g. a failed batch) keep the full budget.
			let (first, rest) = scout_windows(
				passages,
				n.name_score.unwrap_or(1.0),
				prior_hits.contains(&i),
				adaptive,
				scout_tau,
			);
			if !rest.is_empty() {
				deferred.insert(i, rest);
			}
			if first.is_empty() {
				if prior_hits.contains(&i) {
					ctx.tree.nodes[i].state = State::Hit;
					if ctx.ui.is_live() {
						let n = &ctx.tree.nodes[i];
						let top = n.heat.iter().max_by(|a, b| a.p.total_cmp(&b.p));
						ctx.ui
							.hit(&n.rel, n.content_score.unwrap_or(0.0), top, true);
					}
				} else {
					ctx.mark_fin(i, 1);
					if ctx.ui.is_live() {
						ctx.ui.miss(&ctx.tree.nodes[i].rel, 0.0, 1);
					}
				}
				continue;
			}
			work.enqueue(
				&ctx.opts.query,
				&n.rel,
				Check { node: i, total_lines, truncated, passages: first },
				pack,
				compact,
			);
			ctx.tree.nodes[i].state = State::Reading;
		}
		ctx.stats.waves = 2;
		while !work.jobs.is_empty() || in_flight > 0 {
			while in_flight < ctx.opts.parallel.max(1) {
				let Some(job) = work.jobs.pop_front() else {
					break;
				};
				let id = match &job {
					Job::Ask { id, .. } => *id,
					_ => unreachable!("window content work only submits passage requests"),
				};
				ctx.pool.submit(job);
				in_flight += 1;
				if ctx.ui.is_live()
					&& let Some(check) = work.checks.get(&id)
				{
					let rel = &ctx.tree.nodes[check.node].rel;
					for passage in &check.passages {
						ctx.ui.range_started(rel, passage.start, passage.end);
					}
				}
			}
			let outcome = ctx.pool.recv();
			in_flight -= 1;
			if let Outcome::Ask { id, result, elapsed } = outcome {
				let Some(check) = work.checks.remove(&id) else {
					continue;
				};
				match result {
					Ok(resp) => {
						ctx.stats.window_content_tokens += resp.usage.input_tokens;
						ctx.stats.record(resp.usage, elapsed);
						let accumulated =
							results
								.entry(check.node)
								.or_insert((0.0, Vec::new(), 0, check.truncated, 0));
						let mut invalid = false;
						for (k, passage) in check.passages.iter().enumerate() {
							let score = resp
								.noul(&format!("p{k:02}"))
								.filter(|p| p.is_finite() && (0.0..=1.0).contains(p));
							ctx.ui.range_scored(
								&ctx.tree.nodes[check.node].rel,
								passage.start,
								passage.end,
								score,
							);
							let Some(p) = score else {
								invalid = true;
								continue;
							};
							ctx.stats.windows_judged += 1;
							accumulated.0 = accumulated.0.max(p);
							accumulated.1.push(HeatRange {
								start: passage.start,
								end: passage.end,
								p,
								snippet: plain_content(passage)
									.lines()
									.find(|line| !line.trim().is_empty())
									.unwrap_or("")
									.chars()
									.take(100)
									.collect(),
							});
							accumulated.2 += passage.end - passage.start + 1;
							accumulated.4 += if compact {
								plain_content(passage).len()
							} else {
								passage.text.len()
							};
						}
						if invalid {
							failed.insert(check.node);
							ctx.stats.errors += 1;
							ctx.ui.error(&format!(
								"window {}: missing or invalid passage judgment",
								ctx.tree.nodes[check.node].rel
							));
						}
					},
					Err(error) => {
						failed.insert(check.node);
						ctx.stats.record_error(elapsed);
						for passage in &check.passages {
							ctx.ui.range_scored(
								&ctx.tree.nodes[check.node].rel,
								passage.start,
								passage.end,
								None,
							);
						}
						ctx.ui
							.error(&format!("window {}: {error}", ctx.tree.nodes[check.node].rel));
					},
				}
				let remaining = work.remaining.get_mut(&check.node).unwrap();
				*remaining -= 1;
				if *remaining == 0 {
					if let Some(rest) = deferred.remove(&check.node) {
						let score = results.get(&check.node).map_or(0.0, |r| r.0);
						if should_expand(score, failed.contains(&check.node), scout_tau) {
							work.enqueue(
								&ctx.opts.query,
								&ctx.tree.nodes[check.node].rel,
								Check {
									node:        check.node,
									total_lines: check.total_lines,
									truncated:   check.truncated,
									passages:    rest,
								},
								pack,
								compact,
							);
							ctx.stats.waves = 3;
							continue;
						}
						if ctx.ui.is_live() {
							for passage in &rest {
								ctx.ui.range_pruned(
									&ctx.tree.nodes[check.node].rel,
									passage.start,
									passage.end,
								);
							}
						}
						ctx.stats.windows_pruned += rest.len() as u32;
					}
					if let Some((mut score, mut heat, lines, truncated, bytes)) =
						results.remove(&check.node)
					{
						if prior_hits.contains(&check.node) {
							score = score.max(ctx.tree.nodes[check.node].content_score.unwrap_or(0.0));
							heat = combine_heat(heat, &ctx.tree.nodes[check.node].heat, tau);
						} else {
							heat = merge_heat(heat, tau);
						}
						let truncated = truncated || lines < check.total_lines;
						ctx.record_content(check.node, score, heat, None, (lines, truncated), bytes);
						if score >= tau {
							ctx.mark_hit(check.node, false);
						} else {
							ctx.mark_fin(check.node, 1);
							if ctx.ui.is_live() {
								ctx.ui.miss(&ctx.tree.nodes[check.node].rel, score, 1);
							}
						}
					} else if prior_hits.contains(&check.node) {
						ctx.tree.nodes[check.node].state = State::Hit;
						if ctx.ui.is_live() {
							let n = &ctx.tree.nodes[check.node];
							let top = n.heat.iter().max_by(|a, b| a.p.total_cmp(&b.p));
							ctx.ui
								.hit(&n.rel, n.content_score.unwrap_or(0.0), top, true);
						}
					} else {
						ctx.mark_skip(check.node, "all content requests failed".into());
					}
				}
			}
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn compact_content_preserves_source_and_original_coordinates() {
		let p = Passage {
			start: 40,
			end:   42,
			score: 1.0,
			text:  "L40|   α | beta\nL41| L999| literal source\nL42| \n".into(),
		};
		assert_eq!(plain_content(&p), "  α | beta\nL999| literal source\n\n");
		let (state, questions) =
			passage_request("needle search", "some/file.rs", std::slice::from_ref(&p), true);
		assert_eq!(state["passages"]["p00"], plain_content(&p));
		assert_eq!((p.start, p.end), (40, 42));
		assert!(
			serde_json::to_string(&questions)
				.unwrap()
				.contains("needle search")
		);
		let (full, full_questions) = passage_request("needle search", "some/file.rs", &[p], false);
		assert!(
			serde_json::to_vec(&(state, questions)).unwrap().len()
				< serde_json::to_vec(&(full, full_questions)).unwrap().len()
		);
	}

	#[test]
	fn scouting_uses_best_windows_anywhere_and_preserves_full_budget() {
		let make = || {
			(0..8)
				.map(|i| Passage {
					start: i * 10 + 1,
					end:   i * 10 + 10,
					text:  String::new(),
					score: i as f64,
				})
				.collect::<Vec<_>>()
		};
		let (first, rest) = scout_windows(make(), 0.1, false, true, 0.2);
		assert_eq!(first.iter().map(|p| p.start).collect::<Vec<_>>(), [61, 71]);
		let mut starts: Vec<_> = first.iter().chain(&rest).map(|p| p.start).collect();
		starts.sort_unstable();
		assert_eq!(starts, make().iter().map(|p| p.start).collect::<Vec<_>>());
		for (name, prior, adaptive) in [(0.2, false, true), (0.0, true, true), (0.0, false, false)] {
			let (first, rest) = scout_windows(make(), name, prior, adaptive, 0.2);
			assert_eq!(first.len(), 8);
			assert!(rest.is_empty());
		}
		let no_matches = make()
			.into_iter()
			.map(|mut p| {
				p.score = 0.0;
				p
			})
			.collect();
		let (first, rest) = scout_windows(no_matches, 0.0, false, true, 0.2);
		assert_eq!(first.len(), 8);
		assert!(rest.is_empty());
	}

	#[test]
	fn accepted_or_failed_scouts_expand_instead_of_losing_deeper_functions() {
		assert!(should_expand(0.0, true, 0.2));
		assert!(should_expand(0.32, false, 0.2));
		assert!(should_expand(0.2, false, 0.2));
		assert!(should_expand(0.1, false, 0.1));
		assert!(!should_expand(0.19, false, 0.2));
	}

	#[test]
	fn followup_jobs_keep_keys_and_original_passage_ranges() {
		let mut work = ContentWork::default();
		let plan = |start| Check {
			node:        7,
			total_lines: 200,
			truncated:   false,
			passages:    vec![Passage {
				start,
				end: start + 9,
				text: format!("L{start}| code\n"),
				score: 1.0,
			}],
		};
		work.enqueue("query", "file.c", plan(101), 3, true);
		let first_id = work.next_id;
		assert_eq!(work.remaining[&7], 1);
		assert_eq!(work.checks[&first_id].passages[0].start, 101);
		work.remaining.insert(7, 0);
		work.enqueue("query", "file.c", plan(181), 3, true);
		assert_eq!(work.remaining[&7], 1);
		assert!(work.next_id > first_id);
		assert_eq!(work.checks[&work.next_id].passages[0].start, 181);
	}

	#[test]
	fn windows_keep_original_lines_without_gaps_or_reindexing() {
		let text = (1..=500)
			.map(|i| format!("original line {i}\n"))
			.collect::<String>();
		let passages = windows(&text, 1024, &["original".into()], &[1.0]);
		assert!(passages.len() > 3);
		assert_eq!(passages[0].start, 1);
		assert_eq!(passages.last().unwrap().end, 500);
		for p in &passages {
			assert!(p.text.len() <= 1024);
			assert!(
				p.text
					.starts_with(&format!("L{}| original line {}", p.start, p.start))
			);
			assert!(
				p.text
					.contains(&format!("L{}| original line {}", p.end, p.end))
			);
		}
		assert!(passages.windows(2).all(|p| p[0].end + 1 == p[1].start));
	}

	#[test]
	fn oversized_utf8_lines_are_clipped_safely_without_losing_next_line() {
		let text = format!("{}\nlast line", "é".repeat(4000));
		let passages = windows(&text, 1024, &[], &[]);
		assert_eq!(passages.len(), 2);
		assert_eq!((passages[1].start, passages[1].end), (2, 2));
		assert!(passages[0].text.len() <= 1024);
		assert!(passages[1].text.contains("L2| last line"));
	}

	#[test]
	fn merging_retains_only_supported_lines_and_does_not_bridge_gaps() {
		let heat = [(1, 10, 0.9), (9, 20, 0.8), (21, 30, 0.7), (31, 40, 0.1), (41, 50, 0.95)]
			.into_iter()
			.map(|(start, end, p)| HeatRange { start, end, p, snippet: String::new() })
			.collect();
		let merged = merge_heat(heat, 0.4);
		assert_eq!(merged.len(), 2);
		assert_eq!((merged[0].start, merged[0].end, merged[0].p), (41, 50, 0.95));
		assert_eq!((merged[1].start, merged[1].end, merged[1].p), (1, 30, 0.9));
	}

	#[test]
	fn prior_choice_heat_survives_new_content_misses() {
		let prior = vec![HeatRange {
			start:   100,
			end:     120,
			p:       0.03,
			snippet: "previous evidence".into(),
		}];
		let new =
			vec![HeatRange { start: 400, end: 420, p: 0.1, snippet: "new miss".into() }];
		let combined = combine_heat(new, &prior, 0.2);
		assert_eq!(combined.len(), 1);
		assert_eq!((combined[0].start, combined[0].end, combined[0].p), (100, 120, 0.03));
	}

	#[test]
	fn prior_choice_bins_do_not_turn_into_a_whole_prefix_span() {
		let prior =
			vec![HeatRange { start: 1, end: 10, p: 0.03, snippet: String::new() }, HeatRange {
				start:   11,
				end:     20,
				p:       0.04,
				snippet: String::new(),
			}];
		let combined = combine_heat(Vec::new(), &prior, 0.2);
		assert_eq!(combined.len(), 2);
		assert_eq!((combined[0].start, combined[0].end), (11, 20));
		assert_eq!((combined[1].start, combined[1].end), (1, 10));
	}

	#[test]
	fn lexical_ranking_uses_term_diversity_path_and_log_frequency() {
		let keywords = vec!["needle".into(), "thread".into()];
		let weights = vec![3.0, 3.0];
		let focused = file_score(&[20, 20], &weights, "needle_thread.rs", &keywords);
		let repetitive = file_score(&[1000, 0], &weights, "other.rs", &keywords);
		assert!(focused > repetitive);
	}

	#[test]
	fn best_windows_include_late_content_and_empty_query_spreads_samples() {
		let make = || {
			(0..10)
				.map(|i| Passage {
					start: 1 + i * 10,
					end:   (i + 1) * 10,
					text:  String::new(),
					score: if i == 9 { 10.0 } else { 0.0 },
				})
				.collect()
		};
		let selected = select_windows(make(), 2);
		assert_eq!(selected.last().unwrap().start, 91);
		let mut empty = make();
		for p in &mut empty {
			p.score = 0.0;
		}
		let selected = select_windows(empty, 3);
		assert_eq!(selected.iter().map(|p| p.start).collect::<Vec<_>>(), vec![1, 41, 91]);
	}
}
