// ABOUTME: Evaluates mixed .NET workspace dependencies without building or restoring.
// ABOUTME: Emits project-scoped package nodes and records imported build inputs.

use super::build_context::BuildContextOutput;
use anyhow::{Context, Result, anyhow};
use codegraph_core::{CodeNode, EdgeRelationship, EdgeType, Language, Location, NodeType};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

const CACHE_NAMESPACE: &str = "dotnet-msbuild-v1";

pub async fn analyze_workspace(root: &Path, project_id: &str) -> Result<BuildContextOutput> {
    let configuration =
        std::env::var("CODEGRAPH_DOTNET_CONFIGURATION").unwrap_or_else(|_| "Debug".into());
    let framework = std::env::var("CODEGRAPH_DOTNET_TARGET_FRAMEWORK").ok();
    analyze_workspace_with_options(root, project_id, &configuration, framework.as_deref()).await
}

async fn analyze_workspace_with_options(
    root: &Path,
    project_id: &str,
    configuration: &str,
    framework_override: Option<&str>,
) -> Result<BuildContextOutput> {
    let root = std::fs::canonicalize(root)?;
    let inputs = crate::reconciliation::support_fingerprints(&root)?;
    let mut pending: BTreeSet<PathBuf> = inputs
        .keys()
        .map(PathBuf::from)
        .filter(|path| codegraph_parser::file_collect::is_msbuild_project_path(path))
        .collect();
    let mut seen = BTreeSet::new();
    let mut external = BTreeMap::new();
    let mut output = BuildContextOutput::default();
    for solution in inputs.keys().map(PathBuf::from).filter(|path| {
        matches!(
            path.extension().and_then(|extension| extension.to_str()),
            Some("sln" | "slnx")
        )
    }) {
        let identity = format!("dotnet-solution::{}", solution.to_string_lossy());
        let mut node = package_node(
            &solution,
            &identity,
            solution
                .file_stem()
                .and_then(|name| name.to_str())
                .unwrap_or("solution"),
            project_id,
            "dotnet_solution",
        );
        node.node_type = Some(NodeType::Other("solution".into()));
        node.language = None;
        node.metadata
            .attributes
            .insert("analyzer".into(), "dotnet_solution".into());
        for manifest in solution_projects(&solution)? {
            let manifest = std::fs::canonicalize(&manifest).with_context(|| {
                format!("Solution project does not exist: {}", manifest.display())
            })?;
            let target = package_node(
                &manifest,
                &project_identity(&manifest),
                manifest
                    .file_stem()
                    .and_then(|name| name.to_str())
                    .unwrap_or("project"),
                project_id,
                "dotnet_project",
            );
            let mut edge = dependency(node.id, &target, "", "solution_project");
            edge.edge_type = EdgeType::Contains;
            edge.metadata
                .insert("analyzer".into(), "dotnet_solution".into());
            output.edges.push(edge);
            pending.insert(manifest);
        }
        output.nodes.push(node);
    }
    while let Some(manifest) = pending.pop_first() {
        let manifest = std::fs::canonicalize(&manifest)
            .with_context(|| format!("MSBuild project does not exist: {}", manifest.display()))?;
        if !seen.insert(manifest.clone()) {
            continue;
        }
        if !is_managed_project(&manifest) {
            let mut node = package_node(
                &manifest,
                &project_identity(&manifest),
                manifest
                    .file_stem()
                    .and_then(|name| name.to_str())
                    .unwrap_or("project"),
                project_id,
                "dotnet_project",
            );
            node.metadata.attributes.insert(
                "evaluation_status".into(),
                "unsupported_project_type".into(),
            );
            node.metadata
                .attributes
                .insert("analyzer".into(), "dotnet_solution".into());
            output.nodes.push(node);
            record_imports(&Value::Null, &manifest, &mut external)?;
            continue;
        }
        let initial = evaluate_project(&manifest, configuration, framework_override).await?;
        let frameworks: Vec<_> = if framework_override.is_none() {
            initial["Properties"]["TargetFrameworks"]
                .as_str()
                .unwrap_or("")
                .split(';')
                .filter(|framework| !framework.is_empty())
                .map(str::to_owned)
                .collect()
        } else {
            Vec::new()
        };
        let evaluations = if frameworks.is_empty() {
            vec![initial]
        } else {
            let mut evaluations = Vec::new();
            record_imports(&initial, &manifest, &mut external)?;
            for framework in frameworks {
                evaluations
                    .push(evaluate_project(&manifest, configuration, Some(&framework)).await?);
            }
            evaluations
        };
        for evaluation in evaluations {
            record_imports(&evaluation, &manifest, &mut external)?;
            for reference in items(&evaluation, "ProjectReference") {
                if let Some(path) = item_path(reference, &manifest) {
                    // Keep dependencies outside the indexed root as project nodes too.
                    // Their sources remain outside the caller's requested source scope.
                    pending.insert(path);
                }
            }
            let parsed = parse_evaluation(&evaluation, &manifest, project_id)?;
            output.nodes.extend(parsed.nodes);
            output.edges.extend(parsed.edges);
        }
    }
    output.nodes.sort_by_key(|node| node.id);
    output.nodes.dedup_by_key(|node| node.id);
    output.edges.sort_by_key(|edge| {
        (
            edge.from,
            edge.to.clone(),
            edge.metadata.get("target_framework").cloned(),
        )
    });
    output.edges.dedup_by(|a, b| {
        a.from == b.from && a.to == b.to && a.edge_type == b.edge_type && a.metadata == b.metadata
    });
    codegraph_core::artifact_cache::ArtifactCache::new(
        root.join(".codegraph/index-cache"),
        CACHE_NAMESPACE,
    )
    .put("external-inputs", &external)?;
    Ok(output)
}

