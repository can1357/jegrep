//! Fixed-size worker pool. Strategies decide *what* to ask and in what order;
//! workers only do blocking I/O: HTTP, and file reads.
//!
//! `id` is opaque to the pool — a strategy uses it to map an outcome back to
//! whatever bookkeeping it keeps (a batch of entries, a node index, ...).

use std::{
	collections::BTreeMap,
	path::PathBuf,
	sync::{
		Arc,
		mpsc::{self, Receiver, Sender},
	},
	thread::{self, JoinHandle},
	time::{Duration, Instant},
};

use parking_lot::Mutex;
use serde_json::Value;

use crate::{
	jev::{self, Client, Question, Response},
	questions::{FileErr, FilePrep, ReadText, prepare_file, read_text},
};

pub enum Job {
	/// Any System One request.
	Ask { id: u64, state: Value, questions: BTreeMap<String, Question> },
	/// Read a file's first bytes, build the standard content check (relevance
	/// Noul + line-range Choice), and ask. Convenience for the common case.
	File { id: u64, path: PathBuf, rel: String, size: u64 },
	/// Just read a file's first `max_bytes` (binary-checked, UTF-8 lossy). No
	/// request.
	Read { id: u64, path: PathBuf, max_bytes: usize },
}

pub enum Outcome {
	Ask { id: u64, result: Result<Response, jev::Error>, elapsed: Duration },
	File { id: u64, result: Result<(FilePrep, Response), FileErr>, elapsed: Duration },
	Read { id: u64, result: Result<ReadText, FileErr>, elapsed: Duration },
}

pub struct FileOpts {
	pub query:     String,
	pub max_bytes: usize,
	pub ranges:    usize,
}

pub struct Pool {
	tx:      Option<Sender<Job>>,
	rx:      Receiver<Outcome>,
	handles: Vec<JoinHandle<()>>,
}

impl Pool {
	pub fn new(client: Arc<Client>, workers: usize, fopts: Arc<FileOpts>) -> Self {
		let (tx, jrx) = mpsc::channel::<Job>();
		let jrx = Arc::new(Mutex::new(jrx));
		let (otx, rx) = mpsc::channel::<Outcome>();
		let handles = (0..workers.max(1))
			.map(|_| {
				let jrx = Arc::clone(&jrx);
				let otx = otx.clone();
				let client = Arc::clone(&client);
				let fopts = Arc::clone(&fopts);
				thread::spawn(move || {
					loop {
						let job = {
							let guard = jrx.lock();
							guard.recv()
						};
						let Ok(job) = job else { break };
						let t = Instant::now();
						let out = match job {
							Job::Ask { id, state, questions } => Outcome::Ask {
								id,
								result: client.system_one(&state, &questions),
								elapsed: t.elapsed(),
							},
							Job::File { id, path, rel, size } => {
								let result = prepare_file(
									&path,
									&rel,
									size,
									&fopts.query,
									fopts.max_bytes,
									fopts.ranges,
								)
								.and_then(|prep| {
									client
										.system_one(&prep.state, &prep.questions)
										.map_err(|e| FileErr::Api(e.to_string()))
										.map(|r| (prep, r))
								});
								Outcome::File { id, result, elapsed: t.elapsed() }
							},
							Job::Read { id, path, max_bytes } => Outcome::Read {
								id,
								result: read_text(&path, max_bytes),
								elapsed: t.elapsed(),
							},
						};
						if otx.send(out).is_err() {
							break;
						}
					}
				})
			})
			.collect();
		Self { tx: Some(tx), rx, handles }
	}

	pub fn submit(&self, job: Job) {
		if let Some(tx) = &self.tx {
			let _ = tx.send(job);
		}
	}

	/// Block until the next outcome.
	pub fn recv(&self) -> Outcome {
		self.rx.recv().expect("worker pool died")
	}

	/// Non-blocking poll.
	pub fn try_recv(&self) -> Option<Outcome> {
		self.rx.try_recv().ok()
	}
}

impl Drop for Pool {
	fn drop(&mut self) {
		self.tx.take();
		for h in self.handles.drain(..) {
			let _ = h.join();
		}
	}
}
