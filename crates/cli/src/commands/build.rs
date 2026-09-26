use crate::cli::BuildArgs;
use anyhow::{bail, Context, Result};
use std::env;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const WASM_TARGET: &str = "wasm32v1-none";
const SIZE_BUDGET_BYTES: u64 = 150_000;
const CONTRACT_PACKAGES: &[&str] = &[
    "soroban-forge-escrow",
    "soroban-forge-vesting",
    "soroban-forge-multi-sig-wallet",
    "soroban-forge-dao-governance",
    "soroban-forge-subscription-payments",
    "soroban-forge-marketplace-royalties",
];

pub fn run(args: BuildArgs) -> Result<()> {
    // Checking size is useful only for a fresh WASM build, so it implies WASM
    // mode even if the caller omits --wasm.
    let wasm = args.wasm || args.check_size;
    if wasm {
        ensure_wasm_target_installed()?;
    }

    let mut cmd = Command::new("cargo");
    cmd.arg("build");
    if wasm {
        cmd.arg("--release").arg("--target").arg(WASM_TARGET);
    }

    match args.package.as_deref() {
        Some(pkg) => {
            cmd.arg("--package").arg(pkg);
        }
        None if wasm => {
            for package in CONTRACT_PACKAGES {
                cmd.arg("--package").arg(package);
            }
        }
        None => {
            cmd.arg("--workspace");
        }
    }

    if args.release && !wasm {
        cmd.arg("--release");
    }
    if args.all_targets {
        cmd.arg("--all-targets");
    }

    let status = cmd.status().context("failed to run cargo build")?;
    if !status.success() {
        bail!("cargo build failed with status {status}");
    }

    if args.check_size {
        let output_dir = wasm_release_dir();
        let artifacts = read_wasm_artifacts(&output_dir, args.package.as_deref())?;
        print_size_summary(&artifacts);
        let oversized: Vec<_> = artifacts
            .iter()
            .filter(|artifact| !within_size_budget(artifact.size_bytes))
            .map(|artifact| artifact.name.as_str())
            .collect();
        if !oversized.is_empty() {
            bail!(
                "{} contract artifact(s) exceed the {SIZE_BUDGET_BYTES}-byte WASM size budget: {}",
                oversized.len(),
                oversized.join(", ")
            );
        }
    }

    Ok(())
}

fn ensure_wasm_target_installed() -> Result<()> {
    let rustc = env::var_os("RUSTC").unwrap_or_else(|| OsString::from("rustc"));
    let output = Command::new(rustc)
        .args(["--print", "target-libdir", "--target", WASM_TARGET])
        .output()
        .context("failed to check whether the WASM target is installed")?;
    if !output.status.success() {
        bail!(missing_target_message());
    }

    let libdir = String::from_utf8_lossy(&output.stdout);
    let libdir = Path::new(libdir.trim());
    ensure_target_libdir(libdir)
}

fn ensure_target_libdir(libdir: &Path) -> Result<()> {
    let has_core = fs::read_dir(libdir)
        .with_context(missing_target_message)?
        .filter_map(std::result::Result::ok)
        .any(|entry| entry.file_name().to_string_lossy().starts_with("libcore-"));
    if has_core {
        Ok(())
    } else {
        bail!(missing_target_message());
    }
}

fn missing_target_message() -> String {
    format!(
        "the `{WASM_TARGET}` target is not installed for the Rust compiler Cargo will use; install it with `rustup target add {WASM_TARGET}`"
    )
}

fn wasm_release_dir() -> PathBuf {
    env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("target"))
        .join(WASM_TARGET)
        .join("release")
}

#[derive(Debug, Eq, PartialEq)]
struct WasmArtifact {
    name: String,
    size_bytes: u64,
}

