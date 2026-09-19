//! Builds the request shapes jegrep sends to Jev, as composable pieces so that
//! strategies can mix them.
//!
//! 1. Directory batch: state = a listing of up to N entries; one Noul per entry
//!    ("is this path likely to hold what the search wants?"). Nouls give
//!    absolute probabilities, so entries can be thresholded independently and
//!    batches are comparable with each other.
//! 2. File check: state = the file's first K bytes with `L0001|` line tags; one
//!    Noul ("does the content actually contain it?") plus one Choice over line
//!    ranges ("where?") whose distribution is the heatmap.

use std::{
	collections::{BTreeMap, HashMap},
	fmt::Write as _,
	fs,
	io::Read,
	path::Path,
};

use serde_json::{Map, Value, json};

use crate::{
	grouped::{self, Leaf},
	jev::{NoulCriteria, Question, Response},
	tree::{HeatRange, Kind, Tree, human_size},
};

pub const TASK: &str = "Semantic grep over a source tree: locate files whose content matches the \
                        search description. Entries are judged by name, size, position in the \
                        tree, and (for folders) a sample of what they contain.";

pub const TREE_FORMAT: &str = "`tree` is a directory listing. Lines starting with # are headers: \
                               `# dir/` is a folder; more #s means deeper nesting under the \
                               header above; a header may fold several levels (`# a/b/c/`). Every \
                               judgeable entry carries a tag like e017 right after the #s: files \
                               as `e017 name (size)`, folders as `e017 name/ — N entries: sample \
                               of names`. Untagged header lines are only structure.";

// ── directory batch ─────────────────────────────────────────────────────────

pub fn entry_key(i: usize) -> String {
	format!("e{i:03}")
}

pub fn file_criteria() -> NoulCriteria {
	NoulCriteria {
		yes: "A file at this path plausibly contains code, text, or data matching the search.".into(),
		no:  "Unrelated by name and location, or a kind of file (generated output, lockfile, asset, \
		      boilerplate) that would not hold it."
			.into(),
	}
}

pub fn dir_criteria() -> NoulCriteria {
	NoulCriteria {
		yes: "The folder plausibly contains, at any depth, at least one file matching the search."
			.into(),
		no:  "Nothing about the folder's name, location, or sampled contents suggests it holds a \
		      match."
			.into(),
	}
}

/// Detail shown after an entry's name: size for files, count + sample for
/// folders.
pub fn entry_detail(tree: &Tree, idx: usize) -> String {
	let n = &tree.nodes[idx];
	match n.kind {
		Kind::File => format!(" ({})", human_size(n.size)),
		Kind::Dir => format!(" — {} entries: {}", n.peek_count, n.peek),
	}
}

/// Render the frontier entries as pi's model-facing grouped tree, each
/// judgeable entry tagged `eNNN` (the question key). Returns the text and, per
/// entry, its tag.
pub fn render_tree(tree: &Tree, entries: &[usize]) -> String {
	let leaves: Vec<Leaf> = entries
		.iter()
		.enumerate()
		.map(|(i, &idx)| Leaf {
			rel:    tree.nodes[idx].rel.clone(),
			header: entry_key(i),
			body:   Vec::new(),
		})
		.collect();
	let detail: HashMap<&str, String> = entries
		.iter()
		.map(|&idx| (tree.nodes[idx].rel.as_str(), entry_detail(tree, idx)))
		.collect();
	grouped::render_model(&leaves, &|leaf, name| {
		format!("{} {name}{}", leaf.header, detail.get(leaf.rel.as_str()).map_or("", String::as_str))
	})
}

/// How per-entry questions are phrased. Selected with `JEGREP_QFMT` for
/// benchmarking: `full` repeats the yes/no criteria in every question and
/// inlines the query; `lean` states the criteria once in the state and inlines
/// the query; `leanref` states the criteria once and refers to `search` in the
/// state.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum QFmt {
	Full,
	Lean,
	LeanRef,
}

impl QFmt {
	pub fn from_env() -> Self {
		match std::env::var("JEGREP_QFMT").as_deref() {
			Ok("full") => Self::Full,
			Ok("leanref") => Self::LeanRef,
			_ => Self::Lean,
		}
	}
}

