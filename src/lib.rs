//! Vue SFC parser plugin — full-parse mode.
//!
//! Handles `.vue` single-file component files.
//!
//! Vue SFCs consist of top-level blocks: `<template>`, `<script>`,
//! `<script setup>`, `<style>`, and custom blocks like `<docs>`.
//! This parser extracts SFC blocks plus lightweight semantic children for
//! common template/script/style constructs. Deep cross-language parsing remains
//! future work, but shipped examples should produce structured diffs.
//!
//! Semantic nodes produced:
//!   vue_component   — root; label = filename stem or "component"
//!   template_block  — `<template ...>` block; label = "template" (+ lang)
//!   script_block    — `<script ...>` block; label = "script" (+ "setup" if setup)
//!   style_block     — `<style ...>` block; label = "style" (+ "scoped" if scoped)
//!   custom_block    — any other top-level block; label = tag name

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
struct VueParser;

// ---------------------------------------------------------------------------
// Block extraction
// ---------------------------------------------------------------------------

struct SfcBlock {
    node_type: &'static str,
    label: String,
    start_line: u32,
    start_col: u32,
    end_line: u32,
    end_col: u32,
    content_start_line: u32,
    content: String,
    /// Content hash derived from the block text (simple djb2 variant).
    content_hash: String,
}

/// Extract a simple hash from a string slice (djb2 algorithm).
fn content_hash(s: &str) -> String {
    let mut h: u64 = 5381;
    for b in s.bytes() {
        h = h.wrapping_mul(33).wrapping_add(b as u64);
    }
    format!("{:016x}", h)
}

/// Parse a tag name and attrs from a raw `<tag attrs...` prefix string.
/// Returns `(tag_name_lowercase, attrs_string)`.
fn parse_tag_open(line: &str) -> Option<(String, String)> {
    let rest = line.trim_start().strip_prefix('<')?;
    // Tag name ends at whitespace, '>', or '/'
    let tag_end = rest
        .find(|c: char| c.is_whitespace() || c == '>' || c == '/')
        .unwrap_or(rest.len());
    let tag = &rest[..tag_end];
    if tag.is_empty() || tag.starts_with('/') || tag.starts_with('!') {
        return None;
    }
    let attrs = rest[tag_end..].trim().trim_end_matches('>').to_string();
    Some((tag.to_lowercase(), attrs))
}

fn attr_contains(attrs: &str, keyword: &str) -> bool {
    attrs.split_whitespace().any(|a| {
        a.to_lowercase() == keyword || a.to_lowercase().starts_with(&format!("{}=", keyword))
    })
}

fn attr_lang(attrs: &str) -> Option<String> {
    for part in attrs.split_whitespace() {
        let p = part.to_lowercase();
        if let Some(rest) = p.strip_prefix("lang=") {
            return Some(rest.trim_matches('"').trim_matches('\'').to_string());
        }
    }
    None
}

