// ABOUTME: Extracts C# declarations and lexical reference edges from immutable syntax trees.
// ABOUTME: Supplies precise identifier spans for Roslyn-backed LSP definition resolution.

use codegraph_core::{
    CodeNode, EdgeRelationship, EdgeType, ExtractionResult, Language, Location, NodeId, NodeType,
    Span,
};
use std::collections::HashMap;
use tree_sitter::{Node, Tree};

pub struct CSharpExtractor;

#[derive(Default, Clone)]
struct CSharpContext {
    namespaces: Vec<String>,
    types: Vec<String>,
    owner: Option<NodeId>,
    variable_kind: Option<&'static str>,
    in_interface: bool,
}

impl CSharpExtractor {
    pub fn extract(tree: &Tree, content: &str, file_path: &str) -> Vec<CodeNode> {
        Self::extract_with_edges(tree, content, file_path).nodes
    }

    pub fn extract_with_edges(tree: &Tree, content: &str, file_path: &str) -> ExtractionResult {
        let mut collector = CSharpCollector {
            content,
            file_path,
            nodes: Vec::new(),
            edges: Vec::new(),
        };
        collector.walk(tree.root_node(), CSharpContext::default());
        ExtractionResult {
            nodes: collector.nodes,
            edges: collector.edges,
        }
    }
}

impl super::LanguageExtractor for CSharpExtractor {
    fn extract_with_edges(tree: &Tree, content: &str, file_path: &str) -> ExtractionResult {
        CSharpExtractor::extract_with_edges(tree, content, file_path)
    }

    fn supported_edge_types() -> &'static [EdgeType] {
        &[
            EdgeType::Imports,
            EdgeType::Calls,
            EdgeType::Extends,
            EdgeType::References,
        ]
    }

    fn language() -> Language {
        Language::CSharp
    }
}

struct CSharpCollector<'a> {
    content: &'a str,
    file_path: &'a str,
    nodes: Vec<CodeNode>,
    edges: Vec<EdgeRelationship>,
}

