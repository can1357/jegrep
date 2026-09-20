//! Terminal output: selectable progress on stderr, the final report on stdout.

mod live;

use std::{
	cell::RefCell,
	io::{IsTerminal, Write},
	sync::{Arc, Weak},
};

use clap::ValueEnum;
use live::{Activity, Event, Live, RangeState};
use parking_lot::Mutex;

use crate::{
	grouped::{self, Leaf, split_rel},
	tree::{HeatRange, Kind, State, Tree},
};

static ACTIVE: Mutex<Weak<Live>> = Mutex::new(Weak::new());

/// Progress presentation selected by the CLI; live falls back on basic
/// terminals.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, ValueEnum)]
pub enum Progress {
	/// Append grouped events to stderr.
	Log,
	/// Redraw a compact activity panel on interactive stderr.
	#[default]
	Live,
}

/// Terminal preferences shared by interactive searches and benchmarks.
#[derive(Default)]
pub struct UiOptions {
	/// Suppress progress, but not diagnostics.
	pub quiet:    bool,
	/// Include cold entries and strategy diagnostics.
	pub verbose:  bool,
	/// Choose a streaming log or an in-place panel.
	pub progress: Progress,
}

/// Print a durable diagnostic without letting worker output corrupt live
/// progress.
pub fn diagnostic(message: &str) {
	let active = ACTIVE.lock().upgrade();
	if let Some(live) = active {
		live.message(message);
	} else {
		eprintln!("{message}");
	}
}

/// Search progress and final-report styling.
pub struct Ui {
	pub color:   bool,
	pub verbose: bool,
	pub quiet:   bool,
	/// Directory header currently open in the live log (streaming grouping).
	cur_dir:     RefCell<Option<String>>,
	progress:    Progress,
	live:        RefCell<Option<Arc<Live>>>,
}

const NAME_W: usize = 40;
const BLOCKS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

impl Ui {
	/// Apply terminal capabilities to the requested progress presentation.
	pub fn new(options: UiOptions) -> Self {
		let terminal = std::io::stderr().is_terminal()
			&& std::env::var("TERM").is_ok_and(|term| !term.is_empty() && term != "dumb");
		let color = terminal && std::env::var_os("NO_COLOR").is_none();
		let progress = if terminal && !options.quiet {
			options.progress
		} else {
			Progress::Log
		};
		Self {
			color,
			verbose: options.verbose,
			quiet: options.quiet,
			cur_dir: RefCell::new(None),
			progress,
			live: RefCell::new(None),
		}
	}

	/// Whether strategy-specific activity is currently visible in the live
	/// panel.
	pub fn is_live(&self) -> bool {
		self.live.borrow().is_some()
	}

	fn update(&self, event: Event<'_>) -> bool {
		if let Some(live) = self.live.borrow().as_ref() {
			live.update(event);
			true
		} else {
			false
		}
	}

	/// Change the live phase without adding noise to the streaming log.
	pub fn phase(&self, text: &str) {
		self.update(Event::Status(text));
	}

	/// Seed stable lanes from the workspace's already-listed root entries.
	pub fn workspace(&self, tree: &Tree) {
		if !self.is_live() {
			return;
		}
		let children = &tree.nodes[0].children;
		let folders: Vec<_> = children
			.iter()
			.filter(|&&i| tree.nodes[i].kind == Kind::Dir)
			.map(|&i| tree.nodes[i].rel.clone())
			.collect();
		self.update(Event::Workspace {
			name:       &tree.name(),
			folders:    &folders,
			root_files: children.iter().any(|&i| tree.nodes[i].kind == Kind::File),
		});
	}

	/// A thread-safe observer for parallel local scans, independent of this UI's
	/// `RefCells`.
	pub fn scan_observer(&self) -> Option<Arc<dyn Fn(&str) + Send + Sync>> {
		let live = self.live.borrow().clone()?;
		Some(Arc::new(move |path| live.update(Event::Scanned(path))))
	}

	pub fn name_queued(&self, path: &str) {
		self.update(Event::Name { path, state: RangeState::Queued });
	}

	pub fn name_started(&self, path: &str) {
		self.update(Event::Name { path, state: RangeState::Active });
	}

	pub fn name_scored(&self, path: &str, score: Option<f64>) {
		self
			.update(Event::Name { path, state: score.map_or(RangeState::Failed, RangeState::Scored) });
	}

