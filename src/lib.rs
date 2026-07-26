//! PL/SQL parser plugin, full-parse mode.
//!
//! This is a compact dialect parser for routine/package-level structure.  It
//! avoids host tree-sitter package requirements while preserving the semantic
//! node vocabulary used by the prior CST adapter.

use intentdiff_plugin_sdk::tree::{SemanticNode, SemanticNodeBuilder};

wit_bindgen::generate!({
    path: "wit/plugin.wit",
    world: "parser-plugin",
});

use crate::exports::intentdiff::plugin::parser::ExamplePair;
use crate::exports::intentdiff::plugin::parser::Guest;
use crate::exports::intentdiff::plugin::parser::LanguageInfoRecord;
use crate::exports::intentdiff::plugin::parser::ParserMode;

const PLUGIN_METADATA: &str = include_str!("../plugin_metadata.info");

fn language_info_for(ids: Vec<String>) -> Vec<LanguageInfoRecord> {
    let metadata = intentdiff_plugin_sdk::metadata::parse_plugin_metadata(PLUGIN_METADATA);
    ids.into_iter()
        .map(|language_id| {
            let info = metadata.language_or_default(&language_id);
            LanguageInfoRecord {
                language_id: info.language_id,
                language_name: info.language_name,
                language_short_name: info.language_short_name,
                monaco_language: info.monaco_language,
                default_filename: info.default_filename,
                language_file_extensions: info.language_file_extensions,
                author: metadata.author().to_string(),
                plugin_version: metadata.plugin_version().to_string(),
                last_updated: metadata.last_updated().to_string(),
            }
        })
        .collect()
}

struct PlsqlParser;

#[derive(Debug, Clone)]
struct SourceLine {
    number: u32,
    text: String,
    trimmed: String,
}

#[derive(Debug, Clone)]
struct Block {
    start: usize,
    end: usize,
}

fn lines(source: &str) -> Vec<SourceLine> {
    source
        .lines()
        .enumerate()
        .map(|(i, text)| SourceLine {
            number: i as u32,
            text: text.to_string(),
            trimmed: text.trim().to_string(),
        })
        .collect()
}

fn split_blocks(source_lines: &[SourceLine]) -> Vec<Block> {
    let mut blocks = Vec::new();
    let mut start: Option<usize> = None;
    for (i, line) in source_lines.iter().enumerate() {
        if line.trimmed.is_empty() {
            continue;
        }
        if start.is_none() {
            start = Some(i);
        }
        if line.trimmed == "/" {
            if let Some(s) = start.take() {
                if i > s {
                    blocks.push(Block {
                        start: s,
                        end: i.saturating_sub(1),
                    });
                }
            }
        }
    }
    if let Some(s) = start {
        blocks.push(Block {
            start: s,
            end: source_lines.len().saturating_sub(1),
        });
    }
    blocks
}

fn clean_name(raw: &str) -> String {
    raw.trim_matches(|c: char| {
        !(c.is_ascii_alphanumeric() || c == '_' || c == '$' || c == '#' || c == '.')
    })
    .to_string()
}

fn object_name(header: &str, keyword: &str) -> String {
    let upper = header.to_uppercase();
    let keyword_upper = keyword.to_uppercase();
    if let Some(pos) = upper.find(&keyword_upper) {
        let rest = &header[pos + keyword.len()..];
        return clean_name(
            rest.split(|c: char| c == '(' || c.is_whitespace())
                .next()
                .unwrap_or(""),
        );
    }
    "(anonymous)".to_string()
}

fn block_kind(header: &str) -> (&'static str, String) {
    let upper = header.to_uppercase();
    if upper.contains("PACKAGE BODY") {
        (
            "create_or_replace_package_body",
            object_name(header, "PACKAGE BODY"),
        )
    } else if upper.contains("PACKAGE") {
        ("create_or_replace_package", object_name(header, "PACKAGE"))
    } else if upper.contains("PROCEDURE") {
        (
            "create_or_replace_procedure_body",
            object_name(header, "PROCEDURE"),
        )
    } else if upper.contains("FUNCTION") {
        (
            "create_or_replace_function_body",
            object_name(header, "FUNCTION"),
        )
    } else if upper.contains("TRIGGER") {
        ("create_or_replace_trigger", object_name(header, "TRIGGER"))
    } else if upper.contains("TYPE BODY") {
        (
            "create_or_replace_type_body",
            object_name(header, "TYPE BODY"),
        )
    } else if upper.contains("TYPE") {
        ("create_or_replace_type", object_name(header, "TYPE"))
    } else {
        ("plsql_block", "plsql_block".to_string())
    }
}