fn extract_blocks(source: &str) -> Vec<SfcBlock> {
    let mut blocks: Vec<SfcBlock> = Vec::new();
    let lines: Vec<&str> = source.lines().collect();

    let mut i = 0;
    while i < lines.len() {
        let line = lines[i].trim_start();
        if let Some((tag, attrs)) = parse_tag_open(line) {
            let tag_str = tag.clone();
            let start_line = i as u32;
            let start_col = (lines[i].len() - lines[i].trim_start().len()) as u32;
            let close_tag = format!("</{}>", tag_str);
            let open_tag_prefix = format!("<{}", tag_str);
            // Scan through lines (starting at the same line) to find the matching close tag.
            // Track nesting depth; the opening tag on line i contributes depth=1 already.
            let mut depth: i32 = 1;
            let mut end_line = lines.len().saturating_sub(1) as u32;
            let mut j = i;
            // On line i we already "consumed" the opening; count extra opens and closes on that line.
            {
                let l = lines[j];
                // Count additional opens on same line (excluding the first one)
                let extra_opens = l.matches(&open_tag_prefix).count().saturating_sub(1) as i32;
                let closes_here = l.matches(&close_tag).count() as i32;
                depth += extra_opens - closes_here;
            }
            if depth <= 0 {
                // Opening and closing tag on the same line
                end_line = i as u32;
            } else {
                j = i + 1;
                'outer: while j < lines.len() {
                    let l = lines[j];
                    let opens = l.matches(&open_tag_prefix).count() as i32;
                    let closes = l.matches(&close_tag).count() as i32;
                    depth += opens - closes;
                    if depth <= 0 {
                        end_line = j as u32;
                        break 'outer;
                    }
                    j += 1;
                }
            }
            let content_start = (start_line + 1) as usize;
            let content_end = end_line as usize;
            let block_content: String =
                lines[content_start.min(content_end)..content_end].join("\n");
            let hash = content_hash(&block_content);
            let node_type: &'static str = match tag_str.as_str() {
                "template" => "template_block",
                "script" => "script_block",
                "style" => "style_block",
                _ => "custom_block",
            };
            let label = build_label(&tag_str, &attrs);
            blocks.push(SfcBlock {
                node_type,
                label,
                start_line,
                start_col,
                end_line,
                end_col: lines[end_line as usize].len() as u32,
                content_start_line: start_line + 1,
                content: block_content,
                content_hash: hash,
            });
            i = end_line as usize;
        }
        i += 1;
    }
    blocks
}

fn build_label(tag: &str, attrs: &str) -> String {
    let mut parts: Vec<String> = vec![tag.to_string()];
    if attr_contains(attrs, "setup") {
        parts.push("setup".to_string());
    }
    if attr_contains(attrs, "scoped") {
        parts.push("scoped".to_string());
    }
    if let Some(lang) = attr_lang(attrs) {
        parts.push(lang);
    }
    parts.join(":")
}


fn make_leaf(
    id: &str,
    node_type: &str,
    label: impl Into<String>,
    line: u32,
    start_col: u32,
    end_col: u32,
) -> SemanticNode {
    SemanticNodeBuilder::new(id, node_type, label.into(), line, start_col, line, end_col, "").build()
}
fn is_identifier_like(value: &str) -> bool {
    let mut chars = value.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' || c == '$' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$' || c == '-')
}

fn normalize_statement(line: &str) -> String {
    line.trim()
        .trim_end_matches(',')
        .trim_end_matches(';')
        .trim()
        .to_string()
}


fn parse_attr_labels(attrs: &str) -> Vec<(String, usize, usize)> {
    let mut cursor = 0;
    attrs
        .split_whitespace()
        .filter_map(|part| {
            let start = cursor + attrs[cursor..].find(part)?;
            let end = start + part.len();
            cursor = end;
            let clean = part.trim().trim_end_matches('>').trim_end_matches('/').trim();
            if clean.is_empty() { return None; }
            let (name, value) = clean.split_once('=').unwrap_or((clean, ""));
            let name = name.trim();
            if name.is_empty() { return None; }
            let label = if value.is_empty() {
                name.to_string()
            } else {
                let value = value.trim().trim_matches('"').trim_matches('\'')
                    .trim_matches('{').trim_matches('}');
                format!("{}={}", name, value)
            };
            Some((label, start, end))
        })
        .collect()
}
fn parse_tag_segment(segment: &str) -> Option<(String, String, usize)> {
    let trimmed_offset = segment.len() - segment.trim_start().len();
    let trimmed = segment.trim();
    if trimmed.is_empty() || trimmed.starts_with('/') || trimmed.starts_with('!') {
        return None;
    }
    let tag_end = trimmed
        .find(|c: char| c.is_whitespace() || c == '>' || c == '/')
        .unwrap_or(trimmed.len());
    let tag = trimmed[..tag_end].trim();
    if tag.is_empty() {
        return None;
    }
    let tail = &trimmed[tag_end..];
    // Compute the original byte offset rather than searching for duplicated attrs.
    let attr_start = trimmed_offset + tag_end + tail.len() - tail.trim_start().len();
    let attrs = tail.trim().trim_end_matches('/').trim().to_string();
    Some((tag.to_lowercase(), attrs, attr_start))
}


