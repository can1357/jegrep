//! The explored directory tree. Nodes are created lazily as folders are
//! expanded.
//!
//! State machine per node:
//!   Unk ──(dir batch)──▶ Pending ──▶ Exp (dir, hot)  | Fin(round) (cold)
//!                                 ──▶ Reading (file, hot) ──▶ Hit | Fin(round)
//!   Fin(r) can reopen in a later round when the cached score clears the new
//! threshold.

use std::{
	fs, io,
	path::{Path, PathBuf},
	sync::Arc,
};

use ignore::gitignore::{Gitignore, GitignoreBuilder};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
	Dir,
	File,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum State {
	/// Listed, not yet judged.
	Unk,
	/// In an in-flight directory batch.
	Pending,
	/// Directory expanded; children are in the tree.
	Exp,
	/// Judged below threshold in round N (1-based). Collapsed.
	Fin(u8),
	/// File content is being read and judged.
	Reading,
	/// File content judged relevant.
	Hit,
	/// Not eligible (binary, empty, unreadable, request failed).
	Skip,
}

#[derive(Clone, Debug)]
pub struct HeatRange {
	pub start:   usize,
	pub end:     usize,
	pub p:       f64,
	pub snippet: String,
}

pub struct Node {
	pub path:          PathBuf,
	/// Root-relative display path; directories end with `/`.
	pub rel:           String,
	pub kind:          Kind,
	pub size:          u64,
	pub depth:         u16,
	pub parent:        Option<usize>,
	pub children:      Vec<usize>,
	pub state:         State,
	/// Judgment from the path/name listing (directory batch).
	pub name_score:    Option<f64>,
	/// Judgment from the file's content.
	pub content_score: Option<f64>,
	/// Choice confidence of the heatmap distribution.
	pub confidence:    Option<f64>,
	pub heat:          Vec<HeatRange>,
	/// Directories: eligible entry count and a sample of names.
	pub peek_count:    usize,
	pub peek:          String,
	pub note:          Option<String>,
	/// Lines of content that were judged, and whether the file was truncated.
	pub lines_seen:    Option<(usize, bool)>,
	gitignore:         Option<Arc<Gitignore>>,
}

impl Node {
	pub fn is_dir(&self) -> bool {
		self.kind == Kind::Dir
	}

	pub fn name(&self) -> &str {
		self
			.rel
			.trim_end_matches('/')
			.rsplit('/')
			.next()
			.unwrap_or(&self.rel)
	}
}

pub struct Tree {
	pub root:           PathBuf,
	pub nodes:          Vec<Node>,
	pub include_hidden: bool,
}

const DENY_DIRS: &[&str] = &[
	".git",
	"node_modules",
	"target",
	"dist",
	"build",
	"out",
	".next",
	".nuxt",
	".turbo",
	".cache",
	"__pycache__",
	".venv",
	"venv",
	".tox",
	"coverage",
	".idea",
	".vscode",
	".gradle",
	".mypy_cache",
	".pytest_cache",
	".ruff_cache",
	".parcel-cache",
];

const DENY_FILES: &[&str] = &[
	"Cargo.lock",
	"package-lock.json",
	"yarn.lock",
	"pnpm-lock.yaml",
	"bun.lock",
	"bun.lockb",
	"poetry.lock",
	"Pipfile.lock",
	"composer.lock",
	"Gemfile.lock",
	"go.sum",
	"flake.lock",
	".DS_Store",
	"Thumbs.db",
];

const BINARY_EXT: &[&str] = &[
	"png",
	"jpg",
	"jpeg",
	"gif",
	"webp",
	"avif",
	"ico",
	"bmp",
	"tiff",
	"psd",
	"svg",
	"woff",
	"woff2",
	"ttf",
	"otf",
	"eot",
	"zip",
	"gz",
	"tgz",
	"tar",
	"bz2",
	"xz",
	"zst",
	"7z",
	"rar",
	"pdf",
	"mp3",
	"mp4",
	"mov",
	"avi",
	"mkv",
	"wav",
	"ogg",
	"flac",
	"wasm",
	"so",
	"dylib",
	"dll",
	"exe",
	"o",
	"a",
	"class",
	"jar",
	"pyc",
	"pyo",
	"bin",
	"dat",
	"db",
	"sqlite",
	"sqlite3",
	"lock",
	"map",
	"min.js",
	"min.css",
	"snap",
	"pb",
	"onnx",
	"safetensors",
	"parquet",
	"arrow",
	"ipynb",
];

