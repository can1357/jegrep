//! Final report for a single search: pretty text (default), JSON, optional
//! tree.

use std::io::Write;

use crate::{
	ctx::Ctx,
	tree::{Kind, Tree},
	ui,
};

/// Line ranges a compact hit row carries.
const COMPACT_RANGES: usize = 3;

/// Digest for LLM readers: one tab-separated `path score spans` row per hit,
/// no color, grouping, snippets or near misses. `spans` is a comma-separated
/// list of `start-end` line ranges, strongest first, or `?` when the hit
/// carries no localized range. Rows keep the terminal report's order: weakest
/// first, strongest last.
fn compact_rows(out: &mut impl Write, tree: &Tree, hits: &[usize]) {
	let mut ordered: Vec<_> = hits.iter().map(|&i| &tree.nodes[i]).collect();
	ordered.sort_by(|a, b| {
		let (sa, sb) = (a.content_score.unwrap_or(0.0), b.content_score.unwrap_or(0.0));
		sa.total_cmp(&sb).then(a.rel.cmp(&b.rel))
	});
	for n in ordered {
		let spans = ui::ranked_heat(&n.heat, COMPACT_RANGES);
		let spans = if spans.is_empty() {
			"?".to_string()
		} else {
			spans
				.iter()
				.map(|r| ui::line_span(r.start, r.end))
				.collect::<Vec<_>>()
				.join(",")
		};
		let _ = writeln!(out, "{}\t{:.2}\t{spans}", n.rel, n.content_score.unwrap_or(0.0));
	}
}

pub fn print(ctx: &Ctx, json: bool, tree: bool, compact: bool) {
	ctx.ui.finish();
	let hits = ctx.hits();
	let elapsed = ctx.started.elapsed();
	let s = &ctx.stats;

	if compact {
		let mut out = std::io::stdout().lock();
		let _ = writeln!(
			out,
			"\"{}\" → {} hit(s) · τ {:.2} · round {}",
			ctx.opts.query,
			hits.len(),
			ctx.tau,
			ctx.rounds
		);
		compact_rows(&mut out, &ctx.tree, &hits);
		let _ = writeln!(
			out,
			"# root {} · judged {} · read {} files · ${:.4} · {:.1}s",
			ctx.tree.root.display(),
			s.judged,
			s.files_read,
			s.usd(),
			elapsed.as_secs_f64()
		);
		return;
	}

	if json {
		let hits_json: Vec<serde_json::Value> = hits
			.iter()
			.map(|&i| {
				let n = &ctx.tree.nodes[i];
				serde_json::json!({
					 "path": n.rel,
					 "name_score": n.name_score,
					 "content_score": n.content_score,
					 "confidence": n.confidence,
					 "lines_seen": n.lines_seen.map(|t| t.0),
					 "truncated": n.lines_seen.map(|t| t.1),
					 "ranges": n.heat.iter().map(|h| serde_json::json!({
						  "start": h.start, "end": h.end, "p": h.p, "snippet": h.snippet
					 })).collect::<Vec<_>>(),
				})
			})
			.collect();
		let out = serde_json::json!({
			 "query": ctx.opts.query,
			 "root": ctx.tree.root,
			 "rounds": ctx.rounds,
			 "threshold": ctx.tau,
			 "hits": hits_json,
			 "stats": {
				  "requests": s.requests,
				  "http_attempts": crate::jev::HTTP_ATTEMPTS.load(std::sync::atomic::Ordering::Relaxed),
				  "errors": s.errors,
				  "input_tokens": s.input_tokens,
				  "output_tokens": s.output_tokens,
				  "retries": crate::jev::RETRIES.load(std::sync::atomic::Ordering::Relaxed),
				  "waves": s.waves,
				  "api_ms": s.api_time.as_millis(),
				  "entries_judged": s.judged,
				  "dirs_expanded": s.expanded,
				  "files_read": s.files_read,
				  "sniffed": s.sniffed,
				  "file_bytes": s.file_bytes,
				  "window_name_tokens": s.window_name_tokens,
				  "cascade_map_tokens": s.cascade_map_tokens,
				  "cascade_map_cards": s.cascade_map_cards,
				  "cascade_prepare_ms": s.cascade_prepare_ms,
				  "window_content_tokens": s.window_content_tokens,
				  "windows_judged": s.windows_judged,
				  "windows_pruned": s.windows_pruned,
				  "usd": s.usd(),
				  "elapsed_ms": elapsed.as_millis(),
			 }
		});
		println!("{}", serde_json::to_string_pretty(&out).unwrap());
		return;
	}

	let ui = &ctx.ui;
	let mut out = std::io::stdout().lock();
	let _ = writeln!(out);
	if hits.is_empty() {
		let _ = writeln!(
			out,
			"{} {}",
			ui.bold("no hits"),
			ui.dim(&format!(
				"for \"{}\" after {} round(s), lowest τ = {:.2}",
				ctx.opts.query, ctx.rounds, ctx.tau
			))
		);
		let mut misses: Vec<usize> = (0..ctx.tree.nodes.len())
			.filter(|&i| ctx.tree.nodes[i].content_score.is_some())
			.collect();
		misses.sort_by(|&a, &b| {
			ctx.tree.nodes[b]
				.content_score
				.partial_cmp(&ctx.tree.nodes[a].content_score)
				.unwrap_or(std::cmp::Ordering::Equal)
		});
		if !misses.is_empty() {
			let _ = writeln!(out, "\n{}", ui.dim("closest by content:"));
			let items: Vec<(String, f64)> = misses
				.iter()
				.take(5)
				.map(|&i| {
					(ctx.tree.nodes[i].rel.clone(), ctx.tree.nodes[i].content_score.unwrap_or(0.0))
				})
				.collect();
			ui.print_scored(&mut out, &items);
		}
		let mut names: Vec<usize> = (0..ctx.tree.nodes.len())
			.filter(|&i| {
				let n = &ctx.tree.nodes[i];
				n.kind == Kind::File && n.name_score.is_some() && n.content_score.is_none()
			})
			.collect();
		names.sort_by(|&a, &b| {
			ctx.tree.nodes[b]
				.name_score
				.partial_cmp(&ctx.tree.nodes[a].name_score)
				.unwrap_or(std::cmp::Ordering::Equal)
		});
		if !names.is_empty() {
			let _ = writeln!(out, "\n{}", ui.dim("closest by name (never read):"));
			let items: Vec<(String, f64)> = names
				.iter()
				.take(5)
				.map(|&i| (ctx.tree.nodes[i].rel.clone(), ctx.tree.nodes[i].name_score.unwrap_or(0.0)))
				.collect();
			ui.print_scored(&mut out, &items);
		}
	} else {
		let _ = writeln!(
			out,
			"{} {}  {}",
			ui.bold(&format!("{} hit(s)", hits.len())),
			ui.dim(&format!("for \"{}\"", ctx.opts.query)),
			ui.dim(&format!("· round {} · τ = {:.2}", ctx.rounds, ctx.tau))
		);
		let _ = writeln!(out);
		ui.print_hits(&mut out, &ctx.tree, &hits);
		let _ = writeln!(out);
	}

	if tree {
		let _ = writeln!(out, "{}", ui.dim("── exploration tree ──"));
		ui.print_tree(&mut out, &ctx.tree);
		let _ = writeln!(out);
	}

	let listed = ctx.tree.nodes.len() - 1;
	let _ = writeln!(
		out,
		"{}",
		ui.dim(&format!(
			"listed {listed} · judged {} · expanded {} dirs · read {} files ({}){} · {} requests{}{} \
			 · {} tokens · ${:.4} · {:.1}s wall / {:.1}s api",
			s.judged,
			s.expanded,
			s.files_read,
			crate::tree::human_size(s.file_bytes.saturating_sub(s.sniff_bytes)),
			if s.sniffed > 0 {
				format!(" · sniffed {} heads ({})", s.sniffed, crate::tree::human_size(s.sniff_bytes))
			} else {
				String::new()
			},
			s.requests,
			if s.errors > 0 {
				format!(" ({} failed)", s.errors)
			} else {
				String::new()
			},
			{
				let r = crate::jev::RETRIES.load(std::sync::atomic::Ordering::Relaxed);
				if r > 0 {
					format!(" ({r} retries)")
				} else {
					String::new()
				}
			},
			human_count(s.input_tokens),
			s.usd(),
			elapsed.as_secs_f64(),
			s.api_time.as_secs_f64(),
		))
	);
}