fn extract_mustache_labels(line: &str) -> Vec<(String, usize, usize)> {
    let mut labels = Vec::new();
    let mut cursor = 0;
    while let Some(relative) = line[cursor..].find("{{") {
        let after_open = cursor + relative + 2;
        let Some(relative_end) = line[after_open..].find("}}") else { break };
        let after_close = after_open + relative_end;
        let raw = &line[after_open..after_close];
        let label = raw.trim();
        if !label.is_empty() {
            let start = after_open + raw.len() - raw.trim_start().len();
            labels.push((label.to_string(), start, start + label.len()));
        }
        cursor = after_close + 2;
    }
    labels
}

fn extract_template_children(content: &str, start_line: u32, id_prefix: &str) -> Vec<SemanticNode> {
    let mut children = Vec::new();
    for (line_idx, line) in content.lines().enumerate() {
        let absolute_line = start_line + line_idx as u32;
        let mut rest = line;
        let mut consumed = 0;
        while let Some(open_idx) = rest.find('<') {
            let after_open = &rest[open_idx + 1..];
            if let Some(close_idx) = after_open.find('>') {
                let segment = &after_open[..close_idx];
                if let Some((tag, attrs, attr_start)) = parse_tag_segment(segment) {
                    let child_id = format!("{}.{}", id_prefix, children.len());
                    let local_start = consumed + open_idx;
                    let local_end = local_start + close_idx + 2;
                    let attr_children: Vec<SemanticNode> = parse_attr_labels(&attrs)
                        .into_iter()
                        .enumerate()
                        .map(|(i, (label, a, b))| {
                            make_leaf(
                                &format!("{}.{}", child_id, i), "attribute", label,
                                absolute_line,
                                (local_start + 1 + attr_start + a) as u32,
                                (local_start + 1 + attr_start + b) as u32,
                            )
                        })
                        .collect();
                    children.push(
                        SemanticNodeBuilder::new(
                            &child_id, "element", tag,
                            absolute_line, local_start as u32,
                            absolute_line, local_end as u32, "",
                        )
                        .children(attr_children)
                        .build(),
                    );
                }
                consumed += open_idx + close_idx + 2;
                rest = &after_open[close_idx + 1..];
            } else {
                break;
            }
        }
        for (label, start, end) in extract_mustache_labels(line) {
            let child_id = format!("{}.{}", id_prefix, children.len());
            children.push(make_leaf(&child_id, "interpolation", label, absolute_line, start as u32, end as u32));
        }
    }
    children
}
fn declaration_name(line: &str) -> Option<&str> {
    for keyword in ["const ", "let ", "var "] {
        if let Some(rest) = line.strip_prefix(keyword) {
            let name = rest
                .split(|c: char| c.is_whitespace() || c == '=' || c == ':' || c == ';')
                .next()
                .unwrap_or("");
            if is_identifier_like(name) {
                return Some(name);
            }
        }
    }
    None
}

fn function_name(line: &str) -> Option<&str> {
    if let Some(rest) = line.strip_prefix("function ") {
        let name = rest
            .split(|c: char| c.is_whitespace() || c == '(')
            .next()
            .unwrap_or("");
        if is_identifier_like(name) {
            return Some(name);
        }
    }
    if let Some(idx) = line.find("()") {
        let name = line[..idx].trim();
        if is_identifier_like(name) {
            return Some(name);
        }
    }
    None
}

fn property_name(line: &str) -> Option<&str> {
    let (name, _) = line.split_once(':')?;
    let name = name.trim().trim_matches('"').trim_matches('\'');
    if is_identifier_like(name) {
        Some(name)
    } else {
        None
    }
}

