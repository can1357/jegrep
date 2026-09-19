//! Minimal Jev (`TypeSafe` System One) HTTP client over ureq.
//!
//! Two providers, one request shape: a `state` the model looks at, and a map of
//! typed questions (noul = yes/no probability, choice = distribution over
//! options). Answers come back under the same keys. Retries 429/529/5xx with
//! backoff.

use std::{collections::BTreeMap, fmt, thread, time::Duration};

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Endpoint {
	Openrouter,
	Typesafe,
}

impl Endpoint {
	pub const fn key_name(self) -> &'static str {
		match self {
			Self::Openrouter => "OPENROUTER_API_KEY",
			Self::Typesafe => "TYPESAFE_API_KEY",
		}
	}

	const fn url(self) -> &'static str {
		match self {
			Self::Openrouter => "https://openrouter.ai/api/alpha/decisions",
			Self::Typesafe => "https://api.typesafe.ai/v1/systemone",
		}
	}
}

struct Provider {
	url:  String,
	auth: String,
}

/// Load both keys independently, preserving process-env precedence for each.
fn providers(
	preferred: Option<Endpoint>,
	mut lookup: impl FnMut(&str) -> Result<String, String>,
) -> Result<Vec<Provider>, String> {
	let first = preferred.unwrap_or(Endpoint::Openrouter);
	let second = match first {
		Endpoint::Openrouter => Endpoint::Typesafe,
		Endpoint::Typesafe => Endpoint::Openrouter,
	};
	let mut providers = Vec::new();
	for endpoint in [first, second] {
		match lookup(endpoint.key_name()) {
			Ok(key) => {
				providers.push(Provider { url: endpoint.url().into(), auth: format!("Bearer {key}") })
			},
			Err(e) if preferred == Some(endpoint) => return Err(e),
			Err(_) => {},
		}
	}
	if providers.is_empty() {
		return Err("set OPENROUTER_API_KEY or TYPESAFE_API_KEY in the environment or ~/.env".into());
	}
	Ok(providers)
}
/// Published price: $42 per billion input tokens; output tokens are free.
pub const USD_PER_INPUT_TOKEN: f64 = 42.0 / 1e9;
/// Total retries and failover attempts performed by every client in this
/// process.
pub static RETRIES: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
/// Physical HTTP attempts, including question chunks, retries, and failovers.
pub static HTTP_ATTEMPTS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
static WARNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

#[derive(Serialize, Clone, Debug)]
pub struct NoulCriteria {
	#[serde(rename = "true")]
	pub yes: String,
	#[serde(rename = "false")]
	pub no:  String,
}

#[derive(Serialize, Clone, Debug)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Question {
	Noul {
		instructions: Value,
		#[serde(skip_serializing_if = "Option::is_none")]
		criteria:     Option<NoulCriteria>,
	},
	Choice {
		instructions: Value,
		/// option name -> description (or null when the name is self-explanatory)
		criteria:     BTreeMap<String, Value>,
	},
}

#[derive(Serialize)]
struct RequestBody<'a> {
	state:     &'a Value,
	model:     &'a str,
	questions: &'a BTreeMap<String, Question>,
}

#[derive(Deserialize, Debug, Default, Clone, Copy)]
pub struct Usage {
	pub input_tokens:  u64,
	pub output_tokens: u64,
}

#[derive(Deserialize, Debug)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Answer {
	Noul { noul: f64 },
	Choice { choice: String, probabilities: BTreeMap<String, f64>, confidence: f64 },
	Score { score: f64 },
}

#[derive(Deserialize, Debug)]
pub struct Response {
	pub model:   String,
	pub answers: BTreeMap<String, Answer>,
	pub usage:   Usage,
}

impl Response {
	pub fn noul(&self, key: &str) -> Option<f64> {
		match self.answers.get(key) {
			Some(Answer::Noul { noul }) => Some(*noul),
			_ => None,
		}
	}