	/// Report actual filename judgments and active filename requests.
	pub fn name_progress(&self, done: usize, total: usize, active: usize) {
		self.update(Event::Names { done, total, active });
	}

	fn range(&self, rel: &str, start: usize, end: usize, state: RangeState) {
		self.update(Event::Range { path: rel, start, end, state });
	}

	/// Register a selected passage before its request is submitted.
	pub fn range_queued(&self, rel: &str, start: usize, end: usize) {
		self.range(rel, start, end, RangeState::Queued);
	}

	/// Mark a passage as submitted and awaiting a judgment.
	pub fn range_started(&self, rel: &str, start: usize, end: usize) {
		self.range(rel, start, end, RangeState::Active);
	}

	/// Complete a passage judgment; missing scores represent failed requests.
	pub fn range_scored(&self, rel: &str, start: usize, end: usize, score: Option<f64>) {
		self.range(rel, start, end, score.map_or(RangeState::Failed, RangeState::Scored));
	}

	/// Mark a deferred passage as deliberately left unjudged.
	pub fn range_pruned(&self, rel: &str, start: usize, end: usize) {
		self.range(rel, start, end, RangeState::Pruned);
	}

	/// Settle an unreadable file without adding a second legacy log message.
	pub fn skipped(&self, rel: &str, reason: &str) {
		self.update(Event::Entry {
			activity: Activity::Skipped,
			path:     rel,
			score:    None,
			detail:   reason,
		});
	}

	/// Stop animation and erase its owned lines before stdout receives the
	/// report.
	pub fn finish(&self) {
		let live = self.live.borrow_mut().take();
		if let Some(live) = live.as_ref() {
			live.stop();
			*ACTIVE.lock() = Weak::new();
		}
		drop(live);
	}

	fn c(&self, code: &str, s: &str) -> String {
		if self.color {
			format!("\x1b[{code}m{s}\x1b[0m")
		} else {
			s.to_string()
		}
	}

	pub fn dim(&self, s: &str) -> String {
		self.c("2", s)
	}

	pub fn bold(&self, s: &str) -> String {
		self.c("1", s)
	}

	pub fn red(&self, s: &str) -> String {
		self.c("31", s)
	}

	pub fn green(&self, s: &str) -> String {
		self.c("32", s)
	}

	pub fn yellow(&self, s: &str) -> String {
		self.c("33", s)
	}

	pub fn blue(&self, s: &str) -> String {
		self.c("34", s)
	}

	pub fn magenta(&self, s: &str) -> String {
		self.c("35", s)
	}

	pub fn cyan(&self, s: &str) -> String {
		self.c("36", s)
	}

	/// Free-form line on the live log (respects --quiet).
	pub fn note(&self, s: &str) {
		self.line(s);
	}

	fn line(&self, s: &str) {
		if !self.quiet && !self.update(Event::Status(s)) {
			diagnostic(s);
		}
	}

	fn p(&self, p: f64) -> String {
		let s = format!("{p:.2}");
		if p >= 0.7 {
			self.green(&s)
		} else if p >= 0.4 {
			self.yellow(&s)
		} else {
			self.dim(&s)
		}
	}

	/// Live-log line, grouped pi-style: the directory header is printed once,
	/// then entries in that directory are indented under it by name only.
	fn event(
		&self,
		activity: Activity,
		glyph: &str,
		tag: &str,
		rel: &str,
		p: Option<f64>,
		extra: &str,
	) {
		if self.quiet || self.update(Event::Entry { activity, path: rel, score: p, detail: extra }) {
			return;
		}
		let (dir, name) = split_rel(rel);
		let mut cur = self.cur_dir.borrow_mut();
		if cur.as_deref() != Some(dir) {
			let shown = if dir.is_empty() { "./" } else { dir };
			self.line(&format!("  {}", self.bold(&self.blue(shown))));
			*cur = Some(dir.to_string());
		}
		let p = p.map_or_else(|| "    ".into(), |p| self.p(p));
		self.line(&format!("    {glyph} {tag:<5} {:<NAME_W$} {p}  {extra}", fit(name, NAME_W)));
	}

	/// Break the current directory grouping (next event reprints its header).
	fn reset_dir(&self) {
		*self.cur_dir.borrow_mut() = None;
	}

