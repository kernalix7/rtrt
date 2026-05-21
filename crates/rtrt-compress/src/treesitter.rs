//! Tree-sitter signature extractor.
//!
//! Walks the parsed AST of a source file and emits only the top-level
//! signatures — function headers, struct / enum / trait / type / const
//! declarations, and `impl` blocks (with the method headers inside) — while
//! replacing every function body with `{ /* body */ }`. This is the
//! highest-leverage compression mode for code-heavy responses: the LLM gets
//! the API shape without the implementation noise.
//!
//! Currently bundles the Rust grammar (`tree-sitter-rust`). Adding another
//! language is a matter of enabling its grammar crate and adding a [`Language`]
//! variant — the walk shape is grammar-specific so it doesn't generalise yet.
//!
//! Gated behind the `treesitter` feature.

use rtrt_core::{Error, Result};
use tree_sitter::{Node, Parser};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Language {
    Rust,
    Python,
    TypeScript,
}

impl Language {
    pub fn from_filename(path: &str) -> Option<Self> {
        if path.ends_with(".rs") {
            Some(Self::Rust)
        } else if path.ends_with(".py") {
            Some(Self::Python)
        } else if path.ends_with(".ts") || path.ends_with(".tsx") {
            Some(Self::TypeScript)
        } else {
            None
        }
    }
}

pub struct SignatureExtractor {
    pub language: Language,
}

impl SignatureExtractor {
    pub fn new(language: Language) -> Self {
        Self { language }
    }

    /// Returns the source rewritten to expose only top-level signatures.
    pub fn extract(&self, source: &str) -> Result<String> {
        match self.language {
            Language::Rust => extract_rust(source),
            Language::Python => extract_python(source),
            Language::TypeScript => extract_typescript(source),
        }
    }
}

fn extract_python(source: &str) -> Result<String> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_python::LANGUAGE.into())
        .map_err(|e| Error::Plugin(format!("tree-sitter-python: {e}")))?;
    let tree = parser
        .parse(source, None)
        .ok_or_else(|| Error::Plugin("tree-sitter parse returned None".into()))?;
    let root = tree.root_node();
    let bytes = source.as_bytes();
    let mut out = String::new();
    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        process_python_node(&child, bytes, &mut out, 0);
    }
    Ok(out)
}

fn process_python_node(node: &Node<'_>, src: &[u8], out: &mut String, depth: usize) {
    let indent = "    ".repeat(depth);
    match node.kind() {
        "function_definition" | "decorated_definition" => {
            // Header runs from the node start to the colon ending the def.
            if let Some(body) = node.child_by_field_name("body") {
                push_slice(out, src, node.start_byte(), body.start_byte(), &indent);
                out.push_str("    ...\n");
            } else {
                push_slice(out, src, node.start_byte(), node.end_byte(), &indent);
                out.push('\n');
            }
        }
        "class_definition" => {
            if let Some(body) = node.child_by_field_name("body") {
                push_slice(out, src, node.start_byte(), body.start_byte(), &indent);
                out.push('\n');
                let mut c = body.walk();
                for child in body.children(&mut c) {
                    process_python_node(&child, src, out, depth + 1);
                }
            } else {
                push_slice(out, src, node.start_byte(), node.end_byte(), &indent);
                out.push('\n');
            }
        }
        "import_statement" | "import_from_statement" | "assignment" if depth == 0 => {
            // Keep top-level imports and module constants.
            push_slice(out, src, node.start_byte(), node.end_byte(), &indent);
            out.push('\n');
        }
        _ => {}
    }
}

fn extract_typescript(source: &str) -> Result<String> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into())
        .map_err(|e| Error::Plugin(format!("tree-sitter-typescript: {e}")))?;
    let tree = parser
        .parse(source, None)
        .ok_or_else(|| Error::Plugin("tree-sitter parse returned None".into()))?;
    let root = tree.root_node();
    let bytes = source.as_bytes();
    let mut out = String::new();
    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        process_ts_node(&child, bytes, &mut out, 0);
    }
    Ok(out)
}