/// One Noul per entry over a shared tree-rendered listing state.
pub fn dir_batch(
	tree: &Tree,
	query: &str,
	entries: &[usize],
) -> (Value, BTreeMap<String, Question>) {
	dir_batch_fmt(tree, query, entries, QFmt::from_env())
}

pub fn dir_batch_fmt(
	tree: &Tree,
	query: &str,
	entries: &[usize],
	fmt: QFmt,
) -> (Value, BTreeMap<String, Question>) {
	let fc = file_criteria();
	let dc = dir_criteria();
	let mut questions = BTreeMap::new();
	for (i, &idx) in entries.iter().enumerate() {
		let n = &tree.nodes[idx];
		let key = entry_key(i);
		let (instructions, criteria) = match (fmt, n.kind) {
			(QFmt::Full, Kind::File) => (
				format!(
					"Judging by its name, size, and where it sits in `tree`, is the file tagged {key} \
					 (\"{}\") likely to contain what this search is looking for: \"{query}\"?",
					n.name()
				),
				Some(fc.clone()),
			),
			(QFmt::Full, Kind::Dir) => (
				format!(
					"Judging by its name, its place in `tree`, and the sample of its contents, does \
					 the folder tagged {key} (\"{}/\") contain, at any depth, at least one file \
					 relevant to this search: \"{query}\"?",
					n.name()
				),
				Some(dc.clone()),
			),
			(QFmt::Lean, Kind::File) => (
				format!(
					"Is the file tagged {key} (\"{}\") likely to contain what this search is looking \
					 for: \"{query}\"? Judge by its name, size, and place in `tree`; apply \
					 `criteria.file`.",
					n.name()
				),
				None,
			),
			(QFmt::Lean, Kind::Dir) => (
				format!(
					"Does the folder tagged {key} (\"{}/\") contain, at any depth, at least one file \
					 relevant to this search: \"{query}\"? Judge by its name, place in `tree`, and \
					 sampled contents; apply `criteria.folder`.",
					n.name()
				),
				None,
			),
			(QFmt::LeanRef, Kind::File) => (
				format!(
					"Is the file tagged {key} (\"{}\") likely to contain what `search` describes? \
					 Judge by its name, size, and place in `tree`; apply `criteria.file`.",
					n.name()
				),
				None,
			),
			(QFmt::LeanRef, Kind::Dir) => (
				format!(
					"Does the folder tagged {key} (\"{}/\") contain, at any depth, at least one file \
					 relevant to `search`? Judge by its name, place in `tree`, and sampled contents; \
					 apply `criteria.folder`.",
					n.name()
				),
				None,
			),
		};
		questions.insert(key, Question::Noul { instructions: Value::String(instructions), criteria });
	}
	let mut state = json!({
		 "task": TASK,
		 "search": query,
		 "project": tree.name(),
		 "format": TREE_FORMAT,
		 "tree": render_tree(tree, entries),
	});
	if fmt != QFmt::Full {
		state["criteria"] = json!({
			 "file": { "yes": fc.yes, "no": fc.no },
			 "folder": { "yes": dc.yes, "no": dc.no },
		});
	}
	(state, questions)
}

pub fn folder_key(i: usize) -> String {
	format!("f{i:03}")
}

/// Cheap "look before you leap": judge several folders at once from their FULL
/// child-name lists (capped at `max_names` each), one Noul per folder. Far
/// cheaper than listing every child as its own entry.
pub fn dir_peek_batch(
	tree: &Tree,
	query: &str,
	dirs: &[usize],
	max_names: usize,
) -> (Value, BTreeMap<String, Question>) {
	let dc = dir_criteria();
	let mut folders = Map::new();
	let mut questions = BTreeMap::new();
	for (i, &idx) in dirs.iter().enumerate() {
		let n = &tree.nodes[idx];
		let key = folder_key(i);
		let mut names = tree.list_names(idx);
		let total = names.len();
		if names.len() > max_names {
			names.truncate(max_names);
			names.push(format!("… +{} more", total - max_names));
		}
		folders.insert(key.clone(), json!({ "path": n.rel, "entries": names }));
		questions.insert(key.clone(), Question::Noul {
			instructions: Value::String(format!(
				"Given the full list of entries in folder `folders.{key}` (\"{}\"), does it contain, \
				 at any depth, at least one file relevant to this search: \"{query}\"?",
				n.rel
			)),
			criteria:     Some(dc.clone()),
		});
	}
	let state = json!({
		 "task": TASK,
		 "search": query,
		 "project": tree.name(),
		 "folders": Value::Object(folders),
	});
	(state, questions)
}

