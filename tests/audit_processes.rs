use std::{
    path::PathBuf,
    process::Command,
    thread,
    time::{Duration, Instant},
};

use maccanvas::audit::{self, Audit, Level};
use serde_json::json;

const EVENTS: usize = 600;

#[test]
#[ignore = "Child process helper for the cross-process rotation test"]
fn audit_process_worker() {
    let path = PathBuf::from(
        std::env::var_os("MACCANVAS_AUDIT_FIXTURE_ROOT")
            .expect("Explicit fixture directory required"),
    );
    let mut guard = Audit::start(path).unwrap();
    let scope = guard.audit().scope(json!({}));
    for sequence in 0..EVENTS {
        scope
            .critical(
                "test.process.record",
                Level::Info,
                json!({"test.sequence":sequence,"test.padding":"x".repeat(16*1024)}),
            )
            .unwrap();
    }
    guard.shutdown().unwrap();
}

#[test]
fn independent_processes_append_and_rotate_without_losing_or_splitting_records() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("audit");
    let mut children = Vec::new();
    for _ in 0..2 {
        children.push(
            Command::new(std::env::current_exe().unwrap())
                .args([
                    "--ignored",
                    "--exact",
                    "audit_process_worker",
                    "--nocapture",
                ])
                .env("MACCANVAS_AUDIT_FIXTURE_ROOT", &path)
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .unwrap(),
        );
    }
    let deadline = Instant::now() + Duration::from_secs(60);
    for mut child in children {
        while child.try_wait().unwrap().is_none() {
            assert!(
                Instant::now() < deadline,
                "Cross-process audit fixture timed out"
            );
            thread::sleep(Duration::from_millis(10));
        }
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "Audit child failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let mut files: Vec<_> = std::fs::read_dir(&path)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "jsonl")
        })
        .collect();
    files.sort();
    assert!(
        !files.is_empty(),
        "Fixture must exercise an actual 16 MiB rotation"
    );
    files.push(path.join("log.json"));
    assert_eq!(audit::verify(&files).unwrap(), (EVENTS * 2) as u64);
    for file in files {
        assert!(std::fs::metadata(file).unwrap().len() <= 16 * 1024 * 1024);
    }
}