impl Tree {
	pub fn new(root: &Path, include_hidden: bool) -> io::Result<Self> {
		let root = root.canonicalize()?;
		if !root.is_dir() {
			return Err(io::Error::new(io::ErrorKind::InvalidInput, "path is not a directory"));
		}
		let mut t = Self { root: root.clone(), nodes: Vec::new(), include_hidden };
		t.nodes.push(Node {
			path:          root,
			rel:           String::new(),
			kind:          Kind::Dir,
			size:          0,
			depth:         0,
			parent:        None,
			children:      Vec::new(),
			state:         State::Unk,
			name_score:    None,
			content_score: None,
			confidence:    None,
			heat:          Vec::new(),
			peek_count:    0,
			peek:          String::new(),
			note:          None,
			lines_seen:    None,
			gitignore:     None,
		});
		t.expand(0);
		Ok(t)
	}

	pub fn name(&self) -> String {
		self
			.root
			.file_name()
			.map(|s| s.to_string_lossy().into_owned())
			.unwrap_or_default()
	}

	/// List a directory and add its eligible entries as `Unk` children.
	/// Returns the new child indices. Marks the node `Exp` (or `Skip` when
	/// empty).
	pub fn expand(&mut self, idx: usize) -> Vec<usize> {
		let dir = self.nodes[idx].path.clone();
		let depth = self.nodes[idx].depth + 1;
		let parent_rel = self.nodes[idx].rel.clone();

		let gi_path = dir.join(".gitignore");
		if gi_path.is_file() {
			let mut b = GitignoreBuilder::new(&dir);
			if b.add(&gi_path).is_none()
				&& let Ok(gi) = b.build()
			{
				self.nodes[idx].gitignore = Some(Arc::new(gi));
			}
		}

		let rd = match fs::read_dir(&dir) {
			Ok(rd) => rd,
			Err(e) => {
				self.nodes[idx].state = State::Skip;
				self.nodes[idx].note = Some(e.to_string());
				return Vec::new();
			},
		};

		let mut entries: Vec<(String, PathBuf, Kind, u64)> = Vec::new();
		for e in rd.flatten() {
			let name = e.file_name().to_string_lossy().into_owned();
			let Ok(md) = e.metadata() else { continue }; // does not follow symlinks
			let kind = if md.is_dir() {
				Kind::Dir
			} else if md.is_file() {
				Kind::File
			} else {
				continue;
			};
			if !self.eligible(&name, kind, md.len()) {
				continue;
			}
			let path = e.path();
			if self.ignored(idx, &path, kind == Kind::Dir) {
				continue;
			}
			entries.push((name, path, kind, md.len()));
		}
		entries.sort_by(|a, b| {
			(a.2 == Kind::File)
				.cmp(&(b.2 == Kind::File))
				.then(a.0.cmp(&b.0))
		});

		let mut kids = Vec::new();
		for (name, path, kind, size) in entries {
			let (peek_count, peek) = match kind {
				Kind::Dir => match self.peek_dir(&path) {
					Some(p) => p,
					None => continue, // nothing eligible inside; don't even list it
				},
				Kind::File => (0, String::new()),
			};
			let rel = match kind {
				Kind::Dir => format!("{parent_rel}{name}/"),
				Kind::File => format!("{parent_rel}{name}"),
			};
			let n = self.nodes.len();
			self.nodes.push(Node {
				path,
				rel,
				kind,
				size,
				depth,
				parent: Some(idx),
				children: Vec::new(),
				state: State::Unk,
				name_score: None,
				content_score: None,
				confidence: None,
				heat: Vec::new(),
				peek_count,
				peek,
				note: None,
				lines_seen: None,
				gitignore: None,
			});
			self.nodes[idx].children.push(n);
			kids.push(n);
		}
		self.nodes[idx].state = if kids.is_empty() {
			State::Skip
		} else {
			State::Exp
		};
		kids
	}