// ── file reading ────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub enum FileErr {
	Binary,
	Empty,
	Io(String),
	Api(String),
}

impl std::fmt::Display for FileErr {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			Self::Binary => write!(f, "binary"),
			Self::Empty => write!(f, "empty"),
			Self::Io(e) => write!(f, "io: {e}"),
			Self::Api(e) => write!(f, "api: {e}"),
		}
	}
}

pub struct ReadText {
	pub text:      String,
	/// Bytes actually used (after trimming to a line boundary).
	pub bytes:     usize,
	pub truncated: bool,
}

/// Read up to `max_bytes` of a text file. Rejects binaries (NUL in the first 8
/// KB) and blank files; trims a truncated read back to the last full line.
pub fn read_text(path: &Path, max_bytes: usize) -> Result<ReadText, FileErr> {
	let mut f = fs::File::open(path).map_err(|e| FileErr::Io(e.to_string()))?;
	let mut buf = vec![0u8; max_bytes + 1];
	let mut read = 0;
	loop {
		let n = f
			.read(&mut buf[read..])
			.map_err(|e| FileErr::Io(e.to_string()))?;
		if n == 0 {
			break;
		}
		read += n;
		if read == buf.len() {
			break;
		}
	}
	buf.truncate(read);
	if buf[..buf.len().min(8192)].contains(&0) {
		return Err(FileErr::Binary);
	}
	let truncated = read > max_bytes;
	if truncated {
		buf.truncate(max_bytes);
		if let Some(p) = buf.iter().rposition(|&b| b == b'\n') {
			buf.truncate(p + 1);
		}
	}
	let text = String::from_utf8_lossy(&buf).into_owned();
	if text.lines().all(|l| l.trim().is_empty()) {
		return Err(FileErr::Empty);
	}
	Ok(ReadText { bytes: buf.len(), text, truncated })
}

// ── content check pieces ────────────────────────────────────────────────────

pub fn range_key(k: usize) -> String {
	format!("R{k:02}")
}

/// Prefix every line with `L0001| `.
pub fn tag_lines(lines: &[&str]) -> String {
	let mut tagged = String::with_capacity(lines.iter().map(|l| l.len() + 8).sum());
	for (i, line) in lines.iter().enumerate() {
		let _ = writeln!(tagged, "L{:04}| {}", i + 1, line);
	}
	tagged
}

/// Split `lines` into at most `n_ranges` contiguous ranges. Returns
/// `(start, end, snippet)` per range (1-based inclusive) and the matching
/// Choice criteria.
pub fn line_ranges(
	lines: &[&str],
	n_ranges: usize,
) -> (Vec<(usize, usize, String)>, BTreeMap<String, Value>) {
	let r = n_ranges.clamp(1, 255).min(lines.len().max(1));
	let per = lines.len().div_ceil(r).max(1);
	let mut ranges = Vec::new();
	let mut criteria = BTreeMap::new();
	let mut start = 0;
	while start < lines.len() {
		let end = (start + per).min(lines.len());
		let snippet: String = lines[start..end]
			.iter()
			.map(|l| l.trim())
			.find(|l| !l.is_empty())
			.unwrap_or("")
			.chars()
			.take(80)
			.collect();
		criteria
			.insert(range_key(ranges.len()), Value::String(format!("lines {}-{}", start + 1, end)));
		ranges.push((start + 1, end, snippet));
		start = end;
	}
	(ranges, criteria)
}

