use crate::eco::{Level, Source};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Record {
    /// Unix milliseconds.
    pub ts: u64,
    pub source: Source,
    pub level: Level,
    pub line: String,
}

/// Opaque to clients: `<process id>@<byte offset>`.
pub fn cursor(process: &str, offset: u64) -> String {
    format!("{process}@{offset}")
}

pub fn parse_cursor(c: &str) -> Result<(String, u64)> {
    // The id names a file in the logs dir: no separators, so it can't name one elsewhere.
    let parsed = c.rsplit_once('@').filter(|(p, _)| !p.contains('/') && !p.starts_with('.')).and_then(|(p, off)| Some((p.to_string(), off.parse().ok()?)));
    parsed.ok_or_else(|| {
        let grammar = "expected <process id>@<byte offset>, as --mark or a previous logs call prints it; logs --all reads a whole run";
        crate::daemon::coded("bad_request", format!("malformed cursor {c:?}: {grammar}"))
    })
}

pub struct LogFile {
    file: File,
    pub len: u64,
}

impl LogFile {
    pub fn open(path: &Path) -> Result<Self> {
        let file = File::options().create(true).append(true).open(path)?;
        Ok(LogFile { len: file.metadata()?.len(), file })
    }

    /// Returns the offset just past the written record.
    pub fn append(&mut self, rec: &Record) -> u64 {
        let mut line = serde_json::to_vec(rec).expect("record serializes");
        line.push(b'\n');
        if self.file.write_all(&line).is_ok() {
            self.len += line.len() as u64;
        }
        self.len
    }
}

#[derive(Default)]
pub struct Filter {
    pub level: Option<Level>,
    pub source: Option<Source>,
}

impl Filter {
    pub fn keep(&self, r: &Record) -> bool {
        self.level.is_none_or(|l| r.level >= l) && self.source.is_none_or(|s| r.source == s)
    }
}

/// Complete records from `offset` on, and the offset after the last complete one.
pub fn read(path: &Path, offset: u64) -> Result<(Vec<Record>, u64)> {
    let mut f = File::open(path)?;
    f.seek(SeekFrom::Start(offset))?;
    let mut buf = vec![];
    f.read_to_end(&mut buf)?;
    // A record being appended concurrently may be half-written; stop at the last newline.
    let complete = buf.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
    let recs = buf[..complete]
        .split(|&b| b == b'\n')
        .filter_map(|l| serde_json::from_slice(l).ok())
        .collect();
    Ok((recs, offset + complete as u64))
}

// ponytail: reads the whole file to take the tail; seek backwards if logs grow past a few MB
pub fn tail(path: &Path, n: usize, filter: &Filter) -> Result<(Vec<Record>, u64)> {
    let (mut recs, end) = read(path, 0)?;
    recs.retain(|r| filter.keep(r));
    Ok((recs.split_off(recs.len().saturating_sub(n)), end))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn since_mark_returns_only_new_records() {
        let dir = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/target/tmp/logs-test"));
        std::fs::create_dir_all(dir).unwrap();
        let path = dir.join("p.log");
        let _ = std::fs::remove_file(&path);
        let mut log = LogFile::open(&path).unwrap();
        let rec = |level, line: &str| Record { ts: 0, source: Source::Stdout, level, line: line.into() };
        for i in 0..5 {
            log.append(&rec(Level::Info, &format!("old {i}")));
        }
        let mark = cursor("p", log.len);
        log.append(&rec(Level::Info, "new 0"));
        log.append(&rec(Level::Warn, "new 1"));
        log.append(&rec(Level::Error, "new 2"));

        let (_, off) = parse_cursor(&mark).unwrap();
        let (recs, end) = read(&path, off).unwrap();
        assert_eq!(recs.iter().map(|r| r.line.as_str()).collect::<Vec<_>>(), ["new 0", "new 1", "new 2"]);
        assert_eq!(end, log.len);
        assert!(read(&path, end).unwrap().0.is_empty());

        let warn = Filter { level: Some(Level::Warn), source: None };
        assert_eq!(tail(&path, 200, &warn).unwrap().0.len(), 2);
    }

    #[test]
    fn a_cursor_names_a_log_in_the_logs_dir_only() {
        for c in ["../x@0", "a/b@0", ".hidden@0", "p", "p@x"] {
            let e = parse_cursor(c).unwrap_err();
            assert_eq!(e.downcast_ref::<crate::daemon::Coded>().unwrap().0, "bad_request", "{c}");
        }
        assert_eq!(parse_cursor("cargo test-17@42").unwrap(), ("cargo test-17".to_string(), 42));
    }
}
