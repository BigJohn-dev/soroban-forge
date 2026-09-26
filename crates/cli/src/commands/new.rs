use crate::cli::NewArgs;
use anyhow::{bail, Context, Result};
use std::fs;
use std::path::{Path, PathBuf};

/// Templates bundled at compile time into the binary.
const TEMPLATE_CARGO_TOML: &str = include_str!("../../../../templates/Cargo.toml");
const TEMPLATE_LIB_RS: &str = include_str!("../../../../templates/lib.rs");
const TEMPLATE_CONTRACT_RS: &str = include_str!("../../../../templates/contract.rs");
const TEMPLATE_TYPES_RS: &str = include_str!("../../../../templates/types.rs");
const TEMPLATE_ERRORS_RS: &str = include_str!("../../../../templates/errors.rs");
const TEMPLATE_README_MD: &str = include_str!("../../../../templates/contract/README.md");

/// Validates that a contract name is non-empty and contains only alphanumeric characters,
/// hyphens, or underscores.
pub fn validate_contract_name(name: &str) -> Result<()> {
    if name.trim().is_empty() {
        bail!("Contract name cannot be empty");
    }

    if !name
        .chars()
        .all(|c| c.is_alphanumeric() || c == '-' || c == '_')
    {
        bail!(
            "Invalid contract name '{}': name must contain only alphanumeric characters, hyphens, or underscores",
            name
        );
    }

    Ok(())
}

/// Converts a contract name to kebab-case (e.g., `my_contract` -> `my-contract`).
pub fn to_kebab_case(name: &str) -> String {
    name.replace('_', "-").to_lowercase()
}

/// Converts a contract name to snake_case (e.g., `my-contract` -> `my_contract`).
#[allow(dead_code)]
pub fn to_snake_case(name: &str) -> String {
    name.replace('-', "_").to_lowercase()
}

pub fn run(args: NewArgs) -> Result<()> {
    validate_contract_name(&args.name)?;

    let kebab_name = to_kebab_case(&args.name);

    let target_dir = match args.path {
        Some(path) => path,
        None => {
            if Path::new("crates").is_dir() {
                PathBuf::from("crates").join(&kebab_name)
            } else {
                PathBuf::from(&kebab_name)
            }
        }
    };

    if target_dir.exists() {
        bail!(
            "Target directory '{}' already exists; refusing to overwrite",
            target_dir.display()
        );
    }

    let src_dir = target_dir.join("src");
    fs::create_dir_all(&src_dir).with_context(|| {
        format!(
            "Failed to create contract directory structure at '{}'",
            src_dir.display()
        )
    })?;

    // Perform template substitution
    let cargo_toml_content = TEMPLATE_CARGO_TOML.replace("<CONTRACT_NAME>", &kebab_name);
    let lib_rs_content = TEMPLATE_LIB_RS.replace("<CONTRACT_NAME>", &kebab_name);
    let contract_rs_content = TEMPLATE_CONTRACT_RS.replace("<CONTRACT_NAME>", &kebab_name);
    let types_rs_content = TEMPLATE_TYPES_RS.replace("<CONTRACT_NAME>", &kebab_name);
    let errors_rs_content = TEMPLATE_ERRORS_RS.replace("<CONTRACT_NAME>", &kebab_name);
    let readme_md_content = TEMPLATE_README_MD.replace("<CONTRACT_NAME>", &kebab_name);

    fs::write(target_dir.join("Cargo.toml"), cargo_toml_content)
        .context("Failed to write Cargo.toml")?;
    fs::write(src_dir.join("lib.rs"), lib_rs_content).context("Failed to write src/lib.rs")?;
    fs::write(src_dir.join("contract.rs"), contract_rs_content)
        .context("Failed to write src/contract.rs")?;
    fs::write(src_dir.join("types.rs"), types_rs_content)
        .context("Failed to write src/types.rs")?;
    fs::write(src_dir.join("errors.rs"), errors_rs_content)
        .context("Failed to write src/errors.rs")?;
    fs::write(target_dir.join("README.md"), readme_md_content)
        .context("Failed to write README.md")?;

    println!(
        "Successfully scaffolded new Soroban contract '{}' at '{}'",
        kebab_name,
        target_dir.display()
    );

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_contract_name_validation() {
        assert!(validate_contract_name("my-contract").is_ok());
        assert!(validate_contract_name("my_contract_1").is_ok());
        assert!(validate_contract_name("token").is_ok());

        assert!(validate_contract_name("").is_err());
        assert!(validate_contract_name("  ").is_err());
        assert!(validate_contract_name("my contract").is_err());
        assert!(validate_contract_name("contract!").is_err());
        assert!(validate_contract_name("contract@name").is_err());
    }

    #[test]
    fn test_case_conversions() {
        assert_eq!(to_kebab_case("My_Contract"), "my-contract");
        assert_eq!(to_snake_case("my-contract"), "my_contract");
    }

    #[test]
    fn test_scaffold_new_contract() -> Result<()> {
        let temp_dir = tempdir()?;
        let target_path = temp_dir.path().join("test-token");

        let args = NewArgs {
            name: "test-token".to_string(),
            path: Some(target_path.clone()),
        };

        run(args)?;

        assert!(target_path.join("Cargo.toml").exists());
        assert!(target_path.join("README.md").exists());
        assert!(target_path.join("src/lib.rs").exists());
        assert!(target_path.join("src/contract.rs").exists());
        assert!(target_path.join("src/types.rs").exists());
        assert!(target_path.join("src/errors.rs").exists());

        let cargo_content = fs::read_to_string(target_path.join("Cargo.toml"))?;
        assert!(cargo_content.contains("name = \"soroban-forge-test-token\""));

        Ok(())
    }

    #[test]
    fn test_refuses_existing_directory() -> Result<()> {
        let temp_dir = tempdir()?;
        let target_path = temp_dir.path().join("existing-contract");
        fs::create_dir_all(&target_path)?;

        let args = NewArgs {
            name: "existing-contract".to_string(),
            path: Some(target_path),
        };

        assert!(run(args).is_err());
        Ok(())
    }
}