impl CSharpCollector<'_> {
    fn walk(&mut self, node: Node, mut ctx: CSharpContext) {
        match node.kind() {
            "compilation_unit" => {
                // File-scoped namespaces are siblings of their members in this grammar.
                let mut cursor = node.walk();
                if let Some(namespace) = node
                    .named_children(&mut cursor)
                    .find(|child| child.kind() == "file_scoped_namespace_declaration")
                    .and_then(|child| child.child_by_field_name("name"))
                {
                    ctx.namespaces.push(self.text(namespace));
                }
            }
            "namespace_declaration" | "file_scoped_namespace_declaration" => {
                if let Some(name) = node.child_by_field_name("name") {
                    let mut namespace_ctx = ctx.clone();
                    if node.kind() == "file_scoped_namespace_declaration" {
                        namespace_ctx.namespaces.clear();
                    }
                    ctx.owner = Some(self.add_node(
                        node,
                        self.text(name),
                        NodeType::Module,
                        "namespace",
                        &namespace_ctx,
                    ));
                    if node.kind() == "namespace_declaration" {
                        ctx.namespaces.push(self.text(name));
                    }
                }
            }
            "class_declaration"
            | "interface_declaration"
            | "struct_declaration"
            | "record_declaration"
            | "enum_declaration"
            | "delegate_declaration" => {
                if let Some(name) = node.child_by_field_name("name") {
                    let kind = node.kind().trim_end_matches("_declaration");
                    let node_type = match kind {
                        "interface" => NodeType::Interface,
                        "struct" => NodeType::Struct,
                        "enum" => NodeType::Enum,
                        "delegate" => NodeType::Type,
                        "record" => {
                            let mut cursor = node.walk();
                            if node
                                .children(&mut cursor)
                                .any(|child| child.kind() == "struct")
                            {
                                NodeType::Struct
                            } else {
                                NodeType::Class
                            }
                        }
                        _ => NodeType::Class,
                    };
                    let id = self.add_node(node, self.text(name), node_type, kind, &ctx);
                    self.extract_bases(node, id);
                    ctx.owner = Some(id);
                    ctx.types.push(self.text(name));
                    ctx.variable_kind = None;
                    ctx.in_interface = node.kind() == "interface_declaration";
                }
            }
            "method_declaration"
            | "constructor_declaration"
            | "destructor_declaration"
            | "local_function_statement" => {
                if let Some(name) = node.child_by_field_name("name") {
                    let kind = match node.kind() {
                        "constructor_declaration" => "constructor",
                        "destructor_declaration" => "destructor",
                        "local_function_statement" => "local_function",
                        _ => "method",
                    };
                    ctx.owner =
                        Some(self.add_node(node, self.text(name), NodeType::Function, kind, &ctx));
                    ctx.variable_kind = None;
                }
            }
            "global_statement" => {
                ctx.owner = Some(self.add_node(
                    node,
                    "<top-level>".into(),
                    NodeType::Function,
                    "top_level_statement",
                    &ctx,
                ));
                ctx.variable_kind = None;
            }
            "property_declaration" | "event_declaration" | "enum_member_declaration" => {
                if let Some(name) = node.child_by_field_name("name") {
                    let kind = node.kind().trim_end_matches("_declaration");
                    ctx.owner =
                        Some(self.add_node(node, self.text(name), NodeType::Variable, kind, &ctx));
                }
            }
            "indexer_declaration" => {
                ctx.owner =
                    Some(self.add_node(node, "this[]".into(), NodeType::Variable, "indexer", &ctx));
            }
            "operator_declaration" | "conversion_operator_declaration" => {
                let name = node
                    .child_by_field_name("operator")
                    .or_else(|| node.child_by_field_name("type"))
                    .map(|name| format!("operator {}", self.text(name)))
                    .unwrap_or_else(|| "operator".into());
                ctx.owner = Some(self.add_node(node, name, NodeType::Function, "operator", &ctx));
            }
            "field_declaration" => ctx.variable_kind = Some("field"),
            "event_field_declaration" => ctx.variable_kind = Some("event"),
            "variable_declarator" => {
                if let (Some(kind), Some(name)) =
                    (ctx.variable_kind, node.child_by_field_name("name"))
                {
                    ctx.owner =
                        Some(self.add_node(node, self.text(name), NodeType::Variable, kind, &ctx));
                    ctx.variable_kind = None;
                }
            }
            "using_directive" => {
                // `name` denotes an alias, not the imported namespace. The target is
                // the final named child for normal, static, global and alias imports.
                let mut cursor = node.walk();
                if let Some(target) = node.named_children(&mut cursor).last() {
                    let namespace = self.text(target);
                    let id = self.add_node(
                        node,
                        format!("using {namespace}"),
                        NodeType::Import,
                        "using",
                        &ctx,
                    );
                    self.edge(
                        id,
                        namespace.clone(),
                        EdgeType::Imports,
                        target,
                        "csharp_using",
                    );
                    if let Some(edge) = self.edges.last_mut() {
                        if namespace.starts_with("System") {
                            edge.metadata.insert("framework".into(), "system".into());
                        } else if namespace.starts_with("Microsoft") {
                            edge.metadata.insert("framework".into(), "microsoft".into());
                        }
                    }
                    if let Some(alias) = node.child_by_field_name("name") {
                        let alias = self.text(alias);
                        self.nodes
                            .last_mut()
                            .unwrap()
                            .metadata
                            .attributes
                            .insert("alias".into(), alias);
                    }
                }
                return;
            }
            "invocation_expression" => {
                if let (Some(owner), Some(function)) =
                    (ctx.owner, node.child_by_field_name("function"))
                {
                    let identifier = self.reference_token(function);
                    self.edge(
                        owner,
                        self.text(function),
                        EdgeType::Calls,
                        identifier,
                        "csharp_invocation",
                    );
                }
            }
            "object_creation_expression" => {
                if let (Some(owner), Some(target)) = (ctx.owner, node.child_by_field_name("type")) {
                    self.edge(
                        owner,
                        self.text(target),
                        EdgeType::Calls,
                        self.reference_token(target),
                        "csharp_constructor",
                    );
                }
            }
            "member_access_expression" | "member_binding_expression" => {
                // Calls already have an edge at the final member identifier.
                let is_call = node.parent().is_some_and(|parent| {
                    parent.kind() == "invocation_expression"
                        && parent.child_by_field_name("function") == Some(node)
                });
                if !is_call && let Some(owner) = ctx.owner {
                    self.edge(
                        owner,
                        self.text(node),
                        EdgeType::References,
                        self.reference_token(node),
                        "csharp_member",
                    );
                }
            }
            _ => {}
        }
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            self.walk(child, ctx.clone());
        }
    }

    fn add_node(
        &mut self,
        syntax: Node,
        name: String,
        node_type: NodeType,
        kind: &str,
        ctx: &CSharpContext,
    ) -> NodeId {
        let mut code = CodeNode::new(
            name.clone(),
            Some(node_type.clone()),
            Some(Language::CSharp),
            Location {
                file_path: self.file_path.into(),
                line: syntax.start_position().row as u32 + 1,
                column: syntax.start_position().column as u32,
                end_line: Some(syntax.end_position().row as u32 + 1),
                end_column: Some(syntax.end_position().column as u32),
            },
        )
        .with_content(self.text(syntax));
        code.span = Some(Self::span(syntax));
        let attributes = &mut code.metadata.attributes;
        attributes.insert("kind".into(), kind.into());
        attributes.insert(
            "qualified_name".into(),
            ctx.namespaces
                .iter()
                .chain(&ctx.types)
                .map(String::as_str)
                .chain(std::iter::once(name.as_str()))
                .collect::<Vec<_>>()
                .join("."),
        );
        if let Some(parent) = ctx.types.last() {
            attributes.insert("parent_class".into(), parent.clone());
        }
        let declaration = if syntax.kind() == "variable_declarator" {
            syntax
                .parent()
                .and_then(|parent| parent.parent())
                .unwrap_or(syntax)
        } else {
            syntax
        };
        let mut cursor = declaration.walk();
        let modifiers: Vec<_> = declaration
            .named_children(&mut cursor)
            .filter(|child| child.kind() == "modifier")
            .map(|child| self.text(child))
            .collect();
        let access: Vec<_> = modifiers
            .iter()
            .map(String::as_str)
            .filter(|modifier| matches!(*modifier, "public" | "internal" | "protected" | "private"))
            .collect();
        let visibility = if access.is_empty() {
            if ctx.in_interface || kind == "enum_member" {
                "public"
            } else if ctx.types.is_empty() {
                "internal"
            } else {
                "private"
            }
            .to_owned()
        } else {
            access.join(" ")
        };
        attributes.insert("visibility".into(), visibility);
        if modifiers.iter().any(|modifier| modifier == "static") {
            attributes.insert("static".into(), "true".into());
        }
        if kind == "field"
            && (modifiers.iter().any(|modifier| modifier == "const")
                || (modifiers.iter().any(|modifier| modifier == "static")
                    && modifiers.iter().any(|modifier| modifier == "readonly")))
        {
            attributes.insert("kind".into(), "constant".into());
            attributes.insert("const".into(), "true".into());
        }
        if node_type == NodeType::Function {
            let content = self.text(syntax);
            if modifiers.iter().any(|modifier| modifier == "async") {
                attributes.insert("async".into(), "true".into());
            }
            if content.contains("Task<") {
                attributes.insert("returns_task".into(), "true".into());
            }
            if [".Where(", ".Select(", ".FirstOrDefault("]
                .iter()
                .any(|pattern| content.contains(pattern))
            {
                attributes.insert("uses_linq".into(), "true".into());
            }
            code.complexity = Some(crate::complexity::calculate_cyclomatic_complexity(
                &syntax,
                self.content,
            ));
        }
        if kind == "class" {
            if name.ends_with("Controller") {
                attributes.insert("pattern".into(), "controller".into());
            } else if name.ends_with("Service") {
                attributes.insert("pattern".into(), "service".into());
            }
        }
        if kind == "property" {
            let content = self.text(syntax);
            if content.contains("{ get; set; }") {
                attributes.insert("auto_property".into(), "true".into());
            }
            if content.contains("=>") {
                attributes.insert("computed_property".into(), "true".into());
            }
        }
        let id = code.id;
        self.nodes.push(code);
        let typed = if syntax.kind() == "variable_declarator" {
            syntax.parent().unwrap_or(syntax)
        } else {
            syntax
        };
        for field in ["returns", "type"] {
            if let Some(type_node) = typed.child_by_field_name(field) {
                self.extract_type_references(type_node, id);
            }
        }
        if let Some(parameters) = syntax.child_by_field_name("parameters") {
            let mut cursor = parameters.walk();
            for parameter in parameters.named_children(&mut cursor) {
                if let Some(type_node) = parameter.child_by_field_name("type") {
                    self.extract_type_references(type_node, id);
                }
            }
        }
        id
    }

    fn extract_type_references(&mut self, node: Node, owner: NodeId) {
        match node.kind() {
            "predefined_type" | "implicit_type" => return,
            "identifier" | "qualified_name" | "alias_qualified_name" | "generic_name" => {
                self.edge(
                    owner,
                    self.text(node),
                    EdgeType::References,
                    self.reference_token(node),
                    "csharp_type",
                );
                // Namespace qualifiers are not type arguments.
                let simple = node.child_by_field_name("name").unwrap_or(node);
                let mut cursor = simple.walk();
                for child in simple
                    .named_children(&mut cursor)
                    .filter(|child| child.kind() == "type_argument_list")
                {
                    self.extract_type_references(child, owner);
                }
                return;
            }
            "tuple_element" => {
                if let Some(typed) = node.child_by_field_name("type") {
                    self.extract_type_references(typed, owner);
                }
                return;
            }
            _ => {}
        }
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            self.extract_type_references(child, owner);
        }
    }

    fn extract_bases(&mut self, node: Node, owner: NodeId) {
        let mut cursor = node.walk();
        for base_list in node
            .named_children(&mut cursor)
            .filter(|child| child.kind() == "base_list")
        {
            let inheritance = self.text(base_list);
            self.nodes
                .last_mut()
                .unwrap()
                .metadata
                .attributes
                .insert("inheritance".into(), inheritance);
            let mut bases = base_list.walk();
            for base in base_list
                .named_children(&mut bases)
                .filter(|child| child.kind() != "argument_list")
            {
                // Syntax alone cannot distinguish a base class from an interface.
                // Preserve an extends edge; the resolved target carries its real kind.
                self.edge(
                    owner,
                    self.text(base),
                    EdgeType::Extends,
                    self.reference_token(base),
                    "csharp_base",
                );
            }
        }
    }

    fn reference_token<'a>(&self, node: Node<'a>) -> Node<'a> {
        if let Some(name) = node.child_by_field_name("name") {
            return self.reference_token(name);
        }
        if node.kind() == "generic_name" {
            let mut cursor = node.walk();
            if let Some(identifier) = node
                .named_children(&mut cursor)
                .find(|child| child.kind() == "identifier")
            {
                return identifier;
            }
        }
        // Conditional calls such as service?.Run() put the member binding last.
        if node.kind() == "conditional_access_expression" {
            let mut cursor = node.walk();
            if let Some(member) = node.named_children(&mut cursor).last() {
                return self.reference_token(member);
            }
        }
        node
    }

    fn edge(
        &mut self,
        from: NodeId,
        to: String,
        edge_type: EdgeType,
        token: Node,
        reference_kind: &str,
    ) {
        self.edges.push(EdgeRelationship {
            from,
            to,
            edge_type,
            metadata: HashMap::from([
                ("reference_kind".into(), reference_kind.into()),
                ("source_file".into(), self.file_path.into()),
            ]),
            span: Some(Self::span(token)),
        });
    }

    fn text(&self, node: Node) -> String {
        node.utf8_text(self.content.as_bytes())
            .unwrap_or("")
            .to_owned()
    }

    fn span(node: Node) -> Span {
        Span {
            start_byte: node.start_byte() as u32,
            end_byte: node.end_byte() as u32,
        }
    }
}
