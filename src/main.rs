#![allow(dead_code)] // extension points for strategies are intentionally unused by the baseline
//! jegrep — semantic grep over a directory tree, driven by Jev.
//!
//! Code owns the exploration (frontier, thresholds, parallelism, caching);
//! Jev supplies the judgments (is this path / this content what we're after?).

mod bench;
mod ctx;
mod env;
mod grep;
mod grouped;
mod jev;
mod pool;
mod questions;
mod report;
mod secrets;
mod strategies;
mod tree;
mod ui;
mod walker;

use std::{path::PathBuf, sync::Arc};

use clap::Parser;
use ctx::{Ctx, Opts};
use ui::{Progress, Ui, UiOptions};

#[derive(Parser, Debug)]
#[command(
	name = "jegrep",
	version,
	about = "Semantic grep: find files by describing what you're looking for, powered by Jev.",
	long_about = "jegrep finds source files and relevant line ranges from a plain-language query. \
	              The default cascade strategy ranks candidates using query-derived keywords, \
	              judges filenames, then uses small source sketches to select up to 40 full \
	              passages for parallel verification. Only verified source supplies results, with \
	              original line numbers and merged adjacent passages. Use --list-strategies to \
	              explore other policies."
)]
struct Cli {
	/// What you're looking for, in plain language. (Omit with --bench.)
	query:           Option<String>,
	/// Directory to search.
	#[arg(default_value = ".")]
	path:            PathBuf,
	/// Exploration strategy.
	#[arg(short = 's', long, default_value = "cascade")]
	strategy:        String,
	/// Concurrent requests in flight.
	#[arg(short = 'p', long, default_value_t = 16)]
	parallel:        usize,
	/// Entries per filename/directory batch. Tree strategies may eagerly list
	/// unjudged folders to fill the frontier to this target.
	#[arg(short = 'n', long, default_value_t = 64)]
	batch:           usize,
	/// Hard cap on entries per request (Jev caps a Choice at 255 options; this
	/// keeps Noul batches in the same ballpark and under the token budget).
	#[arg(long, default_value_t = 128)]
	max_batch:       usize,
	/// Relevance thresholds, one per round. A round ends when the frontier is
	/// exhausted; the next round reopens cached judgments at the lower bar.
	#[arg(short = 't', long, default_value = "0.4,0.2", value_delimiter = ',')]
	thresholds:      Vec<f64>,
	/// Bytes of each candidate file to send for the content check (default
	/// 32768). Whole-file strategies only; `cascade` and `window` budget by
	/// passage (see `JEGREP_CASCADE_BYTES` / `JEGREP_WINDOW_BYTES`).
	#[arg(long, value_name = "N")]
	bytes:           Option<usize>,
	/// Number of line ranges in the per-file heatmap (default 16). Whole-file
	/// strategies only.
	#[arg(long, value_name = "N")]
	ranges:          Option<usize>,
	/// Stop lowering the threshold once this many hits are found (default 1).
	/// Whole-file strategies only; `cascade` and `window` run a single pass.
	#[arg(long, value_name = "N")]
	min_hits:        Option<usize>,
	/// Preferred API provider (default: `OpenRouter` when its key exists). Fails
	/// over if both keys exist; see --only to forbid that.
	#[arg(long, value_enum, conflicts_with = "only")]
	endpoint:        Option<jev::Endpoint>,
	/// Use exactly this provider: error if its key is absent or a request
	/// fails, never fail over to the other account.
	#[arg(long, value_enum, value_name = "ENDPOINT")]
	only:            Option<jev::Endpoint>,
	/// Jev model id or alias.
	#[arg(long, default_value = "jev-latest")]
	model:           String,
	/// Extra keywords for the lexical scan of `cascade`, `window` and
	/// `paged-grep*` (comma-separated; added to those derived from the query).
	#[arg(short = 'k', long, value_delimiter = ',')]
	keywords:        Vec<String>,
	/// Include hidden (dot) files and folders.
	#[arg(long)]
	hidden:          bool,
	/// Send files whose content looks like credential material (private keys,
	/// service-account JSON, cloud/kubeconfig secrets, API tokens). Without
	/// this they are skipped and counted in the footer.
	#[arg(long)]
	allow_secrets:   bool,
	/// Print the annotated exploration tree at the end.
	#[arg(long)]
	tree:            bool,
	/// Emit the result as JSON on stdout.
	#[arg(long)]
	json:            bool,
	/// Compact, LLM-oriented digest on stdout: one tab-separated `path score
	/// spans` line per hit, no color, grouping, or snippets.
	#[arg(long, conflicts_with_all = ["json", "tree"])]
	compact:         bool,
	/// Run a benchmark: a JSON file of {name, query, expect[]} cases over PATH
	/// (the lone positional). Prints a table.
	#[arg(long, value_name = "CASES.json")]
	bench:           Option<PathBuf>,
	/// With --bench: also append the rows as JSON lines to this file.
	#[arg(long, value_name = "OUT.jsonl")]
	bench_out:       Option<PathBuf>,
	/// With --bench: label rows with this name instead of the strategy name (to
	/// compare variants).
	#[arg(long, value_name = "LABEL")]
	bench_label:     Option<String>,
	/// Summarize one or more --bench-out JSONL files into a per-strategy
	/// comparison and exit.
	#[arg(long, value_name = "RESULTS.jsonl", num_args = 1..)]
	bench_summary:   Vec<PathBuf>,
	/// List available strategies and exit.
	#[arg(long)]
	list_strategies: bool,
	/// Search progress on stderr. Live redraws on interactive terminals and
	/// falls back to the log for redirected output or basic terminals.
	#[arg(long, value_enum, default_value_t = Progress::default())]
	progress:        Progress,
	/// Log every judgment (including cold files and frontier fills).
	#[arg(short, long)]
	verbose:         bool,
	/// Suppress search progress on stderr.
	#[arg(short, long)]
	quiet:           bool,
}

