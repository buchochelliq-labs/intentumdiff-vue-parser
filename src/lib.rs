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
//!
//! Blocks are cut from exact byte slices, so content on the same row as an
//! opening or closing tag is payload. Every outer tag attribute (including
//! `src`) becomes an `attribute` child. Children: template `element`,
//! `attribute`, `text`, `interpolation`; script declarations, properties,
//! assignments and otherwise-unmodeled `statement`s; style `style_rule`,
//! `style_declaration`/`style_value` and `style_statement`; custom-block
//! `block_content`. Layout and comments are excluded from labels.

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
// Source positions
// ---------------------------------------------------------------------------

/// Maps absolute UTF-8 byte offsets to 0-based (row, byte column) pairs.
struct LineIndex {
    starts: Vec<usize>,
}

impl LineIndex {
    fn new(source: &str) -> Self {
        let mut starts = vec![0];
        starts.extend(source.bytes().enumerate().filter(|(_, b)| *b == b'\n').map(|(i, _)| i + 1));
        LineIndex { starts }
    }

    fn pos(&self, offset: usize) -> (u32, u32) {
        let line = match self.starts.binary_search(&offset) {
            Ok(i) => i,
            Err(i) => i.saturating_sub(1),
        };
        (line as u32, (offset - self.starts[line]) as u32)
    }
}

fn leaf_at(
    idx: &LineIndex,
    id: &str,
    node_type: &str,
    label: impl Into<String>,
    start: usize,
    end: usize,
) -> SemanticNode {
    let (sl, sc) = idx.pos(start);
    let (el, ec) = idx.pos(end);
    SemanticNodeBuilder::new(id, node_type, label.into(), sl, sc, el, ec, "").build()
}

/// Collapse whitespace runs outside quoted strings to one space.
fn collapse_ws(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut quote: Option<char> = None;
    let mut escaped = false;
    let mut pending_space = false;
    for c in text.trim().chars() {
        if let Some(q) = quote {
            out.push(c);
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == q {
                quote = None;
            }
            continue;
        }
        if c.is_whitespace() {
            pending_space = true;
            continue;
        }
        if pending_space {
            out.push(' ');
            pending_space = false;
        }
        if matches!(c, '"' | '\'' | '`') {
            quote = Some(c);
        }
        out.push(c);
    }
    out
}

/// Canonical spelling of a JavaScript expression label: insignificant
/// whitespace collapsed and plain single-quoted strings (no escapes, no double
/// quotes) spelled with double quotes, since both spellings denote the same value.
fn canonical_js(text: &str) -> String {
    let collapsed = collapse_ws(text);
    let mut out = String::with_capacity(collapsed.len());
    let mut chars = collapsed.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        match c {
            '\'' => {
                let rest = &collapsed[i + 1..];
                match rest.find('\'') {
                    Some(end) if !rest[..end].contains(['"', '\\']) => {
                        out.push('"');
                        out.push_str(&rest[..end]);
                        out.push('"');
                        let stop = i + 1 + end;
                        while chars.peek().is_some_and(|(j, _)| *j <= stop) {
                            chars.next();
                        }
                    }
                    _ => {
                        // Keep the literal verbatim up to its closing quote.
                        out.push(c);
                        let mut escaped = false;
                        for (_, d) in chars.by_ref() {
                            out.push(d);
                            if escaped {
                                escaped = false;
                            } else if d == '\\' {
                                escaped = true;
                            } else if d == '\'' {
                                break;
                            }
                        }
                    }
                }
            }
            '"' | '`' => {
                out.push(c);
                let mut escaped = false;
                for (_, d) in chars.by_ref() {
                    out.push(d);
                    if escaped {
                        escaped = false;
                    } else if d == '\\' {
                        escaped = true;
                    } else if d == c {
                        break;
                    }
                }
            }
            _ => out.push(c),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Tag scanning
// ---------------------------------------------------------------------------

struct Attr {
    name: String,
    value: Option<String>,
    start: usize,
    end: usize,
}

impl Attr {
    fn label(&self) -> String {
        match &self.value {
            None => self.name.clone(),
            Some(value) => {
                let value = collapse_ws(value);
                let value = value.trim_matches('{').trim_matches('}');
                format!("{}={}", self.name, value)
            }
        }
    }
}

struct OpenTag {
    name: String,
    attrs: Vec<Attr>,
    /// Byte offset just past the closing `>` (or the end of input).
    end: usize,
    self_closing: bool,
}

fn is_name_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b':' | b'.')
}

