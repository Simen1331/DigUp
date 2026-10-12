//! How the panel, Waybar and the CLI talk to the daemon: one JSON object per line each way over a Unix socket,
//! `$XDG_RUNTIME_DIR/showme.sock`.
//!
//! ```text
//! → {"op":"search","query":"lease pets","limit":20}
//! ← {"ok":true,"hits":[{"path":"/home/you/lease.pdf","kind":"pdf","segment":"page","loc":2,…}],"vectors":812,"ms":31}
//! → {"op":"status"}
//! ← {"ok":true,"status":{"state":"indexing","pending":120,…}}
//! → {"op":"watch"}                       (the status now, then again each time it changes, until the client leaves)
//! → {"op":"segment_text","segment_id":7731,"code":false}   ← {"ok":true,"text":"…"}
//! → {"op":"prewarm"}                     (the panel opened: load the query encoder and the vectors now)
//! → {"op":"download"}                    (the model and pdfium; progress comes in the status)
//! → {"op":"pause"} | {"op":"resume"} | {"op":"sync"} | {"op":"reload"} | {"op":"quit"}
//! ← {"ok":true}  or  {"ok":false,"error":"…"}
//! ```

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

use crate::model::FileKind;
use crate::searcher::SearchHit;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

/// The daemon's socket.
pub fn socket_path() -> PathBuf {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join(format!("showme-{}", unsafe { libc_getuid() })));
    runtime.join("showme.sock")
}

unsafe extern "C" {
    #[link_name = "getuid"]
    fn libc_getuid() -> u32;
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    Search {
        query: String,
        #[serde(default = "default_limit")]
        limit: usize,
        /// Only these kinds ("pdf", "image"…); empty: all.
        #[serde(default)]
        kinds: Vec<String>,
        /// The last word is still being typed: match it as a prefix.
        #[serde(default)]
        prefix_last: bool,
        /// Words only, no meaning: answers at once, without the query encoder (search-as-you-type).
        #[serde(default)]
        words_only: bool,
        /// Echoed in the reply, so a client can drop replies to keystrokes it has moved past. A full search that
        /// hasn't started when a newer one comes on the same connection is answered `superseded`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        seq: Option<u64>,
    },
    /// The text of a page, a passage or a stretch of code, as it's written (for the preview).
    SegmentText {
        segment_id: i64,
        #[serde(default)]
        code: bool,
    },
    Prewarm,
    Download,
    Status,
    Watch,
    Pause,
    Resume,
    Sync,
    Reload,
    Quit,
}

fn default_limit() -> usize {
    20
}

/// One result, as the panel shows it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Hit {
    pub path: String,
    pub kind: String,
    /// What the hit points at: page, page_image, chunk, image, ocr, frame, audio, lines, cells, name.
    pub segment: String,
    /// For `segment_text`.
    #[serde(default)]
    pub segment_id: i64,
    pub score: f64,
    /// Results scoring under half the top one are "weaker matches" (folded away by default).
    pub strong: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub z: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub keyword_rank: Option<usize>,
    /// A page, a second into a recording or video, a chunk, or a code stretch's first line.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub loc: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub loc_end: Option<f64>,
    /// Code: the line to open at.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub focus: Option<f64>,
    /// The words that matched are marked « ».
    #[serde(skip_serializing_if = "Option::is_none")]
    pub excerpt: Option<String>,
    pub modified: f64,
    pub size: i64,
    /// What indexing learned: width, height, taken, pages, duration…
    #[serde(default)]
    pub info: std::collections::BTreeMap<String, String>,
}

impl Hit {
    /// Back into the engine's form, for printing.
    pub fn to_search_hit(&self) -> SearchHit {
        SearchHit {
            path: self.path.clone(),
            kind: self.kind.parse().unwrap_or(FileKind::Doc),
            score: self.score,
            z: self.z,
            cosine: None,
            keyword_rank: self.keyword_rank,
            segment: crate::model::SegmentKind::parse(&self.segment).unwrap_or(crate::model::SegmentKind::Name),
            segment_id: self.segment_id,
            loc: self.loc,
            loc_end: self.loc_end,
            excerpt: self.excerpt.clone(),
            modified: self.modified,
            size: self.size,
            info: self.info.clone(),
            focus: self.focus,
        }
    }