fn leaf(id: &str, node_type: &str, label: &str, line: &SourceLine) -> SemanticNode {
    SemanticNodeBuilder::new(
        id,
        node_type,
        label,
        line.number,
        0,
        line.number,
        line.text.len() as u32,
        "",
    )
    .build()
}

fn statement_type(trimmed: &str) -> &'static str {
    let upper = trimmed.to_uppercase();
    if upper.starts_with("RETURN ") {
        "return_statement"
    } else if trimmed.contains(":=") || trimmed.contains(" = ") {
        "assignment_statement"
    } else if upper.starts_with("DBMS_OUTPUT.") || trimmed.contains('(') {
        "call_statement"
    } else if upper.starts_with("EXCEPTION") {
        "exception_section"
    } else {
        "statement"
    }
}

fn block_children(id: &str, source_lines: &[SourceLine], block: &Block) -> Vec<SemanticNode> {
    let mut children = Vec::new();
    let mut child_index = 0usize;
    let mut in_body = false;

    for i in block.start..=block.end {
        let line = &source_lines[i];
        let trimmed = line.trimmed.as_str();
        if trimmed.is_empty() {
            continue;
        }
        let upper = trimmed.to_uppercase();
        let node_type = if i == block.start {
            "signature"
        } else if upper == "BEGIN" {
            in_body = true;
            "begin_statement"
        } else if upper.starts_with("END") {
            "end_statement"
        } else if in_body {
            statement_type(trimmed)
        } else {
            "declaration_statement"
        };
        children.push(leaf(
            &format!("{}.{}", id, child_index),
            node_type,
            trimmed,
            line,
        ));
        child_index += 1;
    }
    children
}

fn block_node(id: &str, source_lines: &[SourceLine], block: &Block) -> SemanticNode {
    let header = &source_lines[block.start];
    let (node_type, label) = block_kind(&header.trimmed);
    let last = &source_lines[block.end];
    SemanticNodeBuilder::new(
        id,
        node_type,
        label,
        header.number,
        0,
        last.number,
        last.text.len() as u32,
        "",
    )
    .children(block_children(id, source_lines, block))
    .build()
}

fn parse_source(source: &str) -> SemanticNode {
    let source_lines = lines(source);
    let blocks = split_blocks(&source_lines);
    let children = blocks
        .iter()
        .enumerate()
        .map(|(i, block)| block_node(&format!("0.{}", i), &source_lines, block))
        .collect();
    let end_line = source.lines().count().max(1) as u32;
    SemanticNodeBuilder::new("0", "source_file", "source_file", 1, 0, end_line, 0, "")
        .children(children)
        .build()
}