fn main() {
	let mut cli = Cli::parse();
	// `jegrep --bench cases.json PATH`: the lone positional is the path, not a
	// query.
	if cli.bench.is_some()
		&& cli.path == *"."
		&& let Some(q) = cli.query.take()
	{
		cli.path = PathBuf::from(q);
	}
	let ui =
		Ui::new(UiOptions { quiet: cli.quiet, verbose: cli.verbose, progress: cli.progress });

	if cli.list_strategies {
		for n in strategies::NAMES {
			println!("{n}");
		}
		return;
	}
	if !cli.bench_summary.is_empty() {
		let mut rows: Vec<bench::Row> = Vec::new();
		for p in &cli.bench_summary {
			let text = std::fs::read_to_string(p).unwrap_or_else(|e| {
				ui.fatal(&format!("{}: {e}", p.display()));
				std::process::exit(2)
			});
			for line in text.lines().filter(|l| !l.trim().is_empty()) {
				match serde_json::from_str::<bench::Row>(line) {
					Ok(r) => rows.push(r),
					Err(e) => ui.error(&format!("{}: skipping row: {e}", p.display())),
				}
			}
		}
		bench::summarize(&rows);
		return;
	}
	let Some(mut strat) = strategies::make(&cli.strategy) else {
		ui.fatal(&format!(
			"unknown strategy '{}'; available: {}",
			cli.strategy,
			strategies::NAMES.join(", ")
		));
		std::process::exit(2);
	};
	if !strat.file_knobs() {
		let ignored: Vec<&str> = [
			("--bytes", cli.bytes.is_some()),
			("--ranges", cli.ranges.is_some()),
			("--min-hits", cli.min_hits.is_some()),
		]
		.into_iter()
		.filter_map(|(flag, set)| set.then_some(flag))
		.collect();
		if !ignored.is_empty() {
			ui.warn(&format!(
				"{} ignored: strategy '{}' budgets by passage (see --help)",
				ignored.join(", "),
				cli.strategy
			));
		}
	}
	if cli.thresholds.is_empty() || cli.thresholds.iter().any(|t| !(0.0..=1.0).contains(t)) {
		ui.fatal("thresholds must be probabilities in 0..=1");
		std::process::exit(2);
	}
	secrets::allow(cli.allow_secrets);
	let route = match cli.only {
		Some(endpoint) => jev::Route::Only(endpoint),
		None => jev::Route::Prefer(cli.endpoint),
	};
	let client = match jev::Client::new(route, cli.model.clone()) {
		Ok(k) => k,
		Err(e) => {
			ui.fatal(&e);
			std::process::exit(2);
		},
	};
	let client = Arc::new(client);
	let max_batch = cli.max_batch.clamp(1, 255);
	let opts = Opts {
		query: cli.query.clone().unwrap_or_default(),
		parallel: cli.parallel.max(1),
		batch: cli.batch.clamp(1, max_batch),
		max_batch,
		thresholds: cli.thresholds.clone(),
		bytes: cli.bytes.unwrap_or(32 * 1024).max(256),
		ranges: cli.ranges.unwrap_or(16).clamp(1, 255),
		min_hits: cli.min_hits.unwrap_or(1).max(1),
		keywords: cli.keywords.clone(),
	};

	if let Some(cases_path) = &cli.bench {
		let text = std::fs::read_to_string(cases_path).unwrap_or_else(|e| {
			ui.fatal(&format!("{}: {e}", cases_path.display()));
			std::process::exit(2)
		});
		let cases: Vec<bench::Case> = serde_json::from_str(&text).unwrap_or_else(|e| {
			ui.fatal(&format!("{}: {e}", cases_path.display()));
			std::process::exit(2)
		});
		eprintln!(
			"bench: {} cases · strategy {} · {}",
			cases.len(),
			cli.strategy,
			cli.path.display()
		);
		let mut rows =
			bench::run(&cases, &cli.strategy, &opts, &cli.path, cli.hidden, client, cli.verbose);
		if let Some(label) = &cli.bench_label {
			for r in &mut rows {
				r.strategy = label.clone();
			}
		}
		bench::print_table(&rows);
		if let Some(out) = &cli.bench_out {
			use std::io::Write;
			let mut f = std::fs::OpenOptions::new()
				.create(true)
				.append(true)
				.open(out)
				.unwrap_or_else(|e| {
					ui.fatal(&format!("{}: {e}", out.display()));
					std::process::exit(2)
				});
			for r in &rows {
				let _ = writeln!(f, "{}", serde_json::to_string(r).unwrap());
			}
		}
		return;
	}

	let Some(query) = cli.query.as_deref().filter(|q| !q.trim().is_empty()) else {
		ui.fatal("a query is required (or use --bench)");
		std::process::exit(2);
	};
	let mut ctx = match Ctx::new(opts, &cli.path, cli.hidden, client, ui) {
		Ok(c) => c,
		Err(e) => {
			Ui::new(UiOptions::default()).fatal(&format!("{}: {e}", cli.path.display()));
			std::process::exit(2);
		},
	};
	ctx.ui.banner(
		query,
		&ctx.tree.root.display().to_string(),
		&cli.model,
		&ctx.client.route(),
		ctx.opts.parallel,
		ctx.opts.batch,
		ctx.opts.max_batch,
	);
	ctx.ui.workspace(&ctx.tree);
	strat.run(&mut ctx);
	report::print(&ctx, cli.json, cli.tree, cli.compact);
}