	pub fn banner(&self, query: &str, root: &str, model: &str, p: usize, n: usize, cap: usize) {
		self.line(&format!(
			"{} {}  {}  {}",
			self.bold("jegrep"),
			self.cyan(&format!("\"{query}\"")),
			self.dim(&format!("in {root}")),
			self.dim(&format!("· {model} · P={p} N={n} cap={cap}")),
		));
		if self.progress == Progress::Live && !self.is_live() {
			let live = Arc::new(Live::new(self.color));
			*ACTIVE.lock() = Arc::downgrade(&live);
			*self.live.borrow_mut() = Some(live);
		}
	}

	pub fn round(&self, round: u8, tau: f64) {
		if self.update(Event::Round { number: round, threshold: tau }) {
			return;
		}
		self.reset_dir();
		self.line(&self.bold(&format!("── round {round}  τ = {tau:.2} ──")));
	}

	pub fn round_end(&self, round: u8, tau: f64, hits: usize, next: Option<f64>) {
		self.reset_dir();
		let tail = match next {
			Some(t) if hits == 0 => format!("→ lowering τ to {t:.2}"),
			_ => String::new(),
		};
		self.line(&self.dim(&format!("── round {round} done: {hits} hit(s) at τ={tau:.2} {tail}")));
	}

	pub fn batch(&self, id: u64, n: usize, in_flight: usize) {
		self.reset_dir();
		self.line(&format!(
			"  {} {}",
			self.blue("⟳"),
			self.dim(&format!("batch #{id}: {n} entries → jev   ({in_flight} in flight)"))
		));
	}

	pub fn batch_inline(&self, id: u64, entries: usize, files: usize, kb: f64, in_flight: usize) {
		self.line(&format!(
			"  {} {}",
			self.blue("⟳"),
			self.dim(&format!(
				"batch #{id}: {entries} entries + {files} files ({kb:.0} KB) → jev   ({in_flight} in \
				 flight)"
			))
		));
	}

	pub fn fill(&self, rel: &str, kids: usize) {
		if self.verbose || self.is_live() {
			self.event(
				Activity::Folder,
				&self.dim("·"),
				"list",
				rel,
				None,
				&self.dim(&format!("{kids} entries (filling frontier)")),
			);
		}
	}

	pub fn exp(&self, rel: &str, p: f64, kids: usize, why: &str) {
		self.event(
			Activity::Folder,
			&self.green("▸"),
			"exp",
			rel,
			Some(p),
			&self.dim(&format!("{kids} entries {why}")),
		);
	}

	pub fn fin_dir(&self, rel: &str, p: f64, count: usize, round: u8) {
		self.event(
			Activity::Collapsed,
			&self.dim("▹"),
			&format!("fin{round}"),
			rel,
			Some(p),
			&self.dim(&format!("collapsed {count} entries")),
		);
	}

	pub fn fin_file(&self, rel: &str, p: f64, round: u8) {
		if self.verbose || self.is_live() {
			self.event(Activity::Miss, &self.dim("·"), &format!("fin{round}"), rel, Some(p), "");
		}
	}

	pub fn read(&self, rel: &str, p: f64) {
		self.event(
			Activity::Reading,
			&self.yellow("○"),
			"read",
			rel,
			Some(p),
			&self.dim("name looks promising → reading content"),
		);
	}

	pub fn read_why(&self, rel: &str, p: f64, why: &str) {
		self.event(Activity::Reading, &self.yellow("○"), "read", rel, Some(p), &self.dim(why));
	}

	pub fn sniff_batch(&self, id: u64, n: usize, in_flight: usize) {
		self.line(&format!(
			"  {} {}",
			self.blue("⟳"),
			self.dim(&format!("sniff #{id}: {n} file heads → jev   ({in_flight} in flight)"))
		));
	}

	pub fn sniff(&self, rel: &str, p: f64, pass: bool) {
		if self.verbose {
			let glyph = if pass {
				self.yellow("◌")
			} else {
				self.dim("·")
			};
			let extra = if pass {
				String::new()
			} else {
				self.dim("opening not relevant")
			};
			self.event(Activity::Note, &glyph, "sniff", rel, Some(p), &extra);
		}
	}

