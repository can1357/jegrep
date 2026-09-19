//! Keyword grep index — a port of the core of [pi's native grep] on top of the
//! vendored [`crate::walker`] module. Kept: matcher construction (grep-regex,
//! case-insensitive, line-terminated),
//! searcher config (NUL binary detection), the candidate walk (files only, gitignore,
//! skip .`git/node_modules`, never follow links, minimal detail), and per-thread
//! searcher reuse under rayon. Dropped: the napi surface, output modes, context
//! lines, PCRE2 fallback, match collection — jegrep only needs counts per file.
//!
//! [pi's native grep]: https://github.com/can1357/oh-my-pi/blob/main/crates/pi-natives/src/grep.rs

use crate::walker;
use grep_regex::{RegexMatcher, RegexMatcherBuilder};
use grep_searcher::{BinaryDetection, Searcher, SearcherBuilder, Sink, SinkMatch};
use rayon::prelude::*;
use std::{
    cell::RefCell,
    collections::HashMap,
    io,
    path::Path,
    time::{Duration, Instant},
};

/// Same cap as pi: files larger than this are skipped rather than searched.
const MAX_FILE_BYTES: u64 = 4 * 1024 * 1024;

pub struct GrepIndex {
    pub keywords: Vec<String>,
    /// rel file path -> matching lines.
    pub per_file: HashMap<String, u32>,
    /// rel file path -> per-keyword occurrence counts (aligned with `keywords`).
    pub per_file_kw: HashMap<String, Vec<u32>>,
    /// rel dir path (trailing `/`; "" = root) -> (matching lines in descendants, files with matches).
    pub per_dir: HashMap<String, (u32, u32)>,
    pub files_scanned: usize,
    pub elapsed: Duration,
}

impl GrepIndex {
    pub fn file_hits(&self, rel: &str) -> u32 {
        self.per_file.get(rel).copied().unwrap_or(0)
    }
    pub fn dir_hits(&self, rel: &str) -> (u32, u32) {
        self.per_dir.get(rel).copied().unwrap_or((0, 0))
    }
    /// Entry hits regardless of kind (dirs end with `/`).
    pub fn hits(&self, rel: &str) -> u32 {
        if rel.ends_with('/') {
            self.dir_hits(rel).0
        } else {
            self.file_hits(rel)
        }
    }
    /// `timeout×5, kill×2` for a file, strongest first.
    pub fn breakdown(&self, rel: &str) -> String {
        let Some(counts) = self.per_file_kw.get(rel) else {
            return String::new();
        };
        let mut pairs: Vec<(&str, u32)> = self
            .keywords
            .iter()
            .map(String::as_str)
            .zip(counts.iter().copied())
            .filter(|(_, c)| *c > 0)
            .collect();
        pairs.sort_by(|a, b| b.1.cmp(&a.1));
        pairs
            .iter()
            .take(4)
            .map(|(k, c)| format!("{k}×{c}"))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

const STOPWORDS: &[&str] = &[
    "the",
    "a",
    "an",
    "or",
    "and",
    "to",
    "is",
    "are",
    "be",
    "when",
    "where",
    "how",
    "that",
    "this",
    "of",
    "in",
    "on",
    "at",
    "for",
    "with",
    "by",
    "its",
    "it",
    "as",
    "from",
    "into",
    "like",
    "gets",
    "get",
    "up",
    "which",
    "what",
    "does",
    "do",
    "code",
    "file",
    "files",
    "over",
    "all",
    "user",
    "using",
    "then",
    "than",
    "there",
    "their",
    "they",
    "them",
    "you",
    "your",
    "we",
    "our",
    "has",
    "have",
    "had",
    "was",
    "were",
    "been",
    "being",
    "will",
    "would",
    "should",
    "can",
    "could",
    "not",
    "but",
    "if",
    "so",
    "such",
    "via",
    "per",
    "any",
    "some",
    "each",
    "every",
    "also",
    "just",
    "only",
    "more",
    "most",
    "other",
    "out",
    "off",
    "about",
    "after",
    "before",
    "between",
    "through",
    "during",
    "without",
    "within",
    "one",
    "two",
    "new",
    "used",
    "use",
    "make",
    "makes",
    "made",
    "run",
    "runs",
    "way",
    "thing",
    "things",
    "something",
    "actually",
    "really",
    "still",
    "yet",
];

/// Cheap stem so a substring match covers inflections: spawned→spawn, compacted→compact.
fn stem(t: &str) -> String {
    for suf in ["ing", "ed", "es", "s"] {
        if let Some(base) = t.strip_suffix(suf)
            && base.len() >= 4 {
                return base.to_string();
            }
    }
    t.to_string()
}

/// Keywords for the grep prior: quoted phrases whole, then tokens minus stopwords.
pub fn keywords_from_query(q: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut rest = String::new();
    let mut chars = q.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '"' || c == '\'' {
            let mut phrase = String::new();
            let mut closed = false;
            for d in chars.by_ref() {
                if d == c {
                    closed = true;
                    break;
                }
                phrase.push(d);
            }
            let phrase = phrase.trim().to_lowercase();
            if closed && phrase.len() >= 3 {
                out.push(phrase);
                rest.push(' ');
                continue;
            }
            rest.push_str(&phrase);
            rest.push(' ');
        } else {
            rest.push(c);
        }
    }
    for tok in rest.split(|c: char| !c.is_alphanumeric() && c != '_') {
        let t = tok.to_lowercase();
        if t.len() < 3 || STOPWORDS.contains(&t.as_str()) || t.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let s = stem(&t);
        if !out.contains(&s) {
            out.push(s);
        }
    }
    out
}

fn escape_regex(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        if r"\.+*?()|[]{}^$#&-~".contains(c) {
            o.push('\\');
        }
        o.push(c);
    }
    o
}

