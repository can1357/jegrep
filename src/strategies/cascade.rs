//! Semantic passage maps before full-source reading. Sketch judgments are only
//! routing signals: a reported heat range always comes from a complete passage.

use std::collections::{BTreeMap, HashMap, VecDeque};

use serde_json::{Value, json};

use super::{
	Strategy,
	window::{self, Passage},
};
use crate::{
	ctx::Ctx,
	grep,
	jev::Question,
	pool::{Job, Outcome},
	questions,
	tree::{HeatRange, Kind, State},
};

pub struct Cascade;

fn knob(name: &str, default: usize, low: usize, high: usize) -> usize {
	std::env::var(format!("JEGREP_CASCADE_{name}"))
		.ok()
		.and_then(|v| v.parse().ok())
		.unwrap_or(default)
		.clamp(low, high)
}

fn cutoff() -> f64 {
	std::env::var("JEGREP_CASCADE_CUTOFF")
		.ok()
		.and_then(|v| v.parse::<f64>().ok())
		.filter(|v| v.is_finite() && (0.0..=1.0).contains(v))
		.unwrap_or(0.45)
}

/// A budgeted map of verbatim source lines, not an invented summary. Select
/// evidence across the passage, so deep implementation text can outrank
/// headers.
fn sketch(passage: &Passage, keywords: &[String], weights: &[f64], budget: usize) -> String {
	let plain = window::plain_content(passage);
	let lines: Vec<&str> = plain.lines().collect();
	let mut ranked: Vec<(usize, f64)> = lines
		.iter()
		.enumerate()
		.filter(|(_, l)| !l.trim().is_empty())
		.map(|(i, l)| {
			let lower = l.to_lowercase();
			let score: f64 = keywords
				.iter()
				.zip(weights)
				.filter(|(k, _)| lower.contains(k.as_str()))
				.map(|(_, w)| w)
				.sum();
			(i, score + if l.contains('(') { 0.1 } else { 0.0 })
		})
		.collect();
	ranked.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
	let mut selected = Vec::new();
	let mut used = 0;
	for (i, _) in ranked {
		let line = lines[i].trim();
		let available = budget.saturating_sub(used + 12);
		if available < 24 {
			break;
		}
		let mut end = line.len().min(available).min(180);
		while !line.is_char_boundary(end) {
			end -= 1;
		}
		let text = format!("{}: {}", passage.start + i, &line[..end]);
		used += text.len() + 1;
		selected.push((i, text));
	}
	selected.sort_by_key(|(i, _)| *i);
	selected
		.into_iter()
		.map(|(_, s)| s)
		.collect::<Vec<_>>()
		.join("\n")
}

struct FilePlan {
	node:      usize,
	total:     usize,
	truncated: bool,
	passages:  Vec<Passage>,
}

fn dispatch(
	ctx: &mut Ctx,
	mut jobs: VecDeque<Job>,
	// None announces submission; Some carries the completed judgment. Keeping
	// both events together lets the UI share the same request-to-source map.
	mut event: impl FnMut(&mut Ctx, u64, Option<Result<crate::jev::Response, crate::jev::Error>>, usize),
) {
	let mut running = 0;
	while !jobs.is_empty() || running > 0 {
		while running < ctx.opts.parallel.max(1) {
			let Some(job) = jobs.pop_front() else {
				break;
			};
			running += 1;
			if let Job::Ask { id, .. } = &job {
				event(ctx, *id, None, running);
			}
			ctx.pool.submit(job);
		}
		if let Outcome::Ask { id, result, elapsed } = ctx.pool.recv() {
			running -= 1;
			match &result {
				Ok(r) => ctx.stats.record(r.usage, elapsed),
				Err(_) => ctx.stats.record_error(elapsed),
			}
			event(ctx, id, Some(result), running);
		}
	}
}