    pub fn new(hit: &SearchHit, strong: bool) -> Hit {
        Hit {
            path: hit.path.clone(),
            kind: hit.kind.as_str().into(),
            segment: hit.segment.as_str().into(),
            segment_id: hit.segment_id,
            score: hit.score,
            strong,
            z: hit.z,
            keyword_rank: hit.keyword_rank,
            loc: hit.loc,
            loc_end: hit.loc_end,
            focus: hit.focus,
            excerpt: hit.excerpt.clone(),
            modified: hit.modified,
            size: hit.size,
            info: hit.info.clone(),
        }
    }
}

/// What the daemon is doing, for Waybar, the panel and `showme status`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Status {
    /// starting | syncing | indexing | finishing (the rest of long files) | idle | paused | waiting | error
    pub state: String,
    /// Why it waits ("model", "on battery", "power saver"), or what went wrong.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    pub files: usize,
    pub searchable: usize,
    pub pending: usize,
    pub vectors: usize,
    /// Long files with pages or chunks still to read.
    pub unfinished: usize,
    pub code_files: usize,
    pub code_pending: usize,
    /// The file being read now, and how far the current run has got.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current: Option<String>,
    pub done: usize,
    pub total: usize,
    /// Folders that aren't there right now (an unplugged drive): their files stay indexed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub missing: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub code_missing: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    /// The model's download: missing | downloading | ready | failed.
    #[serde(default)]
    pub model: ModelStatus,
    /// The query encoder: idle | starting | ready | failed.
    #[serde(default)]
    pub encoder: String,
    /// Code search is on (there are code folders).
    #[serde(default)]
    pub has_code: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ModelStatus {
    pub state: String,
    /// How much of the download is there, 0 to 1.
    pub fraction: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Response {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hits: Option<Vec<Hit>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vectors: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ms: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<Box<Status>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,
    /// False when meaning search couldn't run (no model yet, or words only): the hits are word matches.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub meaning: Option<bool>,
    /// The search went to the code index.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<bool>,
    /// The query's words, for marking them in previews.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub terms: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

impl Response {
    pub fn ok() -> Response {
        Response { ok: true, ..Default::default() }
    }

    pub fn error(message: impl Into<String>) -> Response {
        Response { ok: false, error: Some(message.into()), ..Default::default() }
    }
}

/// A connection to the daemon.
pub struct Client {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
}

impl Client {
    /// None when no daemon is listening.
    pub fn connect() -> Option<Client> {
        let stream = UnixStream::connect(socket_path()).ok()?;
        stream.set_read_timeout(Some(Duration::from_secs(120))).ok()?;
        Some(Client { reader: BufReader::new(stream.try_clone().ok()?), writer: stream })
    }

    pub fn send(&mut self, request: &Request) -> Result<Response> {
        let mut line = serde_json::to_string(request)?;
        line.push('\n');
        self.writer.write_all(line.as_bytes()).context("the daemon went away")?;
        self.read()
    }

    pub fn read(&mut self) -> Result<Response> {
        let mut line = String::new();
        if self.reader.read_line(&mut line).context("the daemon went away")? == 0 {
            bail!("the daemon went away");
        }
        let response: Response = serde_json::from_str(&line).context("the daemon said something unexpected")?;
        Ok(response)
    }

    /// Sends a request that only says ok or not.
    pub fn command(&mut self, request: &Request) -> Result<()> {
        let response = self.send(request)?;
        if response.ok { Ok(()) } else { bail!(response.error.unwrap_or_else(|| "the daemon said no".into())) }
    }
}

/// The kinds named in a request.
pub fn kinds(names: &[String]) -> Result<Option<std::collections::HashSet<FileKind>>, String> {
    if names.is_empty() {
        return Ok(None);
    }
    names.iter().map(|name| name.parse()).collect::<Result<_, _>>().map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_read_as_the_protocol_says() {
        let request: Request = serde_json::from_str(r#"{"op":"search","query":"lease pets"}"#).unwrap();
        assert_eq!(
            request,
            Request::Search {
                query: "lease pets".into(),
                limit: 20,
                kinds: vec![],
                prefix_last: false,
                words_only: false,
                seq: None
            }
        );
        assert_eq!(serde_json::to_string(&Request::Pause).unwrap(), r#"{"op":"pause"}"#);
        assert!(serde_json::from_str::<Request>(r#"{"op":"dance"}"#).is_err());
        assert_eq!(kinds(&["pdf".into(), "video".into()]).unwrap().unwrap().len(), 2);
        assert!(kinds(&["film".into()]).is_err());
    }
}
