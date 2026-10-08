# Supported languages

This document describes which languages CodeGraph can index today, and what “support” means (AST extraction vs analyzer enrichment).

## What counts as “supported”

CodeGraph’s indexing pipeline has multiple layers:

1. **AST extraction** (Tree-sitter): parses files and emits initial nodes/edges.
2. **FastML** enhancement: fast heuristics that can improve recall.
3. **Analyzers** (optional but enabled by default): build context, module linking, LSP enrichment, docs/contracts, architecture, dataflow (Rust-local).

So a language can be:

- **AST-supported**: Tree-sitter parsing + baseline node extraction works.
- **Analyzer-supported**: additional enrichers run (module linker, LSP, etc.).

## AST-supported languages (Tree-sitter)

These languages are registered in `crates/codegraph-parser/src/language.rs`:

| Language | File extensions |
|---|---|
| Rust | `rs` |
| TypeScript | `ts`, `tsx` |
| JavaScript | `js`, `jsx` |
| Python | `py`, `pyi` |
| Go | `go` |
| Java | `java` |
| C/C++ | `c`, `h`, `cc`, `cpp`, `cxx`, `hpp`, `hxx` |
| Swift | `swift` |
| C# | `cs` |
| Ruby | `rb`, `rake`, `gemspec` |
| PHP | `php`, `phtml`, `php3`, `php4`, `php5` |

## Analyzer support by language

### LSP-backed enrichment

CodeGraph can optionally enrich nodes/edges using Language Server Protocol tooling. The LSP server selection is defined in `crates/codegraph-mcp/src/analyzers/mod.rs`.

| Language | LSP tool | Notes |
|---|---|---|
| Rust | `rust-analyzer` | Adds qualified names and resolves references |
| TypeScript | `typescript-language-server` (+ `node`) | Requires Node + TS LS |
| JavaScript | `typescript-language-server` (+ `node`) | Same server as TS |
| Python | `pyright-langserver` (+ `node`) | Requires Node + Pyright LS |
| Go | `gopls` | |
| Java | `jdtls` | |
| C/C++ | `clangd` | |
| C# | `csharp-ls` (+ `dotnet`) | Roslyn-backed symbols and source definition resolution |

You can disable analyzers entirely (or tool requirements) via:

- `CODEGRAPH_ANALYZERS=false`
- `CODEGRAPH_ANALYZERS_REQUIRE_TOOLS=false`

### Module linking

Module/import linking currently targets:

- TypeScript
- JavaScript
- Python
- Go

### Rust-specific analyzers

Some analyzers are Rust-only today:

- Cargo workspace graph and feature edges (MSBuild context is described below)
- Rustdoc/API surface enrichment
- Local dataflow enrichment

### C# and .NET projects

C# extraction supports block and file-scoped namespaces, nested classes, interfaces,
structs, records, enums and enum members, delegates, methods, constructors,
destructors, local functions, properties, fields, events, indexers, operators and
top-level statements.
Imports, method calls, explicit constructor calls, base types and member references
carry source spans for language-server definition requests. Base-type edges use
`extends` until LSP identifies an indexed interface, when they become `implements`.
Declared parameter, return, field and property types produce type references.
Default visibility follows C# scope rules; `const` and `static readonly` fields
are marked as constants.

The Fast tier indexes `.cs` files without a .NET SDK or language server. Balanced
adds MSBuild project context and LSP symbols. Full also resolves reference targets
through Roslyn. Definitions must map to indexed source nodes; external assemblies,
generated metadata documents and ambiguous results are not given invented targets.
Unmapped Roslyn results remain unresolved instead of falling back to unrelated
symbols with the same name.
F# and Visual Basic project context is supported; their source extraction is not
implemented by this change.

Install the .NET SDK required by the target project and a compatible
[csharp-ls](https://github.com/razzmatazz/csharp-language-server) version:

```shell
dotnet tool install --global csharp-ls
codegraph index /path/to/solution-directory --languages csharp --index-tier full
```

The `csharp-ls` executable must be on PATH (normally `$HOME/.dotnet/tools` on Unix
or `%USERPROFILE%\.dotnet\tools` on Windows). Current csharp-ls releases require
the .NET 10 SDK or newer. The server starts in the indexed directory and loads
`.sln`, `.slnx` or standalone `.csproj` workspaces. Set
`CODEGRAPH_CSHARP_SOLUTION` to a solution path when automatic selection is unsuitable;
relative paths are resolved against the indexed directory and passed as one argument.

Solution discovery reads `.sln` and `.slnx` membership and keeps C#, F#, VB, shared,
native and other MSBuild project types distinct. MSBuild context evaluates `.csproj`,
`.fsproj` and `.vbproj` files under the indexed directory and their project references.
Other project types remain in the graph with `evaluation_status=unsupported_project_type`;
they do not require unavailable native or SDK-specific build tools. The graph
records project and NuGet dependencies, including central
package versions and target-framework conditions. It uses MSBuild's JSON evaluation
interface (17.8+) without invoking build or restore targets. Set
`CODEGRAPH_DOTNET_CONFIGURATION` (default `Debug`) or
`CODEGRAPH_DOTNET_TARGET_FRAMEWORK` to select the build context. Without a framework
override, each declared target framework is evaluated. These variables select
MSBuild context; the language server uses its own workspace configuration.

The source scan remains directory-based and respects ignore/include/exclude rules;
it is not restricted to evaluated `Compile` items or the selected solution's members.
`bin` and `obj` directories next to an MSBuild project are excluded by default.
Project, solution, `.props`,
`.targets`, SDK selection and NuGet configuration changes invalidate reconciliation;
MSBuild preprocessing supplies imported inputs for subsequent invalidation checks,
including custom imports omitted from `MSBuildAllProjects`. Each evaluated context
uses a JSON evaluation and a preprocessing call; neither call runs build targets.
Legacy .NET Framework projects require the reference assemblies and MSBuild tooling
that their projects need. Evaluation errors propagate and prevent a successful
completion marker. This support does not compile projects, run source generators,
or index external assembly metadata.

The C# rules were compared with the original
[CodeGraph C# extractor](https://github.com/colbymchenry/codegraph/blob/b635dd467f0578926a9c01a37b9d28d2b26689f1/src/extraction/languages/csharp.ts)
and its namespace, member-initializer and type-position regressions at that commit.
Namespace qualification, record/struct classification, member ownership, default
visibility, constants and type-only targets follow the same language rules. This
Rust repository retains its own graph types and dotted qualified names. This is
not a full parity port: Razor/ASP.NET route extraction, target-typed constructor
edges and the original conditional-directive preprocessing are not included.

## Kotlin and Dart status

CodeGraph’s core language enum includes Kotlin and Dart, and file-extension detection exists in parts of the pipeline, but Tree-sitter parsing for these languages is currently disabled due to Tree-sitter version conflicts:

- `tree-sitter-kotlin` and `tree-sitter-dart` are not enabled in `crates/codegraph-parser/Cargo.toml`

Contributions to restore Kotlin/Dart parsing support are welcome (see `CONTRIBUTING.md`).
