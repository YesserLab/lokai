//! The Python environment for scripting: a virtual environment in the scratch directory, made
//! with uv when it's installed (fast, and `uv pip install` works) or with `python3 -m venv`.
//! Commands run with it activated, so `python` and `pip` never touch the system installation.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;

pub fn venv_dir(scratch: &Path) -> PathBuf {
    scratch.join(".venv")
}

fn on_path(program: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join(program).is_file()))
}

pub fn has_uv() -> bool {
    on_path("uv")
}

/// Creates the scratch venv unless it already exists. Returns its directory, or why it failed.
pub fn ensure_venv(scratch: &Path) -> Result<PathBuf, String> {
    static CREATING: Mutex<()> = Mutex::new(());
    let _guard = CREATING.lock().unwrap_or_else(|e| e.into_inner());
    let dir = venv_dir(scratch);
    if dir.join("bin/python").exists() {
        return Ok(dir);
    }
    // A leftover from an interrupted attempt would make both tools refuse.
    if dir.exists() {
        std::fs::remove_dir_all(&dir).map_err(|e| format!("cannot replace {}: {e}", dir.display()))?;
    }
    std::fs::create_dir_all(scratch).map_err(|e| format!("cannot create {}: {e}", scratch.display()))?;
    let (program, mut cmd) = if has_uv() {
        let mut cmd = Command::new("uv");
        cmd.args(["venv", "--seed", "--quiet"]).arg(&dir);
        ("uv venv", cmd)
    } else {
        let mut cmd = Command::new("python3");
        cmd.args(["-m", "venv"]).arg(&dir);
        ("python3 -m venv", cmd)
    };
    let out = cmd.env_remove("VIRTUAL_ENV").stdin(Stdio::null()).output().map_err(|e| format!("cannot run {program}: {e}"))?;
    if out.status.success() && dir.join("bin/python").exists() {
        Ok(dir)
    } else {
        Err(format!("{program} failed: {}", String::from_utf8_lossy(&out.stderr).trim()))
    }
}

/// The system-prompt lines about Python, if the venv exists.
pub fn prompt_hint(scratch: &Path) -> Option<String> {
    let dir = venv_dir(scratch);
    dir.join("bin/python").exists().then(|| {
        let install = if has_uv() { "`uv pip install <package>`" } else { "`pip install <package>`" };
        format!(
            "- Python: commands run inside a virtual environment at {} (`python`, `pip`). For Python scripts, \
             use it and install missing packages into it with {install}; never install packages system-wide.\n",
            dir.display()
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Creates a real venv (with uv if installed): `cargo test -- --ignored venv_roundtrip`
    #[test]
    #[ignore]
    fn venv_roundtrip() {
        let scratch = std::env::temp_dir().join(format!("lokai-venv-{}", std::process::id()));
        let dir = ensure_venv(&scratch).unwrap();
        assert!(dir.join("bin/python").exists() && dir.join("bin/pip").exists(), "python and pip in {}", dir.display());
        let again = ensure_venv(&scratch).unwrap(); // already there: no second creation
        assert_eq!(again, dir);
        assert!(prompt_hint(&scratch).unwrap().contains(&dir.display().to_string()));
        std::fs::remove_dir_all(&scratch).unwrap();
    }
}
