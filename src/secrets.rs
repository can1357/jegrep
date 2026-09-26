//! Credential material detected from file *content*, independent of the name
//! lists in [`crate::tree`]. Runs in [`crate::questions::read_text`] on the
//! bytes about to leave the machine, so a service-account key saved as
//! `my-project-4f3a1c.json` or an AWS profile in `aws_credentials.txt` is
//! withheld the same way `.env` is.
//!
//! Markers are deliberately shaped so parsers and docs that merely *mention*
//! a format are not flagged: a PEM header only counts when a real newline
//! follows it (source literals end in a quote or `\n` escape), `"private_key"`
//! only when its value opens a PEM block, `aws_secret_access_key` only when a
//! key-sized value follows, and token prefixes only when a key-length
//! alphanumeric run follows. Fixtures holding real key material do match;
//! `--allow-secrets` turns the check off for such repositories.

use std::{
	path::{Path, PathBuf},
	sync::atomic::{AtomicBool, Ordering},
};

use memchr::memmem;
use parking_lot::Mutex;

static ALLOW: AtomicBool = AtomicBool::new(false);
/// Paths withheld so far; a set because strategies may read a file more than
/// once (hybrid runs, sniff-then-read) and the footer counts files.
static WITHHELD: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

/// `--allow-secrets`: send credential-looking files instead of withholding
/// them.
pub fn allow(yes: bool) {
	ALLOW.store(yes, Ordering::Relaxed);
}

/// Distinct files withheld by [`withhold`] in this process, for the run
/// footer.
pub fn withheld() -> usize {
	WITHHELD.lock().len()
}

/// Decide whether `text`, read from `path`, may be sent. `Some(marker)` means
/// it was withheld (and counted once per path); `None` means it is clean or
/// `--allow-secrets` is on.
pub fn withhold(path: &Path, text: &[u8]) -> Option<&'static str> {
	if ALLOW.load(Ordering::Relaxed) {
		return None;
	}
	let marker = marker(text)?;
	let mut withheld = WITHHELD.lock();
	if !withheld.iter().any(|p| p == path) {
		withheld.push(path.to_owned());
	}
	Some(marker)
}

/// The first credential marker found in `text`, if any.
pub fn marker(text: &[u8]) -> Option<&'static str> {
	if pem_private_key(text) {
		return Some("private key");
	}
	if json_pair(text, b"\"type\"", b"\"service_account\"")
		|| json_pair(text, b"\"private_key\"", b"\"-----BEGIN")
	{
		return Some("service-account json");
	}
	if aws_secret(text) || aws_access_key_id(text) {
		return Some("aws credentials");
	}
	if kubeconfig_key(text) {
		return Some("kubeconfig client key");
	}
	if api_token(text) {
		return Some("api token");
	}
	None
}

const fn is_alnum(b: u8) -> bool {
	b.is_ascii_alphanumeric()
}

const fn is_base64(b: u8) -> bool {
	is_alnum(b) || matches!(b, b'+' | b'/' | b'=' | b'-' | b'_')
}

const fn skip_ws(text: &[u8], mut i: usize) -> usize {
	while i < text.len() && matches!(text[i], b' ' | b'\t' | b'\n' | b'\r') {
		i += 1;
	}
	i
}

/// Length of the run of bytes satisfying `pred` starting at `i`.
fn run(text: &[u8], i: usize, pred: fn(u8) -> bool) -> usize {
	text[i.min(text.len())..]
		.iter()
		.take_while(|&&b| pred(b))
		.count()
}

/// `-----BEGIN <label>-----` where the label names a private key and the
/// header ends the line: the shape of a key file, not of a string literal.
fn pem_private_key(text: &[u8]) -> bool {
	const OPEN: &[u8] = b"-----BEGIN ";
	let finder = memmem::Finder::new(OPEN);
	for start in finder.find_iter(text) {
		let label_start = start + OPEN.len();
		let window = &text[label_start..text.len().min(label_start + 48)];
		let Some(close) = memmem::find(window, b"-----") else {
			continue;
		};
		let label = &window[..close];
		if memmem::find(label, b"PRIVATE KEY").is_none() {
			continue;
		}
		let after = label_start + close + 5;
		if after >= text.len() || matches!(text[after], b'\n' | b'\r') {
			return true;
		}
	}
	false
}

/// `"key" : <value_prefix>` anywhere in the text, JSON whitespace allowed.
fn json_pair(text: &[u8], key: &[u8], value_prefix: &[u8]) -> bool {
	memmem::find_iter(text, key).any(|start| {
		let i = skip_ws(text, start + key.len());
		if text.get(i) != Some(&b':') {
			return false;
		}
		let i = skip_ws(text, i + 1);
		text[i..].starts_with(value_prefix)
	})
}

/// `aws_secret_access_key = <40-char key>` (credentials file or exported
/// variable), not the bare identifier an SDK signature uses.
fn aws_secret(text: &[u8]) -> bool {
	[b"aws_secret_access_key".as_slice(), b"AWS_SECRET_ACCESS_KEY".as_slice()]
		.into_iter()
		.any(|name| {
			memmem::find_iter(text, name).any(|start| {
				let i = skip_ws(text, start + name.len());
				if !matches!(text.get(i), Some(b'=' | b':')) {
					return false;
				}
				let mut i = skip_ws(text, i + 1);
				if matches!(text.get(i), Some(b'"' | b'\'')) {
					i += 1;
				}
				run(text, i, |b| is_alnum(b) || matches!(b, b'+' | b'/')) >= 32
			})
		})
}

