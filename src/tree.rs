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

/// Credential files by exact name. Never listed or read, even with `--hidden`.
const SECRET_FILES: &[&str] = &[
	".env",
	".envrc",
	".netrc",
	".npmrc",
	".pypirc",
	".pgpass",
	".boto",
	".s3cfg",
	".dockercfg",
	".git-credentials",
	".htpasswd",
	"htpasswd",
	"credentials",
	"credentials.json",
	"client_secret.json",
	"service-account.json",
	"id_rsa",
	"id_dsa",
	"id_ecdsa",
	"id_ed25519",
];

/// Credential files by extension: keys, certificate stores, encrypted vaults,
/// and infrastructure state that embeds secrets.
const SECRET_EXT: &[&str] = &[
	"pem",
	"key",
	"p12",
	"pfx",
	"jks",
	"keystore",
	"bks",
	"ppk",
	"kdbx",
	"gpg",
	"pgp",
	"asc",
	"der",
	"crt",
	"cer",
	"tfvars",
	"tfvars.json",
	"tfstate",
	"tfstate.backup",
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

/// A directory entry that passed [`Tree::eligible`] and the gitignore chain.
struct Entry {
	name: String,
	path: PathBuf,
	kind: Kind,
	size: u64,
}

impl Entry {
	/// Listing text: folders end with `/`.
	fn label(&self) -> String {
		match self.kind {
			Kind::Dir => format!("{}/", self.name),
			Kind::File => self.name.clone(),
		}
	}
}

/// `.gitignore` of `dir`, when present and parseable.
fn load_gitignore(dir: &Path) -> Option<Arc<Gitignore>> {
	let gi_path = dir.join(".gitignore");
	if !gi_path.is_file() {
		return None;
	}
	let mut b = GitignoreBuilder::new(dir);
	if b.add(&gi_path).is_some() {
		return None;
	}
	b.build().ok().map(Arc::new)
}

/// `lower` ends with `.<ext>` for some `ext` in `exts`.
fn has_ext(lower: &str, exts: &[&str]) -> bool {
	exts.iter().any(|ext| {
		lower.len() > ext.len()
			&& lower.ends_with(ext)
			&& lower.as_bytes()[lower.len() - ext.len() - 1] == b'.'
	})
}

/// Credential material: exact names, `.env.*` variants (except committed
/// templates like `.env.example`), and key/vault extensions. Checked
/// regardless of `--hidden`.
fn secret(name: &str) -> bool {
	const ENV_TEMPLATES: &[&str] = &[".env.example", ".env.sample", ".env.template", ".env.dist"];
	if SECRET_FILES.contains(&name) {
		return true;
	}
	if name.starts_with(".env.") {
		return !ENV_TEMPLATES.contains(&name);
	}
	has_ext(&name.to_ascii_lowercase(), SECRET_EXT)
}

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
	/// Returns the child indices. Marks the node `Exp` (or `Skip` when empty).
	/// An already-expanded node returns its existing children rather than
	/// listing them again.
	pub fn expand(&mut self, idx: usize) -> Vec<usize> {
		if self.nodes[idx].state == State::Exp {
			return self.nodes[idx].children.clone();
		}
		let dir = self.nodes[idx].path.clone();
		let depth = self.nodes[idx].depth + 1;
		let parent_rel = self.nodes[idx].rel.clone();
		let parent = self.nodes[idx].parent;

		// Peeking already loaded it for every node but the root.
		if self.nodes[idx].gitignore.is_none() {
			self.nodes[idx].gitignore = load_gitignore(&dir);
		}
		let own = self.nodes[idx].gitignore.clone();

		let mut entries = match self.entries(&dir, own.as_deref(), parent) {
			Ok(entries) => entries,
			Err(e) => {
				self.nodes[idx].state = State::Skip;
				self.nodes[idx].note = Some(e.to_string());
				return Vec::new();
			},
		};
		entries.sort_by(|a, b| {
			(a.kind == Kind::File)
				.cmp(&(b.kind == Kind::File))
				.then(a.name.cmp(&b.name))
		});

		let mut kids = Vec::new();
		for Entry { name, path, kind, size } in entries {
			let (peek_count, peek, gitignore) = match kind {
				Kind::Dir => match self.peek_dir(&path, idx) {
					Some(p) => p,
					None => continue, // nothing eligible inside; don't even list it
				},
				Kind::File => (0, String::new(), None),
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
				gitignore,
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

	/// All eligible, non-ignored child names of a directory node (sorted;
	/// folders end with `/`), without creating child nodes. Used for cheap
	/// "full listing" judgments.
	pub fn list_names(&self, idx: usize) -> Vec<String> {
		let n = &self.nodes[idx];
		if n.kind != Kind::Dir {
			return Vec::new();
		}
		let own = n.gitignore.clone().or_else(|| load_gitignore(&n.path));
		let mut names: Vec<String> = self
			.entries(&n.path, own.as_deref(), n.parent)
			.map(|entries| entries.iter().map(Entry::label).collect())
			.unwrap_or_default();
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

	/// Entries of `dir` that pass [`Self::eligible`] and are not gitignored.
	/// `own` is `dir`'s `.gitignore`; `parent` starts the ancestor chain.
	/// Symlinks are never followed.
	fn entries(
		&self,
		dir: &Path,
		own: Option<&Gitignore>,
		parent: Option<usize>,
	) -> io::Result<Vec<Entry>> {
		let mut entries = Vec::new();
		for e in fs::read_dir(dir)?.flatten() {
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
			let path = e.path();
			let is_dir = kind == Kind::Dir;
			if own.is_some_and(|gi| gi.matched_path_or_any_parents(&path, is_dir).is_ignore())
				|| self.ignored(parent, &path, is_dir)
			{
				continue;
			}
			entries.push(Entry { name, path, kind, size: md.len() });
		}
		Ok(entries)
	}

	fn eligible(&self, name: &str, kind: Kind, size: u64) -> bool {
		if !self.include_hidden && name.starts_with('.') {
			return false;
		}
		match kind {
			Kind::Dir => !DENY_DIRS.contains(&name),
			Kind::File => {
				size > 0
					&& !DENY_FILES.contains(&name)
					&& !secret(name)
					&& !has_ext(&name.to_ascii_lowercase(), BINARY_EXT)
			},
		}
	}

	/// Whether any `.gitignore` from node `from` up to the root ignores `path`.
	fn ignored(&self, from: Option<usize>, path: &Path, is_dir: bool) -> bool {
		let mut cur = from;
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

	/// Count eligible entries of a not-yet-listed child of `parent` and sample
	/// a few names, applying `dir`'s own `.gitignore` (returned for the node
	/// to keep). `None` when nothing eligible.
	fn peek_dir(
		&self,
		dir: &Path,
		parent: usize,
	) -> Option<(usize, String, Option<Arc<Gitignore>>)> {
		let own = load_gitignore(dir);
		let entries = self.entries(dir, own.as_deref(), Some(parent)).ok()?;
		if entries.is_empty() {
			return None;
		}
		let mut names: Vec<String> = entries.iter().map(Entry::label).collect();
		names.sort();
		let count = names.len();
		let shown = 8.min(count);
		let mut s = names[..shown].join(", ");
		if count > shown {
			s.push_str(&format!(", … +{}", count - shown));
		}
		Some((count, s, own))
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

#[cfg(test)]
mod tests {
	use std::{
		fs,
		path::PathBuf,
		sync::atomic::{AtomicU64, Ordering},
		time::{SystemTime, UNIX_EPOCH},
	};

	use super::Tree;

	static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

	struct TempRoot(PathBuf);

	impl TempRoot {
		fn new() -> Self {
			let nanos = SystemTime::now()
				.duration_since(UNIX_EPOCH)
				.unwrap()
				.as_nanos();
			let n = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
			let root = std::env::temp_dir().join(format!("jegrep-tree-{nanos}-{n}"));
			fs::create_dir_all(&root).unwrap();
			Self(root)
		}

		fn write(&self, rel: &str, text: &str) {
			let p = self.0.join(rel);
			fs::create_dir_all(p.parent().unwrap()).unwrap();
			fs::write(p, text).unwrap();
		}
	}

	impl Drop for TempRoot {
		fn drop(&mut self) {
			let _ = fs::remove_dir_all(&self.0);
		}
	}

	/// Every listed path after expanding every directory (`Tree::new` already
	/// expanded the root).
	fn listed(tree: &mut Tree) -> Vec<String> {
		let mut stack = tree.nodes[0].children.clone();
		while let Some(i) = stack.pop() {
			if tree.nodes[i].is_dir() {
				stack.extend(tree.expand(i));
			}
		}
		let mut rels: Vec<String> = tree.nodes.iter().skip(1).map(|n| n.rel.clone()).collect();
		rels.sort();
		rels
	}

	#[test]
	fn credential_files_are_never_listed_even_with_hidden() {
		let root = TempRoot::new();
		root.write("src/main.rs", "fn main() {}\n");
		root.write("config/service-account-key.pem", "-----BEGIN PRIVATE KEY-----\n");
		root.write("config/credentials.json", "{\"token\":\"x\"}\n");
		root.write("config/prod.tfvars", "db_password = \"x\"\n");
		root.write("config/site.KEY", "x\n");
		root.write("deploy/id_ed25519", "x\n");
		root.write(".env", "TOKEN=x\n");
		root.write(".env.production", "TOKEN=x\n");
		root.write(".env.example", "TOKEN=\n");
		root.write(".npmrc", "//registry/:_authToken=x\n");
		for hidden in [false, true] {
			let mut tree = Tree::new(&root.0, hidden).unwrap();
			let rels = listed(&mut tree);
			let mut expected = vec!["src/".to_owned(), "src/main.rs".to_owned()];
			if hidden {
				expected.push(".env.example".to_owned());
			}
			expected.sort();
			assert_eq!(rels, expected, "hidden={hidden}");
		}
	}

	#[test]
	fn expanding_twice_does_not_duplicate_children() {
		let root = TempRoot::new();
		root.write("a.rs", "x\n");
		root.write("sub/b.rs", "x\n");
		let mut tree = Tree::new(&root.0, false).unwrap();
		let first = tree.nodes[0].children.clone();
		assert_eq!(tree.expand(0), first);
		assert_eq!(listed(&mut tree), ["a.rs", "sub/", "sub/b.rs"]);
	}

	#[test]
	fn gitignored_names_are_absent_from_peeks_and_listings() {
		let root = TempRoot::new();
		root.write(".gitignore", "secrets/\n*.local\n");
		root.write("app/.gitignore", "generated.rs\n");
		root.write("app/lib.rs", "pub fn f() {}\n");
		root.write("app/generated.rs", "pub fn g() {}\n");
		root.write("app/notes.local", "x\n");
		root.write("secrets/token.txt", "x\n");
		root.write("only-secrets/secrets/token.txt", "x\n");
		let mut tree = Tree::new(&root.0, false).unwrap();
		// Root listing: `secrets/` is ignored; `only-secrets/` has nothing
		// eligible once its ignored child is dropped, so it is not listed.
		let app = tree.nodes.iter().position(|n| n.rel == "app/").unwrap();
		assert_eq!(listed(&mut tree), ["app/", "app/lib.rs"]);
		// The peek sampled when `app/` was listed honors both `.gitignore`s.
		assert_eq!(tree.nodes[app].peek_count, 1);
		assert_eq!(tree.nodes[app].peek, "lib.rs");
		assert_eq!(tree.list_names(app), ["lib.rs"]);
	}
}
