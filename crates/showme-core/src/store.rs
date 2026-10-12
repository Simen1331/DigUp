//! The on-disk index: one SQLite file with files, their segments (with fp16 vectors), and an FTS5 table.
//!
//! Vectors live in the same database as everything else, so a crash can never leave them out of step with the rows
//! that describe them. At this scale (~20k vectors, ~30 MB) brute-force search over an in-memory copy takes a few
//! milliseconds; no vector database needed. The schema is DigUp's (version 5), so the two can read each other's.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, Row, params, params_from_iter, types::Value};

use crate::folder_selection;
use crate::model::{FileKind, Modality, SegmentKind};
use crate::search_text;
use crate::vectors;

pub type Result<T> = rusqlite::Result<T>;

/// A file the crawler decided to index.
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    pub path: String,
    pub kind: FileKind,
    pub size: i64,
    pub mtime: f64,
    pub inode: u64,
    pub device: u64,
}

/// A file as the index knows it.
#[derive(Debug, Clone, PartialEq)]
pub struct FileRecord {
    pub id: i64,
    pub path: String,
    pub kind: FileKind,
    pub size: i64,
    pub mtime: f64,
    pub inode: u64,
    pub device: u64,
    pub state: String,
}

/// What indexing learned about a file (`IndexStore::finish`'s `info`): pixels, pages, duration; read_to/read_end.
pub type Info = BTreeMap<String, String>;

#[derive(Debug, Default, Clone)]
pub struct Counts {
    pub by_kind_and_state: BTreeMap<String, BTreeMap<String, usize>>,
    pub vectors: usize,
    pub segments: usize,
    /// Long files still being read (`unfinished_count`), and their pages and chunks left.
    pub unfinished: usize,
    pub left: usize,
}

pub struct IndexStore {
    pub directory: PathBuf,
    pub file: PathBuf,
    pub db: Connection,
    /// When set, `pending`, `pending_count` and `counts` see only files under these folders: files on an unplugged
    /// drive neither fail nor count as waiting.
    scope: std::cell::RefCell<Option<Vec<String>>>,
}

impl IndexStore {
    pub const SCHEMA_VERSION: i64 = 5;
    /// The main index's file in the index folder.
    pub const MAIN_FILE: &'static str = "index.sqlite";
    /// The code index is a file of its own beside it: code search never moves the rest, and turning it off deletes one
    /// file.
    pub const CODE_FILE: &'static str = "code.sqlite";

    /// Keyword search's tokenizer. Marks (M*) count as letters, so Bengali vowel signs and Arabic harakat stay inside
    /// their words.
    const TOKENIZER: &'static str = "porter unicode61 remove_diacritics 2 categories 'L* N* Co M*'";

    pub fn open(directory: &Path, name: &str) -> Result<IndexStore> {
        std::fs::create_dir_all(directory).map_err(|error| io_error(&error))?;
        let file = directory.join(name);
        let db = Connection::open(&file)?;
        db.busy_timeout(std::time::Duration::from_secs(5))?;
        db.execute_batch("PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL; PRAGMA foreign_keys = ON;")?;
        let store = IndexStore { directory: directory.to_path_buf(), file, db, scope: std::cell::RefCell::new(None) };
        store.migrate()?;
        Ok(store)
    }

    pub fn open_main(directory: &Path) -> Result<IndexStore> {
        Self::open(directory, Self::MAIN_FILE)
    }

    pub fn is_code(&self) -> bool {
        self.file.file_name().is_some_and(|name| name == Self::CODE_FILE)
    }

    /// Bytes on disk: the database and its write-ahead log.
    pub fn size(file: &Path) -> u64 {
        let wal = PathBuf::from(format!("{}-wal", file.display()));
        [file.to_path_buf(), wal].iter().filter_map(|path| std::fs::metadata(path).ok()).map(|meta| meta.len()).sum()
    }

