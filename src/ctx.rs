//! Shared search context handed to a strategy: the tree, the worker pool, the
//! UI, the knobs, and the running tally that the benchmark compares.

use crate::jev::{self, Client, Usage};
use crate::pool::{FileOpts, Pool};
use crate::tree::{HeatRange, State, Tree};
use crate::ui::Ui;
use std::{
    io,
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};

#[derive(Clone, Debug)]
pub struct Opts {
    pub query: String,
    pub parallel: usize,
    /// Soft target for entries per directory batch.
    pub batch: usize,
    /// Hard cap on entries per request.
    pub max_batch: usize,
    /// Relevance thresholds, one per round.
    pub thresholds: Vec<f64>,
    /// Bytes of a file to send for a content check.
    pub bytes: usize,
    /// Heatmap line ranges per file.
    pub ranges: usize,
    /// Stop lowering the threshold once this many hits exist.
    pub min_hits: usize,
    /// Extra grep keywords for grep-prior strategies (added to the ones derived from the query).
    pub keywords: Vec<String>,
}

#[derive(Default, Debug, Clone)]
pub struct Stats {
    pub requests: u32,
    pub errors: u32,
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// Entries judged by name/path.
    pub judged: u32,
    /// Directories listed.
    pub expanded: u32,
    /// Files whose content was read and sent.
    pub files_read: u32,
    /// Bytes of file content sent.
    pub file_bytes: u64,
    /// Files whose opening was sniffed, and the bytes of those heads (also counted in `file_bytes`).
    pub sniffed: u32,
    pub sniff_bytes: u64,
    pub api_time: Duration,
    /// Sequential dependent round trips (critical-path length). 0 when a strategy does not track it.
    pub waves: u32,
    /// Window-strategy accounting: candidate prompts versus source passages.
    pub window_name_tokens: u64,
    pub window_content_tokens: u64,
    pub windows_judged: u32,
    pub windows_pruned: u32,
    /// Cascade-only routing cost and cold local preparation (no saved index).
    pub cascade_map_tokens: u64,
    pub cascade_map_cards: u32,
    pub cascade_prepare_ms: u64,
}

impl Stats {
    pub fn record(&mut self, usage: Usage, elapsed: Duration) {
        self.requests += 1;
        self.input_tokens += usage.input_tokens;
        self.output_tokens += usage.output_tokens;
        self.api_time += elapsed;
    }
    pub fn record_error(&mut self, elapsed: Duration) {
        self.requests += 1;
        self.errors += 1;
        self.api_time += elapsed;
    }
    pub fn usd(&self) -> f64 {
        self.input_tokens as f64 * jev::USD_PER_INPUT_TOKEN
    }
}

pub struct Ctx {
    pub opts: Opts,
    pub tree: Tree,
    pub pool: Pool,
    pub ui: Ui,
    pub stats: Stats,
    pub started: Instant,
    /// Informational: how many threshold rounds ran and the final threshold.
    pub rounds: u8,
    pub tau: f64,
}

impl Ctx {
    pub fn new(
        opts: Opts,
        root: &Path,
        hidden: bool,
        client: Arc<Client>,
        ui: Ui,
    ) -> io::Result<Self> {
        let tree = Tree::new(root, hidden)?;
        let fopts = Arc::new(FileOpts {
            query: opts.query.clone(),
            max_bytes: opts.bytes,
            ranges: opts.ranges,
        });
        let pool = Pool::new(client, opts.parallel, fopts);
        Ok(Self {
            opts,
            tree,
            pool,
            ui,
            stats: Stats::default(),
            started: Instant::now(),
            rounds: 0,
            tau: 0.0,
        })
    }

    /// Hit nodes, best content score first.
    pub fn hits(&self) -> Vec<usize> {
        let mut v: Vec<usize> = (0..self.tree.nodes.len())
            .filter(|&i| self.tree.nodes[i].state == State::Hit)
            .collect();
        v.sort_by(|&a, &b| {
            let ca = self.tree.nodes[a].content_score.unwrap_or(0.0);
            let cb = self.tree.nodes[b].content_score.unwrap_or(0.0);
            cb.partial_cmp(&ca).unwrap_or(std::cmp::Ordering::Equal)
        });
        v
    }

    /// Store a content judgment on a node (does not decide hit/miss).
    pub fn record_content(
        &mut self,
        idx: usize,
        score: f64,
        heat: Vec<HeatRange>,
        confidence: Option<f64>,
        lines_seen: (usize, bool),
        bytes: usize,
    ) {
        self.stats.files_read += 1;
        self.stats.file_bytes += bytes as u64;
        let n = &mut self.tree.nodes[idx];
        n.content_score = Some(score);
        n.heat = heat;
        n.confidence = confidence;
        n.lines_seen = Some(lines_seen);
    }

    pub fn mark_hit(&mut self, idx: usize, cached: bool) {
        let n = &mut self.tree.nodes[idx];
        n.state = State::Hit;
        let c = n.content_score.unwrap_or(0.0);
        let top = n
            .heat
            .iter()
            .max_by(|a, b| a.p.partial_cmp(&b.p).unwrap_or(std::cmp::Ordering::Equal))
            .cloned();
        self.ui.hit(&n.rel, c, top.as_ref(), cached);
    }

    pub fn mark_fin(&mut self, idx: usize, round: u8) {
        self.tree.nodes[idx].state = State::Fin(round);
    }

    pub fn mark_skip(&mut self, idx: usize, why: String) {
        let n = &mut self.tree.nodes[idx];
        n.state = State::Skip;
        self.ui.skipped(&n.rel, &why);
        n.note = Some(why);
    }
}
