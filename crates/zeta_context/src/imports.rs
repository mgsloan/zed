use std::{collections::HashSet, ops::Range};

use tree_sitter::{Node, Tree};

use crate::{identifier_index::Identifier, zed_code::Language};

pub struct CollectImportsInput<'a> {
    pub language: &'a Language,
    pub tree: &'a Tree,
    pub source: &'a str,
}

// Initial goals:
//
// * Get region that has top-of-file imports
//
// * symbol -> (namespace, count)

#[derive(Debug, Clone)]
pub struct Imports {
    pub range: Range<usize>,
    pub symbols: Vec<(Namespace, Identifier)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Namespace(Vec<Identifier>);

impl CollectImportsInput<'_> {
    pub fn collect_imports(&self) -> Option<Imports> {
        match self.language.name.0.as_ref() {
            "rust" => self.collect_rust_imports(),
            _ => None,
        }
    }

    pub fn collect_rust_imports(&self) -> Option<Imports> {
        let imports = self.find_rust_imports();
        dbg!(&imports);
        let mut range: Option<Range<usize>> = None;
        let mut symbols = Vec::new();
        for import in imports {
            let offset_range = import.byte_range();
            if let Some(range) = range.as_mut() {
                range.end = offset_range.end;
            } else {
                range = Some(offset_range)
            }
            Self::add_rust_import_symbols(import, &mut symbols, self.source);
        }
        range.map(|range| Imports { range, symbols })
    }

