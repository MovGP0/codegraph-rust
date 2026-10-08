// ABOUTME: Verifies C# declarations and precise reference positions for Roslyn LSP.
// ABOUTME: Keeps lexical ownership isolated across methods, accessors and nested types.

use codegraph_core::{EdgeType, ExtractionResult, NodeType};
use codegraph_parser::CSharpExtractor;

fn extract(source: &str) -> ExtractionResult {
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_c_sharp::LANGUAGE.into())
        .unwrap();
    let tree = parser.parse(source, None).unwrap();
    assert!(
        !tree.root_node().has_error(),
        "{}",
        tree.root_node().to_sexp()
    );
    CSharpExtractor::extract_with_edges(&tree, source, "Example.cs")
}

#[test]
fn csharp_extracts_modern_declarations_and_qualified_names() {
    let result = extract(
        "using System;\nnamespace Demo.App;\npublic interface IService {}\n\
         public struct Point {}\npublic enum State { Ready, Busy }\n\
         public record Item(int Id);\npublic class Worker : IService {\n\
         private int first, second;\npublic event Action Changed, Saved;\n\
         public Worker() {}\npublic int Value { get; set; }\n\
         public void Run() {}\npublic class Nested { public void Run() {} }\n}\n",
    );
    for (name, kind) in [
        ("Demo.App", "namespace"),
        ("IService", "interface"),
        ("Point", "struct"),
        ("State", "enum"),
        ("Ready", "enum_member"),
        ("Item", "record"),
        ("Worker", "class"),
        ("Worker", "constructor"),
        ("first", "field"),
        ("second", "field"),
        ("Changed", "event"),
        ("Saved", "event"),
        ("Value", "property"),
    ] {
        assert!(
            result.nodes.iter().any(|node| &*node.name == name
                && node.metadata.attributes.get("kind").map(String::as_str) == Some(kind)),
            "missing {kind} {name}"
        );
    }
    let names: Vec<_> = result
        .nodes
        .iter()
        .filter(|node| &*node.name == "Run")
        .map(|node| {
            node.metadata
                .attributes
                .get("qualified_name")
                .unwrap()
                .as_str()
        })
        .collect();
    assert_eq!(names, ["Demo.App.Worker.Run", "Demo.App.Worker.Nested.Run"]);
    assert!(result.nodes.iter().all(|node| node.span.is_some()));
    assert!(
        result
            .nodes
            .iter()
            .any(|node| node.node_type == Some(NodeType::Struct) && &*node.name == "Point")
    );
}

#[test]
fn csharp_calls_imports_and_bases_use_identifier_spans() {
    let source = "using System.Text;\nnamespace Demo {\nclass Worker : Base, IService {\n\
        void Run() { service.Execute(); Direct(); }\n}\n}";
    let result = extract(source);
    for (target, token, edge_type) in [
        ("System.Text", "System.Text", EdgeType::Imports),
        ("service.Execute", "Execute", EdgeType::Calls),
        ("Direct", "Direct", EdgeType::Calls),
        ("Base", "Base", EdgeType::Extends),
        ("IService", "IService", EdgeType::Extends),
    ] {
        let edge = result
            .edges
            .iter()
            .find(|edge| edge.to == target && edge.edge_type == edge_type)
            .unwrap_or_else(|| panic!("missing {target}"));
        let span = edge.span.as_ref().unwrap();
        assert_eq!(
            &source[span.start_byte as usize..span.end_byte as usize],
            token
        );
    }
}

#[test]
fn csharp_member_initializers_do_not_inherit_previous_method_owner() {
    let result =
        extract("class C { void Run() { First(); } int value = Second(); int P => Third(); }");
    for (target, owner) in [("First", "Run"), ("Second", "value"), ("Third", "P")] {
        let edge = result.edges.iter().find(|edge| edge.to == target).unwrap();
        let node = result
            .nodes
            .iter()
            .find(|node| node.id == edge.from)
            .unwrap();
        assert_eq!(&*node.name, owner);
    }
}