// Locate the outer assignment. Operators in nested expressions or strings
// belong to the RHS and cannot delimit the current statement.
fn outer_assignment_operator(line: &str) -> Option<(usize, usize)> {
    let bytes = line.as_bytes();
    let mut quote: Option<u8> = None;
    let mut escaped = false;
    let mut depth = 0usize;
    for (index, byte) in bytes.iter().copied().enumerate() {
        if let Some(delimiter) = quote {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == delimiter {
                quote = None;
            }
            continue;
        }
        match byte {
            b'\'' | b'"' | 0x60 => quote = Some(byte),
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => depth = depth.saturating_sub(1),
            b'=' if depth == 0 => {
                if bytes.get(index + 1).is_some_and(|b| matches!(b, b'=' | b'>'))
                    || (index > 0 && matches!(bytes[index - 1], b'=' | b'!' | b'<' | b'>'))
                {
                    continue;
                }
                let start = if index > 0
                    && matches!(bytes[index - 1], b'+' | b'-' | b'*' | b'/')
                {
                    index - 1
                } else {
                    index
                };
                return Some((start, index + 1 - start));
            }
            _ => {}
        }
    }
    None
}

fn assignment_label(line: &str) -> Option<String> {
    let (index, _) = outer_assignment_operator(line)?;
    let label = line[..index].trim();
    if label.is_empty()
        || label.starts_with("return")
        || label.starts_with("const ")
        || label.starts_with("let ")
        || label.starts_with("var ")
    {
        return None;
    }
    Some(label.to_string())
}


fn extract_script_children(content: &str, start_line: u32, id_prefix: &str) -> Vec<SemanticNode> {
    let mut children = Vec::new();
    for (line_idx, raw_line) in content.lines().enumerate() {
        let line = normalize_statement(raw_line);
        if line.is_empty() || line == "{" || line == "}" {
            continue;
        }
        let absolute_line = start_line + line_idx as u32;
        let leading = raw_line.find(&line).expect("normalized statement occurs in input line");
        let start_col = leading as u32;
        let end_col = start_col + line.len() as u32;
        let child_id = format!("{}.{}", id_prefix, children.len());
        let node = if line.starts_with("import ") {
            Some(make_leaf(&child_id, "import_statement", line, absolute_line, start_col, end_col))
        } else if line.starts_with("export default") {
            Some(make_leaf(&child_id, "export_default", "export default", absolute_line, start_col, end_col))
        } else if let Some(name) = declaration_name(&line) {
            let mut decl = make_leaf(&child_id, "variable_declaration", name.to_string(), absolute_line, start_col, end_col);
            if let Some(equals) = line.find('=') {
                let remainder = &line[equals + 1..];
                let rhs = remainder.trim();
                if !rhs.is_empty() {
                    let rhs_start = start_col + (equals + 1 + remainder.len() - remainder.trim_start().len()) as u32;
                    decl.children = vec![make_leaf(
                        &format!("{child_id}.0"), "declaration_value", rhs,
                        absolute_line, rhs_start, rhs_start + rhs.len() as u32,
                    )];
                }
            }
            Some(decl)
        } else if let Some(name) = function_name(&line) {
            Some(make_leaf(&child_id, "method_declaration", name.to_string(), absolute_line, start_col, end_col))
        } else if let Some(name) = property_name(&line) {
            let mut prop = make_leaf(&child_id, "property", name.to_string(), absolute_line, start_col, end_col);
            if let Some(colon) = line.find(':') {
                let remainder = &line[colon + 1..];
                let rhs = remainder.trim();
                if !rhs.is_empty() {
                    let rhs_start = start_col + (colon + 1 + remainder.len() - remainder.trim_start().len()) as u32;
                    prop.children = vec![make_leaf(
                        &format!("{child_id}.0"), "property_value", rhs,
                        absolute_line, rhs_start, rhs_start + rhs.len() as u32,
                    )];
                }
            }
            Some(prop)
        } else if let Some(label) = assignment_label(&line) {
            let mut assignment = make_leaf(
                &child_id, "assignment_statement", label, absolute_line, start_col, end_col,
            );
            if let Some((index, op_len)) = outer_assignment_operator(&line) {
                let remainder = &line[index + op_len..];
                let rhs = remainder.trim();
                if !rhs.is_empty() {
                    let rhs_start = start_col + (index + op_len + remainder.len() - remainder.trim_start().len()) as u32;
                    assignment.children = vec![make_leaf(
                        &format!("{child_id}.0"), "assignment_value", rhs,
                        absolute_line, rhs_start, rhs_start + rhs.len() as u32,
                    )];
                }
            }
            Some(assignment)
        } else {
            None
        };
        if let Some(node) = node { children.push(node); }
    }
    children
}

