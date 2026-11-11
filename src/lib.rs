//! PL/SQL parser plugin, full-parse mode.
//!
//! This is a compact dialect parser for routine/package-level structure.  It
//! avoids host tree-sitter package requirements while preserving the semantic
//! node vocabulary used by the prior CST adapter.

use intentumdiff_plugin_sdk::tree::{SemanticNode, SemanticNodeBuilder};

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
    let metadata = intentumdiff_plugin_sdk::metadata::parse_plugin_metadata(PLUGIN_METADATA);
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
    /// 0-based source row.
    number: u32,
    /// Raw row text without its line terminator (`\n` or `\r\n`).
    text: String,
    /// The row's code with SQL comments removed, trimmed. Empty for blank
    /// and comment-only rows, which carry no semantic content.
    trimmed: String,
}

#[derive(Debug, Clone)]
struct Block {
    start: usize,
    end: usize,
}

/// Lexer state carried across rows so multi-line block comments and string
/// literals are classified correctly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Lex {
    Code,
    /// Inside an ordinary `/* ... */` comment (dropped).
    BlockComment,
    /// Inside an optimizer hint `/*+ ... */` (kept: hints affect plans).
    Hint,
    /// Inside a `'...'` string literal (kept verbatim).
    Str,
    /// Inside a `"..."` quoted identifier (kept verbatim; `""` escapes `"`).
    QuotedIdent,
}

/// Removes ordinary `--` and `/* */` comments from one row, honouring string
/// literals (`''` escapes), quoted identifiers (`""` escapes) and optimizer
/// hints (`/*+`, `--+`), which are kept.
fn strip_comments(text: &str, state: &mut Lex) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0usize;
    let mut seg_start = 0usize;
    while i < bytes.len() {
        match *state {
            Lex::BlockComment => {
                if bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/') {
                    *state = Lex::Code;
                    i += 2;
                    seg_start = i;
                    // A comment separates tokens like whitespace does.
                    out.push(' ');
                } else {
                    i += 1;
                }
            }
            Lex::Hint => {
                if bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/') {
                    *state = Lex::Code;
                    i += 2;
                } else {
                    i += 1;
                }
            }
            Lex::Str | Lex::QuotedIdent => {
                let quote = if *state == Lex::Str { b'\'' } else { b'"' };
                if bytes[i] == quote {
                    if bytes.get(i + 1) == Some(&quote) {
                        i += 2;
                    } else {
                        *state = Lex::Code;
                        i += 1;
                    }
                } else {
                    i += 1;
                }
            }
            Lex::Code => match bytes[i] {
                b'\'' => {
                    *state = Lex::Str;
                    i += 1;
                }
                b'"' => {
                    *state = Lex::QuotedIdent;
                    i += 1;
                }
                b'-' if bytes.get(i + 1) == Some(&b'-') => {
                    if bytes.get(i + 2) == Some(&b'+') {
                        // `--+` hint: keep the rest of the row as code.
                        i = bytes.len();
                    } else {
                        out.push_str(&text[seg_start..i]);
                        seg_start = bytes.len();
                        i = bytes.len();
                    }
                }
                b'/' if bytes.get(i + 1) == Some(&b'*') => {
                    if bytes.get(i + 2) == Some(&b'+') {
                        *state = Lex::Hint;
                        i += 3;
                    } else {
                        out.push_str(&text[seg_start..i]);
                        *state = Lex::BlockComment;
                        i += 2;
                        seg_start = bytes.len();
                    }
                }
                _ => i += 1,
            },
        }
    }
    if seg_start < bytes.len() && *state != Lex::BlockComment {
        out.push_str(&text[seg_start..]);
    }
    out
}

/// Splits on `\n` (stripping a preceding `\r`) so row numbers match the
/// host's 0-based LF rows exactly, including a trailing empty row.
fn lines(source: &str) -> Vec<SourceLine> {
    let mut state = Lex::Code;
    source
        .split('\n')
        .enumerate()
        .map(|(i, raw)| {
            let text = raw.strip_suffix('\r').unwrap_or(raw);
            let code = strip_comments(text, &mut state);
            SourceLine {
                number: i as u32,
                text: text.to_string(),
                trimmed: code.trim().to_string(),
            }
        })
        .collect()
}

