//! Append-only, fsync'd journal: intent is recorded before every change and
//! the outcome after, so an interrupted apply can be reconciled on restart.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::io::{BufRead, Write};
use std::path::PathBuf;
use tidy_ops::dirs::journal_dir;
use tidy_ops::fsops::create_private_dir;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Phase {
    Begin,
    /// Cross-volume copy finished and verified; source not yet removed.
    Copied,
    Done,
    Fail,
    UndoBegin,
    UndoDone,
    UndoFail,
    Recovered,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Record {
    pub ts: i64,
    pub entry: String,
    pub phase: Phase,
    pub action: String,
    pub from: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

pub struct Journal {
    f: std::fs::File,
}

pub fn path(plan_id: &str) -> PathBuf {
    journal_dir().join(format!("{plan_id}.jsonl"))
}

impl Journal {
    pub fn open(plan_id: &str) -> Result<Self> {
        create_private_dir(&journal_dir())?;
        let f = std::fs::OpenOptions::new().create(true).append(true).open(path(plan_id))?;
        Ok(Journal { f })
    }

    pub fn write(&mut self, rec: &Record) -> Result<()> {
        let mut line = serde_json::to_vec(rec)?;
        line.push(b'\n');
        self.f.write_all(&line)?;
        self.f.sync_data()?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn log(
        &mut self,
        entry: &str,
        phase: Phase,
        action: &str,
        from: &std::path::Path,
        to: Option<&std::path::Path>,
        note: Option<String>,
    ) -> Result<()> {
        self.write(&Record {
            ts: tidy_scan::fmt::now_secs(),
            entry: entry.to_string(),
            phase,
            action: action.to_string(),
            from: from.to_path_buf(),
            to: to.map(|p| p.to_path_buf()),
            note,
        })
    }
}

pub fn read(plan_id: &str) -> Result<Vec<Record>> {
    let p = path(plan_id);
    let Ok(f) = std::fs::File::open(&p) else { return Ok(Vec::new()) };
    let mut v = Vec::new();
    for l in std::io::BufReader::new(f).lines() {
        let l = l?;
        // A torn final line (crash mid-write) is ignored.
        if let Ok(r) = serde_json::from_str::<Record>(&l) {
            v.push(r);
        }
    }
    Ok(v)
}
