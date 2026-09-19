//! A small stderr panel. Search events update the model; a timer paints it
//! while network calls block. No raw mode, alternate screen, or persistent
//! cursor modes.

use std::{
	io::{self, Write},
	sync::Arc,
	thread::{self, JoinHandle},
	time::Duration,
};

use parking_lot::{Condvar, Mutex};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

const TICK: Duration = Duration::from_millis(100);
const SPINNER: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
mod model;
use model::Model;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Activity {
	Folder,
	Collapsed,
	Reading,
	Hit,
	Miss,
	Skipped,
	Note,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum RangeState {
	Queued,
	Active,
	Scored(f64),
	Pruned,
	Failed,
}

pub enum Event<'a> {
	Workspace { name: &'a str, folders: &'a [String], root_files: bool },
	Scanned(&'a str),
	Name { path: &'a str, state: RangeState },
	Status(&'a str),
	Round { number: u8, threshold: f64 },
	Names { done: usize, total: usize, active: usize },
	Entry { activity: Activity, path: &'a str, score: Option<f64>, detail: &'a str },
	Range { path: &'a str, start: usize, end: usize, state: RangeState },
}

fn paint(code: &str, text: &str, color: bool) -> String {
	if color {
		format!("\x1b[{code}m{text}\x1b[0m")
	} else {
		text.into()
	}
}

/// Strip terminal controls from paths, snippets and status messages, including
/// styles already supplied by the legacy UI. One event always occupies one row.
fn clean(text: &str) -> String {
	let mut result = String::new();
	let mut chars = text.chars();
	while let Some(c) = chars.next() {
		if c == '\x1b' {
			match chars.next() {
				Some('[') => {
					for c in chars.by_ref() {
						if ('@'..='~').contains(&c) {
							break;
						}
					}
				},
				Some(']') => {
					let mut escape = false;
					for c in chars.by_ref() {
						if c == '\x07' || (escape && c == '\\') {
							break;
						}
						escape = c == '\x1b';
					}
				},
				_ => {},
			}
		} else if c.is_control() {
			result.push(' ');
		} else {
			result.push(c);
		}
	}
	result
}

fn tail(text: &str, width: usize) -> String {
	if text.width() <= width {
		return text.into();
	}
	if width == 0 {
		return String::new();
	}
	let mut used = 1;
	let chars: Vec<_> = text
		.chars()
		.rev()
		.take_while(|c| {
			used += c.width().unwrap_or(0);
			used <= width
		})
		.collect();
	format!("…{}", chars.into_iter().rev().collect::<String>())
}

/// Clip our own SGR-styled text by terminal cells, leaving the last column free
/// so wide characters and long paths cannot wrap into unowned lines.
fn clip(text: &str, width: usize) -> String {
	let mut result = String::new();
	let mut chars = text.chars();
	let mut used = 0;
	let mut styled = false;
	while let Some(c) = chars.next() {
		if c == '\x1b' {
			styled = true;
			result.push(c);
			for c in chars.by_ref() {
				result.push(c);
				if c == 'm' {
					break;
				}
			}
		} else {
			let next = c.width().unwrap_or(0);
			if used + next > width {
				break;
			}
			used += next;
			result.push(c);
		}
	}
	if styled {
		result.push_str("\x1b[0m");
	}
	result
}

#[derive(Default)]
struct Screen {
	widths: Vec<usize>,
}

impl Screen {
	fn clear(&mut self, out: &mut impl Write, columns: usize, rows: usize) -> io::Result<()> {
		let occupied: usize = self
			.widths
			.iter()
			.map(|w| w.saturating_sub(1) / columns.max(1) + 1)
			.sum();
		let occupied = occupied.min(rows.saturating_sub(1));
		if occupied > 0 {
			write!(out, "\r\x1b[{occupied}A\x1b[J")?;
		}
		self.widths.clear();
		Ok(())
	}

	fn draw(
		&mut self,
		out: &mut impl Write,
		lines: &[String],
		columns: usize,
		rows: usize,
	) -> io::Result<()> {
		let mut frame = Vec::new();
		self.clear(&mut frame, columns, rows)?;
		for line in lines {
			write!(frame, "\r{line}\x1b[K\r\n")?;
			self.widths.push(clean(line).width());
		}
		out.write_all(&frame)?;
		out.flush()
	}
}

struct State {
	model:   Model,
	screen:  Screen,
	stopped: bool,
	tick:    usize,
}
struct Shared {
	state: Mutex<State>,
	wake:  Condvar,
	color: bool,
}

pub struct Live {
	shared: Arc<Shared>,
	worker: Mutex<Option<JoinHandle<()>>>,
}

fn dimensions() -> (usize, usize) {
	terminal_size::terminal_size_of(io::stderr())
		.map_or((80, 24), |(w, h)| (usize::from(w.0), usize::from(h.0)))
}

impl Live {
	pub fn new(color: bool) -> Self {
		let shared = Arc::new(Shared {
			state: Mutex::new(State {
				model:   Model::default(),
				screen:  Screen::default(),
				stopped: false,
				tick:    0,
			}),
			wake: Condvar::new(),
			color,
		});
		let painter = Arc::clone(&shared);
		let worker = thread::spawn(move || {
			let mut state = painter.state.lock();
			while !state.stopped {
				let (columns, rows) = dimensions();
				let tick = state.tick;
				let lines = state.model.lines(columns, rows, painter.color, tick);
				if state
					.screen
					.draw(&mut io::stderr().lock(), &lines, columns, rows)
					.is_err()
				{
					break;
				}
				state.tick += 1;
				painter.wake.wait_for(&mut state, TICK);
			}
		});
		Self { shared, worker: Mutex::new(Some(worker)) }
	}

	pub fn update(&self, event: Event<'_>) {
		let mut state = self.shared.state.lock();
		if !state.stopped {
			state.model.update(event);
		}
	}

	/// Print diagnostics above the panel, sharing the painter's output lock.
	pub fn message(&self, message: &str) {
		let mut state = self.shared.state.lock();
		let (columns, rows) = dimensions();
		let mut out = io::stderr().lock();
		let _ = state.screen.clear(&mut out, columns, rows);
		let _ = writeln!(out, "{message}");
		if !state.stopped {
			let tick = state.tick;
			let lines = state.model.lines(columns, rows, self.shared.color, tick);
			let _ = state.screen.draw(&mut out, &lines, columns, rows);
		}
		let _ = out.flush();
	}

	pub fn stop(&self) {
		// Keep the join handle lock until cleanup is complete: repeated callers
		// cannot return early while the painter still owns terminal rows.
		let mut worker = self.worker.lock();
		self.shared.state.lock().stopped = true;
		self.shared.wake.notify_one();
		if let Some(handle) = worker.take() {
			let _ = handle.join();
		}
		let mut state = self.shared.state.lock();
		let (columns, rows) = dimensions();
		let mut out = io::stderr().lock();
		let _ = state.screen.clear(&mut out, columns, rows);
		let _ = out.flush();
	}
}

impl Drop for Live {
	fn drop(&mut self) {
		self.stop();
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn redraw_and_clear_only_move_over_owned_rows() {
		let mut screen = Screen::default();
		let mut out = Vec::new();
		screen
			.draw(&mut out, &["one".into(), "two".into()], 80, 24)
			.unwrap();
		out.clear();
		screen.draw(&mut out, &["shorter".into()], 80, 24).unwrap();
		assert_eq!(String::from_utf8(out).unwrap(), "\r\x1b[2A\x1b[J\rshorter\x1b[K\r\n");
		let mut out = Vec::new();
		screen.clear(&mut out, 80, 24).unwrap();
		assert_eq!(out, b"\r\x1b[1A\x1b[J");
		out.clear();
		screen.clear(&mut out, 80, 24).unwrap();
		assert!(out.is_empty());
	}
}
