//! Pure conversions between LSP payloads and model-facing text.

use serde_json::{Value, json};
use std::path::Path;

/// Largest tool output the model receives.
pub const MAX_OUTPUT_BYTES: usize = 64 * 1024;

const TRUNCATION_MARKER: &str = "\n[truncated]";

/// The `file://` URI for a filesystem path.
#[must_use]
pub fn path_to_uri(path: &Path) -> String {
    format!("file://{}", path.to_string_lossy())
}

/// The filesystem path inside a `file://` URI.
#[must_use]
pub fn uri_to_path(uri: &str) -> &str {
    uri.strip_prefix("file://").unwrap_or(uri)
}

/// The language id a file extension opens with.
#[must_use]
pub fn language_id(extension: &str) -> &str {
    match extension {
        "rs" => "rust",
        "ts" => "typescript",
        "tsx" => "typescriptreact",
        "js" => "javascript",
        "jsx" => "javascriptreact",
        "py" => "python",
        "go" => "go",
        "c" | "h" => "c",
        "cc" | "cpp" | "hpp" => "cpp",
        "java" => "java",
        "rb" => "ruby",
        "json" => "json",
        "md" => "markdown",
        other => other,
    }
}

/// Convert a 1-based tool position into a 0-based LSP position.
///
/// A character one past the last UTF-16 unit is the end of the line.
#[must_use]
pub fn position(text: &str, line: u64, character: u64) -> Option<Value> {
    if line == 0 || character == 0 {
        return None;
    }
    let line_count = u64::try_from(text.split('\n').count()).ok()?;
    if line > line_count {
        return None;
    }
    let index = usize::try_from(line - 1).ok()?;
    let line_text = text.split('\n').nth(index)?;
    let units = u64::try_from(line_text.encode_utf16().count()).ok()?;
    if character > units + 1 {
        return None;
    }
    Some(json!({"line": line - 1, "character": character - 1}))
}

/// The text of a hover result.
#[must_use]
pub fn hover_text(result: &Value) -> String {
    result
        .get("contents")
        .map_or_else(String::new, marked_strings)
}

fn marked_strings(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Array(items) => items
            .iter()
            .map(marked_strings)
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Object(map) => map
            .get("value")
            .and_then(Value::as_str)
            .map_or_else(String::new, ToString::to_string),
        Value::Null | Value::Bool(_) | Value::Number(_) => String::new(),
    }
}

/// One `path:line:character` line per location or location link.
#[must_use]
pub fn locations_text(result: &Value) -> String {
    let locations: Vec<&Value> = match result {
        Value::Array(items) => items.iter().collect(),
        other => vec![other],
    };
    locations
        .iter()
        .filter_map(|location| location_line(location))
        .collect::<Vec<_>>()
        .join("\n")
}

fn location_line(value: &Value) -> Option<String> {
    let uri = value
        .get("uri")
        .or_else(|| value.get("targetUri"))?
        .as_str()?;
    let start = value
        .get("range")
        .or_else(|| value.get("targetRange"))?
        .get("start")?;
    let line = start.get("line")?.as_u64()?;
    let character = start.get("character")?.as_u64()?;
    Some(format!(
        "{}:{}:{}",
        uri_to_path(uri),
        line + 1,
        character + 1
    ))
}

/// Indented `name (kind)` lines for a document symbol tree.
#[must_use]
pub fn symbols_text(result: &Value) -> String {
    let mut lines = Vec::new();
    if let Some(symbols) = result.as_array() {
        for symbol in symbols {
            symbol_lines(symbol, 0, &mut lines);
        }
    }
    lines.join("\n")
}

fn symbol_lines(symbol: &Value, depth: usize, lines: &mut Vec<String>) {
    let Some(name) = symbol.get("name").and_then(Value::as_str) else {
        return;
    };
    let kind = symbol
        .get("kind")
        .and_then(Value::as_u64)
        .map_or("Unknown", symbol_kind_name);
    lines.push(format!("{}{name} ({kind})", "  ".repeat(depth)));
    if let Some(children) = symbol.get("children").and_then(Value::as_array) {
        for child in children {
            symbol_lines(child, depth + 1, lines);
        }
    }
}