/// Groups code rows into `/`-terminated blocks. Blank and comment-only rows
/// never start or end a block, so a leading comment cannot become the
/// routine signature.
fn split_blocks(source_lines: &[SourceLine]) -> Vec<Block> {
    let mut blocks = Vec::new();
    let mut start: Option<usize> = None;
    let mut last_code: usize = 0;
    for (i, line) in source_lines.iter().enumerate() {
        if line.trimmed.is_empty() {
            continue;
        }
        if line.trimmed == "/" {
            if let Some(s) = start.take() {
                blocks.push(Block {
                    start: s,
                    end: last_code,
                });
            }
            continue;
        }
        if start.is_none() {
            start = Some(i);
        }
        last_code = i;
    }
    if let Some(s) = start {
        blocks.push(Block {
            start: s,
            end: last_code,
        });
    }
    blocks
}

/// Exact 0-based EOF position: (number of LFs, byte length of the final row).
fn eof_position(source: &str) -> (u32, u32) {
    match source.rfind('\n') {
        Some(pos) => (
            source.matches('\n').count() as u32,
            (source.len() - pos - 1) as u32,
        ),
        None => (0, source.len() as u32),
    }
}

fn clean_name(raw: &str) -> String {
    raw.trim_matches(|c: char| {
        !(c.is_alphanumeric() || c == '_' || c == '$' || c == '#' || c == '.')
    })
    .to_string()
}

