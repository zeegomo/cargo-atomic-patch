use anyhow::Result;
use rayon::iter::ParallelBridge;
use rayon::prelude::ParallelIterator;
use std::{
    fs::OpenOptions,
    io::{BufReader, BufWriter, Seek, Write},
    path::Path,
    process::Command,
};
use walkdir::WalkDir;

// Do not patch crates these crates to avoid cyclic dependencies
const NO_PATCH: &[&str] = &["atomic-core", "critical-section", "portable-atomic"];

#[allow(dead_code)]
enum Source {
    Git(String),
    CratesIo,
}

struct Crate {
    name: String,
    rename: Option<String>,
    source: Source,
    features: Vec<String>,
}

fn add_crate(manifest_path: &Path, new_crate: &Crate) -> Result<()> {
    let mut cmd = Command::new("cargo");

    let Crate {
        name,
        rename,
        source,
        features,
    } = new_crate;

    cmd.args(["add", name])
        .arg("--manifest-path")
        .arg(manifest_path)
        .arg("--no-optional");

    if let Source::Git(url) = source {
        cmd.args(["--git", url.as_str()]);
    }

    if let Some(rename) = rename {
        cmd.args(["--rename", rename]);
    }

    if !features.is_empty() {
        cmd.args(["--features", new_crate.features.join(",").as_str()]);
    }

    let output = cmd.output()?;
    if !output.status.success() {
        anyhow::bail!(
            "cargo add failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(())
}

// Add the new dependency to the manifest
fn patch_manifest(manifest_path: &Path) -> Result<()> {
    add_crate(
        manifest_path,
        &Crate {
            name: "atomic-core".into(),
            rename: Some("core".into()),
            source: Source::CratesIo,
            features: vec!["critical-section".into()],
        },
    )?;
    Ok(())
}

fn patch_crate(manifest: &Path) -> Result<()> {
    patch_manifest(manifest)
}

fn vendor(manifest_path: &Path, dir: &Path) -> Result<()> {
    eprintln!("Vendoring crates into {}", dir.display());
    let status = Command::new("cargo")
        .arg("vendor")
        .arg("--manifest-path")
        .arg(manifest_path)
        .current_dir(dir)
        .status()?;

    if !status.success() {
        anyhow::bail!("cargo vendor failed");
    }

    Ok(())
}

// Needed if the patched project is part of a workspace
fn add_empty_workspace(manifest_path: &Path) -> Result<()> {
    let mut file = OpenOptions::new().append(true).open(manifest_path)?;
    file.write_all(b"\n[workspace]\n")?;
    Ok(())
}

// Cargo saves a checksum for each file in the vendor directory.
// Removing such file will cause cargo to ignore it and it's more convenient than recomputing it.
fn remove_cargo_toml_checksum(manifest: &Path) -> Result<()> {
    let metadata_path = manifest.parent().unwrap().join(".cargo-checksum.json");
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(metadata_path)?;
    let mut metadata: serde_json::Value = serde_json::from_reader(BufReader::new(&file)).unwrap();
    metadata.as_object_mut().unwrap().insert(
        "files".into(),
        serde_json::Value::Object(serde_json::Map::new()),
    );
    file.set_len(0)?;
    file.seek(std::io::SeekFrom::Start(0))?;
    serde_json::to_writer(BufWriter::new(file), &metadata).unwrap();
    Ok(())
}

fn patch(manifest_path: &Path) -> Result<()> {
    let dir = manifest_path.parent().unwrap();
    patch_crate(manifest_path)?;
    vendor(manifest_path, dir)?;
    let vendor_dir = dir.join("vendor");
    let manifests = WalkDir::new(vendor_dir)
        .max_depth(2)
        .into_iter()
        .par_bridge()
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.file_type().is_file()
                && e.path()
                    .file_name()
                    .map(|n| n == "Cargo.toml")
                    .unwrap_or(false)
        })
        // Do not recusively patch crates used in the patch
        .filter(|file| {
            for krate in NO_PATCH {
                if file.path().parent().unwrap().ends_with(krate) {
                    return false;
                }
            }
            true
        });

    manifests.for_each(|manifest| {
        add_empty_workspace(manifest.path()).unwrap();
        if let Err(e) = patch_crate(manifest.path()) {
            eprintln!("error patching {}: {}", manifest.path().display(), e);
        }
        remove_cargo_toml_checksum(manifest.path()).unwrap();
    });

    Ok(())
}

fn main() -> Result<()> {
    let manifest = std::env::current_dir()
        .unwrap()
        .join("Cargo.toml")
        .canonicalize()?;
    patch(&manifest)
}
