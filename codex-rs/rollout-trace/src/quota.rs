//! Optional cross-process budget for raw diagnostic capture.

use std::fs::File;
use std::fs::OpenOptions;
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;

use anyhow::Result;
use anyhow::bail;

#[derive(Debug)]
pub(crate) struct TraceQuota {
    root: PathBuf,
    limit: u64,
    enabled_file: Option<PathBuf>,
}

impl TraceQuota {
    pub(crate) fn from_environment(bundle: &Path) -> Option<Self> {
        let root = PathBuf::from(std::env::var_os("CODEX_ROLLOUT_TRACE_ROOT")?);
        let limit = std::env::var("CODEX_ROLLOUT_TRACE_MAX_BYTES")
            .ok()?
            .parse::<u64>()
            .unwrap_or(512 * 1024 * 1024);
        bundle.starts_with(&root).then(|| Self {
            root,
            limit: limit.min(512 * 1024 * 1024),
            enabled_file: std::env::var_os("CODEX_ROLLOUT_TRACE_ENABLED_FILE").map(PathBuf::from),
        })
    }

    // Keep the reservation locked until the caller has finished writing. Pruners
    // use the same lock and may then safely reconcile the on-disk byte count.
    pub(crate) fn reserve(&self, bytes: usize) -> Result<File> {
        if self
            .enabled_file
            .as_ref()
            .is_some_and(|path| !path.is_file())
        {
            bail!("raw trace capture disabled");
        }
        std::fs::create_dir_all(&self.root)?;
        let budget_path = self.root.join(".budget");
        if budget_path.is_symlink() {
            bail!("trace budget must not be a symlink");
        }
        let mut budget = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(budget_path)?;
        budget.lock()?;
        let mut stored = [0_u8; 8];
        let used = if budget.read_exact(&mut stored).is_ok() {
            u64::from_le_bytes(stored)
        } else {
            directory_bytes(&self.root)?
        };
        let next = used.saturating_add(u64::try_from(bytes).unwrap_or(u64::MAX));
        if next > self.limit {
            bail!("raw trace capture budget exhausted");
        }
        budget.seek(SeekFrom::Start(0))?;
        budget.write_all(&next.to_le_bytes())?;
        budget.flush()?;
        Ok(budget)
    }
}

fn directory_bytes(root: &Path) -> Result<u64> {
    let mut total: u64 = 0;
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        if kind.is_symlink() || entry.file_name() == ".budget" {
            continue;
        }
        total = total.saturating_add(if kind.is_dir() {
            directory_bytes(&entry.path())?
        } else {
            entry.metadata()?.len()
        });
    }
    Ok(total)
}

#[cfg(test)]
#[path = "quota_tests.rs"]
mod tests;