async fn evaluate_project(
    manifest: &Path,
    configuration: &str,
    framework: Option<&str>,
) -> Result<Value> {
    let result = run_msbuild(manifest, configuration, framework, false).await?;
    let mut evaluation: Value = serde_json::from_slice(&result).with_context(|| {
        format!(
            "Invalid MSBuild JSON for {} (MSBuild 17.8+ is required)",
            manifest.display()
        )
    })?;
    // MSBuildAllProjects is populated by convention and omits some custom imports.
    // Preprocessing reports every evaluated import without running build targets.
    let expanded = run_msbuild(manifest, configuration, framework, true).await?;
    evaluation["CodeGraphImports"] = serde_json::to_value(preprocessed_imports(&expanded)?)?;
    Ok(evaluation)
}

fn preprocessed_imports(xml: &[u8]) -> Result<BTreeSet<PathBuf>> {
    use quick_xml::events::Event;
    let mut reader = quick_xml::Reader::from_reader(xml);
    let mut inputs = BTreeSet::new();
    loop {
        match reader.read_event()? {
            Event::Comment(comment) => {
                let text = comment.decode()?;
                let lines: Vec<_> = text.lines().map(str::trim).collect();
                // MSBuild places each imported absolute path before a separator.
                for pair in lines.windows(2) {
                    let path = PathBuf::from(pair[0]);
                    if path.is_absolute()
                        && pair[1].len() > 10
                        && pair[1].bytes().all(|byte| byte == b'=')
                    {
                        inputs.insert(std::fs::canonicalize(&path).with_context(|| {
                            format!("Cannot read MSBuild import {}", path.display())
                        })?);
                    }
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(inputs)
}

async fn run_msbuild(
    manifest: &Path,
    configuration: &str,
    framework: Option<&str>,
    preprocess: bool,
) -> Result<Vec<u8>> {
    let mut command = tokio::process::Command::new("dotnet");
    #[cfg(windows)]
    command.creation_flags(0x08000000); // CREATE_NO_WINDOW
    command
        .args(["msbuild", "-nologo", "-verbosity:quiet"])
        .arg(msbuild_path(manifest))
        .arg(format!("-property:Configuration={configuration}"))
        .current_dir(msbuild_path(manifest.parent().unwrap()))
        .env("DOTNET_NOLOGO", "1")
        .env("DOTNET_SKIP_FIRST_TIME_EXPERIENCE", "1")
        .stdin(Stdio::null())
        .kill_on_drop(true);
    if let Some(framework) = framework {
        command.arg(format!("-property:TargetFramework={framework}"));
    }
    if preprocess {
        command.arg("-preprocess");
    } else {
        command.arg("-getProperty:MSBuildProjectName,TargetFramework,TargetFrameworks,TargetFrameworkVersion,AssemblyName,MSBuildAllProjects,ProjectAssetsFile")
            .arg("-getItem:ProjectReference,PackageReference,PackageVersion");
    }
    let result = tokio::time::timeout(Duration::from_secs(120), command.output())
        .await
        .with_context(|| format!("MSBuild evaluation timed out: {}", manifest.display()))??;
    if !result.status.success() {
        return Err(anyhow!(
            "MSBuild evaluation failed for {}: {}\n{}",
            manifest.display(),
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        ));
    }
    Ok(result.stdout)
}

fn msbuild_path(path: &Path) -> PathBuf {
    // Rust canonical paths use extended Windows prefixes. MSBuild interprets the
    // question mark in that prefix as a wildcard when it expands SDK imports.
    #[cfg(windows)]
    {
        use std::path::{Component, Prefix};
        let mut components = path.components();
        if let Some(Component::Prefix(prefix)) = components.next() {
            let mut normal = match prefix.kind() {
                Prefix::VerbatimDisk(drive) => {
                    std::ffi::OsString::from(format!("{}:", drive as char))
                }
                Prefix::VerbatimUNC(server, share) => {
                    let mut normal = std::ffi::OsString::from(r"\\");
                    normal.push(server);
                    normal.push(r"\");
                    normal.push(share);
                    normal
                }
                _ => return path.to_path_buf(),
            };
            normal.push(components.as_path());
            return normal.into();
        }
    }
    path.to_path_buf()
}

fn items<'a>(evaluation: &'a Value, name: &str) -> &'a [Value] {
    evaluation["Items"][name]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

fn item_path(item: &Value, manifest: &Path) -> Option<PathBuf> {
    let path = item["FullPath"]
        .as_str()
        .filter(|path| !path.is_empty())
        .or_else(|| item["Identity"].as_str())?;
    let path = PathBuf::from(path.replace('\\', "/"));
    let path = if path.is_absolute() {
        path
    } else {
        manifest.parent()?.join(path)
    };
    Some(std::fs::canonicalize(&path).unwrap_or(path))
}

fn package_node(
    manifest: &Path,
    identity: &str,
    name: &str,
    project_id: &str,
    kind: &str,
) -> CodeNode {
    let mut node = CodeNode::new(
        name,
        Some(NodeType::Other("package".into())),
        project_language(manifest),
        Location {
            file_path: manifest.to_string_lossy().into_owned(),
            line: 1,
            column: 0,
            end_line: Some(1),
            end_column: Some(0),
        },
    );
    node.id = codegraph_core::generate_node_id(
        project_id,
        &node.location.file_path,
        identity,
        "package",
        1,
    );
    node.metadata.attributes.extend([
        ("qualified_name".into(), identity.into()),
        ("kind".into(), kind.into()),
        ("analyzer".into(), "dotnet_msbuild".into()),
        ("analyzer_confidence".into(), "1.0".into()),
    ]);
    if let Some(extension) = manifest
        .extension()
        .and_then(|extension| extension.to_str())
    {
        node.metadata
            .attributes
            .insert("project_type".into(), extension.into());
    }
    node
}

fn project_identity(manifest: &Path) -> String {
    format!("dotnet-project::{}", manifest.to_string_lossy())
}

fn is_managed_project(manifest: &Path) -> bool {
    matches!(
        manifest
            .extension()
            .and_then(|extension| extension.to_str())
            .map(str::to_ascii_lowercase)
            .as_deref(),
        Some("csproj" | "fsproj" | "vbproj")
    )
}

fn project_language(manifest: &Path) -> Option<Language> {
    match manifest
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("csproj") => Some(Language::CSharp),
        Some("fsproj") => Some(Language::Other("FSharp".into())),
        Some("vbproj") => Some(Language::Other("VisualBasic".into())),
        Some("vcxproj") => Some(Language::Cpp),
        _ => None,
    }
}

fn solution_projects(solution: &Path) -> Result<Vec<PathBuf>> {
    let text = std::fs::read_to_string(solution)?;
    let text = text.trim_start_matches('\u{feff}');
    let mut paths = BTreeSet::new();
    if solution
        .extension()
        .is_some_and(|extension| extension == "slnx")
    {
        use quick_xml::events::Event;
        let mut reader = quick_xml::Reader::from_str(text);
        loop {
            match reader.read_event()? {
                Event::Start(element) | Event::Empty(element)
                    if element.name().as_ref() == b"Project" =>
                {
                    for attribute in element.attributes() {
                        let attribute = attribute?;
                        if attribute.key.as_ref() == b"Path" {
                            paths.insert(
                                attribute
                                    .decode_and_unescape_value(reader.decoder())?
                                    .into_owned(),
                            );
                        }
                    }
                }
                Event::Eof => break,
                _ => {}
            }
        }
    } else {
        let pattern =
            regex::Regex::new(r#"(?m)^\s*Project\("[^"]+"\)\s*=\s*"[^"]*",\s*"([^"]+)""#)?;
        paths.extend(
            pattern
                .captures_iter(text)
                .map(|capture| capture[1].to_owned()),
        );
    }
    Ok(paths
        .into_iter()
        .map(|path| solution.parent().unwrap().join(path.replace('\\', "/")))
        .filter(|path| codegraph_parser::file_collect::is_msbuild_project_path(path))
        .collect())
}

fn parse_evaluation(
    evaluation: &Value,
    manifest: &Path,
    project_id: &str,
) -> Result<BuildContextOutput> {
    let properties = evaluation
        .get("Properties")
        .and_then(Value::as_object)
        .ok_or_else(|| anyhow!("Missing Properties in MSBuild output"))?;
    let name = properties
        .get("MSBuildProjectName")
        .and_then(Value::as_str)
        .unwrap_or_else(|| {
            manifest
                .file_stem()
                .and_then(|name| name.to_str())
                .unwrap_or("project")
        });
    let framework = properties
        .get("TargetFramework")
        .and_then(Value::as_str)
        .unwrap_or("");
    let identity = project_identity(manifest);
    let mut project = package_node(manifest, &identity, name, project_id, "dotnet_project");
    for property in [
        "AssemblyName",
        "TargetFramework",
        "TargetFrameworks",
        "TargetFrameworkVersion",
    ] {
        if let Some(value) = properties.get(property).and_then(Value::as_str) {
            project
                .metadata
                .attributes
                .insert(property.into(), value.into());
        }
    }
    let from = project.id;
    let mut output = BuildContextOutput {
        nodes: vec![project],
        edges: Vec::new(),
    };
    for reference in items(evaluation, "ProjectReference") {
        let path = item_path(reference, manifest)
            .ok_or_else(|| anyhow!("ProjectReference has no path"))?;
        let target_identity = project_identity(&path);
        let target = package_node(
            &path,
            &target_identity,
            path.file_stem()
                .and_then(|name| name.to_str())
                .unwrap_or("project"),
            project_id,
            "dotnet_project",
        );
        output
            .edges
            .push(dependency(from, &target, framework, "project_reference"));
        // The dependency is evaluated separately; avoid a placeholder shadowing it.
    }
    for package in items(evaluation, "PackageReference") {
        let name = package["Identity"]
            .as_str()
            .ok_or_else(|| anyhow!("PackageReference has no Identity"))?;
        let central = items(evaluation, "PackageVersion").iter().find(|version| {
            version["Identity"]
                .as_str()
                .is_some_and(|identity| identity.eq_ignore_ascii_case(name))
        });
        let version = package["VersionOverride"]
            .as_str()
            .filter(|value| !value.is_empty())
            .or_else(|| {
                package["Version"]
                    .as_str()
                    .filter(|value| !value.is_empty())
            })
            .or_else(|| central.and_then(|version| version["Version"].as_str()))
            .unwrap_or("");
        // Package IDs are case-insensitive; keep versions and projects distinct.
        let identity = format!("nuget::{}@{}", name.to_ascii_lowercase(), version);
        let mut target = package_node(manifest, &identity, name, project_id, "nuget_package");
        target
            .metadata
            .attributes
            .insert("version".into(), version.into());
        output
            .edges
            .push(dependency(from, &target, framework, "package_reference"));
        output.nodes.push(target);
    }
    Ok(output)
}

fn dependency(
    from: codegraph_core::NodeId,
    target: &CodeNode,
    framework: &str,
    kind: &str,
) -> EdgeRelationship {
    EdgeRelationship {
        from,
        to: target.metadata.attributes["qualified_name"].clone(),
        edge_type: EdgeType::Other("depends_on".into()),
        metadata: HashMap::from([
            ("analyzer".into(), "dotnet_msbuild".into()),
            ("analyzer_confidence".into(), "1.0".into()),
            ("target_node_id".into(), target.id.to_string()),
            ("target_framework".into(), framework.into()),
            ("dependency_kind".into(), kind.into()),
        ]),
        span: None,
    }
}

fn record_imports(
    evaluation: &Value,
    manifest: &Path,
    external: &mut BTreeMap<String, String>,
) -> Result<()> {
    let imports = evaluation["Properties"]["MSBuildAllProjects"]
        .as_str()
        .unwrap_or("");
    for path in std::iter::once(manifest.to_path_buf())
        .chain(
            imports
                .split(';')
                .filter(|path| !path.is_empty())
                .map(|path| manifest.parent().unwrap().join(path)),
        )
        .chain(
            evaluation["CodeGraphImports"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(PathBuf::from),
        )
    {
        let path = std::fs::canonicalize(&path)
            .with_context(|| format!("Cannot read MSBuild input {}", path.display()))?;
        let key = path.to_string_lossy().into_owned();
        if let std::collections::btree_map::Entry::Vacant(entry) = external.entry(key) {
            entry.insert(crate::reconciliation::file_fingerprint(&path)?);
        }
    }
    if let Some(assets) = evaluation["Properties"]["ProjectAssetsFile"]
        .as_str()
        .filter(|path| !path.is_empty())
    {
        let path = manifest.parent().unwrap().join(assets);
        let path = std::fs::canonicalize(&path).unwrap_or(path);
        let hash = if path.is_file() {
            crate::reconciliation::file_fingerprint(&path)?
        } else {
            "<missing>".into()
        };
        external.insert(path.to_string_lossy().into_owned(), hash);
    }
    Ok(())
}

pub(crate) fn external_inputs(root: &Path) -> BTreeMap<String, String> {
    codegraph_core::artifact_cache::ArtifactCache::new(
        root.join(".codegraph/index-cache"),
        CACHE_NAMESPACE,
    )
    .get("external-inputs")
    .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn solution_formats_preserve_mixed_project_types_and_ignore_solution_folders() {
        let root = tempfile::tempdir().unwrap();
        let sln = root.path().join("Mixed.sln");
        std::fs::write(&sln, "Project(\"type\") = \"C#\", \"App\\App.csproj\", \"id\"\nProject(\"type\") = \"F#\", \"Lib\\Lib.fsproj\", \"id\"\nProject(\"type\") = \"VB\", \"Legacy.vbproj\", \"id\"\nProject(\"folder\") = \"Folder\", \"Folder\", \"id\"\n").unwrap();
        let projects = solution_projects(&sln).unwrap();
        assert_eq!(projects.len(), 3);
        assert!(projects.contains(&root.path().join("Lib/Lib.fsproj")));
        let slnx = root.path().join("Mixed.slnx");
        std::fs::write(&slnx, "<Solution><Folder Name=\"/Shared/\"><Project Path=\"F &amp; VB/Library.fsproj\" /><Project Path=\"Legacy.vbproj\" /></Folder><Project Path=\"Native.vcxproj\" /></Solution>").unwrap();
        let projects = solution_projects(&slnx).unwrap();
        assert_eq!(projects.len(), 3);
        assert!(projects.contains(&root.path().join("F & VB/Library.fsproj")));
        assert_eq!(
            project_language(Path::new("Lib.fsproj")),
            Some(Language::Other("FSharp".into()))
        );
        assert_eq!(
            project_language(Path::new("Lib.vbproj")),
            Some(Language::Other("VisualBasic".into()))
        );
    }

    #[tokio::test]
    async fn mixed_solution_keeps_cross_language_references_and_unevaluated_native_projects() {
        if !std::process::Command::new("dotnet")
            .arg("--version")
            .output()
            .is_ok_and(|output| output.status.success())
        {
            eprintln!("SKIP: dotnet SDK is not available for the mixed solution fixture");
            return;
        }
        let fixture = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(fixture.path()).unwrap();
        for (name, reference) in [
            ("App.csproj", Some("Lib.fsproj")),
            ("Lib.fsproj", Some("Legacy.vbproj")),
            ("Legacy.vbproj", Some("Native.vcxproj")),
        ] {
            let references = reference
                .map(|reference| {
                    format!("<ItemGroup><ProjectReference Include=\"{reference}\" /></ItemGroup>")
                })
                .unwrap_or_default();
            std::fs::write(root.join(name), format!("<Project><PropertyGroup><TargetFramework>net10.0</TargetFramework></PropertyGroup>{references}</Project>")).unwrap();
        }
        // Native tooling is deliberately unavailable; project membership must survive.
        std::fs::write(
            root.join("Native.vcxproj"),
            "<Project><Import Project=\"UnavailableCppTools.props\" /></Project>",
        )
        .unwrap();
        std::fs::write(root.join("Mixed.slnx"), "<Solution><Project Path=\"App.csproj\" /><Project Path=\"Lib.fsproj\" /><Project Path=\"Legacy.vbproj\" /><Project Path=\"Native.vcxproj\" /></Solution>").unwrap();
        let output = analyze_workspace_with_options(&root, "mixed", "Debug", None)
            .await
            .unwrap();
        assert_eq!(output.nodes.len(), 5);
        assert_eq!(
            output
                .edges
                .iter()
                .filter(|edge| edge.edge_type == EdgeType::Contains)
                .count(),
            4
        );
        let ids: BTreeSet<_> = output
            .nodes
            .iter()
            .map(|node| node.id.to_string())
            .collect();
        assert!(
            output
                .edges
                .iter()
                .all(|edge| ids.contains(&edge.metadata["target_node_id"]))
        );
        let native = output
            .nodes
            .iter()
            .find(|node| node.location.file_path.ends_with("Native.vcxproj"))
            .unwrap();
        assert_eq!(
            native.metadata.attributes["evaluation_status"],
            "unsupported_project_type"
        );
        assert!(
            output
                .nodes
                .iter()
                .any(|node| node.language == Some(Language::Other("FSharp".into())))
        );
        assert!(
            output
                .nodes
                .iter()
                .any(|node| node.language == Some(Language::Other("VisualBasic".into())))
        );
    }

    #[test]
    fn evaluated_projects_preserve_dependencies_versions_and_provenance() {
        let root = tempfile::tempdir().unwrap();
        let manifest = root.path().join("App.csproj");
        let target = root.path().join("Lib.csproj");
        std::fs::write(&target, "<Project />").unwrap();
        let evaluated = serde_json::json!({"Properties": {"MSBuildProjectName": "App", "TargetFramework": "net10.0"},
            "Items": {"ProjectReference": [{"Identity": "Lib.csproj"}],
                "PackageReference": [{"Identity": "Example", "Version": "1.2.3"}, {"Identity": "Central"}],
                "PackageVersion": [{"Identity": "Central", "Version": "4.5.6"}]}});
        let output = parse_evaluation(&evaluated, &manifest, "project-a").unwrap();
        assert_eq!(output.nodes.len(), 3);
        assert_eq!(output.edges.len(), 3);
        assert!(
            output
                .edges
                .iter()
                .any(|edge| edge.to == "nuget::example@1.2.3")
        );
        assert!(
            output
                .edges
                .iter()
                .any(|edge| edge.to == "nuget::central@4.5.6")
        );
        assert!(
            output
                .edges
                .iter()
                .all(|edge| edge.metadata["target_framework"] == "net10.0"
                    && edge.metadata["analyzer"] == "dotnet_msbuild")
        );
        let other = parse_evaluation(&evaluated, &manifest, "project-b").unwrap();
        assert_ne!(output.nodes[0].id, other.nodes[0].id);
        let again = parse_evaluation(&evaluated, &manifest, "project-a").unwrap();
        assert_eq!(output.nodes[0].id, again.nodes[0].id);
    }

    #[test]
    fn invalid_msbuild_output_is_an_error() {
        assert!(
            parse_evaluation(&serde_json::json!({}), Path::new("App.csproj"), "project").is_err()
        );
    }

    #[test]
    fn imported_inputs_and_restore_assets_invalidate_cached_context() {
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path().join("App");
        std::fs::create_dir(&root).unwrap();
        let manifest = root.join("App.csproj");
        let imported = fixture.path().join("Versions.props");
        std::fs::write(&manifest, "<Project />").unwrap();
        std::fs::write(&imported, "version one").unwrap();
        let evaluation = serde_json::json!({"Properties": {
            "MSBuildAllProjects": "../Versions.props",
            "ProjectAssetsFile": "obj/project.assets.json"
        }});
        let mut inputs = BTreeMap::new();
        record_imports(&evaluation, &manifest, &mut inputs).unwrap();
        assert_eq!(inputs.len(), 3);
        let cache = codegraph_core::artifact_cache::ArtifactCache::new(
            root.join(".codegraph/index-cache"),
            CACHE_NAMESPACE,
        );
        cache.put("external-inputs", &inputs).unwrap();
        let before = super::super::build_context::external_input_fingerprints(&root).unwrap();
        std::fs::write(&imported, "version two").unwrap();
        let changed = super::super::build_context::external_input_fingerprints(&root).unwrap();
        assert_ne!(before, changed);
        std::fs::create_dir(root.join("obj")).unwrap();
        std::fs::write(root.join("obj/project.assets.json"), "restored").unwrap();
        let restored = super::super::build_context::external_input_fingerprints(&root).unwrap();
        assert_ne!(changed, restored);
        std::fs::remove_file(&imported).unwrap();
        assert_ne!(
            restored,
            super::super::build_context::external_input_fingerprints(&root).unwrap()
        );
    }

    #[tokio::test]
    async fn msbuild_evaluation_reads_imports_conditions_and_central_versions_without_building() {
        if !std::process::Command::new("dotnet")
            .arg("--version")
            .output()
            .is_ok_and(|output| output.status.success())
        {
            eprintln!("SKIP: dotnet SDK is not available for the offline MSBuild fixture");
            return;
        }
        let root = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(root.path()).unwrap();
        let imported = root.join("Versions.props");
        std::fs::write(&imported, "<Project><ItemGroup><PackageVersion Include=\"Central\" Version=\"4.5.6\" /></ItemGroup></Project>").unwrap();
        let manifest = root.join("App.csproj");
        std::fs::write(&manifest, r#"<Project>
            <Import Project="Versions.props" />
            <PropertyGroup><TargetFrameworks>net8.0;net10.0</TargetFrameworks></PropertyGroup>
            <ItemGroup><PackageReference Include="Central" /></ItemGroup>
            <ItemGroup Condition="'$(TargetFramework)' == 'net10.0'"><PackageReference Include="Modern" Version="2.0.0" /></ItemGroup>
            <Target Name="Build"><Error Text="Indexing must not build projects" /></Target>
        </Project>"#).unwrap();
        let evaluated = evaluate_project(&manifest, "Debug", Some("net10.0"))
            .await
            .unwrap();
        assert!(
            evaluated["CodeGraphImports"]
                .as_array()
                .unwrap()
                .iter()
                .any(|path| path.as_str() == imported.to_str())
        );
        let parsed = parse_evaluation(&evaluated, &manifest, "fixture").unwrap();
        assert!(
            parsed
                .edges
                .iter()
                .any(|edge| edge.to == "nuget::central@4.5.6")
        );
        assert!(
            parsed
                .edges
                .iter()
                .any(|edge| edge.to == "nuget::modern@2.0.0")
        );
        let evaluated = evaluate_project(&manifest, "Debug", Some("net8.0"))
            .await
            .unwrap();
        let parsed = parse_evaluation(&evaluated, &manifest, "fixture").unwrap();
        assert!(
            !parsed
                .edges
                .iter()
                .any(|edge| edge.to.starts_with("nuget::modern@"))
        );
        let missing_import = root.join("Broken.csproj");
        std::fs::write(root.join("Lib.csproj"), "<Project><PropertyGroup><TargetFramework>net8.0</TargetFramework></PropertyGroup></Project>").unwrap();
        let app = std::fs::read_to_string(&manifest).unwrap().replace("<ItemGroup><PackageReference", "<ItemGroup><ProjectReference Include=\"Lib.csproj\" /></ItemGroup><ItemGroup><PackageReference");
        std::fs::write(&manifest, app).unwrap();
        let workspace = analyze_workspace_with_options(&root, "fixture", "Debug", None)
            .await
            .unwrap();
        let ids: BTreeSet<_> = workspace
            .nodes
            .iter()
            .map(|node| node.id.to_string())
            .collect();
        assert!(
            workspace
                .edges
                .iter()
                .all(|edge| ids.contains(&edge.metadata["target_node_id"]))
        );
        let frameworks: BTreeSet<_> = workspace
            .edges
            .iter()
            .map(|edge| edge.metadata["target_framework"].as_str())
            .collect();
        assert_eq!(frameworks, BTreeSet::from(["net10.0", "net8.0"]));
        assert_eq!(
            workspace
                .nodes
                .iter()
                .filter(|node| node.metadata.attributes["kind"] == "dotnet_project")
                .count(),
            2
        );
        std::fs::write(
            &missing_import,
            "<Project><Import Project=\"Missing.props\" /></Project>",
        )
        .unwrap();
        let error = evaluate_project(&missing_import, "Debug", None)
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("MSBuild evaluation failed"),
            "{error}"
        );
    }
}