	pub fn hit(&self, rel: &str, c: f64, top: Option<&HeatRange>, cached: bool) {
		let extra = match top {
			Some(r) => format!("L{}–{}  {}", r.start, r.end, self.dim(&fit(&r.snippet, 48))),
			None => String::new(),
		};
		let extra = if cached {
			format!("{extra} {}", self.dim("(from cache)"))
		} else {
			extra
		};
		self.event(Activity::Hit, &self.green("●"), &self.bold("HIT"), rel, Some(c), &extra);
	}

	pub fn miss(&self, rel: &str, c: f64, round: u8) {
		self.event(
			Activity::Miss,
			&self.dim("✕"),
			&format!("miss{round}"),
			rel,
			Some(c),
			&self.dim("content not relevant"),
		);
	}

	pub fn skip(&self, rel: &str, why: &str) {
		if self.verbose || self.is_live() {
			self.event(Activity::Skipped, &self.dim("–"), "skip", rel, None, &self.dim(why));
		}
	}

	/// Generic strategy-specific event line.
	pub fn note_entry(&self, tag: &str, rel: &str, p: Option<f64>, extra: &str) {
		self.event(Activity::Note, &self.yellow("◌"), tag, rel, p, &self.dim(extra));
	}

	pub fn error(&self, what: &str) {
		self.reset_dir();
		diagnostic(&format!("  {} {}", self.red("!"), what));
	}

	/// Non-fatal misuse (e.g. a flag the chosen strategy ignores). Shown even
	/// with `--quiet`.
	pub fn warn(&self, what: &str) {
		self.reset_dir();
		diagnostic(&format!("  {} {}", self.yellow("!"), what));
	}

	pub fn fatal(&self, what: &str) {
		self.finish();
		diagnostic(&format!("{} {}", self.red("error:"), what));
	}

	// ── final report ────────────────────────────────────────────────────────

	pub fn heat_strip(&self, heat: &[HeatRange]) -> String {
		let max = heat.iter().map(|h| h.p).fold(0.0, f64::max).max(1e-9);
		heat
			.iter()
			.map(|h| {
				let lvl = ((h.p / max) * 7.0).round() as usize;
				let ch = BLOCKS[lvl.min(7)].to_string();
				if h.p / max >= 0.6 {
					self.red(&ch)
				} else if h.p / max >= 0.25 {
					self.yellow(&ch)
				} else {
					self.dim(&ch)
				}
			})
			.collect()
	}

	/// A whole-file result has no useful localization to expand underneath it.
	const fn whole_file(heat: &[HeatRange], lines_seen: Option<(usize, bool)>) -> bool {
		matches!((heat, lines_seen), ([r], Some((lines, false)))
            if lines > 0 && r.start == 1 && r.end == lines)
	}

	/// Body lines for localized hits, with the strongest of the top three last.
	fn hit_body(&self, heat: &[HeatRange], lines_seen: Option<(usize, bool)>) -> Vec<String> {
		let mut body = Vec::new();
		if heat.is_empty() || Self::whole_file(heat, lines_seen) {
			return body;
		}
		let mut sorted: Vec<&HeatRange> = heat.iter().collect();
		sorted.sort_by(|a, b| b.p.total_cmp(&a.p).then(a.start.cmp(&b.start)));
		sorted.truncate(3);
		// One normalized heat glyph is always a full block, which adds no
		// information.
		if heat.len() > 1 {
			body.push(self.heat_strip(heat));
		}
		for r in sorted.iter().rev().filter(|r| r.p >= 0.05) {
			body.push(format!(
				"{:<11} {}  {}",
				self.cyan(&format!("L{}–{}", r.start, r.end)),
				self.p(r.p),
				fit(&r.snippet, 76)
			));
		}
		body
	}

	fn hit_leaf(
		&self,
		rel: &str,
		score: f64,
		name_score: Option<f64>,
		heat: &[HeatRange],
		lines_seen: Option<(usize, bool)>,
	) -> Leaf {
		let mut detail = String::new();
		if Self::whole_file(heat, lines_seen) {
			detail.push_str(" · whole file");
		}
		if let Some((n, truncated)) = lines_seen {
			detail.push_str(&format!(" · {n} lines{}", if truncated { " shown" } else { "" }));
		}
		if self.verbose
			&& let Some(score) = name_score
		{
			detail.push_str(&format!(" · name {score:.2}"));
		}
		Leaf {
			rel:    rel.to_owned(),
			header: format!("  {}{}", self.bold(&self.p(score)), self.dim(&detail)),
			body:   self.hit_body(heat, lines_seen),
		}
	}