fn process_ts_node(node: &Node<'_>, src: &[u8], out: &mut String, depth: usize) {
    let indent = "    ".repeat(depth);
    match node.kind() {
        "function_declaration" | "method_definition" | "method_signature" => {
            if let Some(body) = node.child_by_field_name("body") {
                push_slice(out, src, node.start_byte(), body.start_byte(), &indent);
                out.push_str("{ /* body */ }\n");
            } else {
                push_slice(out, src, node.start_byte(), node.end_byte(), &indent);
                out.push('\n');
            }
        }
        "class_declaration" | "abstract_class_declaration" | "interface_declaration" => {
            if let Some(body) = node.child_by_field_name("body") {
                push_slice(out, src, node.start_byte(), body.start_byte(), &indent);
                out.push_str("{\n");
                let mut c = body.walk();
                for child in body.children(&mut c) {
                    if child.kind() == "{" || child.kind() == "}" {
                        continue;
                    }
                    process_ts_node(&child, src, out, depth + 1);
                }
                out.push_str(&indent);
                out.push_str("}\n");
            } else {
                push_slice(out, src, node.start_byte(), node.end_byte(), &indent);
                out.push('\n');
            }
        }
        "export_statement" => {
            // `export function foo() {...}` parses as an export_statement
            // wrapping a function_declaration. Recurse so the inner item's
            // body still gets stripped; if none of the children matched,
            // fall back to the raw slice (e.g. `export { foo };`).
            let pre_len = out.len();
            let mut c = node.walk();
            for child in node.children(&mut c) {
                process_ts_node(&child, src, out, depth);
            }
            if out.len() == pre_len {
                push_slice(out, src, node.start_byte(), node.end_byte(), &indent);
                out.push('\n');
            }
        }
        "type_alias_declaration"
        | "enum_declaration"
        | "import_statement"
        | "lexical_declaration"
        | "variable_statement" => {
            push_slice(out, src, node.start_byte(), node.end_byte(), &indent);
            out.push('\n');
        }
        _ => {}
    }
}

fn extract_rust(source: &str) -> Result<String> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_rust::LANGUAGE.into())
        .map_err(|e| Error::Plugin(format!("tree-sitter-rust: {e}")))?;
    let tree = parser
        .parse(source, None)
        .ok_or_else(|| Error::Plugin("tree-sitter parse returned None".into()))?;
    let root = tree.root_node();
    let bytes = source.as_bytes();
    let mut out = String::new();
    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        process_rust_node(&child, bytes, &mut out, 0);
    }
    Ok(out)
}

fn process_rust_node(node: &Node<'_>, src: &[u8], out: &mut String, depth: usize) {
    let kind = node.kind();
    let indent = "    ".repeat(depth);
    match kind {
        "function_item" => {
            if let Some(body) = node.child_by_field_name("body") {
                push_slice(out, src, node.start_byte(), body.start_byte(), &indent);
                out.push_str("{ /* body */ }\n");
            } else {
                // signatures without bodies (trait method decls) end at the semicolon
                push_slice(out, src, node.start_byte(), node.end_byte(), &indent);
                out.push('\n');
            }
        }
        "struct_item"
        | "enum_item"
        | "type_item"
        | "const_item"
        | "static_item"
        | "use_declaration"
        | "extern_crate_declaration" => {
            push_slice(out, src, node.start_byte(), node.end_byte(), &indent);
            out.push('\n');
        }
        "trait_item" => {
            if let Some(body) = node.child_by_field_name("body") {
                push_slice(out, src, node.start_byte(), body.start_byte(), &indent);
                out.push_str("{\n");
                let mut c = body.walk();
                for child in body.children(&mut c) {
                    if child.kind() == "{" || child.kind() == "}" {
                        continue;
                    }
                    process_rust_node(&child, src, out, depth + 1);
                }
                out.push_str(&indent);
                out.push_str("}\n");
            } else {
                push_slice(out, src, node.start_byte(), node.end_byte(), &indent);
                out.push('\n');
            }
        }
        "impl_item" => {
            if let Some(body) = node.child_by_field_name("body") {
                push_slice(out, src, node.start_byte(), body.start_byte(), &indent);
                out.push_str("{\n");
                let mut c = body.walk();
                for child in body.children(&mut c) {
                    if child.kind() == "{" || child.kind() == "}" {
                        continue;
                    }
                    process_rust_node(&child, src, out, depth + 1);
                }
                out.push_str(&indent);
                out.push_str("}\n");
            } else {
                push_slice(out, src, node.start_byte(), node.end_byte(), &indent);
                out.push('\n');
            }
        }
        "mod_item" => {
            // Keep `mod foo;` declarations; recurse into inline modules.
            if let Some(body) = node.child_by_field_name("body") {
                push_slice(out, src, node.start_byte(), body.start_byte(), &indent);
                out.push_str("{\n");
                let mut c = body.walk();
                for child in body.children(&mut c) {
                    if child.kind() == "{" || child.kind() == "}" {
                        continue;
                    }
                    process_rust_node(&child, src, out, depth + 1);
                }
                out.push_str(&indent);
                out.push_str("}\n");
            } else {
                push_slice(out, src, node.start_byte(), node.end_byte(), &indent);
                out.push('\n');
            }
        }
        "macro_definition" | "macro_invocation" => {
            // Keep macros — they often define API shape.
            push_slice(out, src, node.start_byte(), node.end_byte(), &indent);
            out.push('\n');
        }
        _ => {
            // Drop comments, attributes between items, and anything else not on
            // the API surface. Outer `#[derive(...)]` attributes are part of
            // the parent item's range so they're already included above.
        }
    }
}

