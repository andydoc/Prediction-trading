/// File-based IPC between the main test binary and the helper binary.

use serde::{Serialize, Deserialize};
use std::path::{Path, PathBuf};

const PID_FILE: &str = "data/clob-test.pid";
const CHECKPOINT_FILE: &str = "data/clob_test_checkpoint.json";
const D6_READY_FLAG: &str = "data/clob_test_d6_ready.flag";

/// Checkpoint saved by main binary for resume after D6 restart.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Checkpoint {
    pub timestamp: String,
    pub phase: String,
    pub d2_done: bool,
    pub d3_done: bool,
    pub d4_done: bool,
    pub d5_done: bool,
    pub open_position_ids: Vec<String>,
    pub test_results: Vec<crate::report::TestResult>,
    pub initial_usdc: f64,
    pub initial_pol: f64,
}

/// Write the PID file.
pub fn write_pid(workspace: &Path) -> std::io::Result<()> {
    let path = workspace.join(PID_FILE);
    let _ = std::fs::create_dir_all(path.parent().unwrap());
    std::fs::write(&path, std::process::id().to_string())
}

/// Read the PID from the PID file.
pub fn read_pid(workspace: &Path) -> Option<u32> {
    let path = workspace.join(PID_FILE);
    std::fs::read_to_string(&path).ok()?.trim().parse().ok()
}

/// Remove the PID file.
pub fn remove_pid(workspace: &Path) {
    let _ = std::fs::remove_file(workspace.join(PID_FILE));
}

/// Write a checkpoint for D6 resume.
pub fn write_checkpoint(workspace: &Path, checkpoint: &Checkpoint) -> std::io::Result<()> {
    let path = workspace.join(CHECKPOINT_FILE);
    let _ = std::fs::create_dir_all(path.parent().unwrap());
    let json = serde_json::to_string_pretty(checkpoint)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
    std::fs::write(&path, json)
}

/// Read a checkpoint from disk.
pub fn read_checkpoint(path: &Path) -> Option<Checkpoint> {
    let content = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
}

/// Default checkpoint path.
pub fn checkpoint_path(workspace: &Path) -> PathBuf {
    workspace.join(CHECKPOINT_FILE)
}

/// Signal the helper that D6 is ready (2+ positions open).
pub fn signal_d6_ready(workspace: &Path) -> std::io::Result<()> {
    let path = workspace.join(D6_READY_FLAG);
    std::fs::write(&path, "ready")
}

/// Check if D6 ready flag exists.
pub fn is_d6_ready(workspace: &Path) -> bool {
    workspace.join(D6_READY_FLAG).exists()
}

/// Remove D6 ready flag (cleanup).
pub fn clear_d6_flag(workspace: &Path) {
    let _ = std::fs::remove_file(workspace.join(D6_READY_FLAG));
}

/// Clean up all IPC files.
pub fn cleanup(workspace: &Path) {
    remove_pid(workspace);
    clear_d6_flag(workspace);
    // Don't remove checkpoint or reports — those are deliverables
}
