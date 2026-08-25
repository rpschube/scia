//! A small, dependency-free size-rotating file appender.
//!
//! [`RotatingFile`] is an [`io::Write`] that caps the live file at a byte size
//! and keeps a fixed number of rolled-over generations, so a long-running log
//! sink stays bounded on disk. It is the file sink behind the structured-logging
//! subscriber (the app wraps it in a `Mutex` and hands it to `tracing`), and it
//! is deliberately tiny and allocation-light so it never becomes the reason a
//! log line is expensive.
//!
//! Layout: the live file is `<dir>/<base>`; rolled generations are
//! `<dir>/<base>.1` (newest) through `<dir>/<base>.{keep}` (oldest). When a
//! write would push the live file past `max_bytes`, the generations shift up by
//! one (dropping the oldest past `keep`) and a fresh live file is opened. A
//! single write larger than `max_bytes` is still written whole (never split), so
//! one oversized line cannot be lost — the bound is a target, not a hard cap on
//! an individual record.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// A size-rotating append-only file sink. See the module docs for the layout.
pub struct RotatingFile {
    dir: PathBuf,
    base: String,
    max_bytes: u64,
    keep: usize,
    file: File,
    len: u64,
}

impl RotatingFile {
    /// Open (creating if needed) the live file `<dir>/<base>`, rotating it when a
    /// write would exceed `max_bytes` and keeping `keep` rolled generations
    /// besides the live file.
    ///
    /// `max_bytes` is clamped to at least 1 and `keep` to at least 1 so the sink
    /// always rotates rather than growing without bound.
    ///
    /// # Errors
    /// Returns any I/O error creating the directory or opening the file.
    pub fn open(
        dir: impl AsRef<Path>,
        base: impl Into<String>,
        max_bytes: u64,
        keep: usize,
    ) -> io::Result<Self> {
        let dir = dir.as_ref().to_path_buf();
        fs::create_dir_all(&dir)?;
        let base = base.into();
        let path = dir.join(&base);
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        let len = file.metadata().map(|m| m.len()).unwrap_or(0);
        Ok(Self {
            dir,
            base,
            max_bytes: max_bytes.max(1),
            keep: keep.max(1),
            file,
            len,
        })
    }

    /// Path of the nth rolled generation (`n >= 1`); `0` is the live file.
    fn gen_path(&self, n: usize) -> PathBuf {
        if n == 0 {
            self.dir.join(&self.base)
        } else {
            self.dir.join(format!("{}.{n}", self.base))
        }
    }

    /// Roll the generations up by one and open a fresh live file. Best-effort:
    /// a missing intermediate generation is simply skipped.
    fn rotate(&mut self) -> io::Result<()> {
        // Drop the oldest, then shift each generation up toward it.
        let _ = fs::remove_file(self.gen_path(self.keep));
        for n in (1..self.keep).rev() {
            let from = self.gen_path(n);
            if from.exists() {
                let _ = fs::rename(&from, self.gen_path(n + 1));
            }
        }
        let live = self.gen_path(0);
        if live.exists() {
            fs::rename(&live, self.gen_path(1))?;
        }
        self.file = OpenOptions::new().create(true).append(true).open(&live)?;
        self.len = 0;
        Ok(())
    }
}

impl Write for RotatingFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // Rotate before a write that would cross the bound, but never rotate an
        // empty live file (that would just churn empty generations for one huge
        // record).
        if self.len > 0 && self.len + buf.len() as u64 > self.max_bytes {
            self.rotate()?;
        }
        let n = self.file.write(buf)?;
        self.len += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A unique scratch directory for one test, under the system temp dir.
    fn scratch(tag: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        let uniq = format!(
            "scia-rotate-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        p.push(uniq);
        p
    }

    #[test]
    fn rotation_keeps_a_bounded_number_of_files() {
        let dir = scratch("bound");
        let mut f = RotatingFile::open(&dir, "scia.log", 64, 2).unwrap();
        // Each line is ~20 bytes; write far more than 64*3 total to force many
        // rotations, then assert the file count never exceeds keep + live.
        for i in 0..200 {
            writeln!(f, "line {i:08} xxxxxx").unwrap();
        }
        f.flush().unwrap();
        let count = fs::read_dir(&dir).unwrap().count();
        assert!(
            count <= 3,
            "expected at most keep(2)+live(1)=3 files, found {count}"
        );
        // And the live file itself stayed near the bound (one line may cross it).
        let live_len = fs::metadata(dir.join("scia.log")).unwrap().len();
        assert!(
            live_len <= 64 + 32,
            "live file {live_len} bytes exceeds bound"
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn reopen_appends_to_the_live_file() {
        let dir = scratch("reopen");
        {
            let mut f = RotatingFile::open(&dir, "scia.log", 1 << 20, 3).unwrap();
            f.write_all(b"first\n").unwrap();
            f.flush().unwrap();
        }
        {
            let mut f = RotatingFile::open(&dir, "scia.log", 1 << 20, 3).unwrap();
            f.write_all(b"second\n").unwrap();
            f.flush().unwrap();
        }
        let body = fs::read_to_string(dir.join("scia.log")).unwrap();
        assert_eq!(body, "first\nsecond\n");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_oversized_record_is_written_whole() {
        // A record is one `write` call (RecordWriter and tracing's fmt layer both
        // hand the whole line to the sink at once): a single write larger than
        // the bound onto an empty live file is never split.
        let dir = scratch("oversized");
        let mut f = RotatingFile::open(&dir, "scia.log", 8, 2).unwrap();
        let big = "x".repeat(100);
        f.write_all(big.as_bytes()).unwrap();
        f.flush().unwrap();
        let live = fs::read_to_string(dir.join("scia.log")).unwrap();
        assert_eq!(
            live, big,
            "the whole oversized record survives in the live file"
        );
        fs::remove_dir_all(&dir).ok();
    }
}