// A CSS value may contain a balanced block (for instance a custom property).
// End at a top-level semicolon or rule brace, not at a value's closing brace.
fn css_declaration_value(remainder: &str) -> &str {
    let mut quote: Option<u8> = None;
    let mut escaped = false;
    let mut depth = 0usize;
    let mut end = remainder.len();
    for (index, byte) in remainder.bytes().enumerate() {
        if let Some(delimiter) = quote {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == delimiter {
                quote = None;
            }
            continue;
        }
        match byte {
            b'\'' | b'"' => quote = Some(byte),
            b'{' | b'(' | b'[' => depth += 1,
            b'}' | b')' | b']' if depth > 0 => depth -= 1,
            b'}' | b';' if depth == 0 => {
                end = index;
                break;
            }
            _ => {}
        }
    }
    remainder[..end].trim()
}

fn extract_style_children(content: &str, start_line: u32, id_prefix: &str) -> Vec<SemanticNode> {
    let mut children = Vec::new();
    for (line_idx, raw_line) in content.lines().enumerate() {
        let line = raw_line.trim();
        if line.is_empty() || line == "}" { continue; }
        let absolute_line = start_line + line_idx as u32;
        let start_col = raw_line.find(line).expect("trimmed style line occurs in input") as u32;
        let end_col = start_col + line.len() as u32;
        let child_id = format!("{}.{}", id_prefix, children.len());
        if let Some(selector) = line.strip_suffix('{') {
            let selector = selector.trim();
            if !selector.is_empty() {
                children.push(make_leaf(&child_id, "style_rule", selector, absolute_line, start_col, end_col));
            }
        } else if let Some((property, _)) = line.split_once(':') {
            let property = property.trim();
            if !property.is_empty() {
                let mut decl = make_leaf(&child_id, "style_declaration", property, absolute_line, start_col, end_col);
                if let Some(colon) = line.find(':') {
                    let remainder = &line[colon + 1..];
                    let rhs = css_declaration_value(remainder);
                    if !rhs.is_empty() {
                        let rhs_start = start_col + (colon + 1 + remainder.len() - remainder.trim_start().len()) as u32;
                        decl.children = vec![make_leaf(
                            &format!("{child_id}.0"), "style_value", rhs,
                            absolute_line, rhs_start, rhs_start + rhs.len() as u32,
                        )];
                    }
                }
                children.push(decl);
            }
        }
    }
    children
}
fn sfc_block_to_node(block: &SfcBlock, id: &str) -> SemanticNode {
    let children = match block.node_type {
        "template_block" => extract_template_children(&block.content, block.content_start_line, id),
        "script_block" => extract_script_children(&block.content, block.content_start_line, id),
        "style_block" => extract_style_children(&block.content, block.content_start_line, id),
        _ => Vec::new(),
    };
    SemanticNodeBuilder::new(
        id,
        block.node_type,
        block.label.clone(),
        block.start_line,
        block.start_col,
        block.end_line,
        block.end_col,
        block.content_hash.clone(),
    )
    .children(children)
    .build()
}

