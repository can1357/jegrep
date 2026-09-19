//! Hierarchical semantic discovery followed by whole-file passage refinement.
//!
//! Beam supplies candidate hits without relying on literal query vocabulary.
//! Window reuses those hits, adds its independent query-derived candidates,
//! and preserves the original line coordinates of all accepted passages.

use super::{Strategy, beam::Beam, window::Window};
use crate::ctx::Ctx;

#[derive(Default)]
pub struct HybridWindow;

impl Strategy for HybridWindow {
    fn run(&mut self, ctx: &mut Ctx) {
        // Beam drains its outstanding requests before returning, so the shared
        // pool and tree are ready for Window's independent request sequence.
        Beam::default().run(ctx);
        let discovery_rounds = ctx.rounds;
        let discovery_waves = ctx.stats.waves;
        ctx.rounds = 0;
        ctx.stats.waves = 0;
        Window.run(ctx);
        ctx.rounds = discovery_rounds.saturating_add(ctx.rounds);
        ctx.stats.waves = discovery_waves.saturating_add(ctx.stats.waves);
    }
}
