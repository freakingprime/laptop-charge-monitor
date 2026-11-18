use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

const RELEASE_EXE_NAME: &str = "laptop-charge-monitor.exe";

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask crate must live under repository root")
        .to_path_buf()
}

fn is_portable_runtime_file(file_name: &str) -> bool {
    file_name.eq_ignore_ascii_case(RELEASE_EXE_NAME)
}

fn run() -> Result<(), String> {
    let root = repo_root();
    let cargo = env::var_os("CARGO").unwrap_or_else(|| "cargo".into());

    let status = Command::new(&cargo)
        .current_dir(&root)
        .args(["build", "--release", "--package", "laptop-charge-monitor"])
        .status()
        .map_err(|e| format!("failed to launch cargo build --release: {e}"))?;

    if !status.success() {
        return Err(format!("cargo build --release failed with status {status}"));
    }

    let src_exe = root.join("target").join("release").join(RELEASE_EXE_NAME);
    if !src_exe.is_file() {
        return Err(format!(
            "expected release binary not found at {}",
            src_exe.display()
        ));
    }

    let dist_dir = root.join("dist").join("PowerMonitor");
    fs::create_dir_all(&dist_dir)
        .map_err(|e| format!("failed to create {}: {e}", dist_dir.display()))?;

    // Remove any non-runtime/stale files from the portable folder before copying.
    let entries = fs::read_dir(&dist_dir)
        .map_err(|e| format!("failed to read {}: {e}", dist_dir.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("failed to inspect entry: {e}"))?;
        let path = entry.path();
        let name = entry.file_name();
        let keep = path.is_file() && is_portable_runtime_file(&name.to_string_lossy());
        if !keep {
            if path.is_dir() {
                fs::remove_dir_all(&path)
                    .map_err(|e| format!("failed to remove stale dir {}: {e}", path.display()))?;
            } else {
                fs::remove_file(&path)
                    .map_err(|e| format!("failed to remove stale file {}: {e}", path.display()))?;
            }
        }
    }

    let dst_exe = dist_dir.join(RELEASE_EXE_NAME);
    let bytes = fs::copy(&src_exe, &dst_exe).map_err(|e| {
        format!(
            "failed to copy {} -> {}: {e}",
            src_exe.display(),
            dst_exe.display()
        )
    })?;

    println!(
        "Portable folder ready: {} ({RELEASE_EXE_NAME}, {:.2} MB)",
        dist_dir.display(),
        bytes as f64 / (1024.0 * 1024.0)
    );
    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn portable_filter_allows_only_release_executable() {
        assert!(is_portable_runtime_file("laptop-charge-monitor.exe"));
        assert!(is_portable_runtime_file("LAPTOP-CHARGE-MONITOR.EXE"));
        assert!(!is_portable_runtime_file("laptop_charge_monitor.pdb"));
        assert!(!is_portable_runtime_file("liblaptop_charge_monitor.rlib"));
        assert!(!is_portable_runtime_file("power-monitor.db"));
        assert!(!is_portable_runtime_file("laptop-charge-monitor.d"));
    }
}