/// Parse attributes starting at `from` until the tag's `>` (quote-aware).
/// Returns the attributes, the offset just past `>` and whether it was `/>`.
fn parse_attrs(src: &str, from: usize) -> (Vec<Attr>, usize, bool) {
    let b = src.as_bytes();
    let mut attrs = Vec::new();
    let mut i = from;
    loop {
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= b.len() {
            return (attrs, b.len(), false);
        }
        match b[i] {
            b'>' => return (attrs, i + 1, false),
            b'/' if b.get(i + 1) == Some(&b'>') => return (attrs, i + 2, true),
            b'/' => {
                i += 1;
                continue;
            }
            _ => {}
        }
        let start = i;
        while i < b.len()
            && !b[i].is_ascii_whitespace()
            && b[i] != b'='
            && b[i] != b'>'
            && !(b[i] == b'/' && b.get(i + 1) == Some(&b'>'))
        {
            i += 1;
        }
        let name = src[start..i].to_string();
        let mut j = i;
        while j < b.len() && b[j].is_ascii_whitespace() {
            j += 1;
        }
        let mut value = None;
        if j < b.len() && b[j] == b'=' {
            j += 1;
            while j < b.len() && b[j].is_ascii_whitespace() {
                j += 1;
            }
            if j < b.len() && (b[j] == b'"' || b[j] == b'\'') {
                let q = b[j];
                let vstart = j + 1;
                let vend = src[vstart..].find(q as char).map(|k| vstart + k).unwrap_or(b.len());
                value = Some(src[vstart..vend].to_string());
                i = (vend + 1).min(b.len());
            } else {
                let vstart = j;
                while j < b.len() && !b[j].is_ascii_whitespace() && b[j] != b'>' {
                    j += 1;
                }
                value = Some(src[vstart..j].to_string());
                i = j;
            }
        }
        if !name.is_empty() {
            attrs.push(Attr { name, value, start, end: i });
        } else {
            i += 1;
        }
    }
}

/// Scan an opening tag whose `<` is at `lt`.
fn scan_open_tag(src: &str, lt: usize) -> Option<OpenTag> {
    let b = src.as_bytes();
    if b.get(lt) != Some(&b'<') || !b.get(lt + 1).is_some_and(|c| c.is_ascii_alphabetic()) {
        return None;
    }
    let mut i = lt + 1;
    while i < b.len() && is_name_byte(b[i]) {
        i += 1;
    }
    let name = src[lt + 1..i].to_ascii_lowercase();
    let (attrs, end, self_closing) = parse_attrs(src, i);
    Some(OpenTag { name, attrs, end, self_closing })
}

fn starts_with_ci(b: &[u8], at: usize, needle: &str) -> bool {
    b.len() >= at + needle.len() && b[at..at + needle.len()].eq_ignore_ascii_case(needle.as_bytes())
}

/// Is there a `</tag` (followed by whitespace or `>`) at `at`?
fn closing_tag_at(b: &[u8], at: usize, tag: &str) -> bool {
    starts_with_ci(b, at, "</")
        && starts_with_ci(b, at + 2, tag)
        && b.get(at + 2 + tag.len()).is_none_or(|c| c.is_ascii_whitespace() || *c == b'>')
}

fn skip_past(src: &str, from: usize, needle: &str) -> usize {
    src[from..].find(needle).map(|k| from + k + needle.len()).unwrap_or(src.len())
}