/// pi's `build_regex_matcher`: case-insensitive, line-terminated when possible.
fn build_matcher(keywords: &[String]) -> Result<RegexMatcher, grep_regex::Error> {
    let pattern = keywords
        .iter()
        .map(|k| escape_regex(k))
        .collect::<Vec<_>>()
        .join("|");
    let build = |line_terminated: bool| {
        let mut b = RegexMatcherBuilder::new();
        b.case_insensitive(true).multi_line(false);
        if line_terminated {
            b.line_terminator(Some(b'\n'));
        }
        b.build(&pattern)
    };
    build(true).or_else(|_| build(false))
}

/// pi's `build_searcher` with no context and no line numbers.
fn build_searcher() -> Searcher {
    SearcherBuilder::new()
        .binary_detection(BinaryDetection::quit(b'\x00'))
        .line_number(false)
        .multi_line(false)
        .build()
}

struct CountSink<'a> {
    lower_kws: &'a [Vec<u8>],
    per_kw: Vec<u32>,
    lines: u32,
}

impl Sink for CountSink<'_> {
    type Error = io::Error;
    fn matched(&mut self, _: &Searcher, m: &SinkMatch<'_>) -> Result<bool, io::Error> {
        self.lines += 1;
        let line = m.bytes().to_ascii_lowercase();
        for (i, kw) in self.lower_kws.iter().enumerate() {
            self.per_kw[i] += count_occurrences(&line, kw);
        }
        Ok(true)
    }
}

fn count_occurrences(hay: &[u8], needle: &[u8]) -> u32 {
    if needle.is_empty() || hay.len() < needle.len() {
        return 0;
    }
    let mut n = 0;
    let mut i = 0;
    while i + needle.len() <= hay.len() {
        if &hay[i..i + needle.len()] == needle {
            n += 1;
            i += needle.len();
        } else {
            i += 1;
        }
    }
    n
}

thread_local! {
    static SEARCHER: RefCell<Option<Searcher>> = const { RefCell::new(None) };
}