fn read_wasm_artifacts(dir: &Path, package: Option<&str>) -> Result<Vec<WasmArtifact>> {
    let entries = fs::read_dir(dir).with_context(|| {
        format!(
            "could not read WASM output directory {}; build a contract with `--wasm` first",
            dir.display()
        )
    })?;
    let expected_stem = package.map(|name| name.replace('-', "_"));
    let mut artifacts = Vec::new();

    for entry in entries {
        let entry = entry.context("failed to read WASM output directory entry")?;
        let path = entry.path();
        if path.extension().is_none_or(|extension| extension != "wasm") {
            continue;
        }
        let stem = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or_default();
        if expected_stem
            .as_deref()
            .is_some_and(|expected| stem != expected)
        {
            continue;
        }
        let size_bytes = fs::metadata(&path)
            .with_context(|| format!("failed to inspect WASM file {}", path.display()))?
            .len();
        artifacts.push(WasmArtifact {
            name: path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or_default()
                .to_owned(),
            size_bytes,
        });
    }

    artifacts.sort_by(|left, right| left.name.cmp(&right.name));
    if artifacts.is_empty() {
        bail!(
            "no WASM artifacts found in {}; check that the selected package builds a contract",
            dir.display()
        );
    }
    Ok(artifacts)
}

fn within_size_budget(size_bytes: u64) -> bool {
    size_bytes <= SIZE_BUDGET_BYTES
}

fn print_size_summary(artifacts: &[WasmArtifact]) {
    let name_width = artifacts
        .iter()
        .map(|artifact| artifact.name.len())
        .max()
        .unwrap_or("Contract".len())
        .max("Contract".len());
    println!(
        "{:<name_width$}  {:>12}  {:>10}  {:>15}  Status",
        "Contract", "Size (bytes)", "Size (KB)", "Budget (bytes)"
    );
    println!(
        "{:-<name_width$}  {:-<12}  {:-<10}  {:-<15}  {:-<6}",
        "", "", "", "", ""
    );
    for artifact in artifacts {
        let status = if within_size_budget(artifact.size_bytes) {
            "PASS"
        } else {
            "FAIL"
        };
        let kilobytes = artifact.size_bytes / 1024;
        let hundredths = (artifact.size_bytes % 1024) * 100 / 1024;
        println!(
            "{:<name_width$}  {:>12}  {:>7}.{:02}  {:>15}  {}",
            artifact.name, artifact.size_bytes, kilobytes, hundredths, SIZE_BUDGET_BYTES, status
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir() -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock must be after unix epoch")
            .as_nanos();
        env::temp_dir().join(format!("soroban-forge-cli-{}-{nonce}", std::process::id()))
    }

    #[test]
    fn size_budget_accepts_exact_limit_and_rejects_over_limit() {
        assert!(within_size_budget(SIZE_BUDGET_BYTES));
        assert!(within_size_budget(SIZE_BUDGET_BYTES - 1));
        assert!(!within_size_budget(SIZE_BUDGET_BYTES + 1));
    }

    #[test]
    fn missing_target_error_gives_rustup_install_command() {
        assert!(missing_target_message().contains("rustup target add wasm32v1-none"));
        let missing_dir = temp_dir();
        let err = ensure_target_libdir(&missing_dir).unwrap_err();
        assert!(err.to_string().contains("rustup target add wasm32v1-none"));
    }

    #[test]
    fn reads_wasm_sizes_and_filters_to_selected_package() {
        let dir = temp_dir();
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("soroban_forge_escrow.wasm"), vec![0; 24]).unwrap();
        fs::write(dir.join("soroban_forge_vesting.wasm"), vec![0; 48]).unwrap();
        fs::write(dir.join("not-wasm.txt"), b"ignore").unwrap();

        let artifacts = read_wasm_artifacts(&dir, Some("soroban-forge-escrow")).unwrap();
        assert_eq!(
            artifacts,
            vec![WasmArtifact {
                name: "soroban_forge_escrow.wasm".to_owned(),
                size_bytes: 24,
            }]
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn missing_wasm_artifacts_returns_actionable_error() {
        let dir = temp_dir();
        fs::create_dir_all(&dir).unwrap();
        let err = read_wasm_artifacts(&dir, None).unwrap_err();
        assert!(err.to_string().contains("no WASM artifacts found"));
        fs::remove_dir_all(dir).unwrap();
    }
}