fn object_name(header: &str, keyword: &str) -> String {
    // ASCII uppercasing keeps byte offsets aligned with `header`.
    let upper = header.to_ascii_uppercase();
    let keyword_upper = keyword.to_ascii_uppercase();
    if let Some(pos) = upper.find(&keyword_upper) {
        let rest = header.get(pos + keyword.len()..).unwrap_or("").trim_start();
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

    for (i, line) in source_lines
        .iter()
        .enumerate()
        .take(block.end + 1)
        .skip(block.start)
    {
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
    let (end_row, end_col) = eof_position(source);
    SemanticNodeBuilder::new(
        "0",
        "source_file",
        "source_file",
        0,
        0,
        end_row,
        end_col,
        "",
    )
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
    use intentumdiff_plugin_sdk::testing as t;

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

    const PLAYGROUND_OLD: &str = "CREATE OR REPLACE PROCEDURE greet(p_name IN VARCHAR2) IS\n  v_msg VARCHAR2(100);\nBEGIN\n  v_msg := 'Hello, ' || p_name || '!';\n  DBMS_OUTPUT.PUT_LINE(v_msg);\nEND greet;\n/\n\nCREATE OR REPLACE FUNCTION add_nums(p_a IN NUMBER, p_b IN NUMBER)\n  RETURN NUMBER IS\nBEGIN\n  RETURN p_a + p_b;\nEND add_nums;\n/\n\nCREATE OR REPLACE FUNCTION multiply_nums(p_a IN NUMBER, p_b IN NUMBER)\n  RETURN NUMBER IS\nBEGIN\n  RETURN p_a * p_b;\nEND multiply_nums;\n/\n";

    fn tree(source: &str) -> serde_json::Value {
        serde_json::from_str(&process_impl(source)).expect("valid json")
    }

    /// Type/label/child projection, ignoring positions (which legitimately
    /// shift when a comment row is inserted).
    fn projection(node: &serde_json::Value) -> serde_json::Value {
        let children: Vec<serde_json::Value> = node["children"]
            .as_array()
            .map(|c| c.iter().map(projection).collect())
            .unwrap_or_default();
        serde_json::json!([node["node_type"], node["label"], children])
    }

    fn span(node: &serde_json::Value) -> (u64, u64, u64, u64) {
        let p = &node["position"];
        (
            p["start_line"].as_u64().unwrap(),
            p["start_col"].as_u64().unwrap(),
            p["end_line"].as_u64().unwrap(),
            p["end_col"].as_u64().unwrap(),
        )
    }

    /// Every span is ordered, inside the source, and contained by its parent;
    /// ids are globally unique.
    fn assert_spans_valid(source: &str) {
        let rows: Vec<&str> = source.split('\n').collect();
        let root = tree(source);
        let mut ids = std::collections::HashSet::new();
        fn walk(
            n: &serde_json::Value,
            parent: Option<(u64, u64, u64, u64)>,
            rows: &[&str],
            ids: &mut std::collections::HashSet<String>,
        ) {
            let (sl, sc, el, ec) = span(n);
            assert!(ids.insert(n["id"].as_str().unwrap().to_string()), "dup id");
            assert!((sl, sc) <= (el, ec), "unordered span {:?}", n["id"]);
            assert!(
                (el as usize) < rows.len(),
                "row out of bounds {:?}",
                n["id"]
            );
            assert!(ec as usize <= rows[el as usize].len(), "col out of bounds");
            assert!(sc as usize <= rows[sl as usize].len(), "col out of bounds");
            if let Some((psl, psc, pel, pec)) = parent {
                assert!(
                    (psl, psc) <= (sl, sc) && (el, ec) <= (pel, pec),
                    "not contained"
                );
            }
            for c in n["children"].as_array().into_iter().flatten() {
                walk(c, Some((sl, sc, el, ec)), rows, ids);
            }
        }
        walk(&root, None, &rows, &mut ids);
    }

    // Regression: intentumdiff-plsql-parser#5 (data_config.plsql.safe_preservation).
    #[test]
    fn leading_line_comment_does_not_become_signature() {
        let new = format!("-- ordinary assessment comment\n{}", PLAYGROUND_OLD);
        let old_tree = tree(PLAYGROUND_OLD);
        let new_tree = tree(&new);
        assert_eq!(projection(&old_tree), projection(&new_tree));
        let first = &new_tree["children"][0];
        assert_eq!(first["node_type"], "create_or_replace_procedure_body");
        assert_eq!(first["label"], "greet");
        assert_eq!(
            first["children"][0]["label"],
            "CREATE OR REPLACE PROCEDURE greet(p_name IN VARCHAR2) IS"
        );
        // The routine starts on the row after the comment.
        assert_eq!(span(first).0, 1);
        assert_spans_valid(&new);
    }

    #[test]
    fn comments_before_each_routine_and_anonymous_block_are_ignored() {
        let old = "CREATE OR REPLACE FUNCTION f RETURN NUMBER IS\nBEGIN\n  RETURN 1;\nEND f;\n/\nBEGIN\n  NULL;\nEND;\n/\n";
        let new = "/* header\n   block comment */\nCREATE OR REPLACE FUNCTION f RETURN NUMBER IS\nBEGIN\n  -- body note\n  RETURN 1; -- trailing note\nEND f;\n/\n-- before anonymous block\nBEGIN\n  NULL; /* inline */\nEND;\n/\n-- trailing comment after last terminator\n";
        assert_eq!(projection(&tree(old)), projection(&tree(new)));
        assert_spans_valid(new);
    }

    #[test]
    fn comment_markers_inside_strings_and_hints_are_preserved() {
        let src = "BEGIN\n  v := '-- not /* a comment';\n  SELECT /*+ INDEX(t) */ x INTO y FROM t;\nEND;\n";
        let t = tree(src);
        let labels: Vec<&str> = t["children"][0]["children"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["label"].as_str().unwrap())
            .collect();
        assert!(labels.contains(&"v := '-- not /* a comment';"));
        assert!(labels.contains(&"SELECT /*+ INDEX(t) */ x INTO y FROM t;"));
    }

    fn body_labels(source: &str) -> Vec<String> {
        tree(source)["children"][0]["children"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["label"].as_str().unwrap().to_string())
            .collect()
    }

    // Regression: intentumdiff-plsql-parser#7.
    #[test]
    fn comment_markers_inside_quoted_identifiers_are_preserved() {
        let old = "BEGIN\n  \"counter--value\" := 1;\nEND;\n";
        let new = "BEGIN\n  \"counter--value\" := 2;\nEND;\n";
        assert!(body_labels(old).contains(&"\"counter--value\" := 1;".to_string()));
        assert!(body_labels(new).contains(&"\"counter--value\" := 2;".to_string()));
        assert_ne!(projection(&tree(old)), projection(&tree(new)));
        // `""` escapes a quote; `/*` inside the identifier is not a comment,
        // and a real comment after the identifier is still stripped.
        let src = "BEGIN\n  \"a\"\"/*b\" := 1; -- note\n  x := 2;\nEND;\n";
        let labels = body_labels(src);
        assert!(
            labels.contains(&"\"a\"\"/*b\" := 1;".to_string()),
            "{labels:?}"
        );
        assert!(labels.contains(&"x := 2;".to_string()), "{labels:?}");
        assert_spans_valid(src);
    }

    #[test]
    fn real_change_next_to_comment_is_still_visible() {
        let old = "-- note\nBEGIN\n  RETURN p_a + p_b;\nEND;\n";
        let new = "-- note\nBEGIN\n  RETURN p_a * p_b;\nEND;\n";
        assert_ne!(projection(&tree(old)), projection(&tree(new)));
    }

    // Regression: intentumdiff-plsql-parser#4 (data_config.plsql.known_no_final_newline).
    #[test]
    fn root_span_ends_at_exact_eof_without_final_newline() {
        let src = "BEGIN\n  NULL;\nEND;";
        let t = tree(src);
        assert_eq!(span(&t), (0, 0, 2, 4));
        assert_eq!(span(&t["children"][0]), (0, 0, 2, 4));
        assert_spans_valid(src);
    }

    #[test]
    fn root_span_ends_at_exact_eof_with_final_newline() {
        let t = tree("BEGIN\n  NULL;\nEND;\n");
        assert_eq!(span(&t), (0, 0, 3, 0));
        assert_spans_valid("BEGIN\n  NULL;\nEND;\n");
    }

    #[test]
    fn root_span_for_empty_and_incomplete_sources() {
        assert_eq!(span(&tree("")), (0, 0, 0, 0));
        assert_spans_valid("");
        assert_spans_valid("   \n  ");
        // data_config.plsql.incomplete new side: truncated mid-statement.
        let incomplete = "CREATE OR REPLACE PROCEDURE greet(p_name IN VARCHAR2) IS\n  v_msg VARCHAR2(100);\nBEGIN\n  v_msg := 'Hello, ' || p_name || '!';\n  DBMS_OUTPUT.PUT_LI";
        assert_eq!(span(&tree(incomplete)), (0, 0, 4, 20));
        assert_spans_valid(incomplete);
        assert_spans_valid(PLAYGROUND_OLD);
    }

    #[test]
    fn multibyte_and_crlf_columns_are_utf8_bytes_excluding_terminator() {
        // Columns are UTF-8 byte offsets; a `\r\n` terminator is not part of the row.
        let src = "BEGIN\r\n  v := 'héllo';\r\nEND;";
        let t = tree(src);
        assert_eq!(span(&t), (0, 0, 2, 4));
        let stmt = &t["children"][0]["children"][1];
        assert_eq!(span(stmt), (1, 0, 1, "  v := 'héllo';".len() as u64));
    }

    #[test]
    fn routine_labels_carry_object_names() {
        let t = tree(PLAYGROUND_OLD);
        let names: Vec<&str> = t["children"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["label"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["greet", "add_nums", "multiply_nums"]);
        // Non-ASCII header must not panic or misalign the name slice.
        let t = tree("CREATE OR REPLACE PROCEDURE ßtraße_p IS\nBEGIN\n  NULL;\nEND;\n/\n");
        assert_eq!(t["children"][0]["label"], "ßtraße_p");
    }

    #[test]
    fn output_is_deterministic() {
        assert_eq!(process_impl(PLAYGROUND_OLD), process_impl(PLAYGROUND_OLD));
    }
}