/// A standalone `AKIA` + 16 upper-case alphanumerics access key id.
fn aws_access_key_id(text: &[u8]) -> bool {
	memmem::find_iter(text, b"AKIA").any(|start| {
		let bounded = start == 0 || !is_alnum(text[start - 1]);
		let id = run(text, start + 4, |b| b.is_ascii_uppercase() || b.is_ascii_digit());
		bounded && id == 16
	})
}

/// kubeconfig `client-key-data: <base64 key>`.
fn kubeconfig_key(text: &[u8]) -> bool {
	const KEY: &[u8] = b"client-key-data:";
	memmem::find_iter(text, KEY).any(|start| {
		let i = skip_ws(text, start + KEY.len());
		run(text, i, is_base64) >= 20
	})
}

/// Well-known token prefixes at a word boundary, followed by a token that
/// contains a key-length (20+) alphanumeric run: `ghp_…`, `xoxb-…`,
/// `sk-proj-…`. Kebab-case identifiers such as `sk-fading-circle` do not.
fn api_token(text: &[u8]) -> bool {
	const PREFIXES: &[&[u8]] = &[
		b"ghp_",
		b"gho_",
		b"ghu_",
		b"ghs_",
		b"ghr_",
		b"github_pat_",
		b"xoxb-",
		b"xoxp-",
		b"xoxa-",
		b"xoxr-",
		b"xoxs-",
		b"sk-",
	];
	PREFIXES.iter().any(|prefix| {
		memmem::find_iter(text, prefix).any(|start| {
			let bounded =
				start == 0 || !(is_alnum(text[start - 1]) || matches!(text[start - 1], b'_' | b'-'));
			if !bounded {
				return false;
			}
			let body_start = start + prefix.len();
			let body = &text[body_start..body_start + run(text, body_start, is_base64)];
			body
				.split(|b| !is_alnum(*b))
				.any(|segment| segment.len() >= 20)
		})
	})
}

#[cfg(test)]
mod tests {
	use super::marker;

	#[test]
	fn credential_files_saved_under_any_name_are_flagged() {
		let cases: [(&str, &[u8]); 8] = [
			("private key", b"-----BEGIN PRIVATE KEY-----\nMIIEvQIBADANBgkqhkiG9w0BAQEFAASC\n"),
			("private key", b"-----BEGIN OPENSSH PRIVATE KEY-----\r\nb3BlbnNzaC1rZXktdjEAAAAABG5vbmUA\r\n"),
			("private key", b"-----BEGIN RSA PRIVATE KEY-----\nProc-Type: 4,ENCRYPTED\nDEK-Info: AES-128-CBC\n"),
			(
				"service-account json",
				b"{\"type\": \"service_account\",\"project_id\":\"my-project-4f3a1c\"}",
			),
			(
				"service-account json",
				b"{\n  \"private_key\" :\n \"-----BEGIN PRIVATE KEY-----\\nMIIE\\n\"\n}",
			),
			(
				"aws credentials",
				b"[default]\naws_access_key_id = AKIAIOSFODNN7EXAMPLE\naws_secret_access_key = wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY\n",
			),
			(
				"kubeconfig client key",
				b"apiVersion: v1\nusers:\n- name: admin\n  user:\n    client-key-data: LS0tLS1CRUdJTiBSU0EgUFJJVkFURSBLRVktLS0tLQo=\n",
			),
			("api token", b"export GITHUB_TOKEN=ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghij\n"),
		];
		for (expected, text) in cases {
			assert_eq!(marker(text), Some(expected), "{}", String::from_utf8_lossy(text));
		}
		assert_eq!(
			marker(b"SLACK=xoxb-1234567890-1234567890123-AbCdEfGhIjKlMnOpQrStUvWx"),
			Some("api token")
		);
		assert_eq!(
			marker(b"OPENAI_API_KEY=sk-proj-abcdefghijklmnopqrstuvwxyz0123456789"),
			Some("api token")
		);
		assert_eq!(
			marker(b"key: \"AWS_SECRET_ACCESS_KEY=abcdefghijklmnopqrstuvwxyz0123456789ABCD\""),
			Some("aws credentials")
		);
	}

	#[test]
	fn code_that_mentions_secret_formats_is_not_flagged() {
		let clean: [&[u8]; 9] = [
			// PEM parser: header is a string literal, not a key file.
			b"const HEADER: &str = \"-----BEGIN RSA PRIVATE KEY-----\";\nif line == HEADER {\n",
			b"const HEADER = \"-----BEGIN PRIVATE KEY-----\\n\" + body;\n",
			// Public material and certificates are not withheld by content.
			b"-----BEGIN PUBLIC KEY-----\nMIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8AMIIBCgKCAQEA\n",
			b"-----BEGIN CERTIFICATE-----\nMIIDdzCCAl+gAwIBAgIEbXpUZTANBgkqhkiG9w0BAQsFADBt\n",
			// SDK signature and schema, not a credentials file.
			b"def __init__(self, aws_access_key_id=None, aws_secret_access_key=None):\n",
			b"{\"private_key\": {\"type\": \"string\"}, \"type\": \"object\"}\n",
			// Kebab-case identifiers share token prefixes.
			b".sk-fading-circle .sk-circle-bounce-something-else { animation: sk-circleBounceDelay }\n",
			b"let task = tasks-of-the-day-with-a-long-name-here;\n",
			// A short placeholder, not a key.
			b"client-key-data: REDACTED\n",
		];
		for text in clean {
			assert_eq!(marker(text), None, "{}", String::from_utf8_lossy(text));
		}
	}
}