fn process_impl(source: &str, filename: &str) -> String {
    let stem = filename
        .rsplit(['/', '\\'])
        .next()
        .and_then(|f| f.rsplit('.').nth(1))
        .unwrap_or("component");

    let blocks = extract_blocks(source);
    let end_line = source.lines().count().saturating_sub(1) as u32;
    let end_col = source.lines().last().map(|line| line.len() as u32).unwrap_or(0);

    let children: Vec<SemanticNode> = blocks
        .iter()
        .enumerate()
        .map(|(i, b)| sfc_block_to_node(b, &format!("0.{}", i)))
        .collect();

    // Compute root hash from children hashes
    let root_hash: String = {
        let combined: String = blocks
            .iter()
            .map(|b| b.content_hash.as_str())
            .collect::<Vec<_>>()
            .join("|");
        content_hash(&combined)
    };

    let root = SemanticNodeBuilder::new(
        "0",
        "vue_component",
        stem.to_string(),
        0,
        0,
        end_line,
        end_col,
        root_hash,
    )
    .children(children)
    .build();

    match serde_json::to_string(&root) {
        Ok(s) => s,
        Err(e) => format!(r#"{{"error":"Serialisation error: {}"}}"#, e),
    }
}

impl Guest for VueParser {
    fn get_parser_mode() -> ParserMode {
        ParserMode::FullParse
    }
    fn grammar_id() -> String {
        "vue".to_string()
    }
    fn detect_language(filename: String, _content: String) -> String {
        if filename.to_lowercase().ends_with(".vue") {
            return "vue".to_string();
        }
        String::new()
    }
    fn preprocess_source(source: String) -> String {
        source
    }
    fn example(_language: String) -> ExamplePair {
        ExamplePair {
            old: "<template>\n  <h1>{{ greeting }}</h1>\n</template>\n\n<script>\nexport default {\n  data() {\n    return {\n      greeting: 'Hello, World!'\n    }\n  }\n}\n</script>\n".to_string(),
            new: "<template>\n  <div>\n    <h1>{{ greeting }}</h1>\n    <button @click=\"changeGreeting\">Change</button>\n  </div>\n</template>\n\n<script>\nexport default {\n  data() {\n    return {\n      greeting: 'Hello, World!',\n      names: ['Alice', 'Bob', 'Carol'],\n      current: 0\n    }\n  },\n  methods: {\n    changeGreeting() {\n      this.current = (this.current + 1) % this.names.length;\n      this.greeting = 'Hello, ' + this.names[this.current] + '!';\n    }\n  }\n}\n</script>\n".to_string(),
        }
    }
    fn process(input: String, _language: String, filename: String) -> String {
        process_impl(&input, &filename)
    }
    fn trivia_node_types() -> Vec<String> {
        vec![]
    }
    fn language_ids() -> Vec<String> {
        vec!["vue".to_string()]
    }
    fn language_info() -> Vec<LanguageInfoRecord> {
        language_info_for(Self::language_ids())
    }
    fn priority() -> i32 {
        0
    }
}

export!(VueParser);

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exports::intentdiff::plugin::parser::Guest;

    #[test]
    fn grammar_id_nonempty() {
        assert!(!VueParser::grammar_id().is_empty());
    }

    #[test]
    fn language_ids_contain_grammar_id() {
        let gid = VueParser::grammar_id();
        let ids = VueParser::language_ids();
        assert!(ids.contains(&gid));
    }

    #[test]
    fn detect_language_vue() {
        assert_eq!(
            VueParser::detect_language("App.vue".to_string(), "".to_string()),
            "vue"
        );
    }

    #[test]
    fn detect_language_unknown() {
        assert_eq!(
            VueParser::detect_language("main.ts".to_string(), "".to_string()),
            ""
        );
    }

    #[test]
    fn empty_source_produces_valid_json() {
        let out = process_impl("", "Component.vue");
        serde_json::from_str::<serde_json::Value>(&out).expect("valid JSON");
    }

    #[test]
    fn sfc_blocks_extracted() {
        let src = r#"<template><div>Hello</div></template>
<script setup lang="ts">
const x = 1;
</script>
<style scoped>
.btn { color: red; }
</style>"#;
        let blocks = extract_blocks(src);
        assert_eq!(blocks.len(), 3);
        assert_eq!(blocks[0].node_type, "template_block");
        assert_eq!(blocks[1].node_type, "script_block");
        assert_eq!(blocks[2].node_type, "style_block");
        assert!(blocks[1].label.contains("setup"));
        assert!(blocks[1].label.contains("ts"));
        assert!(blocks[2].label.contains("scoped"));
    }

    #[test]
    fn attr_label_setup() {
        assert_eq!(
            build_label("script", "setup lang=\"ts\""),
            "script:setup:ts"
        );
    }

    #[test]
    fn attr_label_scoped() {
        assert_eq!(build_label("style", "scoped"), "style:scoped");
    }

    #[test]
    fn example_extracts_component_children() {
        let example = VueParser::example("vue".to_string());
        let out = process_impl(&example.new, "Component.vue");
        let v: serde_json::Value = serde_json::from_str(&out).expect("valid JSON");
        let labels = collect_labels(&v);
        assert!(labels.iter().any(|label| label == "button"));
        assert!(labels.iter().any(|label| label == "@click=changeGreeting"));
        assert!(labels.iter().any(|label| label == "changeGreeting"));
        assert!(labels.iter().any(|label| label == "names"));
    }


    #[test]
    fn numeric_property_mutation_has_source_byte_spans_on_both_sides() {
        let old_source = VueParser::example("vue".to_string()).new;
        assert_eq!(old_source.matches("current: 0").count(), 1);
        let new_source = old_source.replacen("current: 0", "current: 1", 1);
        fn find<'a>(node: &'a serde_json::Value, kind: &str, label: &str) -> Option<&'a serde_json::Value> {
            if node["node_type"] == kind && node["label"] == label { return Some(node); }
            node["children"].as_array()?.iter().find_map(|child| find(child, kind, label))
        }
        for (source, label) in [(&old_source, "0"), (&new_source, "1")] {
            let line_index = source.lines().position(|line| line.contains("current: ")).unwrap();
            let line = source.lines().nth(line_index).unwrap();
            let expected_start = line.find(&format!("current: {label}")).unwrap() + "current: ".len();
            let tree: serde_json::Value = serde_json::from_str(&process_impl(source, "App.vue")).unwrap();
            let leaf = find(&tree, "property_value", label).expect("Rust semantic value must exist");
            assert_eq!(leaf["position"]["start_line"], line_index as u32);
            assert_eq!(leaf["position"]["end_line"], line_index as u32);
            assert_eq!(leaf["position"]["start_col"], expected_start as u32);
            assert_eq!(leaf["position"]["end_col"], (expected_start + label.len()) as u32);
        }
        let old: serde_json::Value = serde_json::from_str(&process_impl(&old_source, "App.vue")).unwrap();
        let new: serde_json::Value = serde_json::from_str(&process_impl(&new_source, "App.vue")).unwrap();
        assert_ne!(old["structural_hash"], new["structural_hash"]);
    }

    #[test]
    fn unicode_indent_interpolation_and_style_rhs_use_utf8_byte_columns() {
        let source = "<template>\n  <h1>{{ café }}</h1>\n</template>\n<script>\n  const city = 'é';\n</script>\n<style>\n  .x {\n    color: red;\n  }\n</style>";
        let tree: serde_json::Value = serde_json::from_str(&process_impl(source, "App.vue")).unwrap();
        fn find<'a>(node: &'a serde_json::Value, kind: &str, label: &str) -> Option<&'a serde_json::Value> {
            if node["node_type"] == kind && node["label"] == label { return Some(node); }
            node["children"].as_array()?.iter().find_map(|child| find(child, kind, label))
        }
        for (kind, label, line_index, needle) in [
            ("interpolation", "café", 1usize, "café"),
            ("declaration_value", "'é'", 4usize, "'é'"),
            ("style_value", "red", 8usize, "red"),
        ] {
            let line = source.lines().nth(line_index).unwrap();
            let col = line.find(needle).unwrap();
            let leaf = find(&tree, kind, label).expect("source-anchored semantic child");
            assert_eq!(leaf["position"]["start_line"], line_index as u32);
            assert_eq!(leaf["position"]["start_col"], col as u32);
            assert_eq!(leaf["position"]["end_col"], (col + needle.len()) as u32);
        }
    }

    #[test]
    fn assignment_rhs_is_semantic_and_byte_anchored() {
        let source = "<script>\n  count += 2;\n</script>";
        let tree: serde_json::Value = serde_json::from_str(&process_impl(source, "App.vue")).unwrap();
        let assignment = &tree["children"][0]["children"][0];
        assert_eq!(assignment["node_type"], "assignment_statement");
        let value = &assignment["children"][0];
        assert_eq!(value["node_type"], "assignment_value");
        assert_eq!(value["label"], "2");
        assert_eq!(value["position"]["start_line"], 1);
        assert_eq!(value["position"]["start_col"], 11);
        assert_eq!(value["position"]["end_col"], 12);
    }

    #[test]
    fn duplicate_tag_attribute_uses_true_source_byte_offset() {
        let source = "<template>\n  <slot slot></slot>\n</template>";
        let tree: serde_json::Value = serde_json::from_str(&process_impl(source, "App.vue")).unwrap();
        let attr = &tree["children"][0]["children"][0]["children"][0];
        assert_eq!(attr["label"], "slot");
        assert_eq!(attr["position"]["start_line"], 1);
        assert_eq!(attr["position"]["start_col"], 8);
        assert_eq!(attr["position"]["end_col"], 12);
    }

    #[test]
    fn css_custom_property_preserves_balanced_value_brace() {
        let source = "<style>\n  .x {\n    --theme: { color: red; };\n  }\n</style>";
        let tree: serde_json::Value = serde_json::from_str(&process_impl(source, "App.vue")).unwrap();
        let value = &tree["children"][0]["children"][1]["children"][0];
        let line = source.lines().nth(2).unwrap();
        let expected = "{ color: red; }";
        let start = line.find(expected).unwrap();
        assert_eq!(value["node_type"], "style_value");
        assert_eq!(value["label"], expected);
        assert_eq!(value["position"]["start_line"], 2);
        assert_eq!(value["position"]["start_col"], start as u32);
        assert_eq!(value["position"]["end_col"], (start + expected.len()) as u32);
    }

    #[test]
    fn nested_compound_and_quoted_operators_keep_outer_assignment_rhs() {
        let source = "<script>\n  result = (count += 1);\n  message = \"a+=b\";\n</script>";
        let tree: serde_json::Value = serde_json::from_str(&process_impl(source, "App.vue")).unwrap();
        for (child_index, line_index, expected) in [
            (0usize, 1usize, "(count += 1)"),
            (1usize, 2usize, "\"a+=b\""),
        ] {
            let assignment = &tree["children"][0]["children"][child_index];
            assert_eq!(assignment["node_type"], "assignment_statement");
            let value = &assignment["children"][0];
            let line = source.lines().nth(line_index).unwrap();
            let start = line.find(expected).unwrap();
            assert_eq!(value["node_type"], "assignment_value");
            assert_eq!(value["label"], expected);
            assert_eq!(value["position"]["start_line"], line_index as u32);
            assert_eq!(value["position"]["start_col"], start as u32);
            assert_eq!(value["position"]["end_col"], (start + expected.len()) as u32);
        }
    }

    fn collect_labels(value: &serde_json::Value) -> Vec<String> {
        let mut labels = Vec::new();
        if let Some(label) = value["label"].as_str() {
            labels.push(label.to_string());
        }
        if let Some(children) = value["children"].as_array() {
            for child in children {
                labels.extend(collect_labels(child));
            }
        }
        labels
    }
}