	pub fn choice(&self, key: &str) -> Option<(&BTreeMap<String, f64>, f64)> {
		match self.answers.get(key) {
			Some(Answer::Choice { probabilities, confidence, .. }) => {
				Some((probabilities, *confidence))
			},
			_ => None,
		}
	}
}

#[derive(Debug, Clone)]
pub enum Error {
	Status(u16, String),
	Transport(String),
	Decode(String),
}

impl Error {
	fn can_failover(&self) -> bool {
		match self {
			Self::Status(status, _) => {
				matches!(status, 401 | 402 | 403 | 408 | 429) || (500..600).contains(status)
			},
			Self::Transport(_) | Self::Decode(_) => true,
		}
	}
}

impl fmt::Display for Error {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			Self::Status(code, body) => write!(f, "HTTP {code}: {body}"),
			Self::Transport(e) => write!(f, "transport: {e}"),
			Self::Decode(e) => write!(f, "decode: {e}"),
		}
	}
}

pub struct Client {
	agent:          ureq::Agent,
	providers:      Vec<Provider>,
	active:         std::sync::atomic::AtomicUsize,
	model:          String,
	max_retries:    u32,
	/// Opt-in limit for independent Noul questions per HTTP request. Each
	/// logical call has at most two chunks in flight, so strategy --parallel P
	/// permits up to 2*P physical requests. Choice and mixed batches remain
	/// intact.
	question_chunk: Option<usize>,
}

impl Client {
	pub fn new(endpoint: Option<Endpoint>, model: String) -> Result<Self, String> {
		let providers = providers(endpoint, crate::env::api_key)?;
		let question_chunk = match std::env::var("JEGREP_QUESTION_CHUNK") {
			Ok(value) => Some(
				value
					.parse::<usize>()
					.ok()
					.filter(|n| *n > 0)
					.ok_or("JEGREP_QUESTION_CHUNK must be a positive integer")?,
			),
			Err(std::env::VarError::NotPresent) => None,
			Err(_) => return Err("JEGREP_QUESTION_CHUNK must be a positive integer".into()),
		};
		let agent = ureq::Agent::config_builder()
			.http_status_as_error(false)
			.timeout_global(Some(Duration::from_secs(120)))
			.build()
			.new_agent();
		Ok(Self {
			agent,
			providers,
			active: std::sync::atomic::AtomicUsize::new(0),
			model,
			max_retries: 6,
			question_chunk,
		})
	}

	pub fn system_one(
		&self,
		state: &Value,
		questions: &BTreeMap<String, Question>,
	) -> Result<Response, Error> {
		let Some(chunk_size) = self.question_chunk.filter(|n| questions.len() > *n) else {
			return self.system_one_request(state, questions);
		};
		// Choice probabilities depend on the complete option set. Leave mixed
		// batches together too, rather than assuming independence across types.
		if !questions
			.values()
			.all(|q| matches!(q, Question::Noul { .. }))
		{
			return self.system_one_request(state, questions);
		}
		let entries: Vec<_> = questions.iter().collect();
		let chunks: Vec<BTreeMap<String, Question>> = entries
			.chunks(chunk_size)
			.map(|chunk| {
				chunk
					.iter()
					.map(|(key, question)| ((*key).clone(), (*question).clone()))
					.collect()
			})
			.collect();
		let mut merged =
			Response { model: String::new(), answers: BTreeMap::new(), usage: Usage::default() };
		// Scoped waves keep fan-out bounded independently of batch size, and
		// wait for already-started siblings before returning any failure.
		for wave in chunks.chunks(2) {
			let responses = thread::scope(|scope| {
				let workers: Vec<_> = wave
					.iter()
					.map(|chunk| scope.spawn(move || self.system_one_request(state, chunk)))
					.collect();
				workers
					.into_iter()
					.map(|worker| worker.join().expect("question chunk worker panicked"))
					.collect::<Vec<_>>()
			});
			for response in responses {
				let response = response?;
				if merged.model.is_empty() {
					merged.model = response.model;
				}
				merged.usage.input_tokens += response.usage.input_tokens;
				merged.usage.output_tokens += response.usage.output_tokens;
				for (key, answer) in response.answers {
					if merged.answers.insert(key.clone(), answer).is_some() {
						return Err(Error::Decode(format!(
							"duplicate answer key across question chunks: {key}"
						)));
					}
				}
			}
		}
		Ok(merged)
	}