pub fn human_count(n: u64) -> String {
	if n >= 1_000_000 {
		format!("{:.2}M", n as f64 / 1e6)
	} else if n >= 1_000 {
		format!("{:.1}k", n as f64 / 1e3)
	} else {
		n.to_string()
	}
}

#[cfg(test)]
mod tests {
	use std::{
		fs,
		time::{SystemTime, UNIX_EPOCH},
	};

	use super::compact_rows;
	use crate::tree::{HeatRange, State, Tree};

	fn heat(start: usize, end: usize, p: f64) -> HeatRange {
		HeatRange { start, end, p, snippet: String::new() }
	}

	#[test]
	fn compact_rows_carry_path_score_and_spans_weakest_first() {
		let nanos = SystemTime::now()
			.duration_since(UNIX_EPOCH)
			.unwrap()
			.as_nanos();
		let root = std::env::temp_dir().join(format!("jegrep-compact-{nanos}"));
		fs::create_dir_all(root.join("core")).unwrap();
		fs::write(root.join("core/handlers.go"), "package core\n").unwrap();
		fs::write(root.join("core/requests.go"), "package core\n").unwrap();
		fs::write(root.join("notes.txt"), "no heat here\n").unwrap();
		let mut tree = Tree::new(&root, false).unwrap();
		let core = tree.nodes.iter().position(|n| n.rel == "core/").unwrap();
		tree.expand(core);
		let mut hits = Vec::new();
		for (i, n) in tree.nodes.iter_mut().enumerate() {
			let (score, ranges) = match n.rel.as_str() {
				// Spans come out strongest first; single-line spans drop the end.
				"core/handlers.go" => {
					(0.72, vec![heat(400, 420, 0.30), heat(120, 180, 0.72), heat(401, 401, 0.60)])
				},
				// A whole-file hit is one span, not a `whole file` word.
				"core/requests.go" => (0.86, vec![heat(1, 546, 0.91)]),
				// No localized range to report.
				"notes.txt" => (0.40, Vec::new()),
				_ => continue,
			};
			n.state = State::Hit;
			n.content_score = Some(score);
			n.heat = ranges;
			hits.push(i);
		}
		let mut out = Vec::new();
		compact_rows(&mut out, &tree, &hits);
		let _ = fs::remove_dir_all(&root);
		assert_eq!(
			String::from_utf8(out).unwrap(),
			concat!(
				"notes.txt\t0.40\t?\n",
				"core/handlers.go\t0.72\t120-180,401,400-420\n",
				"core/requests.go\t0.86\t1-546\n"
			)
		);
	}
}