	/// All eligible child names of a directory node (sorted; folders end with
	/// `/`), without creating child nodes. Used for cheap "full listing"
	/// judgments.
	pub fn list_names(&self, idx: usize) -> Vec<String> {
		let n = &self.nodes[idx];
		if n.kind != Kind::Dir {
			return Vec::new();
		}
		let Ok(rd) = fs::read_dir(&n.path) else {
			return Vec::new();
		};
		let mut names = Vec::new();
		for e in rd.flatten() {
			let name = e.file_name().to_string_lossy().into_owned();
			let Ok(md) = e.metadata() else { continue };
			let kind = if md.is_dir() {
				Kind::Dir
			} else if md.is_file() {
				Kind::File
			} else {
				continue;
			};
			if !self.eligible(&name, kind, md.len()) {
				continue;
			}
			names.push(if kind == Kind::Dir {
				format!("{name}/")
			} else {
				name
			});
		}
		names.sort();
		names
	}

	/// Text shown to the model for one listing entry.
	pub fn label(&self, idx: usize) -> String {
		let n = &self.nodes[idx];
		match n.kind {
			Kind::File => format!("{} ({})", n.rel, human_size(n.size)),
			Kind::Dir => format!("{} — {} entries: {}", n.rel, n.peek_count, n.peek),
		}
	}

	fn eligible(&self, name: &str, kind: Kind, size: u64) -> bool {
		if !self.include_hidden && name.starts_with('.') {
			return false;
		}
		match kind {
			Kind::Dir => !DENY_DIRS.contains(&name),
			Kind::File => {
				if size == 0 || DENY_FILES.contains(&name) {
					return false;
				}
				let lower = name.to_ascii_lowercase();
				!BINARY_EXT.iter().any(|ext| {
					lower.len() > ext.len()
						&& lower.ends_with(ext)
						&& lower.as_bytes()[lower.len() - ext.len() - 1] == b'.'
				})
			},
		}
	}

	fn ignored(&self, parent: usize, path: &Path, is_dir: bool) -> bool {
		let mut cur = Some(parent);
		while let Some(i) = cur {
			if let Some(gi) = &self.nodes[i].gitignore
				&& gi.matched_path_or_any_parents(path, is_dir).is_ignore()
			{
				return true;
			}
			cur = self.nodes[i].parent;
		}
		false
	}

	/// Count eligible entries and sample a few names. `None` when nothing
	/// eligible.
	fn peek_dir(&self, dir: &Path) -> Option<(usize, String)> {
		let rd = fs::read_dir(dir).ok()?;
		let mut names: Vec<String> = Vec::new();
		for e in rd.flatten() {
			let name = e.file_name().to_string_lossy().into_owned();
			let Ok(md) = e.metadata() else { continue };
			let kind = if md.is_dir() {
				Kind::Dir
			} else if md.is_file() {
				Kind::File
			} else {
				continue;
			};
			if !self.eligible(&name, kind, md.len()) {
				continue;
			}
			names.push(if kind == Kind::Dir {
				format!("{name}/")
			} else {
				name
			});
		}
		if names.is_empty() {
			return None;
		}
		names.sort();
		let count = names.len();
		let shown = 8.min(count);
		let mut s = names[..shown].join(", ");
		if count > shown {
			s.push_str(&format!(", … +{}", count - shown));
		}
		Some((count, s))
	}
}

pub fn human_size(n: u64) -> String {
	const U: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
	let mut v = n as f64;
	let mut i = 0;
	while v >= 1024.0 && i < U.len() - 1 {
		v /= 1024.0;
		i += 1;
	}
	if i == 0 {
		format!("{n} B")
	} else {
		format!("{v:.1} {}", U[i])
	}
}