/// The LSP `SymbolKind` name for a numeric kind.
#[must_use]
pub fn symbol_kind_name(kind: u64) -> &'static str {
    const KINDS: [&str; 26] = [
        "File",
        "Module",
        "Namespace",
        "Package",
        "Class",
        "Method",
        "Property",
        "Field",
        "Constructor",
        "Enum",
        "Interface",
        "Function",
        "Variable",
        "Constant",
        "String",
        "Number",
        "Boolean",
        "Array",
        "Object",
        "Key",
        "Null",
        "EnumMember",
        "Struct",
        "Event",
        "Operator",
        "TypeParameter",
    ];
    usize::try_from(kind)
        .ok()
        .and_then(|index| index.checked_sub(1))
        .and_then(|index| KINDS.get(index).copied())
        .unwrap_or("Unknown")
}

/// Cut text to [`MAX_OUTPUT_BYTES`] on a character boundary.
#[must_use]
pub fn cap_output(text: String) -> String {
    if text.len() <= MAX_OUTPUT_BYTES {
        return text;
    }
    let budget = MAX_OUTPUT_BYTES - TRUNCATION_MARKER.len();
    let mut end = 0;
    for (start, character) in text.char_indices() {
        let next = start + character.len_utf8();
        if next > budget {
            break;
        }
        end = next;
    }
    let mut capped = text[..end].to_string();
    capped.push_str(TRUNCATION_MARKER);
    capped
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::PathBuf;

    #[test]
    fn uris_round_trip_through_paths() {
        assert_eq!(path_to_uri(&PathBuf::from("/tmp/a.rs")), "file:///tmp/a.rs");
        assert_eq!(uri_to_path("file:///tmp/a.rs"), "/tmp/a.rs");
        assert_eq!(uri_to_path("relative.rs"), "relative.rs");
    }

    #[test]
    fn extensions_map_to_language_ids() {
        for (extension, expected) in [
            ("rs", "rust"),
            ("ts", "typescript"),
            ("tsx", "typescriptreact"),
            ("js", "javascript"),
            ("jsx", "javascriptreact"),
            ("py", "python"),
            ("go", "go"),
            ("c", "c"),
            ("h", "c"),
            ("cc", "cpp"),
            ("cpp", "cpp"),
            ("hpp", "cpp"),
            ("java", "java"),
            ("rb", "ruby"),
            ("json", "json"),
            ("md", "markdown"),
        ] {
            assert_eq!(language_id(extension), expected, "extension {extension}");
        }
        assert_eq!(language_id("kt"), "kt");
    }

    #[test]
    fn positions_convert_from_one_based_to_zero_based() {
        assert_eq!(
            position("abc", 1, 1),
            Some(json!({"line": 0, "character": 0}))
        );
        assert_eq!(
            position("ab\ncde", 2, 3),
            Some(json!({"line": 1, "character": 2}))
        );
    }

    #[test]
    fn a_trailing_newline_opens_one_more_line() {
        assert_eq!(
            position("a\n", 2, 1),
            Some(json!({"line": 1, "character": 0}))
        );
        assert_eq!(position("a\n", 3, 1), None);
    }

    #[test]
    fn positions_outside_the_text_are_rejected() {
        assert_eq!(position("abc", 0, 1), None);
        assert_eq!(position("abc", 1, 0), None);
        assert_eq!(position("abc", 2, 1), None);
        assert_eq!(position("abc", 1, 5), None);
    }

    #[test]
    fn the_line_end_position_is_inside_and_one_past_it_is_not() {
        assert_eq!(
            position("abc", 1, 4),
            Some(json!({"line": 0, "character": 3}))
        );
        assert_eq!(position("abc", 1, 5), None);
    }

    #[test]
    fn characters_count_utf16_units() {
        assert_eq!(
            position("😀", 1, 3),
            Some(json!({"line": 0, "character": 2}))
        );
        assert_eq!(position("😀", 1, 4), None);
    }

    #[test]
    fn hover_contents_flatten_every_marked_string_shape() {
        assert_eq!(hover_text(&json!({"contents": "plain"})), "plain");
        assert_eq!(
            hover_text(&json!({"contents": {"kind": "markdown", "value": "# title"}})),
            "# title"
        );
        assert_eq!(
            hover_text(&json!({"contents": ["first", {"value": "second"}]})),
            "first\nsecond"
        );
        assert_eq!(hover_text(&json!({"contents": null})), "");
        assert_eq!(hover_text(&json!({"contents": {"kind": "plaintext"}})), "");
        assert_eq!(hover_text(&json!({"contents": [1, true]})), "\n");
        assert_eq!(hover_text(&json!({})), "");
    }

    #[test]
    fn locations_format_as_one_based_paths() {
        let location = json!({
            "uri": "file:///tmp/a.rs",
            "range": {"start": {"line": 2, "character": 4}, "end": {"line": 2, "character": 9}}
        });
        assert_eq!(locations_text(&location), "/tmp/a.rs:3:5");
        assert_eq!(
            locations_text(&json!([location, location])),
            "/tmp/a.rs:3:5\n/tmp/a.rs:3:5"
        );
        assert_eq!(locations_text(&json!([])), "");
        assert_eq!(locations_text(&Value::Null), "");
    }

    #[test]
    fn location_links_format_from_their_target_range() {
        let link = json!({
            "targetUri": "file:///tmp/b.rs",
            "targetRange": {"start": {"line": 0, "character": 0}},
            "targetSelectionRange": {"start": {"line": 0, "character": 0}}
        });
        assert_eq!(locations_text(&link), "/tmp/b.rs:1:1");
    }

    #[test]
    fn incomplete_locations_are_skipped() {
        assert_eq!(
            locations_text(&json!([
                {"uri": "file:///tmp/a.rs"},
                {"range": {"start": {"line": 0, "character": 0}}},
                {"uri": 7, "range": {"start": {"line": 0, "character": 0}}},
                {"uri": "file:///tmp/b.rs", "range": {"start": {"line": 1}}},
                {"uri": "file:///tmp/c.rs", "range": {"start": {"line": 1, "character": 1}}}
            ])),
            "/tmp/c.rs:2:2"
        );
    }

    #[test]
    fn symbols_indent_by_depth() {
        let symbols = json!([
            {"name": "main", "kind": 12, "children": [
                {"name": "x", "kind": 13},
                {"name": "nested", "kind": 23, "children": [{"name": "leaf", "kind": 26}]}
            ]},
            {"name": "S", "kind": 23}
        ]);
        assert_eq!(
            symbols_text(&symbols),
            "main (Function)\n  x (Variable)\n  nested (Struct)\n    leaf (TypeParameter)\nS (Struct)"
        );
    }

    #[test]
    fn symbols_tolerate_missing_fields_and_foreign_documents() {
        assert_eq!(symbols_text(&json!([{"kind": 12}])), "");
        assert_eq!(symbols_text(&json!([{"name": "x"}])), "x (Unknown)");
        assert_eq!(symbols_text(&json!({"name": "x"})), "");
        assert_eq!(symbols_text(&Value::Null), "");
    }

    #[test]
    fn every_symbol_kind_has_a_name() {
        let expected = [
            "File",
            "Module",
            "Namespace",
            "Package",
            "Class",
            "Method",
            "Property",
            "Field",
            "Constructor",
            "Enum",
            "Interface",
            "Function",
            "Variable",
            "Constant",
            "String",
            "Number",
            "Boolean",
            "Array",
            "Object",
            "Key",
            "Null",
            "EnumMember",
            "Struct",
            "Event",
            "Operator",
            "TypeParameter",
        ];
        for (index, name) in expected.iter().enumerate() {
            let kind = u64::try_from(index).expect("index") + 1;
            assert_eq!(symbol_kind_name(kind), *name, "kind {kind}");
        }
        assert_eq!(symbol_kind_name(0), "Unknown");
        assert_eq!(symbol_kind_name(27), "Unknown");
        assert_eq!(symbol_kind_name(u64::MAX), "Unknown");
    }

    #[test]
    fn the_output_cap_is_64_kib() {
        assert_eq!(MAX_OUTPUT_BYTES, 65_536);
    }

    #[test]
    fn small_outputs_pass_through() {
        assert_eq!(cap_output("short".to_string()), "short");
        let exact = "a".repeat(MAX_OUTPUT_BYTES);
        assert_eq!(cap_output(exact.clone()), exact);
    }

    #[test]
    fn large_outputs_are_cut_to_the_cap() {
        let oversized = "a".repeat(MAX_OUTPUT_BYTES + 1);
        let capped = cap_output(oversized);
        assert_eq!(capped.len(), MAX_OUTPUT_BYTES);
        assert!(capped.ends_with(TRUNCATION_MARKER));
    }

    #[test]
    fn truncation_lands_on_a_character_boundary() {
        let prefix = "a".repeat(MAX_OUTPUT_BYTES - TRUNCATION_MARKER.len() - 1);
        let text = format!("{prefix}€{}", "b".repeat(16));
        let capped = cap_output(text);
        assert!(capped.ends_with(TRUNCATION_MARKER));
        assert!(capped.len() <= MAX_OUTPUT_BYTES);
        assert_eq!(capped.len(), MAX_OUTPUT_BYTES - 1);
    }
}