pub fn relevant_noul(rel: &str, query: &str) -> Question {
	Question::Noul {
		instructions: Value::String(format!(
			"Does the content of \"{rel}\" (in `content`) actually contain what this search is \
			 looking for: \"{query}\"?"
		)),
		criteria:     Some(NoulCriteria {
			yes: "The file contains code, text, or data that directly matches, implements, defines, \
			      or documents what the search describes."
				.into(),
			no:  "The file is unrelated, or only shares surface keywords with the search without \
			      containing the thing itself."
				.into(),
		}),
	}
}

pub fn where_choice(query: &str, criteria: BTreeMap<String, Value>) -> Question {
	Question::Choice {
		instructions: Value::String(format!(
			"Which range of lines in `content` (every line is prefixed with its number, e.g. L0042|) \
			 best matches this search: \"{query}\"? Prefer the range where it is implemented or \
			 defined over ranges that merely import or reference it."
		)),
		criteria,
	}
}

pub fn file_state(query: &str, rel: &str, note: &str, tagged: &str) -> Value {
	json!({
		 "task": "Semantic grep: decide whether this file's content contains what the search describes, and where.",
		 "search": query,
		 "file": rel,
		 "note": note,
		 "content": tagged,
	})
}

/// Turn a `where` Choice answer back into ranges with probabilities.
pub fn heat_from(
	resp: &Response,
	key: &str,
	ranges: &[(usize, usize, String)],
) -> (Vec<HeatRange>, Option<f64>) {
	match resp.choice(key) {
		Some((probs, conf)) => (
			ranges
				.iter()
				.enumerate()
				.map(|(k, (start, end, snippet))| HeatRange {
					start:   *start,
					end:     *end,
					p:       probs.get(&range_key(k)).copied().unwrap_or(0.0),
					snippet: snippet.clone(),
				})
				.collect(),
			Some(conf),
		),
		None => (Vec::new(), None),
	}
}

pub struct FilePrep {
	pub state:       Value,
	pub questions:   BTreeMap<String, Question>,
	/// (`start_line`, `end_line`, snippet) per range, in document order; keys
	/// are R00..
	pub ranges:      Vec<(usize, usize, String)>,
	pub total_lines: usize,
	pub bytes_used:  usize,
	pub truncated:   bool,
}

/// The standard content check: first `max_bytes`, relevance Noul + heatmap
/// Choice.
pub fn prepare_file(
	path: &Path,
	rel: &str,
	size: u64,
	query: &str,
	max_bytes: usize,
	n_ranges: usize,
) -> Result<FilePrep, FileErr> {
	let rt = read_text(path, max_bytes)?;
	let lines: Vec<&str> = rt.text.lines().collect();
	let tagged = tag_lines(&lines);
	let (ranges, criteria) = line_ranges(&lines, n_ranges);
	let note = if rt.truncated {
		format!("only the first {} of {} bytes are shown", rt.bytes, size)
	} else {
		"complete file".to_string()
	};
	let state = file_state(query, rel, &note, &tagged);
	let mut questions = BTreeMap::new();
	questions.insert("relevant".to_string(), relevant_noul(rel, query));
	if ranges.len() >= 2 {
		questions.insert("where".to_string(), where_choice(query, criteria));
	}
	Ok(FilePrep {
		state,
		questions,
		ranges,
		total_lines: lines.len(),
		bytes_used: rt.bytes,
		truncated: rt.truncated,
	})
}

// ── sniff (opening-lines triage) ────────────────────────────────────────────

pub fn sniff_key(i: usize) -> String {
	format!("f{i:02}")
}

/// First `max_lines` lines of a file head, long lines clipped, for a sniff
/// batch.
pub fn head_excerpt(text: &str, max_lines: usize) -> String {
	let mut out = String::new();
	for line in text.lines().take(max_lines) {
		let clipped: String = line.chars().take(200).collect();
		out.push_str(clipped.trim_end());
		out.push('\n');
	}
	out
}

