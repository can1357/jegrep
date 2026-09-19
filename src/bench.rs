//! Benchmark harness: run every case with one strategy, score recall against
//! ground truth, and tally the three objectives — wall time, cost, file reads.

use crate::ctx::{Ctx, Opts, Stats};
use crate::jev::Client;
use crate::strategies;
use crate::ui::{Ui, UiOptions};
use serde::{Deserialize, Serialize};
use std::{path::Path, sync::Arc};

#[derive(Deserialize, Debug, Clone)]
pub struct Case {
    pub name: String,
    pub query: String,
    /// Acceptable answers: exact root-relative file paths, or a folder prefix ending in `/`.
    pub expect: Vec<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Row {
    pub case: String,
    pub strategy: String,
    pub found: bool,
    /// 1-based rank of the first expected file among hits.
    pub rank: Option<usize>,
    pub hits: usize,
    pub wall_ms: u128,
    pub requests: u32,
    pub errors: u32,
    pub input_tokens: u64,
    pub usd: f64,
    pub files_read: u32,
    pub file_kb: f64,
    /// Files whose opening was sniffed (head bytes are included in `file_kb`).
    #[serde(default)]
    pub sniffed: u32,
    pub judged: u32,
    pub expanded: u32,
    #[serde(default)]
    pub waves: u32,
    pub top_hits: Vec<String>,
}

fn matches(expect: &[String], rel: &str) -> bool {
    expect.iter().any(|e| {
        if e.ends_with('/') {
            rel.starts_with(e)
        } else {
            rel == e
        }
    })
}

pub fn run(
    cases: &[Case],
    strategy: &str,
    base: &Opts,
    root: &Path,
    hidden: bool,
    client: Arc<Client>,
    verbose: bool,
) -> Vec<Row> {
    let mut rows = Vec::new();
    for case in cases {
        let opts = Opts {
            query: case.query.clone(),
            ..base.clone()
        };
        let ui = Ui::new(UiOptions {
            quiet: !verbose,
            verbose,
            ..UiOptions::default()
        });
        let mut ctx = match Ctx::new(opts, root, hidden, Arc::clone(&client), ui) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("{}: {e}", root.display());
                std::process::exit(2);
            }
        };
        let Some(mut strat) = strategies::make(strategy) else {
            eprintln!("unknown strategy {strategy}");
            std::process::exit(2);
        };
        strat.run(&mut ctx);
        let wall = ctx.started.elapsed();
        let hits = ctx.hits();
        let rels: Vec<String> = hits
            .iter()
            .map(|&i| ctx.tree.nodes[i].rel.clone())
            .collect();
        let rank = rels
            .iter()
            .position(|r| matches(&case.expect, r))
            .map(|p| p + 1);
        let s: &Stats = &ctx.stats;
        rows.push(Row {
            case: case.name.clone(),
            strategy: strategy.to_string(),
            found: rank.is_some(),
            rank,
            hits: hits.len(),
            wall_ms: wall.as_millis(),
            requests: s.requests,
            errors: s.errors,
            input_tokens: s.input_tokens,
            usd: s.usd(),
            files_read: s.files_read,
            file_kb: s.file_bytes as f64 / 1024.0,
            sniffed: s.sniffed,
            judged: s.judged,
            expanded: s.expanded,
            waves: s.waves,
            top_hits: rels.into_iter().take(3).collect(),
        });
        eprint!(".");
    }
    eprintln!();
    rows
}