	fn system_one_request(
		&self,
		state: &Value,
		questions: &BTreeMap<String, Question>,
	) -> Result<Response, Error> {
		use std::sync::atomic::Ordering;
		let first = self.active.load(Ordering::Relaxed);
		let has_fallback = self.providers.len() > 1;
		let result = self.send(first, state, questions, !has_fallback);
		match result {
			Err(ref error) if has_fallback && error.can_failover() => {
				let fallback = 1 - first;
				RETRIES.fetch_add(1, Ordering::Relaxed);
				let result = self.send(fallback, state, questions, true);
				if result.is_ok() {
					self.active.store(fallback, Ordering::Relaxed);
				}
				result
			},
			result => result,
		}
	}

	fn send(
		&self,
		provider: usize,
		state: &Value,
		questions: &BTreeMap<String, Question>,
		retry: bool,
	) -> Result<Response, Error> {
		let provider = &self.providers[provider];
		let body = RequestBody { state, model: &self.model, questions };
		if std::env::var_os("JEGREP_DUMP").is_some()
			&& let Ok(text) = serde_json::to_string_pretty(&body)
		{
			let shown: String = text.chars().take(6000).collect();
			crate::ui::diagnostic(&format!(
				"──── request ({} bytes) ────\n{shown}{}\n────",
				text.len(),
				if text.len() > 6000 { "\n…" } else { "" }
			));
		}
		let mut attempt = 0u32;
		loop {
			HTTP_ATTEMPTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
			let sent = self
				.agent
				.post(&provider.url)
				.header("Authorization", &provider.auth)
				.header("Content-Type", "application/json")
				.send_json(&body);
			match sent {
				Ok(mut resp) => {
					let status = resp.status().as_u16();
					if status == 200 {
						let parsed = resp
							.body_mut()
							.read_json::<Response>()
							.map_err(|e| Error::Decode(e.to_string()));
						if let (Ok(r), Some(_)) = (&parsed, std::env::var_os("JEGREP_DUMP")) {
							crate::ui::diagnostic(&format!(
								"──── usage: {} questions → {} input tokens ({} per question) ────",
								questions.len(),
								r.usage.input_tokens,
								r.usage.input_tokens / questions.len().max(1) as u64
							));
						}
						return parsed;
					}
					let retry_after = resp
						.headers()
						.get("retry-after")
						.and_then(|v| v.to_str().ok())
						.and_then(|s| s.trim().parse::<f64>().ok());
					let text = resp.body_mut().read_to_string().unwrap_or_default();
					let transient = status == 429 || status == 529 || (500..600).contains(&status);
					if retry && transient && attempt < self.max_retries {
						attempt += 1;
						RETRIES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
						if !WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
							crate::ui::diagnostic(&format!(
								"  ! jev: HTTP {status}, backing off (further retries are counted \
								 silently; see footer)"
							));
						}
						thread::sleep(backoff(attempt, retry_after));
						continue;
					}
					return Err(Error::Status(status, truncate(&text, 300)));
				},
				Err(e) => {
					let transient =
						!matches!(e, ureq::Error::BadUri(_) | ureq::Error::Http(_) | ureq::Error::Tls(_));
					if retry && transient && attempt < self.max_retries {
						attempt += 1;
						RETRIES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
						thread::sleep(backoff(attempt, None));
						continue;
					}
					return Err(Error::Transport(e.to_string()));
				},
			}
		}
	}
}

fn backoff(attempt: u32, retry_after: Option<f64>) -> Duration {
	let base = 0.4 * 2f64.powi(attempt as i32 - 1);
	// cheap deterministic jitter from the address of a stack value
	let jitter = (&attempt as *const u32 as usize % 250) as f64 / 1000.0;
	let secs = retry_after.unwrap_or(0.0).max(base) + jitter;
	Duration::from_secs_f64(secs.min(20.0))
}

