//! Exercise the public CLI against disposable files and isolated state.
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

struct Fixture {
    temp: tempfile::TempDir,
    root: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("work");
        fs::create_dir(&root).unwrap();
        fs::create_dir(temp.path().join("cfg")).unwrap();
        fs::write(temp.path().join("cfg/policy.toml"), "[detectors]\nmin_size = \"0B\"\naction = \"quarantine\"\n").unwrap();
        Self { temp, root }
    }

    fn cache(&self, name: &str, old: bool) -> PathBuf {
        let dir = self.root.join(name).join("__pycache__");
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("module.pyc");
        fs::write(&file, b"disposable cache").unwrap();
        if old {
            let time = SystemTime::now() - Duration::from_secs(40 * 86_400);
            for path in [&file, &dir] {
                fs::File::open(path).unwrap().set_modified(time).unwrap();
            }
        }
        dir
    }

    fn run(&self, args: &[&str]) -> String {
        let out = Command::new(env!("CARGO_BIN_EXE_tdy"))
            .args(args)
            .env("HOME", self.temp.path())
            .env("TDY_HOME", self.temp.path().join("state"))
            .env("TDY_CONFIG_DIR", self.temp.path().join("cfg"))
            .env("TDY_CACHE_DIR", self.temp.path().join("cache"))
            .env("XDG_DATA_HOME", self.temp.path().join("share"))
            .env("TDY_NO_FNOX", "1")
            .env_remove("TDY_POLICY")
            .env("NO_COLOR", "1")
            .output()
            .unwrap();
        assert!(out.status.success(), "{args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8(out.stdout).unwrap()
    }

    fn entries(&self) -> Vec<Value> {
        self.run(&["show", "--json"]).lines().map(|l| serde_json::from_str(l).unwrap()).collect()
    }

    fn plan(&self) -> Value {
        serde_json::from_str(&self.run(&["plan", self.root.to_str().unwrap(), "--no-rules", "--json"])).unwrap()
    }

    fn report(&self, args: &[&str]) -> Value {
        self.run(args).lines().filter_map(|l| serde_json::from_str::<Value>(l).ok()).find(|v| v.get("report").is_some()).unwrap()["report"]
            .clone()
    }
}

#[test]
fn wholly_refused_plan_can_be_explicitly_overridden_and_undone() {
    let f = Fixture::new();
    let cache = f.cache("recent", false);
    let plan = f.plan();
    assert_eq!(plan["blocked"], 1);
    assert_eq!(plan["counts"]["rejected"], 1);
    assert!(f.entries()[0]["refusal"].as_str().unwrap().contains("recent"));
    f.run(&["review", "--approve-all"]);
    assert_eq!(f.entries()[0]["status"], "rejected");
    assert!(f.run(&["apply", "--dry-run"]).contains("nothing approved"));
    f.run(&["review", "--override-refusals"]);
    let entry = &f.entries()[0];
    assert_eq!(entry["status"], "approved");
    assert_eq!(entry["approved_by"], "human");
    assert_eq!(entry["override_refusal"], true);
    assert_eq!(f.report(&["apply", "--dry-run", "--json"])["would"], 1);
    assert!(cache.exists());
    assert_eq!(f.report(&["apply", "--json"])["applied"], 1);
    assert!(!cache.exists());
    assert!(f.run(&["undo"]).contains("1 restored"));
    assert_eq!(f.entries()[0]["status"], "undone");
    assert!(cache.exists());
}

#[test]
fn partial_plan_override_is_selective_and_reset_clears_it() {
    let f = Fixture::new();
    let fresh = f.cache("recent", false);
    f.cache("older", true);
    let plan = f.plan();
    assert_eq!(plan["counts"]["pending"], 1);
    assert_eq!(plan["counts"]["rejected"], 1);
    let entries = f.entries();
    let rejected = entries.iter().find(|e| Path::new(e["path"].as_str().unwrap()) == fs::canonicalize(&fresh).unwrap()).unwrap();
    let id = rejected["id"].as_str().unwrap();
    f.run(&["review", "--override-refusals", "--entry", id]);
    let entries = f.entries();
    assert_eq!(entries.iter().filter(|e| e["status"] == "approved").count(), 1);
    assert_eq!(entries.iter().filter(|e| e["status"] == "pending").count(), 1);
    f.run(&["review", "--reset", "--entry", id]);
    let entry = f.entries().into_iter().find(|e| e["id"] == id).unwrap();
    assert_ne!(entry.get("override_refusal"), Some(&Value::Bool(true)));
    f.run(&["review", "--approve-all"]);
    assert_eq!(f.report(&["apply", "--dry-run", "--json"])["would"], 1);
    assert!(fresh.exists());
}

#[test]
fn overridden_entry_still_skips_files_changed_since_planning() {
    let f = Fixture::new();
    let cache = f.cache("recent", false);
    f.plan();
    f.run(&["review", "--override-refusals"]);
    fs::write(cache.join("new.pyc"), b"changed after review").unwrap();
    let report = f.report(&["apply", "--json"]);
    assert_eq!(report["applied"], 0);
    assert_eq!(report["skipped"], 1);
    assert!(cache.exists());
}

#[test]
fn duplicate_and_archive_plans_retain_refusals() {
    let f = Fixture::new();
    let folder = f.root.join("copies");
    fs::create_dir(&folder).unwrap();
    let data = vec![b'x'; 32_768];
    fs::write(folder.join("first.txt"), &data).unwrap();
    fs::write(folder.join("second.txt"), &data).unwrap();
    let result: Value = serde_json::from_str(&f.run(&["dupes", f.root.to_str().unwrap(), "--min-size", "1B", "--plan", "--json"])).unwrap();
    assert_eq!(result["blocked"], 1);
    assert_eq!(result["counts"]["rejected"], 1);
    assert!(f.entries()[0]["refusal"].as_str().unwrap().contains("recent"));
    f.run(&["review", "--override-refusals"]);
    assert_eq!(f.report(&["apply", "--dry-run", "--json"])["would"], 1);

    let f = Fixture::new();
    let folder = f.root.join("cold");
    fs::create_dir(&folder).unwrap();
    fs::write(folder.join("first.txt"), &data).unwrap();
    let recent = SystemTime::now() - Duration::from_secs(2 * 86_400);
    for path in [&folder.join("first.txt"), &folder] {
        fs::File::open(path).unwrap().set_modified(recent).unwrap();
    }
    f.run(&["archive", f.root.to_str().unwrap(), "--older", "0s", "--min-size", "1B", "--max-ratio", "1.0"]);
    let entries = f.entries();
    assert!(!entries.is_empty());
    assert!(entries.iter().all(|e| e["status"] == "rejected" && e["refusal"].as_str().unwrap().contains("recent")));
    f.run(&["review", "--override-refusals"]);
    assert!(f.report(&["apply", "--dry-run", "--json"])["would"].as_u64().unwrap() > 0);
}