pub fn print_table(rows: &[Row]) {
    println!(
        "{:<26} {:<5} {:>4} {:>4} {:>7} {:>4} {:>8} {:>8} {:>5} {:>8} {:>6} {:>5}",
        "case",
        "found",
        "rank",
        "hits",
        "wall s",
        "req",
        "tokens",
        "usd",
        "files",
        "file KB",
        "judged",
        "waves"
    );
    let _ = "waves";
    for r in rows {
        println!(
            "{:<26} {:<5} {:>4} {:>4} {:>7.2} {:>4} {:>8} {:>8.4} {:>5} {:>8.0} {:>6} {:>5}",
            trunc(&r.case, 26),
            if r.found { "yes" } else { "NO" },
            r.rank.map_or("-".into(), |x| x.to_string()),
            r.hits,
            r.wall_ms as f64 / 1000.0,
            r.requests,
            r.input_tokens,
            r.usd,
            r.files_read,
            r.file_kb,
            r.judged,
            r.waves
        );
        if r.waves > 0 {
            println!("{:<26} {}", "", format!("waves {}", r.waves));
        }
    }
    let n = rows.len().max(1) as f64;
    let found = rows.iter().filter(|r| r.found).count();
    let wall: f64 = rows.iter().map(|r| r.wall_ms as f64 / 1000.0).sum();
    let tokens: u64 = rows.iter().map(|r| r.input_tokens).sum();
    let usd: f64 = rows.iter().map(|r| r.usd).sum();
    let files: u32 = rows.iter().map(|r| r.files_read).sum();
    let kb: f64 = rows.iter().map(|r| r.file_kb).sum();
    let req: u32 = rows.iter().map(|r| r.requests).sum();
    println!("{}", "-".repeat(96));
    println!(
        "{:<26} {:<5} {:>4} {:>4} {:>7.2} {:>4} {:>8} {:>8.4} {:>5} {:>8.0} {:>6} {:>5}",
        format!("TOTAL ({} cases)", rows.len()),
        format!("{found}/{}", rows.len()),
        "",
        rows.iter().map(|r| r.hits).sum::<usize>(),
        wall,
        req,
        tokens,
        usd,
        files,
        kb,
        rows.iter().map(|r| r.judged).sum::<u32>(),
        rows.iter().map(|r| r.waves).sum::<u32>()
    );
    println!(
        "mean per case: {:.2}s wall · {} tokens · ${:.4} · {:.1} files read · {:.0} KB read",
        wall / n,
        tokens / n as u64,
        usd / n,
        files as f64 / n,
        kb / n
    );
}

fn trunc(s: &str, w: usize) -> String {
    if s.chars().count() <= w {
        s.to_string()
    } else {
        s.chars().take(w - 1).collect::<String>() + "…"
    }
}

/// Aggregate JSONL rows (possibly several strategies) into one comparison:
/// a per-case found/rank matrix and per-strategy totals.
pub fn summarize(rows: &[Row]) {
    let mut strategies: Vec<String> = Vec::new();
    let mut cases: Vec<String> = Vec::new();
    for r in rows {
        if !strategies.contains(&r.strategy) {
            strategies.push(r.strategy.clone());
        }
        if !cases.contains(&r.case) {
            cases.push(r.case.clone());
        }
    }
    // latest row wins when a (strategy, case) pair was benchmarked more than once
    let cell = |s: &str, c: &str| rows.iter().rev().find(|r| r.strategy == s && r.case == c);

    print!("{:<26}", "case");
    for s in &strategies {
        print!(" {:>12}", trunc(s, 12));
    }
    println!();
    for c in &cases {
        print!("{:<26}", trunc(c, 26));
        for s in &strategies {
            let v = match cell(s, c) {
                Some(r) if r.found => {
                    format!("#{} {:.1}s", r.rank.unwrap_or(0), r.wall_ms as f64 / 1000.0)
                }
                Some(_) => "miss".to_string(),
                None => "-".to_string(),
            };
            print!(" {v:>12}");
        }
        println!();
    }
    println!();
    println!(
        "{:<12} {:>5} {:>7} {:>5} {:>9} {:>8} {:>6} {:>8} {:>7}",
        "strategy", "found", "wall s", "req", "tokens", "usd", "files", "file KB", "judged"
    );
    for s in &strategies {
        let rs: Vec<&Row> = cases.iter().filter_map(|c| cell(s, c)).collect();
        let found = rs.iter().filter(|r| r.found).count();
        let wall: f64 = rs.iter().map(|r| r.wall_ms as f64 / 1000.0).sum();
        let req: u32 = rs.iter().map(|r| r.requests).sum();
        let tok: u64 = rs.iter().map(|r| r.input_tokens).sum();
        let usd: f64 = rs.iter().map(|r| r.usd).sum();
        let files: u32 = rs.iter().map(|r| r.files_read).sum();
        let kb: f64 = rs.iter().map(|r| r.file_kb).sum();
        let judged: u32 = rs.iter().map(|r| r.judged).sum();
        println!(
            "{:<12} {:>5} {:>7.1} {:>5} {:>9} {:>8.4} {:>6} {:>8.0} {:>7}",
            trunc(s, 12),
            format!("{found}/{}", rs.len()),
            wall,
            req,
            tok,
            usd,
            files,
            kb,
            judged
        );
    }
}