fn with_searcher<T>(f: impl FnOnce(&mut Searcher) -> T) -> T {
    SEARCHER.with(|cell| {
        let mut s = cell.borrow_mut();
        let s = s.get_or_insert_with(build_searcher);
        f(s)
    })
}

/// pi's `build_grep_walk_request` / `collect_grep_candidates`.
fn collect_candidates(root: &Path, hidden: bool) -> io::Result<Vec<walker::FileCandidate>> {
    let request = walker::WalkRequest::new(root)
        .hidden(hidden)
        .gitignore(true)
        .skip_git(true)
        .skip_node_modules(true)
        .follow_links(walker::FollowLinks::Never)
        .detail(walker::WalkDetail::Minimal)
        .size_hints(walker::SizeHintPolicy::WhenCheap)
        .order(walker::WalkOrder::Unordered)
        .emit_root(false)
        .depth(1, usize::MAX)
        .directory_errors(walker::DirectoryErrorMode::SkipSkippable)
        .cache(false)
        .filter(walker::WalkFilter::files_only());
    request
        .collect_file_candidates()
        .map_err(|e| io::Error::other(e.to_string()))
}

/// Count keyword matches in every file under `root` (case-insensitive, any keyword),
/// and roll the counts up to every ancestor directory.
pub fn grep_index(root: &Path, keywords: &[String], hidden: bool) -> io::Result<GrepIndex> {
    grep_index_observed(root, keywords, hidden, None)
}

/// Report successful local reads as they happen without changing search results.
pub fn grep_index_observed(
    root: &Path,
    keywords: &[String],
    hidden: bool,
    observer: Option<&(dyn Fn(&str) + Send + Sync)>,
) -> io::Result<GrepIndex> {
    let t = Instant::now();
    let keywords: Vec<String> = keywords
        .iter()
        .map(|k| k.to_lowercase())
        .filter(|k| !k.is_empty())
        .collect();
    let mut index = GrepIndex {
        keywords: keywords.clone(),
        per_file: HashMap::new(),
        per_file_kw: HashMap::new(),
        per_dir: HashMap::new(),
        files_scanned: 0,
        elapsed: Duration::ZERO,
    };
    if keywords.is_empty() {
        return Ok(index);
    }
    let matcher = build_matcher(&keywords).map_err(|e| io::Error::other(e.to_string()))?;
    let lower: Vec<Vec<u8>> = keywords.iter().map(|k| k.as_bytes().to_vec()).collect();
    let candidates = collect_candidates(root, hidden)?;
    index.files_scanned = candidates.len();

    let results: Vec<(String, u32, Vec<u32>)> = candidates
        .par_iter()
        .filter_map(|c| {
            let size = match c.size {
                Some(s) => s as u64,
                None => std::fs::metadata(&c.path).ok()?.len(),
            };
            if size == 0 || size > MAX_FILE_BYTES {
                return None;
            }
            let mut sink = CountSink {
                lower_kws: &lower,
                per_kw: vec![0; lower.len()],
                lines: 0,
            };
            with_searcher(|s| s.search_path(&matcher, &c.path, &mut sink)).ok()?;
            if let Some(observer) = observer {
                observer(&c.relative);
            }
            if sink.lines == 0 {
                return None;
            }
            Some((c.relative.clone(), sink.lines, sink.per_kw))
        })
        .collect();

    for (rel, lines, per_kw) in results {
        // roll up to every ancestor: "a/b/c.ts" -> "a/b/", "a/", ""
        let mut end = rel.len();
        loop {
            let dir = match rel[..end].rfind('/') {
                Some(p) => {
                    end = p;
                    &rel[..=p]
                }
                None => "",
            };
            let e = index.per_dir.entry(dir.to_string()).or_insert((0, 0));
            e.0 += lines;
            e.1 += 1;
            if dir.is_empty() {
                break;
            }
        }
        index.per_file.insert(rel.clone(), lines);
        index.per_file_kw.insert(rel, per_kw);
    }
    index.elapsed = t.elapsed();
    Ok(index)
}
