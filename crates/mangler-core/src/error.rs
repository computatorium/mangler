//! The crate-wide error type and the separate, non-error *notes* channel.
//!
//! There are two distinct concepts here, kept deliberately apart:
//!
//! * [`Error`](enum@Error) — a hard failure. Something could not be done; the operation
//!   aborts and the failure propagates via [`Result`]. These drive non-zero
//!   exit codes.
//! * [`Note`] / [`Notes`] — a *non-fatal* observation. A pass declined to
//!   transform a node to stay functionally safe, or wants to surface an
//!   informational remark. Notes are **never** errors: they are collected
//!   alongside a successful result and shown only when the caller asks (e.g.
//!   `--verbose`). They never affect control flow or exit codes.
//!
//! This mirrors the old `Diagnostic` model's `Error`/`Skipped` split, but
//! promotes the hard-error half to a real `thiserror` enum with one propagation
//! path ([`Result`]) and keeps the soft half as plain data.

use std::fmt;
use thiserror::Error;

/// A source span as a half-open `(start, end)` byte range into the input.
pub type Span = (usize, usize);

/// The one error type every fallible operation in the pipeline returns.
///
/// Construct via the variants directly; propagate via [`Result`]. Downstream
/// crates (passgraph, language impls, config) all funnel their failures through
/// this enum so there is a single `?`-able error path.
#[derive(Debug, Error)]
pub enum Error {
    /// A source file could not be parsed. Carries the language tag, a message,
    /// and an optional byte span pointing at the offending location.
    #[error("parse error ({lang}){}: {msg}", fmt_span(*span))]
    Parse {
        /// Which language front-end produced the error (e.g. `"js"`, `"css"`).
        lang: String,
        /// Human-readable description of the failure.
        msg: String,
        /// Optional `(start, end)` byte range of the offending span.
        span: Option<Span>,
    },

    /// An I/O failure (reading input, writing output, etc.).
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// A transform pass failed. Carries the pass id and a message.
    #[error("transform error in pass `{pass}`: {msg}")]
    Transform {
        /// The `pass_id` of the failing pass.
        pass: String,
        /// Human-readable description of the failure.
        msg: String,
    },

    /// A post-transform verification check failed (e.g. an integrity or
    /// round-trip invariant did not hold).
    #[error("verification failed: {0}")]
    Verify(String),

    /// The configuration was invalid or inconsistent.
    #[error("config error: {0}")]
    Config(String),
}

impl Error {
    /// Build a [`Error::Parse`] with no span.
    pub fn parse(lang: impl Into<String>, msg: impl Into<String>) -> Self {
        Error::Parse {
            lang: lang.into(),
            msg: msg.into(),
            span: None,
        }
    }

    /// Build a [`Error::Parse`] attributed to a byte span.
    pub fn parse_at(lang: impl Into<String>, msg: impl Into<String>, span: Span) -> Self {
        Error::Parse {
            lang: lang.into(),
            msg: msg.into(),
            span: Some(span),
        }
    }

    /// Build a [`Error::Transform`].
    pub fn transform(pass: impl Into<String>, msg: impl Into<String>) -> Self {
        Error::Transform {
            pass: pass.into(),
            msg: msg.into(),
        }
    }

    /// Build a [`Error::Verify`].
    pub fn verify(msg: impl Into<String>) -> Self {
        Error::Verify(msg.into())
    }

    /// Build a [`Error::Config`].
    pub fn config(msg: impl Into<String>) -> Self {
        Error::Config(msg.into())
    }
}

fn fmt_span(span: Option<Span>) -> String {
    match span {
        Some((a, b)) => format!(" at {a}..{b}"),
        None => String::new(),
    }
}

/// The crate's single result alias. Every fallible operation returns this so
/// `?` composes across module boundaries with one error type.
pub type Result<T> = std::result::Result<T, Error>;

// ---------------------------------------------------------------------------
// Notes — the non-error channel
// ---------------------------------------------------------------------------

/// A single non-fatal note. **Not** an error: collected alongside a successful
/// result and surfaced only on request. A pass emits one when it deliberately
/// skips a transform to stay functionally safe, or to record an informational
/// remark. Notes never affect control flow or exit codes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Note {
    /// The `pass_id` (or other origin) that produced this note. `None` for
    /// notes raised outside any pass.
    pub origin: Option<String>,
    /// The human-readable message.
    pub message: String,
}

impl Note {
    /// A note with no attributed origin.
    pub fn new(message: impl Into<String>) -> Self {
        Note {
            origin: None,
            message: message.into(),
        }
    }