    fn migrate(&self) -> Result<()> {
        let version: i64 = self.db.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if version > Self::SCHEMA_VERSION {
            return Err(failure(format!("this index was made by a newer ShowMe (schema {version})")));
        }
        if version == Self::SCHEMA_VERSION {
            return Ok(());
        }
        if version != 0 {
            // DigUp's schemas 1–4 never reach Linux: indexes made there are copied at version 5 or not at all.
            return Err(failure(format!("this index has DigUp's old schema {version}: open it in DigUp first")));
        }
        self.db.busy_timeout(std::time::Duration::from_secs(60))?;
        self.transaction(|db| {
            db.execute_batch(&format!(
                "CREATE TABLE meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);
                CREATE TABLE files(
                    id INTEGER PRIMARY KEY,
                    path TEXT NOT NULL UNIQUE,
                    kind TEXT NOT NULL,
                    size INTEGER NOT NULL,
                    mtime REAL NOT NULL,
                    inode INTEGER NOT NULL,
                    device INTEGER NOT NULL,
                    state TEXT NOT NULL DEFAULT 'pending',   -- pending | done | partial | skipped | failed
                    detail TEXT,                              -- why skipped/failed, or how far a long file has been read
                    info TEXT,                                -- JSON: pixels, pages, duration, date taken; read_to/read_end
                    indexed_at REAL
                );
                CREATE INDEX files_inode ON files(device, inode);
                CREATE INDEX files_state ON files(state, kind);
                CREATE TABLE segments(
                    id INTEGER PRIMARY KEY,
                    file INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
                    kind TEXT NOT NULL,       -- SegmentKind
                    modality TEXT NOT NULL,   -- Modality of the vector ('none' when there is no vector)
                    loc REAL,                 -- PDF page number, or start second for audio/video
                    loc_end REAL,
                    excerpt TEXT,
                    vec BLOB                  -- 768 × float16, L2-normalized
                );
                CREATE INDEX segments_file ON segments(file);
                CREATE VIRTUAL TABLE fts USING fts5(name, body, original UNINDEXED, tokenize = \"{}\");
                PRAGMA user_version = {};",
                Self::TOKENIZER,
                Self::SCHEMA_VERSION
            ))
        })?;
        self.db.busy_timeout(std::time::Duration::from_secs(5))
    }

    /// Runs `body` in one `BEGIN IMMEDIATE` transaction: all of it is saved, or none.
    pub fn transaction<T>(&self, body: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        self.db.execute_batch("BEGIN IMMEDIATE")?;
        match body(&self.db) {
            Ok(value) => {
                self.db.execute_batch("COMMIT")?;
                Ok(value)
            }
            Err(error) => {
                let _ = self.db.execute_batch("ROLLBACK");
                Err(error)
            }
        }
    }

    pub fn scope(&self) -> Option<Vec<String>> {
        self.scope.borrow().clone()
    }

    pub fn set_scope(&self, scope: Option<Vec<String>>) {
        *self.scope.borrow_mut() = scope;
    }

    /// The scope as SQL ("" when there's none) and its values.
    fn scope_sql(&self) -> (String, Vec<Value>) {
        let scope = self.scope.borrow();
        let Some(scope) = scope.as_ref() else {
            return (String::new(), Vec::new());
        };
        if scope.is_empty() {
            return (" AND 0".into(), Vec::new());
        }
        let sql = scope.iter().map(|_| "(path >= ? AND path < ?)").collect::<Vec<_>>().join(" OR ");
        let values = scope.iter().flat_map(|root| prefix_range(root)).collect();
        (format!(" AND ({sql})"), values)
    }

    // Meta

    pub fn meta(&self, key: &str) -> Result<Option<String>> {
        self.db.query_row("SELECT value FROM meta WHERE key = ?", [key], |row| row.get(0)).optional()
    }

    pub fn set_meta(&self, key: &str, value: Option<&str>) -> Result<()> {
        match value {
            Some(value) => self.db.execute(
                "INSERT INTO meta(key, value) VALUES(?, ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                [key, value],
            ),
            None => self.db.execute("DELETE FROM meta WHERE key = ?", [key]),
        }
        .map(|_| ())
    }

    // Files

    const FILE_COLUMNS: &'static str = "id, path, kind, size, mtime, inode, device, state";

    fn record(row: &Row) -> Result<FileRecord> {
        Ok(FileRecord {
            id: row.get(0)?,
            path: row.get(1)?,
            kind: row.get::<_, String>(2)?.parse().unwrap_or(FileKind::Doc),
            size: row.get(3)?,
            mtime: row.get(4)?,
            inode: row.get::<_, i64>(5)? as u64,
            device: row.get::<_, i64>(6)? as u64,
            state: row.get(7)?,
        })
    }

    /// Every file under one of `roots`.
    pub fn files_under(&self, roots: &[String]) -> Result<Vec<FileRecord>> {
        let mut out = Vec::new();
        let mut statement =
            self.db.prepare(&format!("SELECT {} FROM files WHERE path >= ? AND path < ?", Self::FILE_COLUMNS))?;
        // A folder inside another one listed is already covered (each file once).
        for root in roots.iter().filter(|root| !roots.iter().any(|other| folder_selection::is_inside(root, other))) {
            let rows = statement.query_map(params_from_iter(prefix_range(root)), Self::record)?;
            for row in rows {
                out.push(row?);
            }
        }
        Ok(out)
    }

    /// Adds a file (pending) and returns its id.
    pub fn insert(&self, file: &Candidate) -> Result<i64> {
        self.db.execute(
            "INSERT INTO files(path, kind, size, mtime, inode, device) VALUES(?, ?, ?, ?, ?, ?)",
            params![file.path, file.kind.as_str(), file.size, file.mtime, file.inode as i64, file.device as i64],
        )?;
        let id = self.db.last_insert_rowid();
        self.add_name_row(id, &file.path)?;
        Ok(id)
    }

    /// The file changed on disk: drop what was extracted from it and queue it again.
    pub fn requeue_as(&self, id: i64, file: &Candidate) -> Result<()> {
        self.delete_segments(id, true)?;
        self.db.execute(
            "UPDATE files SET kind = ?, size = ?, mtime = ?, inode = ?, device = ?, state = 'pending', detail = NULL,
                info = NULL, indexed_at = NULL WHERE id = ?",
            params![file.kind.as_str(), file.size, file.mtime, file.inode as i64, file.device as i64, id],
        )?;
        Ok(())
    }

    /// Extract and embed the file again, as it is.
    pub fn requeue(&self, id: i64) -> Result<()> {
        self.delete_segments(id, true)?;
        self.db.execute(
            "UPDATE files SET state = 'pending', detail = NULL, info = NULL, indexed_at = NULL WHERE id = ?",
            [id],
        )?;
        Ok(())
    }

    /// Same inode, size and date at a new path: a move or rename. Only the path changes; nothing is re-embedded.
    pub fn move_file(&self, id: i64, path: &str) -> Result<()> {
        self.db.execute("UPDATE files SET path = ? WHERE id = ?", params![path, id])?;
        self.db.execute(
            "UPDATE fts SET name = ?, body = ? WHERE rowid IN (SELECT id FROM segments WHERE file = ? AND kind = 'name')",
            params![search_text::normalized(name_text(path)), search_text::normalized(&folder_text(path)), id],
        )?;
        Ok(())
    }

    pub fn delete(&self, id: i64) -> Result<()> {
        self.db.execute("DELETE FROM fts WHERE rowid IN (SELECT id FROM segments WHERE file = ?)", [id])?;
        self.db.execute("DELETE FROM files WHERE id = ?", [id])?;
        Ok(())
    }

    /// Removes every file that isn't under one of `roots` (folders taken out). Returns how many went.
    pub fn prune(&self, keeping_under: &[String]) -> Result<usize> {
        let prefixes: Vec<String> = keeping_under.iter().map(|root| with_slash(root)).collect();
        let mut gone = Vec::new();
        {
            let mut statement = self.db.prepare("SELECT id, path FROM files")?;
            let mut rows = statement.query([])?;
            while let Some(row) = rows.next()? {
                let path: String = row.get(1)?;
                if !prefixes.iter().any(|prefix| path.starts_with(prefix)) {
                    gone.push(row.get::<_, i64>(0)?);
                }
            }
        }
        if !gone.is_empty() {
            self.transaction(|_| gone.iter().try_for_each(|id| self.delete(*id)))?;
        }
        Ok(gone.len())
    }

    /// Queues every file again and forgets the fingerprint (after the model or settings changed).
    pub fn reset_all(&self) -> Result<()> {
        self.transaction(|db| {
            db.execute("DELETE FROM fts WHERE rowid IN (SELECT id FROM segments WHERE kind != 'name')", [])?;
            db.execute("DELETE FROM segments WHERE kind != 'name'", [])?;
            db.execute("UPDATE files SET state = 'pending', detail = NULL, info = NULL, indexed_at = NULL", [])?;
            db.execute("DELETE FROM meta WHERE key = 'fingerprint'", [])?;
            Ok(())
        })
    }

    /// Files waiting to be embedded, in indexing order. `kinds`: only these (the kinds this build can read).
    pub fn pending(
        &self,
        limit: Option<usize>,
        only: Option<&[i64]>,
        kinds: Option<&[FileKind]>,
    ) -> Result<Vec<FileRecord>> {
        if only.is_some_and(|ids| ids.is_empty()) {
            return Ok(Vec::new());
        }
        let ids = only
            .map(|ids| format!(" AND id IN ({})", ids.iter().map(|id| id.to_string()).collect::<Vec<_>>().join(",")));
        let kinds = kinds.map(|kinds| {
            format!(
                " AND kind IN ({})",
                kinds.iter().map(|kind| format!("'{}'", kind.as_str())).collect::<Vec<_>>().join(",")
            )
        });
        let (scope, mut values) = self.scope_sql();
        values.push(Value::Integer(limit.map_or(-1, |limit| limit as i64)));
        let sql = format!(
            "SELECT {} FROM files WHERE state = 'pending'{}{}{scope} ORDER BY {}, mtime DESC LIMIT ?",
            Self::FILE_COLUMNS,
            ids.unwrap_or_default(),
            kinds.unwrap_or_default(),
            FileKind::INDEXING_ORDER_SQL
        );
        let mut statement = self.db.prepare(&sql)?;
        let rows = statement.query_map(params_from_iter(values), Self::record)?;
        rows.collect()
    }

    pub fn pending_count(&self) -> Result<usize> {
        let (scope, values) = self.scope_sql();
        self.db.query_row(
            &format!("SELECT COUNT(*) FROM files WHERE state = 'pending'{scope}"),
            params_from_iter(values),
            |row| row.get::<_, i64>(0).map(|count| count as usize),
        )
    }

    /// Long files with pages or chunks still to read after their first step, in indexing order.
    pub fn unfinished(&self, kinds: Option<&[FileKind]>) -> Result<Vec<FileRecord>> {
        let (scope, values) = self.scope_sql();
        let kinds = kinds.map(|kinds| {
            format!(
                " AND kind IN ({})",
                kinds.iter().map(|kind| format!("'{}'", kind.as_str())).collect::<Vec<_>>().join(",")
            )
        });
        let sql = format!(
            "SELECT {} FROM files WHERE state = 'partial'{}{scope} ORDER BY {}, mtime DESC",
            Self::FILE_COLUMNS,
            kinds.unwrap_or_default(),
            FileKind::INDEXING_ORDER_SQL
        );
        let mut statement = self.db.prepare(&sql)?;
        let rows = statement.query_map(params_from_iter(values), Self::record)?;
        rows.collect()
    }

    /// How many long files are unfinished, and how many of their pages and chunks are left to read.
    pub fn unfinished_count(&self) -> Result<(usize, usize)> {
        let (scope, values) = self.scope_sql();
        self.db.query_row(
            &format!(
                "SELECT COUNT(*), SUM(MAX(0, CAST(json_extract(info, '$.read_end') AS INTEGER)
                                      - CAST(json_extract(info, '$.read_to') AS INTEGER)))
                FROM files WHERE state = 'partial'{scope}"
            ),
            params_from_iter(values),
            |row| Ok((row.get::<_, i64>(0)? as usize, row.get::<_, Option<i64>>(1)?.unwrap_or(0) as usize)),
        )
    }

    /// What indexing learned about a file (`finish`'s `info`).
    pub fn info(&self, id: i64) -> Result<Info> {
        let json: Option<String> =
            self.db.query_row("SELECT info FROM files WHERE id = ?", [id], |row| row.get(0)).optional()?.flatten();
        Ok(json.and_then(|json| serde_json::from_str(&json).ok()).unwrap_or_default())
    }

    /// The last page or chunk stored for a file (where a long file's reading got to, when its info doesn't say).
    pub fn last_place(&self, id: i64) -> Result<Option<usize>> {
        self.db.query_row(
            "SELECT MAX(loc) FROM segments WHERE file = ? AND kind IN ('page', 'page_image', 'chunk')",
            [id],
            |row| row.get::<_, Option<f64>>(0).map(|loc| loc.map(|loc| loc as usize)),
        )
    }

    pub fn finish(&self, id: i64, state: &str, detail: Option<&str>, info: &Info) -> Result<()> {
        let json = if info.is_empty() { None } else { serde_json::to_string(info).ok() };
        let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|since| since.as_secs_f64()).unwrap_or(0.0);
        self.db.execute(
            "UPDATE files SET state = ?, detail = ?, info = ?, indexed_at = ? WHERE id = ?",
            params![state, detail, json, now, id],
        )?;
        Ok(())
    }

    // Segments

    /// Adds a segment; `text` goes into keyword search (normalized), `vector` into meaning search.
    #[allow(clippy::too_many_arguments)]
    pub fn add_segment(
        &self,
        file: i64,
        kind: SegmentKind,
        modality: Modality,
        loc: Option<f64>,
        loc_end: Option<f64>,
        excerpt: Option<&str>,
        text: Option<&str>,
        vector: Option<&[f32]>,
    ) -> Result<()> {
        let modality = if vector.is_some() { modality } else { Modality::None };
        self.db.execute(
            "INSERT INTO segments(file, kind, modality, loc, loc_end, excerpt, vec) VALUES(?, ?, ?, ?, ?, ?, ?)",
            params![file, kind.as_str(), modality.as_str(), loc, loc_end, excerpt, vector.map(vectors::half_bytes)],
        )?;
        if let Some(text) = text.filter(|text| !text.is_empty()) {
            let (body, original) = keyword_text(text);
            self.db.execute(
                "INSERT INTO fts(rowid, name, body, original) VALUES(?, '', ?, ?)",
                params![self.db.last_insert_rowid(), body, original],
            )?;
        }
        Ok(())
    }

    fn add_name_row(&self, file: i64, path: &str) -> Result<()> {
        self.db.execute("INSERT INTO segments(file, kind, modality) VALUES(?, 'name', 'none')", [file])?;
        self.db.execute(
            "INSERT INTO fts(rowid, name, body) VALUES(?, ?, ?)",
            params![
                self.db.last_insert_rowid(),
                search_text::normalized(name_text(path)),
                search_text::normalized(&folder_text(path))
            ],
        )?;
        Ok(())
    }

    fn delete_segments(&self, file: i64, keeping_name: bool) -> Result<()> {
        let filter = if keeping_name { "AND kind != 'name'" } else { "" };
        self.db.execute(
            &format!("DELETE FROM fts WHERE rowid IN (SELECT id FROM segments WHERE file = ? {filter})"),
            [file],
        )?;
        self.db.execute(&format!("DELETE FROM segments WHERE file = ? {filter}"), [file])?;
        Ok(())
    }

    // Stats

    pub fn counts(&self) -> Result<Counts> {
        let mut counts = Counts::default();
        let (scope, values) = self.scope_sql();
        {
            let mut statement = self
                .db
                .prepare(&format!("SELECT kind, state, COUNT(*) FROM files WHERE 1{scope} GROUP BY kind, state"))?;
            let mut rows = statement.query(params_from_iter(values.clone()))?;
            while let Some(row) = rows.next()? {
                counts
                    .by_kind_and_state
                    .entry(row.get(0)?)
                    .or_default()
                    .insert(row.get(1)?, row.get::<_, i64>(2)? as usize);
            }
        }
        (counts.unfinished, counts.left) = self.unfinished_count()?;
        let in_scope = if self.scope.borrow().is_none() {
            String::new()
        } else {
            format!(" AND file IN (SELECT id FROM files WHERE 1{scope})")
        };
        (counts.segments, counts.vectors) = self.db.query_row(
            &format!("SELECT COUNT(*), COUNT(vec) FROM segments WHERE kind != 'name'{in_scope}"),
            params_from_iter(values),
            |row| Ok((row.get::<_, i64>(0)? as usize, row.get::<_, i64>(1)? as usize)),
        )?;
        Ok(counts)
    }

    /// Each file's kind, by id.
    pub fn file_kinds(&self) -> Result<HashMap<i64, FileKind>> {
        let mut statement = self.db.prepare("SELECT id, kind FROM files")?;
        let rows = statement.query_map([], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)))?;
        let mut kinds = HashMap::new();
        for row in rows {
            let (id, kind) = row?;
            if let Ok(kind) = kind.parse() {
                kinds.insert(id, kind);
            }
        }
        Ok(kinds)
    }
}