    pub fn find_rust_imports(&self) -> Vec<Node<'_>> {
        let ts_language = &self.language.ts_language;
        let use_id = ts_language.id_for_node_kind("use_declaration", true);
        let mut halt_ids = HashSet::new();
        halt_ids.insert(ts_language.id_for_node_kind("struct_item", true));
        halt_ids.insert(ts_language.id_for_node_kind("impl_item", true));
        halt_ids.insert(ts_language.id_for_node_kind("function_item", true));
        halt_ids.insert(ts_language.id_for_node_kind("trait_item", true));
        halt_ids.insert(ts_language.id_for_node_kind("static_item", true));
        let mut cursor = self.tree.walk();
        if !cursor.goto_first_child() {
            return vec![];
        }
        let mut nodes = Vec::new();
        loop {
            if cursor.node().kind_id() == use_id {
                nodes.push(cursor.node());
            }
            if halt_ids.contains(&cursor.node().kind_id()) {
                break;
            }
            if !cursor.goto_next_sibling() {
                break;
            }
        }
        return nodes;
    }

    pub fn add_rust_import_symbols(
        node: Node,
        symbols: &mut Vec<(Namespace, Identifier)>,
        source: &str,
    ) {
        let ts_language = node.language();
        let scoped_identifier_id = ts_language.id_for_node_kind("scoped_identifier", true);
        let scoped_use_list_id = ts_language.id_for_node_kind("scoped_use_list", true);
        let use_list_id = ts_language.id_for_node_kind("use_list", true);
        let identifier_id = ts_language.id_for_node_kind("identifier", true);

        // Find the argument child of use_declaration by field name
        if let Some(argument_node) = node.child_by_field_name("argument") {
            Self::extract_symbols_from_node(
                argument_node,
                &Vec::new(),
                symbols,
                scoped_identifier_id,
                scoped_use_list_id,
                use_list_id,
                identifier_id,
                source,
            );
        }
    }

    fn extract_symbols_from_node(
        node: Node,
        current_namespace: &Vec<Identifier>,
        symbols: &mut Vec<(Namespace, Identifier)>,
        scoped_identifier_id: u16,
        scoped_use_list_id: u16,
        use_list_id: u16,
        identifier_id: u16,
        source: &str,
    ) {
        match node.kind_id() {
            id if id == scoped_identifier_id => {
                // Handle scoped_identifier: path::name
                if let (Some(path_node), Some(name_node)) = (
                    node.child_by_field_name("path"),
                    node.child_by_field_name("name"),
                ) {
                    let mut namespace = current_namespace.clone();
                    Self::extract_path_to_namespace(
                        path_node,
                        &mut namespace,
                        scoped_identifier_id,
                        identifier_id,
                        source,
                    );

                    if name_node.kind_id() == identifier_id {
                        let name_text = Self::node_text(&name_node, source);
                        symbols.push((Namespace(namespace), Identifier(name_text.into())));
                    }
                }
            }
            id if id == scoped_use_list_id => {
                // Handle scoped_use_list: path::{list}
                if let (Some(path_node), Some(list_node)) = (
                    node.child_by_field_name("path"),
                    node.child_by_field_name("list"),
                ) {
                    let mut namespace = current_namespace.clone();
                    Self::extract_path_to_namespace(
                        path_node,
                        &mut namespace,
                        scoped_identifier_id,
                        identifier_id,
                        source,
                    );

                    Self::extract_symbols_from_node(
                        list_node,
                        &namespace,
                        symbols,
                        scoped_identifier_id,
                        scoped_use_list_id,
                        use_list_id,
                        identifier_id,
                        source,
                    );
                }
            }
            id if id == use_list_id => {
                // Handle use_list: {item1, item2, ...}
                let mut cursor = node.walk();
                if cursor.goto_first_child() {
                    loop {
                        let child = cursor.node();
                        if child.is_named() {
                            Self::extract_symbols_from_node(
                                child,
                                current_namespace,
                                symbols,
                                scoped_identifier_id,
                                scoped_use_list_id,
                                use_list_id,
                                identifier_id,
                                source,
                            );
                        }
                        if !cursor.goto_next_sibling() {
                            break;
                        }
                    }
                }
            }
            id if id == identifier_id => {
                // Handle simple identifier
                let name_text = Self::node_text(&node, source);
                symbols.push((
                    Namespace(current_namespace.clone()),
                    Identifier(name_text.into()),
                ));
            }
            _ => {
                // For other node types, recurse through children
                let mut cursor = node.walk();
                if cursor.goto_first_child() {
                    loop {
                        let child = cursor.node();
                        if child.is_named() {
                            Self::extract_symbols_from_node(
                                child,
                                current_namespace,
                                symbols,
                                scoped_identifier_id,
                                scoped_use_list_id,
                                use_list_id,
                                identifier_id,
                                source,
                            );
                        }
                        if !cursor.goto_next_sibling() {
                            break;
                        }
                    }
                }
            }
        }
    }

    fn extract_path_to_namespace(
        path_node: Node,
        namespace: &mut Vec<Identifier>,
        scoped_identifier_id: u16,
        identifier_id: u16,
        source: &str,
    ) {
        match path_node.kind_id() {
            id if id == scoped_identifier_id => {
                // Recursive case: path::name
                if let (Some(inner_path), Some(name)) = (
                    path_node.child_by_field_name("path"),
                    path_node.child_by_field_name("name"),
                ) {
                    Self::extract_path_to_namespace(
                        inner_path,
                        namespace,
                        scoped_identifier_id,
                        identifier_id,
                        source,
                    );
                    if name.kind_id() == identifier_id {
                        namespace.push(Identifier(Self::node_text(&name, source).into()));
                    }
                }
            }
            id if id == identifier_id => {
                // Base case: simple identifier
                namespace.push(Identifier(Self::node_text(&path_node, source).into()));
            }
            _ => {
                // Fallback: treat as identifier
                namespace.push(Identifier(Self::node_text(&path_node, source).into()));
            }
        }
    }

    fn node_text<'a>(node: &Node, source: &'a str) -> &'a str {
        let range = node.byte_range();
        &source[range]
    }
}

#[cfg(test)]
mod test {
    use std::sync::{Arc, LazyLock};

    use itertools::Itertools;