	/// Rank the final display independently of the search's best-first work
	/// order.
	pub fn print_hits(&self, out: &mut impl Write, tree: &Tree, hits: &[usize]) {
		let items: Vec<_> = hits
			.iter()
			.map(|&i| {
				let n = &tree.nodes[i];
				let score = n.content_score.unwrap_or(0.0);
				(score, self.hit_leaf(&n.rel, score, n.name_score, &n.heat, n.lines_seen))
			})
			.collect();
		self.print_ranked(out, &items);
	}

	/// Also put the strongest of the selected near misses last.
	pub fn print_scored(&self, out: &mut impl Write, items: &[(String, f64)]) {
		let leaves: Vec<_> = items
			.iter()
			.map(|(rel, p)| {
				(*p, Leaf {
					rel:    rel.clone(),
					header: format!("  {}", self.p(*p)),
					body:   Vec::new(),
				})
			})
			.collect();
		self.print_ranked(out, &leaves);
	}

	/// Group consecutive directory runs only. A tree regrouping would move a
	/// high-scoring sibling ahead of lower-scoring files in another directory.
	fn print_ranked(&self, out: &mut impl Write, items: &[(f64, Leaf)]) {
		let mut ordered: Vec<_> = items.iter().collect();
		ordered.sort_by(|(a, la), (b, lb)| a.total_cmp(b).then(la.rel.cmp(&lb.rel)));
		let mut current = None;
		for (_, leaf) in ordered {
			let (dir, name) = split_rel(&leaf.rel);
			if current != Some(dir) {
				if current.is_some() {
					let _ = writeln!(out);
				}
				let _ =
					writeln!(out, " {}", self.bold(&self.blue(if dir.is_empty() { "./" } else { dir })));
				current = Some(dir);
			}
			let _ = writeln!(out, "   {name}{}", leaf.header);
			for line in &leaf.body {
				let _ = writeln!(out, "     {line}");
			}
		}
	}

	fn state_tag(&self, n: &crate::tree::Node) -> String {
		match n.state {
			State::Unk => self.dim("[unk]"),
			State::Pending => self.dim("[pend]"),
			State::Exp => self.green("[exp]"),
			State::Fin(r) => self.dim(&format!("[fin{r}]")),
			State::Reading => self.yellow("[read]"),
			State::Hit => self.green(&self.bold("[HIT]")),
			State::Skip => self.dim("[skip]"),
		}
	}

	fn scores_of(&self, n: &crate::tree::Node) -> String {
		let mut s = String::new();
		if let Some(p) = n.name_score {
			s.push_str(&format!(" name {p:.2}"));
		}
		if let Some(c) = n.content_score {
			s.push_str(&format!(" content {c:.2}"));
		}
		self.dim(&s)
	}

	/// The explored tree in pi's grouped style: expanded folders become headers
	/// (annotated with their state), collapsed folders and files are leaves.
	pub fn print_tree(&self, out: &mut impl Write, tree: &Tree) {
		let mut leaves: Vec<Leaf> = Vec::new();
		let mut dir_ann: std::collections::HashMap<String, String> = std::collections::HashMap::new();
		for (i, n) in tree.nodes.iter().enumerate() {
			if i == 0 || (n.state == State::Skip && !self.verbose) {
				continue;
			}
			// hide everything under a collapsed / unexpanded folder
			let mut anc = n.parent;
			let mut hidden = false;
			while let Some(a) = anc {
				if a != 0 && tree.nodes[a].state != State::Exp {
					hidden = true;
					break;
				}
				anc = tree.nodes[a].parent;
			}
			if hidden {
				continue;
			}
			let ann = format!(" {}{}", self.state_tag(n), self.scores_of(n));
			if n.kind == Kind::Dir && n.state == State::Exp {
				dir_ann.insert(n.rel.clone(), ann);
			} else {
				let extra = if n.kind == Kind::Dir && n.peek_count > 0 {
					self.dim(&format!(" ({} entries)", n.peek_count))
				} else {
					String::new()
				};
				leaves.push(Leaf {
					rel:    n.rel.clone(),
					header: format!("{ann}{extra}"),
					body:   Vec::new(),
				});
			}
		}
		let _ = writeln!(
			out,
			"{}/ {}",
			self.bold(&self.blue(&tree.name())),
			self.state_tag(&tree.nodes[0])
		);
		let style = |chain: &str, full: &str| -> String {
			let ann = dir_ann.get(full).cloned().unwrap_or_default();
			format!("{}{}", self.bold(&self.blue(chain)), ann)
		};
		for l in grouped::render(&leaves, "  ", &style) {
			let _ = writeln!(out, "{l}");
		}
	}
}

