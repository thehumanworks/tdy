//! Plan files on disk: `<data>/plans/<id>.jsonl`, written atomically.

use crate::model::{Entry, Header, PLAN_VERSION, Plan};
use anyhow::{Context, Result, bail};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use tdy_ops::dirs::plans_dir;
use tdy_ops::fsops::create_private_dir;

const ALPHABET: &[u8] = b"0123456789abcdefghjkmnpqrstvwxyz";

/// New plan id: 7 Crockford-base32 chars, time-ordered (seconds) with a
/// random tail, e.g. `k3f9q2a`.
pub fn new_id() -> String {
    let t = tdy_scan::fmt::now_secs() as u64;
    let mut seed = [0u8; 8];
    let _ = getrandom(&mut seed);
    let r = u64::from_le_bytes(seed) ^ (std::process::id() as u64).rotate_left(32);
    // 25 bits of time (≈1 year of seconds, wraps) + 10 bits random → 35 bits = 7 chars
    let v = ((t & 0x1FF_FFFF) << 10) | (r & 0x3FF);
    (0..7).rev().map(|i| ALPHABET[((v >> (i * 5)) & 31) as usize] as char).collect()
}

fn getrandom(buf: &mut [u8]) -> std::io::Result<()> {
    use std::io::Read;
    std::fs::File::open("/dev/urandom")?.read_exact(buf)
}

pub fn plan_path(id: &str) -> PathBuf {
    plans_dir().join(format!("{id}.jsonl"))
}

pub fn save(plan: &Plan) -> Result<PathBuf> {
    let dir = plans_dir();
    create_private_dir(&dir)?;
    let path = plan_path(&plan.header.id);
    save_to(plan, &path)?;
    Ok(path)
}

pub fn save_to(plan: &Plan, path: &Path) -> Result<()> {
    let tmp = path.with_extension("jsonl.tmp");
    {
        let mut f = std::io::BufWriter::new(std::fs::File::create(&tmp)?);
        serde_json::to_writer(&mut f, &plan.header)?;
        f.write_all(b"\n")?;
        for e in &plan.entries {
            serde_json::to_writer(&mut f, e)?;
            f.write_all(b"\n")?;
        }
        f.flush()?;
        f.get_ref().sync_data()?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

pub fn load_path(path: &Path) -> Result<Plan> {
    let f = std::fs::File::open(path).with_context(|| format!("opening plan {}", path.display()))?;
    let mut lines = std::io::BufReader::new(f).lines();
    let first = lines.next().context("empty plan file")??;
    let header: Header = serde_json::from_str(&first).context("plan header")?;
    if header.tdy_plan > PLAN_VERSION {
        bail!("plan format v{} is newer than this tdy (v{PLAN_VERSION})", header.tdy_plan);
    }
    let mut entries = Vec::new();
    for (i, l) in lines.enumerate() {
        let l = l?;
        if l.trim().is_empty() {
            continue;
        }
        let e: Entry = serde_json::from_str(&l).with_context(|| format!("plan line {}", i + 2))?;
        entries.push(e);
    }
    Ok(Plan { header, entries })
}

/// All plans, newest first.
pub fn list() -> Result<Vec<Plan>> {
    let dir = plans_dir();
    let mut v = Vec::new();
    let Ok(rd) = std::fs::read_dir(&dir) else { return Ok(v) };
    for e in rd.flatten() {
        let p = e.path();
        if p.extension().and_then(|s| s.to_str()) == Some("jsonl")
            && let Ok(plan) = load_path(&p)
        {
            v.push(plan);
        }
    }
    v.sort_by_key(|p| std::cmp::Reverse(p.header.created));
    Ok(v)
}

/// Resolve `latest`, a full id, a unique id prefix, or a path to a plan file.
pub fn resolve(spec: Option<&str>) -> Result<PathBuf> {
    let spec = spec.unwrap_or("latest");
    let as_path = Path::new(spec);
    if spec.ends_with(".jsonl") && as_path.exists() {
        return Ok(as_path.to_path_buf());
    }
    let plans = list()?;
    if spec == "latest" {
        return plans.first().map(|p| plan_path(&p.header.id)).context("no plans yet — run `tdy plan` first");
    }
    let matches: Vec<_> = plans.iter().filter(|p| p.header.id.starts_with(spec)).collect();
    match matches.len() {
        0 => bail!("no plan matches `{spec}`"),
        1 => Ok(plan_path(&matches[0].header.id)),
        _ => bail!("`{spec}` is ambiguous ({} plans)", matches.len()),
    }
}

pub fn load(spec: Option<&str>) -> Result<(Plan, PathBuf)> {
    let p = resolve(spec)?;
    Ok((load_path(&p)?, p))
}

/// Exclusive advisory lock for applying/undoing a plan.
pub struct PlanLock {
    _f: std::fs::File,
}

pub fn lock(id: &str) -> Result<PlanLock> {
    let dir = plans_dir();
    create_private_dir(&dir)?;
    let f = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(dir.join(format!("{id}.lock")))?;
    use std::os::fd::AsRawFd;
    if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        bail!("plan {id} is being applied by another tdy process");
    }
    Ok(PlanLock { _f: f })
}