/// A structural skim instead of the literal opening: the leading comment block,
/// then column-0 declarations (exports, functions, classes, types, headings…)
/// in file order, with a few imports to fill, capped at `max_lines` /
/// `max_bytes`. Same token budget as a head excerpt, but samples the whole
/// scanned text.
pub fn skeleton_excerpt(text: &str, max_lines: usize, max_bytes: usize) -> String {
	const DECL: &[&str] = &[
		"export ",
		"pub ",
		"fn ",
		"class ",
		"function ",
		"async ",
		"def ",
		"interface ",
		"type ",
		"const ",
		"struct ",
		"enum ",
		"impl ",
		"trait ",
		"func ",
		"public ",
		"private ",
		"protected ",
		"static ",
		"let ",
		"var ",
		"module ",
		"namespace ",
		"abstract ",
		"declare ",
		"#",
		"@",
		"mod ",
		"macro_rules!",
	];
	const IMPORT: &[&str] =
		&["import ", "use ", "from ", "require(", "const {", "#include", "using "];
	let lines: Vec<&str> = text.lines().collect();
	let mut picked: Vec<usize> = Vec::new();
	// 1. leading comment block
	for (i, l) in lines.iter().enumerate().take(12) {
		let t = l.trim_start();
		if t.is_empty() {
			continue;
		}
		if t.starts_with("/*")
			|| t.starts_with('*')
			|| t.starts_with("//")
			|| t.starts_with('#') && !t.starts_with("#[")
			|| t.starts_with("\"\"\"")
			|| t.starts_with("--")
		{
			picked.push(i);
		} else {
			break;
		}
	}
	// 2. column-0 declarations, in order
	let mut imports: Vec<usize> = Vec::new();
	for (i, l) in lines.iter().enumerate() {
		if picked.contains(&i) || l.is_empty() {
			continue;
		}
		let first = l.as_bytes()[0];
		if first == b' ' || first == b'\t' || first == b'}' || first == b')' || first == b']' {
			continue;
		}
		if IMPORT.iter().any(|p| l.starts_with(p)) {
			imports.push(i);
			continue;
		}
		if DECL.iter().any(|p| l.starts_with(p)) || first.is_ascii_alphabetic() {
			picked.push(i);
		}
	}
	// 3. a few imports to fill remaining room
	for i in imports.into_iter().take(6) {
		if picked.len() >= max_lines {
			break;
		}
		picked.push(i);
	}
	picked.sort_unstable();
	picked.dedup();
	let mut out = String::new();
	let mut n = 0;
	for i in picked {
		if n >= max_lines {
			break;
		}
		let clipped: String = lines[i].trim_end().chars().take(160).collect();
		if out.len() + clipped.len() + 1 > max_bytes {
			break;
		}
		out.push_str(&clipped);
		out.push('\n');
		n += 1;
	}
	if out.is_empty() {
		head_excerpt(text, max_lines)
	} else {
		out
	}
}

/// Many file heads in one request, one Noul per file: does the opening indicate
/// the file contains what the search describes?
pub fn sniff_batch(
	query: &str,
	files: &[(String, String, String)],
) -> (Value, BTreeMap<String, Question>) {
	let criteria = NoulCriteria {
		yes: "The imports, header comment, or first definitions show the file implements, defines, \
		      or documents what the search describes, or strongly indicate the rest of the file \
		      does."
			.into(),
		no:  "The opening shows an unrelated purpose, or shares only surface keywords with the \
		      search."
			.into(),
	};
	let mut listing = Map::new();
	let mut questions = BTreeMap::new();
	for (i, (rel, size, head)) in files.iter().enumerate() {
		let key = sniff_key(i);
		listing.insert(key.clone(), json!({ "path": rel, "size": size, "head": head }));
		questions.insert(key.clone(), Question::Noul {
			instructions: Value::String(format!(
				"Judging from its path and opening lines (`files.{key}.head`), does the file \
				 \"{rel}\" likely contain what this search is looking for: \"{query}\"?"
			)),
			criteria:     Some(criteria.clone()),
		});
	}
	let state = json!({
		 "task": "Semantic grep triage: each entry is the opening of a source file. Decide from the opening whether the file likely contains what the search describes.",
		 "search": query,
		 "files": Value::Object(listing),
	});
	(state, questions)
}