    /// A note attributed to `origin` (typically a `pass_id`).
    pub fn from(origin: impl Into<String>, message: impl Into<String>) -> Self {
        Note {
            origin: Some(origin.into()),
            message: message.into(),
        }
    }

    /// Attach (or replace) the origin.
    pub fn with_origin(mut self, origin: impl Into<String>) -> Self {
        self.origin = Some(origin.into());
        self
    }
}

impl fmt::Display for Note {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.origin {
            Some(o) => write!(f, "note [{o}]: {}", self.message),
            None => write!(f, "note: {}", self.message),
        }
    }
}

/// A growable collection of [`Note`]s gathered during a run. Passes push notes
/// into this; the driver renders them only when asked (e.g. `--verbose`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Notes {
    notes: Vec<Note>,
}

impl Notes {
    /// An empty collection.
    pub fn new() -> Self {
        Notes::default()
    }

    /// Append a note.
    pub fn push(&mut self, note: Note) {
        self.notes.push(note);
    }

    /// Move every note out of `other` into this collection.
    pub fn extend(&mut self, other: Notes) {
        self.notes.extend(other.notes);
    }

    /// Whether no notes have been recorded.
    pub fn is_empty(&self) -> bool {
        self.notes.is_empty()
    }

    /// How many notes have been recorded.
    pub fn len(&self) -> usize {
        self.notes.len()
    }

    /// Iterate the notes in insertion order.
    pub fn iter(&self) -> std::slice::Iter<'_, Note> {
        self.notes.iter()
    }
}

impl IntoIterator for Notes {
    type Item = Note;
    type IntoIter = std::vec::IntoIter<Note>;
    fn into_iter(self) -> Self::IntoIter {
        self.notes.into_iter()
    }
}

impl<'a> IntoIterator for &'a Notes {
    type Item = &'a Note;
    type IntoIter = std::slice::Iter<'a, Note>;
    fn into_iter(self) -> Self::IntoIter {
        self.notes.iter()
    }
}

impl FromIterator<Note> for Notes {
    fn from_iter<I: IntoIterator<Item = Note>>(iter: I) -> Self {
        Notes {
            notes: iter.into_iter().collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_error_display_with_and_without_span() {
        let e = Error::parse("js", "unexpected token");
        assert_eq!(e.to_string(), "parse error (js): unexpected token");
        let e = Error::parse_at("css", "bad rule", (10, 14));
        assert_eq!(e.to_string(), "parse error (css) at 10..14: bad rule");
    }

    #[test]
    fn transform_verify_config_display() {
        assert_eq!(
            Error::transform("strings", "no slot").to_string(),
            "transform error in pass `strings`: no slot"
        );
        assert_eq!(
            Error::verify("hash mismatch").to_string(),
            "verification failed: hash mismatch"
        );
        assert_eq!(
            Error::config("bad seed").to_string(),
            "config error: bad seed"
        );
    }

    #[test]
    fn io_error_converts_via_question_mark() {
        fn inner() -> Result<()> {
            Err(std::io::Error::other("disk gone"))?;
            Ok(())
        }
        let e = inner().unwrap_err();
        assert!(matches!(e, Error::Io(_)));
        assert!(e.to_string().starts_with("io error:"));
    }

    #[test]
    fn note_display_with_and_without_origin() {
        assert_eq!(Note::new("skipped node").to_string(), "note: skipped node");
        assert_eq!(
            Note::from("opaque", "unsupported shape").to_string(),
            "note [opaque]: unsupported shape"
        );
        assert_eq!(
            Note::new("x").with_origin("p").to_string(),
            "note [p]: x"
        );
    }

    #[test]
    fn notes_collection_basics() {
        let mut n = Notes::new();
        assert!(n.is_empty());
        n.push(Note::new("a"));
        n.push(Note::from("p", "b"));
        assert_eq!(n.len(), 2);
        assert!(!n.is_empty());

        let mut m = Notes::new();
        m.push(Note::new("c"));
        n.extend(m);
        assert_eq!(n.len(), 3);

        let msgs: Vec<_> = n.iter().map(|x| x.message.clone()).collect();
        assert_eq!(msgs, vec!["a", "b", "c"]);
    }

    #[test]
    fn notes_from_iter_and_into_iter() {
        let notes: Notes = vec![Note::new("a"), Note::new("b")].into_iter().collect();
        assert_eq!(notes.len(), 2);
        let collected: Vec<String> = (&notes).into_iter().map(|n| n.message.clone()).collect();
        assert_eq!(collected, vec!["a", "b"]);
        let owned: Vec<Note> = notes.into_iter().collect();
        assert_eq!(owned.len(), 2);
    }
}
