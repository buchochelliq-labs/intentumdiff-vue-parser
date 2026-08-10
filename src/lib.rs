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
    end_line: u32,
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
                end_line,
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

fn make_leaf(id: &str, node_type: &str, label: impl Into<String>, line: u32) -> SemanticNode {
    SemanticNodeBuilder::new(id, node_type, label.into(), line, 0, line, 0, "").build()
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

fn parse_attr_labels(attrs: &str) -> Vec<String> {
    attrs
        .split_whitespace()
        .filter_map(|part| {
            let clean = part
                .trim()
                .trim_end_matches('>')
                .trim_end_matches('/')
                .trim();
            if clean.is_empty() {
                return None;
            }
            let (name, value) = clean.split_once('=').unwrap_or((clean, ""));
            let name = name.trim();
            if name.is_empty() {
                return None;
            }
            if value.is_empty() {
                Some(name.to_string())
            } else {
                let value = value
                    .trim()
                    .trim_matches('"')
                    .trim_matches('\'')
                    .trim_matches('{')
                    .trim_matches('}');
                Some(format!("{}={}", name, value))
            }
        })
        .collect()
}

fn parse_tag_segment(segment: &str) -> Option<(String, String)> {
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
    let attrs = trimmed[tag_end..]
        .trim()
        .trim_end_matches('/')
        .trim()
        .to_string();
    Some((tag.to_lowercase(), attrs))
}

fn extract_mustache_labels(line: &str) -> Vec<String> {
    let mut labels = Vec::new();
    let mut rest = line;
    while let Some(start) = rest.find("{{") {
        let after = &rest[start + 2..];
        if let Some(end) = after.find("}}") {
            let label = after[..end].trim();
            if !label.is_empty() {
                labels.push(label.to_string());
            }
            rest = &after[end + 2..];
        } else {
            break;
        }
    }
    labels
}

fn extract_template_children(content: &str, start_line: u32, id_prefix: &str) -> Vec<SemanticNode> {
    let mut children = Vec::new();
    for (line_idx, line) in content.lines().enumerate() {
        let absolute_line = start_line + line_idx as u32;
        let mut rest = line;
        while let Some(open_idx) = rest.find('<') {
            let after_open = &rest[open_idx + 1..];
            if let Some(close_idx) = after_open.find('>') {
                let segment = &after_open[..close_idx];
                if let Some((tag, attrs)) = parse_tag_segment(segment) {
                    let child_id = format!("{}.{}", id_prefix, children.len());
                    let attr_children: Vec<SemanticNode> = parse_attr_labels(&attrs)
                        .into_iter()
                        .enumerate()
                        .map(|(i, label)| {
                            make_leaf(
                                &format!("{}.{}", child_id, i),
                                "attribute",
                                label,
                                absolute_line,
                            )
                        })
                        .collect();
                    children.push(
                        SemanticNodeBuilder::new(
                            &child_id,
                            "element",
                            tag,
                            absolute_line,
                            0,
                            absolute_line,
                            0,
                            "",
                        )
                        .children(attr_children)
                        .build(),
                    );
                }
                rest = &after_open[close_idx + 1..];
            } else {
                break;
            }
        }
        for label in extract_mustache_labels(line) {
            let child_id = format!("{}.{}", id_prefix, children.len());
            children.push(make_leaf(&child_id, "interpolation", label, absolute_line));
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

fn assignment_label(line: &str) -> Option<String> {
    for op in ["+=", "-=", "*=", "/=", "="] {
        if let Some((left, _)) = line.split_once(op) {
            let label = left.trim();
            if !label.is_empty()
                && !label.starts_with("return")
                && !label.starts_with("const ")
                && !label.starts_with("let ")
                && !label.starts_with("var ")
            {
                return Some(label.to_string());
            }
        }
    }
    None
}

fn extract_script_children(content: &str, start_line: u32, id_prefix: &str) -> Vec<SemanticNode> {
    let mut children = Vec::new();
    for (line_idx, raw_line) in content.lines().enumerate() {
        let line = normalize_statement(raw_line);
        if line.is_empty() || line == "{" || line == "}" {
            continue;
        }
        let absolute_line = start_line + line_idx as u32;
        let child_id = format!("{}.{}", id_prefix, children.len());
        let node = if line.starts_with("import ") {
            Some(make_leaf(
                &child_id,
                "import_statement",
                line,
                absolute_line,
            ))
        } else if line.starts_with("export default") {
            Some(make_leaf(
                &child_id,
                "export_default",
                "export default",
                absolute_line,
            ))
        } else if let Some(name) = declaration_name(&line) {
            let mut decl = make_leaf(
                &child_id,
                "variable_declaration",
                name.to_string(),
                absolute_line,
            );
            // #46: the RHS is review content — a value edit (let count = 0 -> 1)
            // hashed style-only with name-only declarations.
            if let Some((_, rhs)) = line.split_once('=') {
                let rhs = rhs.trim().trim_end_matches(';').trim();
                if !rhs.is_empty() {
                    decl.children = vec![make_leaf(
                        &format!("{child_id}.0"),
                        "declaration_value",
                        rhs.to_string(),
                        absolute_line,
                    )];
                }
            }
            Some(decl)
        } else if let Some(name) = function_name(&line) {
            Some(make_leaf(
                &child_id,
                "method_declaration",
                name.to_string(),
                absolute_line,
            ))
        } else if let Some(name) = property_name(&line) {
            let mut prop = make_leaf(
                &child_id,
                "property",
                name.to_string(),
                absolute_line,
            );
            // #46: a data property's VALUE is review content (current: 0 -> 1).
            if let Some((_, rhs)) = line.split_once(':') {
                let rhs = rhs.trim().trim_end_matches(',').trim();
                if !rhs.is_empty() {
                    prop.children = vec![make_leaf(
                        &format!("{child_id}.0"),
                        "property_value",
                        rhs.to_string(),
                        absolute_line,
                    )];
                }
            }
            Some(prop)
        } else {
            assignment_label(&line)
                .map(|label| make_leaf(&child_id, "assignment_statement", label, absolute_line))
        };
        if let Some(node) = node {
            children.push(node);
        }
    }
    children
}

fn extract_style_children(content: &str, start_line: u32, id_prefix: &str) -> Vec<SemanticNode> {
    let mut children = Vec::new();
    for (line_idx, raw_line) in content.lines().enumerate() {
        let line = raw_line.trim();
        if line.is_empty() || line == "}" {
            continue;
        }
        let absolute_line = start_line + line_idx as u32;
        let child_id = format!("{}.{}", id_prefix, children.len());
        if let Some(selector) = line.strip_suffix('{') {
            let selector = selector.trim();
            if !selector.is_empty() {
                children.push(make_leaf(&child_id, "style_rule", selector, absolute_line));
            }
        } else if let Some((property, _)) = line.split_once(':') {
            let property = property.trim();
            if !property.is_empty() {
                children.push(make_leaf(
                    &child_id,
                    "style_declaration",
                    property,
                    absolute_line,
                ));
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
        0,
        block.end_line,
        0,
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
        0,
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
