// ABOUTME: Complete-project input fingerprints and durable reconciliation catalogs.
// ABOUTME: Cached ASTs supply unchanged callers when definitions move or disappear.

use anyhow::Result;
use codegraph_core::{CodeNode, artifact_cache::fingerprint};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

#[derive(Default, Serialize, Deserialize)]
pub(crate) struct Catalog {
    pub fingerprint: String,
    pub nodes: BTreeMap<String, String>,
    pub edges: BTreeSet<String>,
    pub chunks: BTreeSet<String>,
    #[serde(default)]
    pub chunk_hashes: BTreeMap<String, String>,
    #[serde(default)]
    pub file_metadata_hashes: BTreeMap<String, String>,
    pub stats: crate::indexer::IndexStats,
}

pub(crate) fn node_digest(node: &CodeNode) -> Result<String> {
    let mut value = serde_json::to_value(node)?;
    if let Some(metadata) = value.get_mut("metadata").and_then(|v| v.as_object_mut()) {
        metadata.remove("created_at");
        metadata.remove("updated_at");
    }
    Ok(fingerprint(&value)?)
}

/// Stream artifact identity rather than materializing a potentially large compiler index.
pub(crate) fn file_fingerprint(path: &Path) -> Result<String> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
    Ok(hash
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

pub(crate) fn support_file(path: &Path) -> bool {
    let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
    matches!(
        name,
        "Cargo.toml"
            | "Cargo.lock"
            | "package.json"
            | "package-lock.json"
            | "pnpm-lock.yaml"
            | "yarn.lock"
            | "tsconfig.json"
            | "pyproject.toml"
            | "requirements.txt"
            | "codegraph.boundaries.toml"
            | "config.toml"
            | "global.json"
            | "NuGet.Config"
            | "nuget.config"
            | "packages.config"
            | "packages.lock.json"
    ) || codegraph_parser::file_collect::is_msbuild_project_path(path)
        || matches!(
            path.extension().and_then(|s| s.to_str()),
            Some("md" | "surql" | "csproj" | "sln" | "slnx" | "props" | "targets")
        )
}

pub(crate) fn support_fingerprints(root: &Path) -> Result<BTreeMap<String, String>> {
    let mut files = BTreeMap::new();
    for entry in walkdir::WalkDir::new(root)
        .into_iter()
        .filter_entry(|entry| {
            entry.depth() == 0
                || (!matches!(
                    entry.file_name().to_str(),
                    Some(".git" | ".codegraph" | "target" | "node_modules" | ".venv")
                ) && !codegraph_parser::file_collect::is_dotnet_build_directory(entry.path()))
        })
    {
        let entry = entry?;
        if entry.file_type().is_file() && support_file(entry.path()) {
            files.insert(
                entry.path().to_string_lossy().into_owned(),
                file_fingerprint(entry.path())?,
            );
        }
    }
    // Default SDK/NuGet inputs may live above the requested source directory.
    for directory in root.ancestors() {
        for name in [
            "global.json",
            "Directory.Build.props",
            "Directory.Build.targets",
            "Directory.Packages.props",
            "NuGet.Config",
            "nuget.config",
        ] {
            let path = directory.join(name);
            if path.is_file() {
                files.insert(
                    path.to_string_lossy().into_owned(),
                    file_fingerprint(&path)?,
                );
            }
        }
    }
    if let Some(solution) = std::env::var_os("CODEGRAPH_CSHARP_SOLUTION") {
        let path = root.join(solution);
        files.insert(
            path.to_string_lossy().into_owned(),
            file_fingerprint(&path)?,
        );
    }
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn dotnet_build_inputs_invalidate_reconciliation() {
        let root = tempfile::tempdir().unwrap();
        for name in [
            "App.csproj",
            "Library.fsproj",
            "Legacy.vbproj",
            "Native.vcxproj",
            "App.sln",
            "App.slnx",
            "Directory.Build.props",
            "Directory.Build.targets",
            "Directory.Packages.props",
            "global.json",
            "NuGet.Config",
            "packages.lock.json",
        ] {
            let before = support_fingerprints(root.path()).unwrap();
            std::fs::write(root.path().join(name), "changed").unwrap();
            assert_ne!(before, support_fingerprints(root.path()).unwrap(), "{name}");
        }
        let before = support_fingerprints(root.path()).unwrap();
        std::fs::create_dir(root.path().join("obj")).unwrap();
        std::fs::write(root.path().join("obj/generated.props"), "ignored").unwrap();
        assert_eq!(before, support_fingerprints(root.path()).unwrap());
    }
    #[test]
    fn manifest_and_document_changes_invalidate_inputs_but_cache_files_do_not() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("Cargo.toml"), "old").unwrap();
        let before = support_fingerprints(root.path()).unwrap();
        std::fs::create_dir(root.path().join(".codegraph")).unwrap();
        std::fs::write(root.path().join(".codegraph/cache.md"), "cache").unwrap();
        assert_eq!(before, support_fingerprints(root.path()).unwrap());
        std::fs::write(root.path().join("Cargo.toml"), "new").unwrap();
        assert_ne!(before, support_fingerprints(root.path()).unwrap());
        std::fs::write(root.path().join("README.md"), "docs").unwrap();
        assert_eq!(support_fingerprints(root.path()).unwrap().len(), 2);
    }
}