#[test]
fn csharp_import_aliases_and_generic_calls_preserve_targets() {
    let source = "global using System;\nusing static System.Math;\nusing Text = System.Text.StringBuilder;\nclass C { void Run() { service.Execute<int>(); service?.Other(); } }";
    let result = extract(source);
    let imports: Vec<_> = result
        .edges
        .iter()
        .filter(|edge| edge.edge_type == EdgeType::Imports)
        .map(|edge| edge.to.as_str())
        .collect();
    assert_eq!(
        imports,
        ["System", "System.Math", "System.Text.StringBuilder"]
    );
    let alias = result
        .nodes
        .iter()
        .find(|node| node.metadata.attributes.contains_key("alias"))
        .unwrap();
    assert_eq!(alias.metadata.attributes["alias"], "Text");
    for token in ["Execute", "Other"] {
        assert!(
            result
                .edges
                .iter()
                .any(|edge| edge.edge_type == EdgeType::Calls
                    && edge.span.as_ref().is_some_and(|span| &source
                        [span.start_byte as usize..span.end_byte as usize]
                        == token)),
            "missing precise span for {token}"
        );
    }
}

#[test]
fn csharp_top_level_calls_have_a_source_owner() {
    let source = "using System;\nConsole.WriteLine(\"hello\");\nnew Worker().Run();\nclass Worker { public void Run() {} }";
    let result = extract(source);
    let calls: Vec<_> = result
        .edges
        .iter()
        .filter(|edge| edge.edge_type == EdgeType::Calls)
        .collect();
    assert_eq!(calls.len(), 3);
    for call in calls {
        let owner = result
            .nodes
            .iter()
            .find(|node| node.id == call.from)
            .unwrap();
        assert_eq!(owner.metadata.attributes["kind"], "top_level_statement");
        assert!(
            owner
                .span
                .as_ref()
                .is_some_and(|span| span.start_byte <= call.span.as_ref().unwrap().start_byte)
        );
    }
}

#[test]
fn csharp_visibility_and_type_positions_follow_language_rules() {
    let source = "namespace App; interface IWorker { Widget Run(Widget value); } class Worker { const int Limit = 1; static readonly Widget Cache = new Widget(); private protected Widget Value { get; set; } Widget Run(System.Collections.Generic.List<Widget> values) => null; } class Widget {}";
    let result = extract(source);
    for (name, visibility) in [
        ("IWorker", "internal"),
        ("Worker", "internal"),
        ("Limit", "private"),
        ("Value", "private protected"),
    ] {
        let node = result
            .nodes
            .iter()
            .find(|node| &*node.name == name)
            .unwrap();
        assert_eq!(node.metadata.attributes["visibility"], visibility, "{name}");
    }
    let interface_method = result
        .nodes
        .iter()
        .find(|node| node.metadata.attributes["qualified_name"] == "App.IWorker.Run")
        .unwrap();
    assert_eq!(interface_method.metadata.attributes["visibility"], "public");
    for name in ["Limit", "Cache"] {
        let node = result
            .nodes
            .iter()
            .find(|node| &*node.name == name)
            .unwrap();
        assert_eq!(node.metadata.attributes["kind"], "constant");
    }
    let method = result
        .nodes
        .iter()
        .find(|node| node.metadata.attributes["qualified_name"] == "App.Worker.Run")
        .unwrap();
    let types: Vec<_> = result
        .edges
        .iter()
        .filter(|edge| edge.from == method.id && edge.metadata["reference_kind"] == "csharp_type")
        .collect();
    assert_eq!(types.len(), 3);
    for edge in types {
        let token = edge.span.as_ref().unwrap();
        assert!(matches!(
            &source[token.start_byte as usize..token.end_byte as usize],
            "Widget" | "List"
        ));
    }
}
