//! Port of pi's grouped file output (`packages/tui/src/tools/grouped-file-output.ts`
//! + `packages/utils/src/path-tree.ts`): a prefix-folded directory tree.
//!
//! - files at a node come before its subdirectories, both in insertion order
//! - single-child directory chains fold into one header (`pkg/ai/`, then `src/util/`)
//! - one nesting level per header depth; a blank line precedes every directory
//!   header and every root-level leaf (after the first emitted line)
//!
//! Leaves are usually files; a leaf whose path ends in `/` is a collapsed folder
//! shown in place (so judged-but-unexpanded folders can be listed like files).

#[derive(Default)]
struct Node {
    leaves: Vec<(String, usize)>,
    subdirs: Vec<(String, Self)>,
}

impl Node {
    fn child(&mut self, seg: &str) -> &mut Self {
        let pos = if let Some(p) = self.subdirs.iter().position(|(n, _)| n == seg) { p } else {
            self.subdirs.push((seg.to_string(), Self::default()));
            self.subdirs.len() - 1
        };
        &mut self.subdirs[pos].1
    }
}

pub struct Leaf {
    /// Root-relative path; trailing `/` marks a collapsed folder leaf.
    pub rel: String,
    /// Text after the name on the header line (scores, tags).
    pub header: String,
    /// Body lines under the header (already formatted; indented by the renderer).
    pub body: Vec<String>,
}

pub enum Event<'a> {
    /// A (possibly folded) directory header. `full` is the root-relative path with trailing `/`.
    Dir {
        depth: usize,
        chain: String,
        full: String,
    },
    Leaf {
        depth: usize,
        name: String,
        leaf: &'a Leaf,
    },
}

fn build(leaves: &[Leaf]) -> Node {
    let mut root = Node::default();
    for (i, leaf) in leaves.iter().enumerate() {
        let is_dir_leaf = leaf.rel.ends_with('/');
        let trimmed = leaf.rel.trim_end_matches('/');
        if trimmed.is_empty() {
            continue;
        }
        let segs: Vec<&str> = trimmed.split('/').collect();
        let mut node = &mut root;
        for seg in &segs[..segs.len() - 1] {
            node = node.child(seg);
        }
        let name = if is_dir_leaf {
            format!("{}/", segs[segs.len() - 1])
        } else {
            segs[segs.len() - 1].to_string()
        };
        node.leaves.push((name, i));
    }
    root
}

fn walk<'a>(node: &Node, leaves: &'a [Leaf], depth: usize, prefix: &str, out: &mut Vec<Event<'a>>) {
    for (name, i) in &node.leaves {
        out.push(Event::Leaf {
            depth,
            name: name.clone(),
            leaf: &leaves[*i],
        });
    }
    for (name, sub) in &node.subdirs {
        let mut dir = sub;
        let mut parts = vec![name.as_str()];
        while dir.leaves.is_empty() && dir.subdirs.len() == 1 {
            let (only_name, only) = &dir.subdirs[0];
            parts.push(only_name.as_str());
            dir = only;
        }
        let chain = parts.join("/");
        let full = format!("{prefix}{chain}/");
        out.push(Event::Dir {
            depth,
            chain,
            full: full.clone(),
        });
        walk(dir, leaves, depth + 1, &full, out);
    }
}

pub fn events(leaves: &[Leaf]) -> Vec<Event<'_>> {
    let root = build(leaves);
    let mut out = Vec::new();
    walk(&root, leaves, 0, "", &mut out);
    out
}

/// Render to terminal lines. `style_dir` styles a directory header given its
/// folded chain text and full path (return the complete header text, e.g. bold
/// name plus an annotation). `indent` is the base indentation.
pub fn render(
    leaves: &[Leaf],
    indent: &str,
    style_dir: &dyn Fn(&str, &str) -> String,
) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    let mut emitted = false;
    for ev in events(leaves) {
        match ev {
            Event::Dir { depth, chain, full } => {
                if emitted {
                    lines.push(String::new());
                }
                emitted = true;
                lines.push(format!(
                    "{indent}{}{}",
                    "  ".repeat(depth),
                    style_dir(&format!("{chain}/"), &full)
                ));
            }
            Event::Leaf { depth, name, leaf } => {
                if emitted && depth == 0 {
                    lines.push(String::new());
                }
                emitted = true;
                let pad = "  ".repeat(depth);
                lines.push(format!("{indent}{pad}{name}{}", leaf.header));
                for b in &leaf.body {
                    lines.push(format!("{indent}{pad}  {b}"));
                }
            }
        }
    }
    lines
}

/// Split a root-relative path into (parent dir with trailing `/` or "", last segment).
/// A trailing `/` stays on the segment so folder leaves read as `name/`.
pub fn split_rel(rel: &str) -> (&str, &str) {
    let trimmed = rel.trim_end_matches('/');
    match trimmed.rfind('/') {
        Some(p) => (&rel[..=p], &rel[p + 1..]),
        None => ("", rel),
    }
}

/// Model-facing rendering, byte-for-byte in the shape pi sends to the model
/// (`formatGroupedFiles(...).model`): `#` per depth, `# dir/` headers, leaf lines
/// produced by `leaf_line(leaf, name)`, a blank line before every directory header
/// and every root-level leaf after the first line.
pub fn render_model(leaves: &[Leaf], leaf_line: &dyn Fn(&Leaf, &str) -> String) -> String {
    let mut out = String::new();
    let mut emitted = false;
    for ev in events(leaves) {
        match ev {
            Event::Dir { depth, chain, .. } => {
                if emitted {
                    out.push('\n');
                }
                emitted = true;
                out.push_str(&"#".repeat(depth + 1));
                out.push(' ');
                out.push_str(&chain);
                out.push_str("/\n");
            }
            Event::Leaf { depth, name, leaf } => {
                if emitted && depth == 0 {
                    out.push('\n');
                }
                emitted = true;
                out.push_str(&"#".repeat(depth + 1));
                out.push(' ');
                out.push_str(&leaf_line(leaf, &name));
                out.push('\n');
            }
        }
    }
    out
}