/// Fit a path into `w` columns, eliding the front.
pub fn fit(s: &str, w: usize) -> String {
	let n = s.chars().count();
	if n <= w {
		return s.to_string();
	}
	let tail: String = s.chars().skip(n - (w - 1)).collect();
	format!("…{tail}")
}

#[cfg(test)]
mod tests {
	use super::*;

	fn plain_ui() -> Ui {
		let mut ui = Ui::new(UiOptions::default());
		ui.color = false;
		ui
	}

	fn passage(start: usize, end: usize, p: f64) -> HeatRange {
		HeatRange { start, end, p, snippet: "//! Opening comment".into() }
	}

	#[test]
	fn whole_file_hits_are_one_row_without_redundant_localization() {
		let ui = plain_ui();
		let heat = vec![passage(1, 854, 0.97)];
		let leaf =
			ui.hit_leaf("src/modes/hashline/parser.rs", 0.97, Some(0.71), &heat, Some((854, false)));
		let mut out = Vec::new();
		ui.print_ranked(&mut out, &[(0.97, leaf)]);
		assert_eq!(
			String::from_utf8(out).unwrap(),
			" src/modes/hashline/\n   parser.rs  0.97 · whole file · 854 lines\n"
		);
		let mut verbose = plain_ui();
		verbose.verbose = true;
		assert!(
			verbose
				.hit_leaf("parser.rs", 0.97, Some(0.71), &heat, Some((854, false)))
				.header
				.contains("name 0.71")
		);
	}

	#[test]
	fn partial_and_truncated_reads_keep_coordinates_without_a_single_heat_block() {
		let ui = plain_ui();
		for (heat, lines) in [
			(vec![passage(1, 854, 0.97)], Some((854, true))),
			(vec![passage(20, 40, 0.97)], Some((854, false))),
			(vec![passage(1, 854, 0.97)], None),
		] {
			let leaf = ui.hit_leaf("parser.rs", 0.97, None, &heat, lines);
			assert!(!leaf.header.contains("whole file"));
			assert_eq!(leaf.body.len(), 1);
			assert!(leaf.body[0].contains(&format!("L{}–{}", heat[0].start, heat[0].end)));
			assert!(!leaf.body[0].contains('█'));
		}
	}

	#[test]
	fn global_ranking_survives_interleaved_directories_and_breaks_ties_by_path() {
		let ui = plain_ui();
		let mut out = Vec::new();
		ui.print_scored(&mut out, &[
			("a/high.rs".into(), 0.97),
			("b/middle.rs".into(), 0.75),
			("a/low.rs".into(), 0.59),
			("b/also-middle.rs".into(), 0.75),
		]);
		let text = String::from_utf8(out).unwrap();
		let names: Vec<_> = text
			.lines()
			.filter(|l| l.starts_with("   "))
			.map(|l| l.split_whitespace().next().unwrap())
			.collect();
		assert_eq!(names, ["low.rs", "also-middle.rs", "middle.rs", "high.rs"]);
		assert_eq!(text.matches(" a/\n").count(), 2);
	}

	#[test]
	fn localized_results_keep_only_the_best_three_and_show_the_strongest_last() {
		let ui = plain_ui();
		let heat = vec![
			passage(1, 10, 0.8),
			passage(11, 20, 0.1),
			passage(21, 30, 0.6),
			passage(31, 40, 0.9),
		];
		let body = ui.hit_body(&heat, Some((50, false)));
		assert_eq!(body.len(), 4);
		assert!(body[1].contains("L21–30"));
		assert!(body[2].contains("L1–10"));
		assert!(body[3].contains("L31–40"));
	}
}