/// Find the closing tag for a block opened before `from`. Returns
/// `(close_start, close_end)`. Raw-text blocks end at the first closing tag;
/// HTML blocks track nesting of the same tag name.
fn find_close(src: &str, from: usize, tag: &str, raw: bool) -> Option<(usize, usize)> {
    let b = src.as_bytes();
    let mut depth = 1usize;
    let mut i = from;
    while let Some(k) = src[i..].find('<') {
        let lt = i + k;
        if closing_tag_at(b, lt, tag) {
            depth -= 1;
            if raw || depth == 0 {
                return Some((lt, skip_past(src, lt, ">")));
            }
            i = lt + 2;
        } else if raw {
            i = lt + 1;
        } else if src[lt..].starts_with("<!--") {
            i = skip_past(src, lt, "-->");
        } else if let Some(open) = scan_open_tag(src, lt) {
            if open.name == tag && !open.self_closing {
                depth += 1;
            }
            i = open.end;
        } else {
            i = lt + 1;
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Block extraction
// ---------------------------------------------------------------------------

struct SfcBlock {
    node_type: &'static str,
    label: String,
    start: usize,
    end: usize,
    attrs: Vec<Attr>,
    content_start: usize,
    content_end: usize,
    /// Content hash derived from the exact tag attributes and block payload.
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

fn label_from_attrs(tag: &str, attrs: &[Attr]) -> String {
    let has = |key: &str| attrs.iter().any(|a| a.name.eq_ignore_ascii_case(key));
    let mut parts: Vec<String> = vec![tag.to_string()];
    if has("setup") {
        parts.push("setup".to_string());
    }
    if has("scoped") {
        parts.push("scoped".to_string());
    }
    if let Some(lang) = attrs
        .iter()
        .find(|a| a.name.eq_ignore_ascii_case("lang"))
        .and_then(|a| a.value.as_ref())
    {
        parts.push(lang.trim().to_lowercase());
    }
    parts.join(":")
}

#[cfg(test)]
fn build_label(tag: &str, attrs: &str) -> String {
    let (attrs, _, _) = parse_attrs(attrs, 0);
    label_from_attrs(tag, &attrs)
}

/// Extract top-level SFC blocks from exact byte slices: content on the same
/// row as the opening or closing tag is part of the block payload.
fn extract_blocks(source: &str) -> Vec<SfcBlock> {
    let mut blocks = Vec::new();
    let mut i = 0;
    while let Some(k) = source[i..].find('<') {
        let lt = i + k;
        if source[lt..].starts_with("<!--") {
            i = skip_past(source, lt, "-->");
            continue;
        }
        let Some(open) = scan_open_tag(source, lt) else {
            i = if matches!(source.as_bytes().get(lt + 1), Some(b'/' | b'!' | b'?')) {
                skip_past(source, lt, ">")
            } else {
                lt + 1
            };
            continue;
        };
        let (content_start, content_end, end) = if open.self_closing {
            (open.end, open.end, open.end)
        } else {
            match find_close(source, open.end, &open.name, open.name != "template") {
                Some((cs, ce)) => (open.end, cs, ce),
                None => (open.end, source.len(), source.len()),
            }
        };
        let node_type: &'static str = match open.name.as_str() {
            "template" => "template_block",
            "script" => "script_block",
            "style" => "style_block",
            _ => "custom_block",
        };
        let label = label_from_attrs(&open.name, &open.attrs);
        let hash = content_hash(&format!(
            "{}\u{0}{}",
            &source[lt..content_start],
            &source[content_start..content_end]
        ));
        blocks.push(SfcBlock {
            node_type,
            label,
            start: lt,
            end,
            attrs: open.attrs,
            content_start,
            content_end,
            content_hash: hash,
        });
        i = end.max(lt + 1);
    }
    blocks
}

// ---------------------------------------------------------------------------
// Template
// ---------------------------------------------------------------------------

fn attr_nodes(idx: &LineIndex, attrs: &[Attr], prefix: &str, first: usize) -> Vec<SemanticNode> {
    attrs
        .iter()
        .enumerate()
        .map(|(n, a)| leaf_at(idx, &format!("{}.{}", prefix, first + n), "attribute", a.label(), a.start, a.end))
        .collect()
}

fn push_text(
    src: &str,
    idx: &LineIndex,
    from: usize,
    to: usize,
    prefix: &str,
    children: &mut Vec<SemanticNode>,
) {
    if from >= to {
        return;
    }
    let raw = &src[from..to];
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return;
    }
    let start = from + (raw.len() - raw.trim_start().len());
    let id = format!("{}.{}", prefix, children.len());
    children.push(leaf_at(idx, &id, "text", collapse_ws(trimmed), start, start + trimmed.len()));
}

fn extract_template_children(
    src: &str,
    idx: &LineIndex,
    cs: usize,
    ce: usize,
    prefix: &str,
    children: &mut Vec<SemanticNode>,
) {
    let scope = &src[..ce];
    let b = scope.as_bytes();
    let mut i = cs;
    let mut text_start = cs;
    while i < ce {
        if scope[i..].starts_with("{{") {
            let Some(rel) = scope[i + 2..].find("}}") else { break };
            push_text(scope, idx, text_start, i, prefix, children);
            let inner_start = i + 2;
            let inner_end = inner_start + rel;
            let raw = &scope[inner_start..inner_end];
            let label = raw.trim();
            if !label.is_empty() {
                let start = inner_start + (raw.len() - raw.trim_start().len());
                let id = format!("{}.{}", prefix, children.len());
                children.push(leaf_at(idx, &id, "interpolation", canonical_js(label), start, start + label.len()));
            }
            i = inner_end + 2;
            text_start = i;
            continue;
        }
        if b[i] == b'<' {
            if scope[i..].starts_with("<!--") {
                push_text(scope, idx, text_start, i, prefix, children);
                i = skip_past(scope, i, "-->");
                text_start = i;
                continue;
            }
            if b.get(i + 1) == Some(&b'/') {
                push_text(scope, idx, text_start, i, prefix, children);
                i = skip_past(scope, i, ">");
                text_start = i;
                continue;
            }
            if let Some(open) = scan_open_tag(scope, i) {
                push_text(scope, idx, text_start, i, prefix, children);
                let id = format!("{}.{}", prefix, children.len());
                let attrs = attr_nodes(idx, &open.attrs, &id, 0);
                let (sl, sc) = idx.pos(i);
                let (el, ec) = idx.pos(open.end);
                children.push(
                    SemanticNodeBuilder::new(&id, "element", open.name.clone(), sl, sc, el, ec, "")
                        .children(attrs)
                        .build(),
                );
                i = open.end;
                text_start = i;
                continue;
            }
        }
        i += scope[i..].chars().next().map_or(1, char::len_utf8);
    }
    push_text(scope, idx, text_start, ce, prefix, children);
}

// ---------------------------------------------------------------------------
// Script
// ---------------------------------------------------------------------------

/// Whether a `/` following `prefix` starts a regular-expression literal rather
/// than a division: at the start, after an operator or opening punctuator, or
/// after a keyword such as `return`/`typeof` (never after an identifier,
/// number or closing bracket).
fn regex_allowed(prefix: &[u8]) -> bool {
    let prefix = prefix.trim_ascii_end();
    let Some(&last) = prefix.last() else {
        return true;
    };
    if b"(,=:[!&|?{};+-*%<>~^".contains(&last) {
        return true;
    }
    let word_start = prefix
        .iter()
        .rposition(|c| !(c.is_ascii_alphanumeric() || *c == b'_' || *c == b'$'))
        .map_or(0, |p| p + 1);
    matches!(
        &prefix[word_start..],
        b"return" | b"typeof" | b"instanceof" | b"in" | b"of" | b"new" | b"delete" | b"void"
            | b"throw" | b"case" | b"do" | b"else" | b"yield" | b"await"
    )
}

/// End (exclusive, after flags) of the regular-expression literal starting at
/// the `/` at `i`, or `None` when that `/` is a division, a comment opener, or
/// an unterminated pattern. Escapes and `[...]` classes may contain `/`.
fn regex_end(b: &[u8], i: usize) -> Option<usize> {
    if matches!(b.get(i + 1), Some(b'/' | b'*')) || !regex_allowed(&b[..i]) {
        return None;
    }
    let mut in_class = false;
    let mut j = i + 1;
    while j < b.len() {
        match b[j] {
            b'\\' => j += 1,
            b'[' => in_class = true,
            b']' => in_class = false,
            b'/' if !in_class => {
                j += 1;
                while j < b.len() && b[j].is_ascii_alphabetic() {
                    j += 1;
                }
                return Some(j);
            }
            b'\n' => return None,
            _ => {}
        }
        j += 1;
    }
    None
}

/// Split one script row into statements at top-level `;`, dropping a trailing
/// `//` comment. String and regex literals are opaque. Returns
/// `(offset_in_row, text)` pairs.
fn script_segments(line: &str) -> Vec<(usize, &str)> {
    let b = line.as_bytes();
    let mut out = Vec::new();
    let mut quote: Option<u8> = None;
    let mut escaped = false;
    let mut depth = 0usize;
    let mut start = 0usize;
    let mut end = line.len();
    let mut i = 0usize;
    while i < b.len() {
        let c = b[i];
        if let Some(q) = quote {
            if escaped {
                escaped = false;
            } else if c == b'\\' {
                escaped = true;
            } else if c == q {
                quote = None;
            }
            i += 1;
            continue;
        }
        match c {
            b'\'' | b'"' | b'`' => quote = Some(c),
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => depth = depth.saturating_sub(1),
            b'/' if b.get(i + 1) == Some(&b'/') => {
                end = i;
                break;
            }
            b'/' => {
                if let Some(j) = regex_end(b, i) {
                    i = j;
                    continue;
                }
            }
            b';' if depth == 0 => {
                out.push((start, &line[start..i]));
                start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    if start < end {
        out.push((start, &line[start..end]));
    }
    out
}

fn value_child(
    idx: &LineIndex,
    id: &str,
    node_type: &str,
    line: &str,
    abs: usize,
    op_end: usize,
) -> Option<SemanticNode> {
    let remainder = &line[op_end..];
    let rhs = remainder.trim();
    if rhs.is_empty() {
        return None;
    }
    let start = abs + op_end + (remainder.len() - remainder.trim_start().len());
    Some(leaf_at(idx, id, node_type, canonical_js(rhs), start, start + rhs.len()))
}

fn classify_statement(idx: &LineIndex, id: &str, line: &str, abs: usize) -> SemanticNode {
    let end = abs + line.len();
    let child_id = format!("{id}.0");
    if line.starts_with("import ") {
        return leaf_at(idx, id, "import_statement", canonical_js(line), abs, end);
    }
    if line.starts_with("export default") {
        let mut node = leaf_at(idx, id, "export_default", "export default", abs, end);
        let rest = line.strip_prefix("export default").unwrap_or_default().trim();
        if !rest.is_empty() && rest != "{" {
            node.children = value_child(idx, &child_id, "export_value", line, abs, "export default".len())
                .into_iter()
                .collect();
        }
        return node;
    }
    if let Some(name) = declaration_name(line) {
        let mut decl = leaf_at(idx, id, "variable_declaration", name.to_string(), abs, end);
        if let Some(equals) = line.find('=') {
            decl.children = value_child(idx, &child_id, "declaration_value", line, abs, equals + 1)
                .into_iter()
                .collect();
        }
        return decl;
    }
    if let Some(name) = function_name(line) {
        return leaf_at(idx, id, "method_declaration", name.to_string(), abs, end);
    }
    if let Some(name) = property_name(line) {
        let mut prop = leaf_at(idx, id, "property", name.to_string(), abs, end);
        if let Some(colon) = line.find(':') {
            prop.children = value_child(idx, &child_id, "property_value", line, abs, colon + 1)
                .into_iter()
                .collect();
        }
        return prop;
    }
    if let Some(label) = assignment_label(line) {
        let mut assignment = leaf_at(idx, id, "assignment_statement", label, abs, end);
        if let Some((index, op_len)) = outer_assignment_operator(line) {
            assignment.children = value_child(idx, &child_id, "assignment_value", line, abs, index + op_len)
                .into_iter()
                .collect();
        }
        return assignment;
    }
    // Unmodeled statements are retained verbatim (canonically spelled) rather
    // than certified away as layout.
    leaf_at(idx, id, "statement", canonical_js(line), abs, end)
}

fn extract_script_children(
    src: &str,
    idx: &LineIndex,
    cs: usize,
    ce: usize,
    prefix: &str,
    children: &mut Vec<SemanticNode>,
) {
    let mut row_start = cs;
    let mut in_block_comment = false;
    for raw_row in src[cs..ce].split('\n') {
        let row_off = row_start;
        row_start += raw_row.len() + 1;
        let row = raw_row.strip_suffix('\r').unwrap_or(raw_row);
        let trimmed = row.trim();
        if in_block_comment {
            if trimmed.contains("*/") {
                in_block_comment = false;
            }
            continue;
        }
        if trimmed.starts_with("/*") {
            in_block_comment = !trimmed.contains("*/");
            continue;
        }
        for (seg_off, segment) in script_segments(row) {
            let line = normalize_statement(segment);
            if line.is_empty() || line.bytes().all(|c| matches!(c, b'{' | b'}' | b'(' | b')' | b'[' | b']' | b',' | b';' | b' ')) {
                continue;
            }
            let Some(leading) = segment.find(&line) else { continue };
            let id = format!("{}.{}", prefix, children.len());
            children.push(classify_statement(idx, &id, &line, row_off + seg_off + leading));
        }
    }
}

// ---------------------------------------------------------------------------
// Style
// ---------------------------------------------------------------------------

fn skip_css_trivia(src: &str, mut i: usize, ce: usize) -> usize {
    let b = src.as_bytes();
    loop {
        while i < ce && b[i].is_ascii_whitespace() {
            i += 1;
        }
        if i + 1 < ce && b[i] == b'/' && b[i + 1] == b'*' {
            i = skip_past(&src[..ce], i + 2, "*/");
            continue;
        }
        return i;
    }
}

/// End of the next top-level CSS segment: the first `;`, `{` or `}` outside
/// quotes and parentheses/brackets.
fn css_segment_end(src: &str, from: usize, ce: usize) -> usize {
    let b = src.as_bytes();
    let mut quote: Option<u8> = None;
    let mut escaped = false;
    let mut depth = 0usize;
    let mut i = from;
    while i < ce {
        let c = b[i];
        if let Some(q) = quote {
            if escaped {
                escaped = false;
            } else if c == b'\\' {
                escaped = true;
            } else if c == q {
                quote = None;
            }
        } else {
            match c {
                b'\'' | b'"' => quote = Some(c),
                b'(' | b'[' => depth += 1,
                b')' | b']' => depth = depth.saturating_sub(1),
                b';' | b'{' | b'}' if depth == 0 => return i,
                _ => {}
            }
        }
        i += 1;
    }
    ce
}

fn extract_style_children(
    src: &str,
    idx: &LineIndex,
    cs: usize,
    ce: usize,
    prefix: &str,
    children: &mut Vec<SemanticNode>,
) {
    let b = src.as_bytes();
    let mut i = cs;
    loop {
        i = skip_css_trivia(src, i, ce);
        if i >= ce {
            break;
        }
        if b[i] == b'}' || b[i] == b';' {
            i += 1;
            continue;
        }
        let seg_end = css_segment_end(src, i, ce);
        let segment = src[i..seg_end].trim_end();
        let id = format!("{}.{}", prefix, children.len());
        let custom_property = segment.starts_with("--");
        if seg_end < ce && b[seg_end] == b'{' && !custom_property {
            let selector = collapse_ws(segment);
            if !selector.is_empty() {
                children.push(leaf_at(idx, &id, "style_rule", selector, i, seg_end + 1));
            }
            i = seg_end + 1;
            continue;
        }
        let Some(colon) = segment.find(':') else {
            // At-rule statements (`@import ...;`) or unmodeled text: keep verbatim.
            if !segment.is_empty() {
                children.push(leaf_at(idx, &id, "style_statement", collapse_ws(segment), i, i + segment.len()));
            }
            i = seg_end.max(i + 1);
            continue;
        };
        let property = segment[..colon].trim();
        let value_from = i + colon + 1;
        let value = css_declaration_value(&src[value_from..ce]);
        let value_start = value.as_ptr() as usize - src.as_ptr() as usize;
        let value_end = if value.is_empty() { value_from } else { value_start + value.len() };
        let mut decl_end = value_end;
        let after = skip_css_trivia(src, value_end, ce);
        let mut next = value_end;
        if after < ce && b[after] == b';' {
            decl_end = after + 1;
            next = after + 1;
        }
        if !property.is_empty() {
            let mut decl = leaf_at(idx, &id, "style_declaration", property, i, decl_end);
            if !value.is_empty() {
                decl.children = vec![leaf_at(idx, &format!("{id}.0"), "style_value", collapse_ws(value), value_start, value_end)];
            }
            children.push(decl);
        }
        i = next.max(i + 1);
    }
}

fn sfc_block_to_node(source: &str, idx: &LineIndex, block: &SfcBlock, id: &str) -> SemanticNode {
    // Every outer attribute (src, scoped, lang, setup, module, ...) is a
    // semantic fact of the block: `src` changes the loaded resource.
    let mut children = attr_nodes(idx, &block.attrs, id, 0);
    let (cs, ce) = (block.content_start, block.content_end);
    match block.node_type {
        "template_block" => extract_template_children(source, idx, cs, ce, id, &mut children),
        "script_block" => extract_script_children(source, idx, cs, ce, id, &mut children),
        "style_block" => extract_style_children(source, idx, cs, ce, id, &mut children),
        _ => push_text(source, idx, cs, ce, id, &mut children),
    }
    if block.node_type == "custom_block" {
        if let Some(last) = children.last_mut().filter(|n| n.node_type == "text") {
            last.node_type = "block_content".to_string();
        }
    }
    let (sl, sc) = idx.pos(block.start);
    let (el, ec) = idx.pos(block.end);
    SemanticNodeBuilder::new(id, block.node_type, block.label.clone(), sl, sc, el, ec, block.content_hash.clone())
        .children(children)
        .build()
}

fn process_impl(source: &str, filename: &str) -> String {
    let stem = filename
        .rsplit(['/', '\\'])
        .next()
        .and_then(|f| f.rsplit('.').nth(1))
        .unwrap_or("component");

    let idx = LineIndex::new(source);
    let blocks = extract_blocks(source);
    let end_line = source.lines().count().saturating_sub(1) as u32;
    let end_col = source.lines().last().map(|line| line.len() as u32).unwrap_or(0);

    let children: Vec<SemanticNode> = blocks
        .iter()
        .enumerate()
        .map(|(i, b)| sfc_block_to_node(source, &idx, b, &format!("0.{}", i)))
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
                if bytes.get(index + 1).is_some_and(|b| matches!(*b, b'=' | b'>'))
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
            ("declaration_value", "\"é\"", 4usize, "'é'"),
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
    // --- buchochelliq-labs/intentumdiff-vue-parser#6 regressions -------------

    fn tree(source: &str) -> serde_json::Value {
        serde_json::from_str(&process_impl(source, "a.vue")).expect("valid JSON")
    }

    fn find_node<'a>(node: &'a serde_json::Value, kind: &str, label: &str) -> Option<&'a serde_json::Value> {
        if node["node_type"] == kind && node["label"] == label {
            return Some(node);
        }
        node["children"].as_array()?.iter().find_map(|child| find_node(child, kind, label))
    }

    /// Projection the core compares: node types and labels, without hashes/spans.
    fn projection(node: &serde_json::Value) -> String {
        let children: Vec<String> = node["children"].as_array().unwrap().iter().map(projection).collect();
        format!("{}:{}[{}]", node["node_type"].as_str().unwrap(), node["label"].as_str().unwrap(), children.join(","))
    }

    /// Assert the leaf exists with label `label` and covers exactly `needle`
    /// (first occurrence) in `source`, in 0-based rows and UTF-8 byte columns.
    fn assert_span(source: &str, kind: &str, label: &str, needle: &str) {
        assert_span_after(source, kind, label, "", needle);
    }

    /// Like `assert_span`, for the first `needle` after the first `anchor`.
    fn assert_span_after(source: &str, kind: &str, label: &str, anchor: &str, needle: &str) {
        let t = tree(source);
        let node = find_node(&t, kind, label).unwrap_or_else(|| panic!("missing {kind} {label:?} in {}", projection(&t)));
        let from = source.find(anchor).expect("anchor in source") + anchor.len();
        let offset = from + source[from..].find(needle).expect("needle in source");
        let row = source[..offset].matches('\n').count();
        let col = offset - source[..offset].rfind('\n').map_or(0, |i| i + 1);
        assert_eq!(node["position"]["start_line"], row as u32, "{kind} {label}");
        assert_eq!(node["position"]["start_col"], col as u32, "{kind} {label}");
        let end = offset + needle.len();
        let end_row = source[..end].matches('\n').count();
        let end_col = end - source[..end].rfind('\n').map_or(0, |i| i + 1);
        assert_eq!(node["position"]["end_line"], end_row as u32, "{kind} {label}");
        assert_eq!(node["position"]["end_col"], end_col as u32, "{kind} {label}");
    }

    fn assert_changed(old: &str, new: &str) {
        assert_ne!(projection(&tree(old)), projection(&tree(new)), "semantic payload must differ:\n{old}\n{new}");
    }

    fn assert_preserved(old: &str, new: &str) {
        assert_eq!(projection(&tree(old)), projection(&tree(new)), "projection must be equal:\n{old}\n{new}");
    }

    #[test]
    fn same_line_script_content_is_retained() {
        let old = "<script>const x = 1;</script>\n<template><div>{{ x }}</div></template>\n";
        let new = "<script>const x = 2;</script>\n<template><div>{{ x }}</div></template>\n";
        assert_changed(old, new);
        assert_span(old, "declaration_value", "1", "1");
        assert_span(new, "declaration_value", "2", "2");
        assert_span_after(old, "interpolation", "x", "{{", "x");
        let old_setup = "<script setup>const title = \"Old\";</script>\n<template><h1>{{ title }}</h1></template>\n";
        let new_setup = "<script setup>const title = \"New\";</script>\n<template><h1>{{ title }}</h1></template>\n";
        assert_changed(old_setup, new_setup);
        assert_span(new_setup, "declaration_value", "\"New\"", "\"New\"");
    }

    #[test]
    fn same_line_template_text_is_retained() {
        let old = "<template><div>safe</div></template>\n";
        let new = "<template><div>danger</div></template>\n";
        assert_changed(old, new);
        assert_span(old, "text", "safe", "safe");
        assert_span(new, "text", "danger", "danger");
        assert_span(new, "element", "div", "<div>");
    }

    #[test]
    fn template_first_line_content_is_retained() {
        let old = "<template><span>safe</span>\n<div>tail</div>\n</template>\n";
        let new = "<template><span>danger</span>\n<div>tail</div>\n</template>\n";
        assert_changed(old, new);
        assert_span(new, "text", "danger", "danger");
        assert_span(new, "text", "tail", "tail");
    }

    #[test]
    fn multibyte_template_text_has_utf8_byte_spans() {
        let old = "<template><p>Café: 5 €</p></template>\n";
        let new = "<template><p>Café: 6 €</p></template>\n";
        assert_changed(old, new);
        assert_span(old, "text", "Café: 5 €", "Café: 5 €");
        assert_span(new, "text", "Café: 6 €", "Café: 6 €");
    }

    #[test]
    fn outer_src_attributes_are_semantic() {
        for (old, new, old_label, new_label, needle_old, needle_new) in [
            ("<script src=\"./safe.js\"></script>\n", "<script src=\"./danger.js\"></script>\n",
             "src=./safe.js", "src=./danger.js", "src=\"./safe.js\"", "src=\"./danger.js\""),
            ("<style src=\"./safe.css\"></style>\n", "<style src=\"./danger.css\"></style>\n",
             "src=./safe.css", "src=./danger.css", "src=\"./safe.css\"", "src=\"./danger.css\""),
            ("<script src='./a.js' />\n", "<script src='./b.js' />\n",
             "src=./a.js", "src=./b.js", "src='./a.js'", "src='./b.js'"),
        ] {
            assert_changed(old, new);
            assert_span(old, "attribute", old_label, needle_old);
            assert_span(new, "attribute", new_label, needle_new);
        }
        // Quote spelling of the attribute value is not semantic.
        assert_preserved("<script src=\"./a.js\"></script>\n", "<script src='./a.js'></script>\n");
    }

    #[test]
    fn same_line_style_scope_and_declarations_are_retained() {
        let plain = "<template><h1>Ready</h1></template>\n<style>h1 { color: red; }</style>\n";
        let scoped = "<template><h1>Ready</h1></template>\n<style scoped>h1 { color: red; }</style>\n";
        assert_changed(plain, scoped);
        let t = tree(scoped);
        assert!(find_node(&t, "style_block", "style:scoped").is_some(), "{}", projection(&t));
        assert_span(scoped, "attribute", "scoped", "scoped");
        let red = "<style scoped>h1 { color: red; }</style>\n";
        let blue = "<style scoped>h1 { color: blue; }</style>\n";
        assert_changed(red, blue);
        assert_span(red, "style_rule", "h1", "h1 {");
        assert_span(red, "style_declaration", "color", "color: red;");
        assert_span(blue, "style_value", "blue", "blue");
    }

    #[test]
    fn css_layout_and_comments_are_not_semantic() {
        assert_preserved(
            "<style scoped>h1 { color: red; }</style>\n",
            "<style scoped>h1 {\n  color: red;\n}</style>\n",
        );
        assert_preserved(
            "<style>\nh1 { color: red; }\n</style>\n",
            "<style>\n/* heading */\nh1   {  color :  red }\n</style>\n",
        );
        // A comment or a delimiter inside a quoted value is data.
        assert_changed(
            "<style>a::after { content: \"/* x */\"; }</style>",
            "<style>a::after { content: \"/* y */\"; }</style>",
        );
        assert_changed(
            "<style>a::after { content: \"}\"; color: red; }</style>",
            "<style>a::after { content: \"}\"; color: blue; }</style>",
        );
    }

    #[test]
    fn script_layout_comments_and_quote_spelling_are_not_semantic() {
        assert_preserved(
            "<script setup>const title = 'Ready';</script>\n",
            "<script setup>const title = \"Ready\";</script>\n",
        );
        assert_preserved(
            "<script>\nconst x = 1;\n</script>\n",
            "<script>\n// note\nconst x = 1; // trailing\n/* block\n comment */\n</script>\n",
        );
        assert_preserved(
            "<script>const x = 1;</script>\n",
            "<script>\n  const x = 1;\n</script>\n",
        );
        // Delimiters inside strings do not split or truncate statements.
        assert_changed(
            "<script>const s = \"a;b // c\";</script>",
            "<script>const s = \"a;b // d\";</script>",
        );
        // Unmodeled statements are retained, not certified as layout.
        assert_changed("<script>foo(1);</script>", "<script>foo(2);</script>");
    }

    #[test]
    fn template_layout_and_comments_are_not_semantic() {
        assert_preserved(
            "<template><div>safe</div></template>\n",
            "<template>\n  <div>\n    safe\n  </div>\n</template>\n",
        );
        assert_preserved(
            "<template><div>safe</div></template>\n",
            "<template><!-- c --><div>safe</div></template>\n",
        );
        // `>` inside a quoted attribute value does not end the tag.
        assert_changed(
            "<template><div :title=\"a > b\">x</div></template>",
            "<template><div :title=\"a > c\">x</div></template>",
        );
    }

    #[test]
    fn identity_and_incomplete_input_are_bounded() {
        let source = "<script setup>const title = \"Ready\";</script>\n<template><h1>{{ title }}</h1></template>\n";
        assert_eq!(process_impl(source, "a.vue"), process_impl(source, "a.vue"));
        for src in ["<template><div>{{", "<template><span>{{", "<script", "<style>h1 {", "<template><div title=\"", "<", "</template>", "<docs>é"] {
            serde_json::from_str::<serde_json::Value>(&process_impl(src, "a.vue")).expect("valid JSON");
        }
    }

    #[test]
    fn custom_block_content_is_retained() {
        assert_changed("<docs>old</docs>\n", "<docs>new</docs>\n");
        assert_span("<docs>\n# Title\n</docs>\n", "block_content", "# Title", "# Title");
    }

    // --- buchochelliq-labs/intentumdiff-vue-parser#8 ---------------------------

    #[test]
    fn regex_literal_slashes_are_not_comments() {
        let old = "<script>\nconst re = /[//]foo/;\n</script>\n";
        let new = "<script>\nconst re = /[//]bar/;\n</script>\n";
        assert_span(old, "declaration_value", "/[//]foo/", "/[//]foo/");
        assert_span(new, "declaration_value", "/[//]bar/", "/[//]bar/");
        assert_changed(old, new);
        // Escaped slash, `;` inside the pattern, flags, and a real trailing comment.
        let src = "<script>\nconst a = /a\\/\\/b;c/g; // note\nconst b = x / 2; // half\n</script>\n";
        assert_span(src, "declaration_value", "/a\\/\\/b;c/g", "/a\\/\\/b;c/g");
        assert_span(src, "declaration_value", "x / 2", "x / 2");
        assert_preserved(src, "<script>\nconst a = /a\\/\\/b;c/g;\nconst b = x / 2;\n</script>\n");
        assert_preserved(
            "<script>\nconst re = /[//]foo/; // one\n</script>\n",
            "<script>\nconst re = /[//]foo/; // two\n</script>\n",
        );
    }
}