fn truncate(s: &str, n: usize) -> String {
	let s = s.trim();
	if s.chars().count() <= n {
		s.to_string()
	} else {
		format!("{}…", s.chars().take(n).collect::<String>())
	}
}

#[cfg(test)]
mod tests {
	use std::{
		io::{Read, Write},
		net::TcpListener,
		sync::atomic::Ordering,
	};

	use super::*;

	#[test]
	fn provider_selection() {
		let both = |name: &str| Ok(name.to_owned());
		let auto = providers(None, both).unwrap();
		assert_eq!(auto[0].url, Endpoint::Openrouter.url());
		assert_eq!(auto[1].auth, "Bearer TYPESAFE_API_KEY");
		let explicit = providers(Some(Endpoint::Typesafe), both).unwrap();
		assert_eq!(explicit[0].url, Endpoint::Typesafe.url());
		let only_typesafe = |name: &str| {
			if name == "TYPESAFE_API_KEY" {
				Ok("direct-key".into())
			} else {
				Err("missing".into())
			}
		};
		assert_eq!(providers(None, only_typesafe).unwrap()[0].url, Endpoint::Typesafe.url());
		assert!(providers(Some(Endpoint::Openrouter), only_typesafe).is_err());
		assert!(providers(None, |_| Err("missing".into())).is_err());
	}