fn push_slice(out: &mut String, src: &[u8], start: usize, end: usize, indent: &str) {
    if start >= end || end > src.len() {
        return;
    }
    let text = match std::str::from_utf8(&src[start..end]) {
        Ok(s) => s,
        Err(_) => return,
    };
    let trimmed = text.trim_end();
    for (i, line) in trimmed.lines().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str(indent);
        out.push_str(line.trim_start());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_struct_and_fn_signature() {
        let src = r#"
pub struct Foo {
    pub x: i32,
    pub y: String,
}

pub fn add(a: i32, b: i32) -> i32 {
    let sum = a + b;
    sum
}
"#;
        let out = SignatureExtractor::new(Language::Rust)
            .extract(src)
            .unwrap();
        assert!(out.contains("struct Foo"), "{out}");
        assert!(out.contains("fn add(a: i32, b: i32) -> i32"), "{out}");
        assert!(out.contains("{ /* body */ }"), "{out}");
        assert!(!out.contains("let sum"), "{out}");
    }

    #[test]
    fn collapses_impl_block_with_methods() {
        let src = r#"
pub struct S;

impl S {
    pub fn new() -> Self {
        S
    }
    pub fn run(&self, n: u32) -> Result<(), String> {
        if n == 0 { return Err("zero".into()); }
        Ok(())
    }
}
"#;
        let out = SignatureExtractor::new(Language::Rust)
            .extract(src)
            .unwrap();
        assert!(out.contains("impl S"), "{out}");
        assert!(out.contains("fn new() -> Self"), "{out}");
        assert!(
            out.contains("fn run(&self, n: u32) -> Result<(), String>"),
            "{out}"
        );
        assert!(!out.contains("if n == 0"), "{out}");
    }

    #[test]
    fn keeps_use_and_const() {
        let src = r#"
use std::collections::HashMap;
const MAX: usize = 64;
"#;
        let out = SignatureExtractor::new(Language::Rust)
            .extract(src)
            .unwrap();
        assert!(out.contains("use std::collections::HashMap"), "{out}");
        assert!(out.contains("const MAX: usize = 64"), "{out}");
    }

    #[test]
    fn python_keeps_signatures_drops_bodies() {
        let src = r#"
import os

def greet(name: str) -> str:
    return f"hello {name}"

class Greeter:
    def __init__(self, prefix: str):
        self.prefix = prefix
    def say(self, name: str) -> str:
        return self.prefix + name
"#;
        let out = SignatureExtractor::new(Language::Python)
            .extract(src)
            .unwrap();
        assert!(out.contains("import os"), "{out}");
        assert!(out.contains("def greet(name: str) -> str"), "{out}");
        assert!(out.contains("class Greeter"), "{out}");
        assert!(out.contains("def say(self, name: str) -> str"), "{out}");
        assert!(!out.contains("self.prefix + name"), "{out}");
        assert!(!out.contains("f\"hello {name}\""), "{out}");
    }

    #[test]
    fn typescript_keeps_signatures_drops_bodies() {
        let src = r#"
import { foo } from "bar";

export type Id = string;

export interface Greeter {
  say(name: string): string;
}

export function greet(name: string): string {
  return "hello " + name;
}

class Impl implements Greeter {
  say(name: string): string {
    return "hi " + name;
  }
}
"#;
        let out = SignatureExtractor::new(Language::TypeScript)
            .extract(src)
            .unwrap();
        assert!(out.contains("type Id = string"), "{out}");
        assert!(out.contains("interface Greeter"), "{out}");
        assert!(out.contains("function greet"), "{out}");
        assert!(out.contains("class Impl"), "{out}");
        assert!(!out.contains("return \"hello \" + name"), "{out}");
        assert!(!out.contains("return \"hi \" + name"), "{out}");
    }
}