impl Strategy for Cascade {
	fn run(&mut self, ctx: &mut Ctx) {
		let prepare_started = std::time::Instant::now();
		let keywords = grep::keywords_from_query(&ctx.opts.query);
		ctx.ui.phase("lexical scan");
		let observer = ctx.ui.scan_observer();
		let index = match grep::grep_index_observed(
			&ctx.tree.root,
			&keywords,
			ctx.tree.include_hidden,
			observer.as_deref(),
		) {
			Ok(i) => i,
			Err(e) => {
				ctx.ui.error(&format!("cascade scan: {e}"));
				return;
			},
		};
		let weights = window::idf(&index);
		let mut dirs = vec![0];
		while let Some(dir) = dirs.pop() {
			let children = ctx.tree.expand(dir);
			ctx.stats.expanded += 1;
			dirs.extend(
				children
					.into_iter()
					.filter(|&i| ctx.tree.nodes[i].kind == Kind::Dir),
			);
		}
		let mut ranked: Vec<(usize, f64)> = ctx
			.tree
			.nodes
			.iter()
			.enumerate()
			.filter(|(_, n)| n.kind == Kind::File)
			.map(|(i, n)| {
				let empty = vec![0; keywords.len()];
				(
					i,
					window::file_score(
						index.per_file_kw.get(&n.rel).unwrap_or(&empty),
						&weights,
						&n.rel,
						&keywords,
					),
				)
			})
			.collect();
		ranked.sort_by(|a, b| {
			b.1.total_cmp(&a.1)
				.then(ctx.tree.nodes[a.0].rel.cmp(&ctx.tree.nodes[b.0].rel))
		});
		ranked.truncate(knob("CANDIDATES", 128, 1, 512));
		let file_limit = knob("FILES", 20, 1, 64);
		let per_file = knob("WINDOWS", 24, 1, 128);
		let window_bytes = knob("BYTES", 8192, 1024, 12288);
		let sketch_bytes = knob("SKETCH_BYTES", 384, 96, 2048);
		let full_limit = knob("FULL_LIMIT", 40, 1, 512);
		let tau = ctx.opts.thresholds.last().copied().unwrap_or(0.2);
		ctx.tau = tau;
		ctx.rounds = 1;
		ctx.ui.round(1, tau);
		let mut jobs = VecDeque::new();
		let mut batches = HashMap::new();
		let mut next_id = 0u64;
		let ids: Vec<_> = ranked.iter().map(|&(i, _)| i).collect();
		if ctx.ui.is_live() {
			for &i in &ids {
				ctx.ui.name_queued(&ctx.tree.nodes[i].rel);
			}
		}
		for batch in ids.chunks(ctx.opts.batch.min(ctx.opts.max_batch).clamp(1, 128)) {
			let (mut state, qs) = questions::dir_batch(&ctx.tree, &ctx.opts.query, batch);
			// Generated implementation is still implementation. File provenance
			// must not itself be negative evidence for source-code searches.
			state["criteria"]["file"]["no"] = json!(
				"The file is unrelated by name and location; generated executable implementation can \
				 still be relevant."
			);
			next_id += 1;
			batches.insert(next_id, batch.to_vec());
			jobs.push_back(Job::Ask { id: next_id, state, questions: qs });
		}
		ctx.ui.phase("filename ranking");
		let mut name_done = 0;
		ctx.ui.name_progress(0, ids.len(), 0);
		ctx.stats.cascade_prepare_ms += prepare_started.elapsed().as_millis() as u64;
		dispatch(ctx, jobs, |ctx, id, result, active| {
			let Some(result) = result else {
				if ctx.ui.is_live() {
					for &i in &batches[&id] {
						ctx.ui.name_started(&ctx.tree.nodes[i].rel);
					}
				}
				ctx.ui.name_progress(name_done, ids.len(), active);
				return;
			};
			let batch = batches.remove(&id).unwrap();
			name_done += batch.len();
			match result {
				Ok(r) => {
					ctx.stats.window_name_tokens += r.usage.input_tokens;
					for (k, i) in batch.into_iter().enumerate() {
						let p = r
							.noul(&questions::entry_key(k))
							.filter(|p| p.is_finite() && (0.0..=1.0).contains(p));
						ctx.tree.nodes[i].name_score = p;
						ctx.ui.name_scored(&ctx.tree.nodes[i].rel, p);
						if p.is_some() {
							ctx.stats.judged += 1;
						} else {
							ctx.stats.errors += 1;
						}
					}
				},
				Err(e) => {
					for i in batch {
						ctx.ui.name_scored(&ctx.tree.nodes[i].rel, None);
					}
					ctx.ui.error(&format!("cascade filenames: {e}"));
				},
			}
			ctx.ui.name_progress(name_done, ids.len(), active);
		});
		let prepare_started = std::time::Instant::now();
		let mut selected: Vec<usize> = ranked
			.iter()
			.take(file_limit.min(2))
			.map(|&(i, _)| i)
			.collect();
		ranked.sort_by(|a, b| {
			ctx.tree.nodes[b.0]
				.name_score
				.unwrap_or(0.0)
				.total_cmp(&ctx.tree.nodes[a.0].name_score.unwrap_or(0.0))
				.then(b.1.total_cmp(&a.1))
		});
		for &(i, _) in &ranked {
			if selected.len() >= file_limit {
				break;
			}
			if !selected.contains(&i) {
				selected.push(i);
			}
		}
		let mut files = Vec::new();
		for node in selected {
			let n = &ctx.tree.nodes[node];
			match questions::read_text(&n.path, 4 * 1024 * 1024) {
				Ok(read) => {
					let total = read.text.lines().count();
					let passages = window::select_windows(
						window::windows(&read.text, window_bytes, &keywords, &weights),
						per_file,
					);
					if !passages.is_empty() {
						if ctx.ui.is_live() {
							ctx.ui.read(&n.rel, n.name_score.unwrap_or(0.0));
							for passage in &passages {
								ctx.ui.range_queued(&n.rel, passage.start, passage.end);
							}
						}
						files.push(FilePlan { node, total, truncated: read.truncated, passages });
					}
				},
				Err(e) => ctx.ui.error(&format!("cascade read {}: {e}", n.rel)),
			}
		}
		// Mixed-file packing pays one state ingestion for many independent small
		// cards, instead of rereading a whole file to discover its useful spans.
		let cards: Vec<(usize, usize)> = files
			.iter()
			.enumerate()
			.flat_map(|(f, p)| (0..p.passages.len()).map(move |i| (f, i)))
			.collect();
		let mut sketch_sent_bytes = 0usize;
		let mut jobs = VecDeque::new();
		let mut maps = HashMap::new();
		for batch in cards.chunks((18000 / sketch_bytes).clamp(1, 48)) {
			let mut excerpts = serde_json::Map::new();
			let mut paths = serde_json::Map::new();
			let mut qs = BTreeMap::new();
			for (k, &(f, p)) in batch.iter().enumerate() {
				let key = format!("p{k:02}");
				let file_key = format!("f{f}");
				paths.insert(file_key.clone(), json!(ctx.tree.nodes[files[f].node].rel));
				let text = sketch(&files[f].passages[p], &keywords, &weights, sketch_bytes);
				sketch_sent_bytes += text.len();
				excerpts.insert(key.clone(), json!([file_key, text]));
				qs.insert(key.clone(), Question::Noul {
					instructions: Value::String(format!(
						"Could passage {key} implement a requested step of search? Apply criteria."
					)),
					criteria:     None,
				});
			}
			let state = json!({"search":ctx.opts.query,"criteria":{"yes":"Likely substantive implementation, definition or explanation of any part of the requested behavior. A matching helper for one step counts. Excerpts omit most source: favor recall.","no":"Unrelated code; mere mentions, declarations, call sites, tests or configuration without implementation."},"files":paths,"passages":excerpts});
			next_id += 1;
			maps.insert(next_id, batch.to_vec());
			jobs.push_back(Job::Ask { id: next_id, state, questions: qs });
		}
		let mut candidates = Vec::new();
		ctx.stats.cascade_map_cards += cards.len() as u32;
		ctx.stats.cascade_prepare_ms += prepare_started.elapsed().as_millis() as u64;
		ctx.ui.phase("passage scoring");
		ctx.ui.phase("selecting passages from source sketches");
		dispatch(ctx, jobs, |ctx, id, result, _| {
			let Some(result) = result else {
				if ctx.ui.is_live() {
					for &(f, p) in &maps[&id] {
						let passage = &files[f].passages[p];
						ctx.ui.range_started(
							&ctx.tree.nodes[files[f].node].rel,
							passage.start,
							passage.end,
						);
					}
				}
				return;
			};
			let batch = maps.remove(&id).unwrap();
			if let Ok(r) = &result {
				ctx.stats.cascade_map_tokens += r.usage.input_tokens;
			}
			if let Err(e) = &result {
				ctx.ui.error(&format!("cascade map: {e}"));
			}
			for (k, (f, p)) in batch.into_iter().enumerate() {
				let score = result
					.as_ref()
					.ok()
					.and_then(|r| r.noul(&format!("p{k:02}")))
					.filter(|s| s.is_finite() && (0.0..=1.0).contains(s));
				if ctx.ui.is_live() {
					let passage = &files[f].passages[p];
					ctx.ui.range_scored(
						&ctx.tree.nodes[files[f].node].rel,
						passage.start,
						passage.end,
						score,
					);
				}
				if score.is_none() && result.is_ok() {
					ctx.stats.errors += 1;
				}
				// Failure is unknown, never grounds for a negative judgment.
				candidates.push((f, p, score.unwrap_or(1.0)));
			}
		});
		let prepare_started = std::time::Instant::now();
		candidates.sort_by(|a, b| {
			b.2.total_cmp(&a.2)
				.then(
					files[b.0].passages[b.1]
						.score
						.total_cmp(&files[a.0].passages[a.1].score),
				)
				.then(
					ctx.tree.nodes[files[a.0].node]
						.rel
						.cmp(&ctx.tree.nodes[files[b.0].node].rel),
				)
				.then(
					files[a.0].passages[a.1]
						.start
						.cmp(&files[b.0].passages[b.1].start),
				)
		});
		let cutoff = cutoff();
		candidates.retain(|c| c.2 >= cutoff);
		candidates.truncate(full_limit);
		ctx.stats.windows_pruned += (cards.len() - candidates.len()) as u32;
		let mut chosen: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
		for (f, p, _) in candidates {
			chosen.entry(f).or_default().push(p);
		}
		if ctx.ui.is_live() {
			for (f, plan) in files.iter().enumerate() {
				let rel = &ctx.tree.nodes[plan.node].rel;
				for (p, passage) in plan.passages.iter().enumerate() {
					if !chosen.get(&f).is_some_and(|ps| ps.contains(&p)) {
						ctx.ui.range_pruned(rel, passage.start, passage.end);
					}
				}
				if !chosen.contains_key(&f) {
					ctx.ui.skip(rel, "no passages selected for verification");
				}
			}
		}
		let mut jobs = VecDeque::new();
		let mut checks = HashMap::new();
		for (&f, ps) in &mut chosen {
			ps.sort_unstable();
			if ctx.ui.is_live() {
				let node = &ctx.tree.nodes[files[f].node];
				ctx.ui.read(&node.rel, node.name_score.unwrap_or(0.0));
				for &p in ps.iter() {
					let passage = &files[f].passages[p];
					ctx.ui.range_queued(&node.rel, passage.start, passage.end);
				}
			}
			for group in ps.chunks((24 * 1024 / window_bytes).max(1)) {
				let passages: Vec<_> = group
					.iter()
					.map(|&p| files[f].passages[p].clone())
					.collect();
				let (state, qs) = window::passage_request(
					&ctx.opts.query,
					&ctx.tree.nodes[files[f].node].rel,
					&passages,
					true,
				);
				next_id += 1;
				checks.insert(next_id, (f, passages));
				jobs.push_back(Job::Ask { id: next_id, state, questions: qs });
			}
		}
		let mut results: HashMap<usize, (f64, Vec<HeatRange>, usize, usize)> = HashMap::new();
		ctx.stats.cascade_prepare_ms += prepare_started.elapsed().as_millis() as u64;
		ctx.ui.phase("verifying selected source passages");
		dispatch(ctx, jobs, |ctx, id, result, _| {
			let Some(result) = result else {
				if ctx.ui.is_live() {
					let (f, passages) = &checks[&id];
					for p in passages {
						ctx.ui
							.range_started(&ctx.tree.nodes[files[*f].node].rel, p.start, p.end);
					}
				}
				return;
			};
			let (f, passages) = checks.remove(&id).unwrap();
			match result {
				Ok(r) => {
					ctx.stats.window_content_tokens += r.usage.input_tokens;
					let e = results.entry(f).or_default();
					for (k, p) in passages.iter().enumerate() {
						let score = r
							.noul(&format!("p{k:02}"))
							.filter(|s| s.is_finite() && (0.0..=1.0).contains(s));
						ctx.ui
							.range_scored(&ctx.tree.nodes[files[f].node].rel, p.start, p.end, score);
						let Some(score) = score else {
							ctx.stats.errors += 1;
							continue;
						};
						let text = window::plain_content(p);
						e.0 = e.0.max(score);
						e.1.push(HeatRange {
							start:   p.start,
							end:     p.end,
							p:       score,
							snippet: text
								.lines()
								.find(|l| !l.trim().is_empty())
								.unwrap_or("")
								.chars()
								.take(100)
								.collect(),
						});
						e.2 += p.end - p.start + 1;
						e.3 += text.len();
						ctx.stats.windows_judged += 1;
					}
				},
				Err(e) => {
					for p in passages {
						ctx.ui
							.range_scored(&ctx.tree.nodes[files[f].node].rel, p.start, p.end, None);
					}
					ctx.ui.error(&format!("cascade evidence: {e}"));
				},
			}
		});
		let fully_judged_files = results.len();
		for (f, (score, heat, lines, bytes)) in results {
			let plan = &files[f];
			ctx.record_content(
				plan.node,
				score,
				window::merge_heat(heat, tau),
				None,
				(lines, plan.truncated || lines < plan.total),
				bytes,
			);
			if score >= tau {
				ctx.mark_hit(plan.node, false);
			} else {
				if ctx.ui.is_live() {
					ctx.ui.miss(&ctx.tree.nodes[plan.node].rel, score, 1);
				}
				ctx.mark_fin(plan.node, 1);
			}
		}
		ctx.stats.file_bytes += sketch_sent_bytes as u64;
		ctx.stats.files_read += (files.len() - fully_judged_files) as u32;
		for f in files {
			if ctx.tree.nodes[f.node].state == State::Unk {
				if ctx.ui.is_live() {
					ctx.ui
						.skip(&ctx.tree.nodes[f.node].rel, "no verified passages");
				}
				ctx.mark_fin(f.node, 1);
			}
		}
		ctx.stats.waves = 3;
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	#[test]
	fn sketches_keep_original_line_coordinates_and_deep_evidence() {
		let p = Passage {
			start: 400,
			end:   403,
			text:  "L400| unrelated\nL401| // λλ\nL402| target_impl();\nL403| target helper\n".into(),
			score: 0.0,
		};
		let s = sketch(&p, &["target".into()], &[4.0], 100);
		assert!(s.contains("402: target_impl();"));
		assert!(s.contains("403: target helper"));
		assert!(s.len() <= 100);
		assert!(!s.contains("L402|"));
	}

	#[test]
	fn sketches_clip_utf8_without_exceeding_the_byte_budget() {
		let text = format!("L9| {}\nL10| needle_impl();\n", "λ".repeat(300));
		let p = Passage { start: 9, end: 10, text, score: 0.0 };
		for budget in 24..650 {
			let s = sketch(&p, &["needle".into()], &[5.0], budget);
			assert!(s.len() <= budget, "{} > {budget}", s.len());
			assert!(s.is_char_boundary(s.len()));
		}
	}
}