    use crate::{
        treesitter_util::{language_for_name, load_languages, parse_source},
        zed_code::LanguageName,
    };

    use super::*;

    static LANGUAGES: LazyLock<Vec<Arc<Language>>> = LazyLock::new(|| load_languages());

    fn tree_to_string(tree: &tree_sitter::Tree) -> String {
        let mut cursor = tree.walk();
        let mut result = String::new();
        let mut depth = 0;
        loop {
            result.push_str(&"  ".repeat(depth));
            if let Some(field_name) = cursor.field_name() {
                result.push_str(field_name);
                result.push_str(": ");
            }
            if cursor.node().is_named() {
                result.push_str(cursor.node().kind());
            } else {
                result.push('"');
                result.push_str(cursor.node().kind());
                result.push('"');
            }
            result.push('\n');

            if cursor.goto_first_child() {
                depth += 1;
                continue;
            }
            if cursor.goto_next_sibling() {
                continue;
            }
            if cursor.goto_parent() {
                depth -= 1;
                if cursor.goto_next_sibling() {
                    continue;
                }
            }
            break;
        }
        result
    }

    fn run_collect_imports(language_name: &str, source: &str) -> (Option<Imports>, Tree) {
        let language = language_for_name(&LANGUAGES, &LanguageName(language_name.into())).unwrap();
        let tree = parse_source(&language, source);
        let input = CollectImportsInput {
            language: &language,
            tree: &tree,
            source,
        };
        (input.collect_imports(), tree)
    }

    fn rust_symbol(symbol: &str) -> (Namespace, Identifier) {
        let parts = symbol.split("::").collect::<Vec<&str>>();
        let last_part = parts.len().saturating_sub(1);
        (
            Namespace(
                parts[0..last_part]
                    .iter()
                    .map(|part| Identifier((*part).into()))
                    .collect::<Vec<_>>(),
            ),
            Identifier(parts[last_part].into()),
        )
    }

    struct ImportsFailure {
        expected: Vec<(Namespace, Identifier)>,
        actual: Vec<(Namespace, Identifier)>,
        source: String,
        tree: Tree,
    }

    impl std::fmt::Display for ImportsFailure {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(
                f,
                "Expected imports: {:?}\n\
                Actual imports: {:?}\n\
                Source:\n{}\n\
                Tree:\n{}",
                self.expected,
                self.actual,
                self.source,
                tree_to_string(&self.tree)
            )
        }
    }

    #[test]
    fn test_collect_rust_imports() {
        let examples = vec![
            (
                "use std::collections::HashMap;",
                vec!["std::collections::HashMap"],
            ),
            (
                "pub use std::collections::HashMap;",
                vec!["std::collections::HashMap"],
            ),
            (
                "use std::collections::{HashMap, HashSet};",
                vec!["std::collections::HashMap", "std::collections::HashSet"],
            ),
            (
                "use std::{any::TypeId, collections::{HashMap, HashSet};",
                vec![
                    "std::any::TypeId",
                    "std::collections::HashMap",
                    "std::collections::HashSet",
                ],
            ),
        ];
        let mut failures = Vec::new();
        for (source, expected) in examples {
            let (imports, tree) = run_collect_imports("rust", source);
            let imports = imports.expect(&format!(
                "Failed to collect imports for source:\n{}",
                source
            ));
            let expected_symbols = expected
                .iter()
                .map(|symbol| rust_symbol(symbol))
                .collect::<Vec<_>>();
            if imports.symbols != expected_symbols {
                failures.push(ImportsFailure {
                    expected: expected_symbols,
                    actual: imports.symbols,
                    source: source.to_string(),
                    tree,
                });
            }
        }

        if !failures.is_empty() {
            panic!(
                "{} cases failed:\n\n{}",
                failures.len(),
                failures
                    .into_iter()
                    .map(|failure| failure.to_string())
                    .join("\n")
            )
        }
    }
}