	fn server(status: u16, count: usize, key: &'static str) -> (String, thread::JoinHandle<()>) {
		let listener = TcpListener::bind("127.0.0.1:0").unwrap();
		let url = format!("http://{}", listener.local_addr().unwrap());
		let handle = thread::spawn(move || {
			for _ in 0..count {
				let (mut stream, _) = listener.accept().unwrap();
				stream
					.set_read_timeout(Some(Duration::from_secs(5)))
					.unwrap();
				let mut request = Vec::new();
				let mut buf = [0; 4096];
				loop {
					let n = stream.read(&mut buf).unwrap();
					assert!(n > 0);
					request.extend_from_slice(&buf[..n]);
					if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
						let headers = String::from_utf8_lossy(&request[..end]).to_lowercase();
						let length: usize = headers
							.lines()
							.find_map(|line| line.strip_prefix("content-length:"))
							.unwrap()
							.trim()
							.parse()
							.unwrap();
						if request.len() >= end + 4 + length {
							break;
						}
					}
				}
				let request = String::from_utf8(request).unwrap();
				assert!(
					request
						.to_lowercase()
						.contains(&format!("authorization: bearer {key}"))
				);
				assert!(request.contains("jev-latest"));
				let body = r#"{"model":"jev-latest","answers":{},"usage":{"input_tokens":1,"output_tokens":0}}"#;
				write!(
					stream,
					"HTTP/1.1 {status} Test\r\nContent-Length: {}\r\nContent-Type: \
					 application/json\r\nConnection: close\r\n\r\n{body}",
					body.len()
				)
				.unwrap();
			}
		});
		(url, handle)
	}

	#[test]
	fn failover_uses_matching_key_and_sticks_to_successful_provider() {
		for status in [401, 402, 403, 408, 429, 500, 529] {
			let (primary, p) = server(status, 1, "primary");
			let (fallback, f) = server(200, 2, "fallback");
			let mut client = Client {
				agent:          ureq::Agent::config_builder()
					.http_status_as_error(false)
					.build()
					.new_agent(),
				providers:      vec![],
				active:         std::sync::atomic::AtomicUsize::new(0),
				model:          "jev-latest".into(),
				max_retries:    0,
				question_chunk: None,
			};
			client.providers =
				vec![Provider { url: primary, auth: "Bearer primary".into() }, Provider {
					url:  fallback,
					auth: "Bearer fallback".into(),
				}];
			client.max_retries = 0;
			for _ in 0..2 {
				client.system_one(&Value::Null, &BTreeMap::new()).unwrap();
			}
			assert_eq!(client.active.load(Ordering::Relaxed), 1);
			p.join().unwrap();
			f.join().unwrap();
		}
	}

	#[test]
	fn request_errors_do_not_fail_over() {
		assert!(!Error::Status(400, String::new()).can_failover());
		assert!(!Error::Status(422, String::new()).can_failover());
		assert!(Error::Transport("connection closed".into()).can_failover());
		assert!(Error::Decode("invalid JSON".into()).can_failover());
	}

	fn noul_questions(count: usize) -> BTreeMap<String, Question> {
		(0..count)
			.map(|i| {
				(format!("q{i}"), Question::Noul {
					instructions: Value::String(format!("Question {i}")),
					criteria:     None,
				})
			})
			.collect()
	}

	fn chunk_client(url: String, question_chunk: Option<usize>) -> Client {
		Client {
			agent: ureq::Agent::config_builder()
				.http_status_as_error(false)
				.timeout_global(Some(Duration::from_secs(5)))
				.build()
				.new_agent(),
			providers: vec![Provider { url, auth: "Bearer primary".into() }],
			active: std::sync::atomic::AtomicUsize::new(0),
			model: "jev-latest".into(),
			max_retries: 0,
			question_chunk,
		}
	}

	// The mock accepts concurrently, records the actual HTTP JSON, and reports
	// peak in-flight requests. A bounded accept deadline makes regressions fail
	// locally instead of hanging forever waiting for an expected request.
	fn question_server(
		count: usize,
		status_for: fn(&Value) -> u16,
	) -> (String, thread::JoinHandle<Vec<Value>>, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
		use std::sync::{Arc, atomic::AtomicUsize};
		let listener = TcpListener::bind("127.0.0.1:0").unwrap();
		listener.set_nonblocking(true).unwrap();
		let url = format!("http://{}", listener.local_addr().unwrap());
		let peak = Arc::new(AtomicUsize::new(0));
		let server_peak = peak.clone();
		let handle = thread::spawn(move || {
			let active = Arc::new(AtomicUsize::new(0));
			let mut workers = Vec::new();
			let deadline = std::time::Instant::now() + Duration::from_secs(5);
			while workers.len() < count {
				let mut stream = match listener.accept() {
					Ok((stream, _)) => stream,
					Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
						assert!(std::time::Instant::now() < deadline, "missing mock HTTP request");
						thread::sleep(Duration::from_millis(1));
						continue;
					},
					Err(e) => panic!("mock accept: {e}"),
				};
				let active = active.clone();
				let peak = server_peak.clone();
				workers.push(thread::spawn(move || {
					stream.set_nonblocking(false).unwrap();
					stream
						.set_read_timeout(Some(Duration::from_secs(5)))
						.unwrap();
					let mut bytes = Vec::new();
					let mut buffer = [0; 4096];
					let request: Value = loop {
						let n = stream.read(&mut buffer).unwrap();
						assert!(n > 0);
						bytes.extend_from_slice(&buffer[..n]);
						if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
							let headers = String::from_utf8_lossy(&bytes[..end]).to_lowercase();
							let length: usize = headers
								.lines()
								.find_map(|line| line.strip_prefix("content-length:"))
								.unwrap()
								.trim()
								.parse()
								.unwrap();
							if bytes.len() >= end + 4 + length {
								break serde_json::from_slice(&bytes[end + 4..end + 4 + length]).unwrap();
							}
						}
					};
					peak.fetch_max(active.fetch_add(1, Ordering::SeqCst) + 1, Ordering::SeqCst);
					thread::sleep(Duration::from_millis(40));
					let status = status_for(&request);
					let answers: BTreeMap<_, _> = request["questions"]
						.as_object()
						.unwrap()
						.keys()
						.map(|key| (key, serde_json::json!({"type": "noul", "noul": 0.75})))
						.collect();
					let n = answers.len();
					let body = serde_json::json!({"model": "jev-latest", "answers": answers,
                        "usage": {"input_tokens": 100 + n * 10, "output_tokens": n}})
					.to_string();
					write!(
						stream,
						"HTTP/1.1 {status} Test\r\nContent-Length: {}\r\nContent-Type: \
						 application/json\r\nConnection: close\r\n\r\n{body}",
						body.len()
					)
					.unwrap();
					active.fetch_sub(1, Ordering::SeqCst);
					request
				}));
			}
			workers
				.into_iter()
				.map(|worker| worker.join().unwrap())
				.collect()
		});
		(url, handle, peak)
	}

	#[test]
	fn question_chunks_preserve_state_merge_usage_and_bound_concurrency() {
		let (url, server, peak) = question_server(4, |_| 200);
		let client = chunk_client(url, Some(2));
		let state = serde_json::json!({"content": "the same state in every request"});
		let questions = noul_questions(7);
		let response = client.system_one(&state, &questions).unwrap();
		let requests = server.join().unwrap();
		assert_eq!(requests.len(), 4);
		assert_eq!(peak.load(Ordering::SeqCst), 2);
		let mut seen = BTreeMap::new();
		for request in requests {
			assert_eq!(request["state"], state);
			assert_eq!(request["model"], "jev-latest");
			let subset = request["questions"].as_object().unwrap();
			assert!((1..=2).contains(&subset.len()));
			for (key, value) in subset {
				assert!(seen.insert(key.clone(), value.clone()).is_none());
			}
		}
		assert_eq!(serde_json::to_value(&seen).unwrap(), serde_json::to_value(&questions).unwrap());
		assert_eq!(response.model, "jev-latest");
		assert_eq!(response.answers.len(), 7);
		for key in questions.keys() {
			assert_eq!(response.noul(key), Some(0.75));
		}
		assert_eq!(response.usage.input_tokens, 470);
		assert_eq!(response.usage.output_tokens, 7);
	}

	#[test]
	fn default_small_choice_and_mixed_batches_are_not_split() {
		let choice = Question::Choice {
			instructions: Value::String("Choose a file".into()),
			criteria:     (0..6)
				.map(|i| (format!("option{i}"), Value::Null))
				.collect(),
		};
		let mut mixed = noul_questions(4);
		mixed.insert("choose".into(), choice.clone());
		let only_choice = [("choose".into(), choice)].into_iter().collect();
		for (questions, chunk) in [
			(noul_questions(5), None),
			(noul_questions(2), Some(2)),
			(only_choice, Some(1)),
			(mixed, Some(1)),
		] {
			let (url, server, _) = question_server(1, |_| 200);
			chunk_client(url, chunk)
				.system_one(&Value::Null, &questions)
				.unwrap();
			let requests = server.join().unwrap();
			assert_eq!(requests[0]["questions"], serde_json::to_value(&questions).unwrap());
		}
	}

	#[test]
	fn question_chunk_error_is_propagated_without_sending_later_waves() {
		let (url, server, _) = question_server(2, |request| {
			if request["questions"].get("q0").is_some() {
				400
			} else {
				200
			}
		});
		let result = chunk_client(url, Some(1)).system_one(&Value::Null, &noul_questions(5));
		assert!(matches!(result, Err(Error::Status(400, _))));
		assert_eq!(server.join().unwrap().len(), 2);
	}

	#[test]
	fn question_chunks_keep_provider_failover() {
		let (primary, p, _) = question_server(2, |_| 401);
		let (fallback, f, _) = question_server(3, |_| 200);
		let mut client = chunk_client(primary, Some(1));
		client
			.providers
			.push(Provider { url: fallback, auth: "Bearer fallback".into() });
		let response = client.system_one(&Value::Null, &noul_questions(3)).unwrap();
		assert_eq!(response.answers.len(), 3);
		assert_eq!(response.usage.input_tokens, 330);
		assert_eq!(response.usage.output_tokens, 3);
		assert_eq!(client.active.load(Ordering::Relaxed), 1);
		assert_eq!(p.join().unwrap().len(), 2);
		assert_eq!(f.join().unwrap().len(), 3);
	}
}