/// `body` and `original` for keyword search: the normalized text, and the text as written if that differs.
fn keyword_text(text: &str) -> (String, Option<&str>) {
    let normalized = search_text::normalized(text);
    let original = if normalized == text { None } else { Some(text) };
    (normalized, original)
}

/// "Screenshot 2026-10-01 at 9.41.07 PM.png" → searchable words.
fn name_text(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// The folders a file sits in, relative to the home folder ("Downloads Invoices"), as low-weight keywords.
fn folder_text(path: &str) -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    let folder = folder_selection::parent(path);
    let folder = if !home.is_empty() { folder.strip_prefix(&home).unwrap_or(folder) } else { folder };
    folder.split('/').filter(|part| !part.is_empty()).collect::<Vec<_>>().join(" ")
}

fn with_slash(root: &str) -> String {
    if root.ends_with('/') { root.to_string() } else { format!("{root}/") }
}

/// Everything that starts with `root` + "/", as a range on the path index.
fn prefix_range(root: &str) -> [Value; 2] {
    let prefix = with_slash(root);
    [Value::Text(prefix.clone()), Value::Text(format!("{prefix}\u{10FFFF}"))]
}

fn failure(message: String) -> rusqlite::Error {
    rusqlite::Error::SqliteFailure(rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_ERROR), Some(message))
}