fn process_impl(source: &str) -> String {
    match serde_json::to_string(&parse_source(source)) {
        Ok(s) => s,
        Err(e) => format!(r#"{{"error":"Serialisation error: {}"}}"#, e),
    }
}

impl Guest for PlsqlParser {
    fn get_parser_mode() -> ParserMode {
        ParserMode::FullParse
    }
    fn grammar_id() -> String {
        "plsql".to_string()
    }
    fn detect_language(filename: String, content: String) -> String {
        let lower = filename.to_lowercase();
        if lower.ends_with(".pls")
            || lower.ends_with(".pck")
            || lower.ends_with(".pks")
            || lower.ends_with(".pkb")
        {
            return "plsql".to_string();
        }
        if !lower.ends_with(".sql") {
            return String::new();
        }
        let upper = content.to_uppercase();
        let indicators = [
            upper.contains(":="),
            upper.contains("%TYPE"),
            upper.contains("%ROWTYPE"),
            upper.contains("CREATE OR REPLACE PACKAGE"),
            upper.contains("CREATE OR REPLACE PROCEDURE"),
            upper.contains("CREATE OR REPLACE FUNCTION"),
            upper.contains("PRAGMA "),
            upper.contains("EXCEPTION WHEN"),
        ];
        if indicators.iter().any(|&b| b) {
            "plsql".to_string()
        } else {
            String::new()
        }
    }
    fn preprocess_source(source: String) -> String {
        source
    }
    fn example(_language: String) -> ExamplePair {
        ExamplePair {
            old: "CREATE OR REPLACE PROCEDURE greet(p_name IN VARCHAR2) IS\nBEGIN\n  DBMS_OUTPUT.PUT_LINE('Hello, ' || p_name);\nEND greet;\n/\n\nCREATE OR REPLACE FUNCTION add_nums(p_a IN NUMBER, p_b IN NUMBER)\n  RETURN NUMBER IS\nBEGIN\n  RETURN p_a + p_b;\nEND add_nums;\n/\n".to_string(),
            new: "CREATE OR REPLACE PROCEDURE greet(p_name IN VARCHAR2) IS\n  v_msg VARCHAR2(100);\nBEGIN\n  v_msg := 'Hello, ' || p_name || '!';\n  DBMS_OUTPUT.PUT_LINE(v_msg);\nEND greet;\n/\n\nCREATE OR REPLACE FUNCTION add_nums(p_a IN NUMBER, p_b IN NUMBER)\n  RETURN NUMBER IS\nBEGIN\n  RETURN p_a + p_b;\nEND add_nums;\n/\n\nCREATE OR REPLACE FUNCTION multiply_nums(p_a IN NUMBER, p_b IN NUMBER)\n  RETURN NUMBER IS\nBEGIN\n  RETURN p_a * p_b;\nEND multiply_nums;\n/\n".to_string(),
        }
    }
    fn process(input: String, _language: String, _filename: String) -> String {
        process_impl(&input)
    }
    fn trivia_node_types() -> Vec<String> {
        Vec::new()
    }
    fn language_ids() -> Vec<String> {
        vec!["plsql".to_string()]
    }
    fn language_info() -> Vec<LanguageInfoRecord> {
        language_info_for(Self::language_ids())
    }
    fn priority() -> i32 {
        10
    }
}

export!(PlsqlParser);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exports::intentdiff::plugin::parser::Guest;
    use intentdiff_plugin_sdk::testing as t;

    #[test]
    fn grammar_id_nonempty() {
        assert!(!PlsqlParser::grammar_id().is_empty());
    }

    #[test]
    fn parser_mode_is_full_parse() {
        assert!(matches!(
            PlsqlParser::get_parser_mode(),
            ParserMode::FullParse
        ));
    }

    #[test]
    fn language_ids_contain_grammar_id() {
        assert!(PlsqlParser::language_ids().contains(&PlsqlParser::grammar_id()));
    }

    #[test]
    fn detect_language_known_ext() {
        assert_eq!(
            PlsqlParser::detect_language("test.pls".to_string(), "".to_string()),
            "plsql"
        );
    }

    #[test]
    fn detect_language_unknown_ext() {
        assert_eq!(
            PlsqlParser::detect_language("test.xyz_notareal_ext_9z8y".to_string(), "".to_string()),
            ""
        );
    }

    #[test]
    fn process_impl_empty_returns_valid_json() {
        t::assert_valid_json(&process_impl(""), "process(empty)");
    }

    #[test]
    fn process_impl_whitespace_returns_valid_json() {
        t::assert_valid_json(&process_impl("   \n  "), "process(whitespace)");
    }

    #[test]
    fn playground_example_produces_routines() {
        let example = <PlsqlParser as Guest>::example("plsql".to_string());
        let out = process_impl(&example.new);
        t::assert_valid_json(&out, "plsql example");
        t::assert_no_error(&out, "plsql example");
        assert!(out.contains("create_or_replace_procedure_body"));
        assert!(out.contains("multiply_nums"));
    }
}
