//! Exploration strategies. Each one drives the same primitives (tree, pool,
//! question builders, Jev client) differently. Add a module, register it here.

use crate::ctx::Ctx;

pub mod baseline;
pub mod beam;
pub mod budget;
pub mod cascade;
pub mod deep;
pub mod hybrid_window;
pub mod inline;
pub mod paged;
pub mod sniff;
pub mod window;

pub trait Strategy {
	fn run(&mut self, ctx: &mut Ctx);
}

pub const NAMES: &[&str] = &[
	"baseline",
	"beam",
	"sniff",
	"deep",
	"paged",
	"paged-grep",
	"paged-grep-fast",
	"paged-grep-labels",
	"inline",
	"inline-shared",
	"inline-solo",
	"inline-16k",
	"budget",
	"window",
	"hybrid-window",
	"cascade",
];

pub fn make(name: &str) -> Option<Box<dyn Strategy>> {
	match name {
		"baseline" => Some(Box::new(baseline::Baseline::default())),
		"beam" => Some(Box::new(beam::Beam::default())),
		"sniff" => Some(Box::new(sniff::Sniff::default())),
		"deep" => Some(Box::new(deep::Deep::default())),
		"paged" => Some(Box::new(paged::Paged::new(false, false, None))),
		"paged-grep" => Some(Box::new(paged::Paged::new(true, false, None))),
		"paged-grep-fast" => Some(Box::new(paged::Paged::new(true, false, Some(2)))),
		"paged-grep-labels" => Some(Box::new(paged::Paged::new(true, true, Some(2)))),
		"inline" => Some(Box::new(inline::Inline::new(inline::HeatMode::PerFile, true))),
		"inline-shared" => Some(Box::new(inline::Inline::new(inline::HeatMode::Shared, true))),
		"inline-solo" => Some(Box::new(inline::Inline::new(inline::HeatMode::PerFile, false))),
		"inline-16k" => {
			Some(Box::new(inline::Inline::with_cap(inline::HeatMode::PerFile, true, Some(16 * 1024))))
		},
		"budget" => Some(Box::new(budget::Budget::default())),
		"cascade" => Some(Box::new(cascade::Cascade)),
		"window" => Some(Box::new(window::Window)),
		"hybrid-window" => Some(Box::new(hybrid_window::HybridWindow)),
		_ => None,
	}
}