fn io_error(error: &std::io::Error) -> rusqlite::Error {
    failure(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(path: &str, inode: u64) -> Candidate {
        Candidate { path: path.into(), kind: FileKind::Doc, size: 1, mtime: 1.0, inode, device: 1 }
    }

    #[test]
    fn files_under_nested_folders_come_once() {
        let dir = tempfile::tempdir().unwrap();
        let store = IndexStore::open_main(dir.path()).unwrap();
        store.insert(&Candidate { kind: FileKind::Video, ..candidate("/D/Media/Courses/a.mp4", 1) }).unwrap();
        assert_eq!(store.files_under(&["/D".into(), "/D/Media/Courses".into()]).unwrap().len(), 1);
    }

    #[test]
    fn pending_scope_and_counts() {
        let dir = tempfile::tempdir().unwrap();
        let store = IndexStore::open_main(dir.path()).unwrap();
        let a = store.insert(&candidate("/A/notes.txt", 1)).unwrap();
        store.insert(&candidate("/B/plan.txt", 2)).unwrap();
        store
            .add_segment(a, SegmentKind::Chunk, Modality::Text, Some(1.0), None, None, Some("hello"), Some(&[0.5; 768]))
            .unwrap();
        store.finish(a, "done", None, &Info::from([("chars".into(), "5".into())])).unwrap();
        assert_eq!(store.pending(None, None, None).unwrap().len(), 1);
        store.set_scope(Some(vec!["/A".into()]));
        assert!(store.pending(None, None, None).unwrap().is_empty());
        let counts = store.counts().unwrap();
        assert_eq!((counts.vectors, counts.segments), (1, 1));
        assert_eq!(store.info(a).unwrap().get("chars").map(String::as_str), Some("5"));
        store.set_scope(None);
        store.reset_all().unwrap();
        assert_eq!(store.pending_count().unwrap(), 2);
        assert_eq!(store.prune(&["/A".into()]).unwrap(), 1);
    }
}
