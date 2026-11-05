// SPDX-License-Identifier: GPL-3.0-or-later
//! The parts of LSP that don't touch the editor: message framing, file
//! URIs, and UTF-16 columns. Unit-tested natively.

use serde_json::Value;

/// Frame a JSON-RPC message: `Content-Length` header and body.
pub fn frame(msg: &Value) -> Vec<u8> {
    let body = msg.to_string();
    let mut out = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
    out.extend_from_slice(body.as_bytes());
    out
}

/// Take the complete messages off the front of `input`, leaving a partial
/// one. Headers without a length and bodies that aren't JSON are skipped
/// (the error is returned alongside).
pub fn unframe(input: &mut Vec<u8>) -> (Vec<Value>, Vec<String>) {
    let (mut msgs, mut errors) = (Vec::new(), Vec::new());
    while let Some(end) = input.windows(4).position(|w| w == b"\r\n\r\n") {
        let header = String::from_utf8_lossy(&input[..end]).to_string();
        let len = header.lines().find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.trim()
                .eq_ignore_ascii_case("content-length")
                .then(|| v.trim().parse::<usize>().ok())
                .flatten()
        });
        let body_start = end + 4;
        let Some(len) = len else {
            input.drain(..body_start);
            errors.push(format!("a header without Content-Length: {header:?}"));
            continue;
        };
        if input.len() < body_start + len {
            break;
        }
        let body: Vec<u8> = input.drain(..body_start + len).skip(body_start).collect();
        match serde_json::from_slice(&body) {
            Ok(v) => msgs.push(v),
            Err(e) => errors.push(format!("bad message: {e}")),
        }
    }
    (msgs, errors)
}

/// `file://` URI for a path (percent-encoding what needs it).
pub fn uri_of(path: &str) -> String {
    let mut out = String::from("file://");
    for b in path.bytes() {
        if b.is_ascii_alphanumeric() || b"/-._~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// The path of a `file://` URI.
pub fn path_of(uri: &str) -> String {
    let p = uri.strip_prefix("file://").unwrap_or(uri).as_bytes();
    let mut out = Vec::with_capacity(p.len());
    let mut i = 0;
    while i < p.len() {
        if p[i] == b'%'
            && i + 2 < p.len()
            && let Ok(b) =
                u8::from_str_radix(std::str::from_utf8(&p[i + 1..i + 3]).unwrap_or("zz"), 16)
        {
            out.push(b);
            i += 3;
            continue;
        }
        out.push(p[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// LSP columns count UTF-16 code units; Forge's count characters.
pub fn utf16_col(line: &str, col: u32) -> u32 {
    line.chars()
        .take(col as usize)
        .map(|c| c.len_utf16() as u32)
        .sum()
}

pub fn char_col(line: &str, units: u32) -> u32 {
    let mut u = 0;
    let mut n = 0;
    for c in line.chars() {
        if u >= units {
            break;
        }
        u += c.len_utf16() as u32;
        n += 1;
    }
    n
}

/// The text of a hover result's `contents` (string, MarkupContent,
/// MarkedString or a list of them), up to three non-code lines joined.
pub fn hover_summary(contents: &Value) -> String {
    fn text(v: &Value) -> String {
        match v {
            Value::String(s) => s.clone(),
            Value::Object(o) => o
                .get("value")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            Value::Array(a) => a.iter().map(text).collect::<Vec<_>>().join("\n"),
            _ => String::new(),
        }
    }
    text(contents)
        .lines()
        .filter(|l| !l.starts_with("```") && !l.trim().is_empty())
        .take(3)
        .collect::<Vec<_>>()
        .join("  ")
}

/// A place in a file: LSP counts UTF-16 units, Forge counts characters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LspPos {
    pub line: u32,
    pub character: u32,
}

/// A `Location`, or a `LocationLink`'s target (which has a different shape
/// for the same thing).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LspLocation {
    pub uri: String,
    pub line: u32,
    pub character: u32,
    /// How the range is spelled, so a caller's reply can echo it back.
    pub start: LspPos,
    pub end: LspPos,
}

/// Read a `Location` (uri/range) or a `LocationLink`
/// (targetUri/targetSelectionRange).
pub fn location(v: &Value) -> Option<LspLocation> {
    let (uri, range) = match v.get("uri").and_then(Value::as_str) {
        Some(uri) => (uri.to_string(), v.get("range")?),
        None => (
            v.get("targetUri")?.as_str()?.to_string(),
            v.get("targetSelectionRange")
                .or_else(|| v.get("targetRange"))?,
        ),
    };
    let (start, end) = range_of(range)?;
    Some(LspLocation {
        line: start.line,
        character: start.character,
        start,
        end,
        uri,
    })
}

/// Where a `workspace/symbol` result says the symbol named `name` is
/// defined (the first match). Its items are `SymbolInformation`s (a nested
/// `location`), or a bare location for a `DocumentSymbol`.
pub fn symbol_named(result: &Value, name: &str) -> Option<LspLocation> {
    result
        .as_array()?
        .iter()
        .find(|v| v["name"].as_str() == Some(name))
        .and_then(|v| location(v.get("location").unwrap_or(v)))
}

/// The `start` and `end` of an LSP `Range`.
fn range_of(v: &Value) -> Option<(LspPos, LspPos)> {
    Some((pos_of(v.get("start")?)?, pos_of(v.get("end")?)?))
}

fn pos_of(v: &Value) -> Option<LspPos> {
    Some(LspPos {
        line: v["line"].as_u64()? as u32,
        character: v["character"].as_u64()? as u32,
    })
}

/// Every location in a result that may be one location, a list of them, or
/// a list of `LocationLink`s.
pub fn locations(result: &Value) -> Vec<LspLocation> {
    let items = match result {
        Value::Array(a) => a.iter().collect::<Vec<_>>(),
        Value::Null => Vec::new(),
        one => vec![one],
    };
    items.into_iter().filter_map(location).collect()
}

/// One text edit: replace `start`..`end` with `text`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LspEdit {
    pub start: LspPos,
    pub end: LspPos,
    pub text: String,
}

/// The `changes` of a `WorkspaceEdit`, flattened across files.
///
/// Servers spell this three ways: `changes` (uri -> edits),
/// `documentChanges` (TextDocumentEdit with edits), or nothing at all. The
/// `documentChanges` entries that aren't text edits (a file rename, say)
/// are skipped: a plugin can't do those through the buffer API.
pub fn workspace_edits(edit: &Value) -> Vec<(String, Vec<LspEdit>)> {
    let mut out = Vec::new();
    if let Some(changes) = edit.get("changes").and_then(Value::as_object) {
        for (uri, edits) in changes {
            out.push((uri.clone(), edits_of(edits)));
        }
    }
    if let Some(docs) = edit.get("documentChanges").and_then(Value::as_array) {
        for doc in docs {
            let Some(uri) = doc
                .get("textDocument")
                .and_then(|d| d.get("uri"))
                .and_then(Value::as_str)
            else {
                continue;
            };
            if let Some(edits) = doc.get("edits") {
                out.push((uri.to_string(), edits_of(edits)));
            }
        }
    }
    out
}

fn edits_of(v: &Value) -> Vec<LspEdit> {
    v.as_array()
        .into_iter()
        .flatten()
        .filter(|e| e.get("newText").is_some())
        .filter_map(|e| {
            let (start, end) = range_of(e.get("range")?)?;
            Some(LspEdit {
                start,
                end,
                text: e["newText"].as_str()?.to_string(),
            })
        })
        .collect()
}

/// The smallest replacement turning `old` into `new`: the shared prefix
/// and suffix are left alone. This is what makes `didChange` incremental.
///
/// The range is in `old`, as LSP wants it: a line and a UTF-16 column at
/// each end (the server converts nothing; columns are code units).
pub fn minimal_edit(old: &str, new: &str) -> Option<LspEdit> {
    if old == new {
        return None;
    }
    let (o, n): (Vec<char>, Vec<char>) = (old.chars().collect(), new.chars().collect());
    let prefix = o.iter().zip(n.iter()).take_while(|(a, b)| a == b).count();
    let suffix = o[prefix..]
        .iter()
        .rev()
        .zip(n[prefix..].iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    Some(LspEdit {
        start: lsp_pos_at(&o, prefix),
        end: lsp_pos_at(&o, o.len() - suffix),
        text: n[prefix..n.len() - suffix].iter().collect(),
    })
}

/// The LSP position of character `index` in `chars`.
fn lsp_pos_at(chars: &[char], index: usize) -> LspPos {
    let mut line = 0;
    let mut col = 0;
    for c in &chars[..index] {
        if *c == '\n' {
            line += 1;
            col = 0;
        } else {
            col += c.len_utf16() as u32;
        }
    }
    LspPos {
        line,
        character: col,
    }
}

/// A bare list of `TextEdit`s (what formatting and code actions reply
/// with, as opposed to a `WorkspaceEdit`).
pub fn text_edits(v: &Value) -> Vec<LspEdit> {
    edits_of(v)
}

/// A code action's title, and the command that runs it when the server
/// gave one instead of an edit.
pub fn signature_summary(result: &Value) -> String {
    let sig = result.get("signatures").and_then(Value::as_array);
    let active = result
        .get("activeSignature")
        .and_then(Value::as_u64)
        .unwrap_or(0) as usize;
    let sig = match sig.and_then(|s| s.get(active).or_else(|| s.first())) {
        Some(s) => s,
        None => return String::new(),
    };
    let label = sig
        .get("label")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let labels: Vec<String> = sig
        .get("parameters")
        .and_then(Value::as_array)
        .map(|p| p.iter().map(|v| param_label(&label, v)).collect())
        .unwrap_or_default();
    let params = labels.join(", ");
    let active_param = result
        .get("activeParameter")
        .and_then(Value::as_u64)
        .map(|i| i as usize);
    // Mark the active parameter the way a tooltip would, so a long
    // signature still shows which one you're inside.
    let active_param = sig
        .get("activeParameter")
        .and_then(Value::as_u64)
        .map(|i| i as usize)
        .or(active_param);
    match active_param
        .and_then(|i| labels.get(i))
        .map(String::as_str)
        .filter(|p| !p.is_empty())
        .and_then(|param| params.split_once(param).map(|split| (split, param)))
    {
        Some(((before, after), param)) => {
            format!("{label}\n  {before}\u{2039}{param}\u{203a}{after}")
        }
        None if params.is_empty() => label,
        None => format!("{label}\n  ({params})"),
    }
}

/// A `ParameterInformation`'s label: a string, or `[start, end]` UTF-16
/// offsets into the signature's label.
/// The active signature on one line, for the echo area (eldoc): its
/// label with the active parameter marked, `name: ` in front when the
/// label doesn't start with the name (pyright's `(x: int) -> str`).
pub fn signature_line(result: &Value, name: &str) -> String {
    let Some(sigs) = result.get("signatures").and_then(Value::as_array) else {
        return String::new();
    };
    let active = result
        .get("activeSignature")
        .and_then(Value::as_u64)
        .unwrap_or(0) as usize;
    let Some(sig) = sigs.get(active).or_else(|| sigs.first()) else {
        return String::new();
    };
    let label = sig.get("label").and_then(Value::as_str).unwrap_or("");
    let index = sig
        .get("activeParameter")
        .or_else(|| result.get("activeParameter"))
        .and_then(Value::as_u64)
        .map(|i| i as usize);
    let param = index.and_then(|i| {
        sig.get("parameters")
            .and_then(Value::as_array)?
            .get(i)
            .cloned()
    });
    // Where the parameter is in the label, in chars.
    let span = param.and_then(|p| match p.get("label")? {
        Value::Array(a) if a.len() == 2 => {
            let units: Vec<u16> = label.encode_utf16().collect();
            let (x, y) = (
                a[0].as_u64()? as usize,
                (a[1].as_u64()? as usize).min(units.len()),
            );
            (x < y).then(|| {
                let before = String::from_utf16_lossy(&units[..x]).chars().count();
                (
                    before,
                    before + String::from_utf16_lossy(&units[x..y]).chars().count(),
                )
            })
        }
        Value::String(text) if !text.is_empty() => {
            // The first occurrence after the opening parenthesis.
            let open = label.find('(').map_or(0, |i| i + 1);
            let at = label[open..].find(text.as_str())? + open;
            let before = label[..at].chars().count();
            Some((before, before + text.chars().count()))
        }
        _ => None,
    });
    let mut line = String::new();
    for (i, c) in label.chars().enumerate() {
        if span.is_some_and(|(a, _)| a == i) {
            line.push('\u{2039}');
        }
        if span.is_some_and(|(_, z)| z == i) {
            line.push('\u{203a}');
        }
        line.push(if c == '\n' { ' ' } else { c });
    }
    if span.is_some_and(|(_, z)| z == label.chars().count()) {
        line.push('\u{203a}');
    }
    if !name.is_empty() && !label.starts_with(name) {
        line = format!("{name}: {line}");
    }
    line
}

pub fn param_label(sig_label: &str, param: &Value) -> String {
    match param.get("label").unwrap_or(param) {
        Value::String(s) => s.clone(),
        Value::Array(a) if a.len() == 2 => {
            let (Some(x), Some(y)) = (a[0].as_u64(), a[1].as_u64()) else {
                return String::new();
            };
            let units: Vec<u16> = sig_label.encode_utf16().collect();
            let (x, y) = (x as usize, (y as usize).min(units.len()));
            if x >= y {
                return String::new();
            }
            String::from_utf16_lossy(&units[x..y])
        }
        _ => String::new(),
    }
}

/// The text of a `MarkupContent`, `MarkedString` or a list of them, whole
/// (for a documentation buffer): code fences dropped, a marked string's
/// language too.
pub fn markup_text(v: &Value) -> String {
    let raw = match v {
        Value::String(s) => s.clone(),
        Value::Object(o) => o
            .get("value")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        Value::Array(a) => a.iter().map(markup_text).collect::<Vec<_>>().join("\n\n"),
        _ => String::new(),
    };
    raw.lines()
        .filter(|l| !l.trim_start().starts_with("```"))
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string()
}

/// A snippet (`foo(${1:x}, $2)$0`) as the plain text it expands to with
/// every field left at its default: `foo(x, )`. And where `$0` (or the
/// first field) is, in characters, for point.
pub fn expand_snippet(s: &str) -> (String, Option<usize>) {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::new();
    let mut n = 0usize;
    let mut point: Option<(u32, usize)> = None;
    let mut i = 0;
    // (field number, default text) parsing, one level of nesting at a time.
    fn note(point: &mut Option<(u32, usize)>, num: u32, at: usize) {
        let better = match point {
            None => true,
            // $0 wins; otherwise the lowest number.
            Some((p, _)) => num == 0 || (*p != 0 && num < *p),
        };
        if better {
            *point = Some((num, at));
        }
    }
    while i < chars.len() {
        let c = chars[i];
        if c == '\\' && i + 1 < chars.len() {
            out.push(chars[i + 1]);
            n += 1;
            i += 2;
            continue;
        }
        if c == '$' && i + 1 < chars.len() {
            if chars[i + 1].is_ascii_digit() {
                let mut j = i + 1;
                while j < chars.len() && chars[j].is_ascii_digit() {
                    j += 1;
                }
                let num: u32 = chars[i + 1..j]
                    .iter()
                    .collect::<String>()
                    .parse()
                    .unwrap_or(0);
                note(&mut point, num, n);
                i = j;
                continue;
            }
            if chars[i + 1] == '{' {
                // ${N:default} / ${N|a,b|} / ${N} / ${VAR:default}
                let mut j = i + 2;
                let mut depth = 1;
                let start = j;
                while j < chars.len() && depth > 0 {
                    match chars[j] {
                        '\\' => j += 1,
                        '{' => depth += 1,
                        '}' => depth -= 1,
                        _ => {}
                    }
                    j += 1;
                }
                let inner: String = chars[start..j.saturating_sub(1).max(start)]
                    .iter()
                    .collect();
                let (head, rest) = match inner.find([':', '|']) {
                    Some(k) => (&inner[..k], Some((&inner[k..k + 1], &inner[k + 1..]))),
                    None => (inner.as_str(), None),
                };
                let num = head.parse::<u32>().ok();
                let text = match rest {
                    Some((":", d)) => expand_snippet(d).0,
                    Some(("|", choices)) => choices
                        .trim_end_matches('|')
                        .split(',')
                        .next()
                        .unwrap_or("")
                        .to_string(),
                    _ => String::new(),
                };
                if let Some(num) = num {
                    note(&mut point, num, n);
                }
                n += text.chars().count();
                out.push_str(&text);
                i = j;
                continue;
            }
        }
        out.push(c);
        n += 1;
        i += 1;
    }
    (out, point.map(|(_, at)| at))
}

/// LSP's SymbolKind names (1-based).
pub const SYMBOL_KINDS: &[&str] = &[
    "file",
    "module",
    "namespace",
    "package",
    "class",
    "method",
    "property",
    "field",
    "constructor",
    "enum",
    "interface",
    "function",
    "variable",
    "constant",
    "string",
    "number",
    "boolean",
    "array",
    "object",
    "key",
    "null",
    "enum member",
    "struct",
    "event",
    "operator",
    "type parameter",
];

pub fn symbol_kind(k: Option<u64>) -> &'static str {
    k.and_then(|k| SYMBOL_KINDS.get((k as usize).wrapping_sub(1)))
        .copied()
        .unwrap_or("symbol")
}

/// A symbol from `documentSymbol` or `workspace/symbol`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Symbol {
    /// With its containers: `Class.method`.
    pub name: String,
    pub kind: &'static str,
    pub detail: Option<String>,
    /// Absent for a document's own symbols (they're in that document).
    pub uri: Option<String>,
    pub start: LspPos,
}

/// The symbols of a `documentSymbol` or `workspace/symbol` reply, both
/// shapes (hierarchical `DocumentSymbol`s are flattened, the path joined
/// with `.`).
pub fn symbols(result: &Value) -> Vec<Symbol> {
    fn walk(v: &Value, prefix: &str, out: &mut Vec<Symbol>) {
        let Some(name) = v.get("name").and_then(Value::as_str) else {
            return;
        };
        let kind = symbol_kind(v.get("kind").and_then(Value::as_u64));
        let full = if prefix.is_empty() {
            name.to_string()
        } else {
            format!("{prefix}.{name}")
        };
        // DocumentSymbol: selectionRange/range; SymbolInformation: location.
        let (uri, range) = match v.get("location") {
            Some(loc) => (
                loc.get("uri").and_then(Value::as_str).map(String::from),
                loc.get("range"),
            ),
            None => (None, v.get("selectionRange").or_else(|| v.get("range"))),
        };
        let Some((start, _)) = range.and_then(range_of) else {
            return;
        };
        let container = v
            .get("containerName")
            .and_then(Value::as_str)
            .filter(|c| !c.is_empty());
        let name = match (container, prefix.is_empty()) {
            (Some(c), true) => format!("{c}.{name}"),
            _ => full.clone(),
        };
        out.push(Symbol {
            name,
            kind,
            detail: v
                .get("detail")
                .and_then(Value::as_str)
                .filter(|d| !d.is_empty())
                .map(String::from),
            uri,
            start,
        });
        for child in v
            .get("children")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            walk(child, &full, out);
        }
    }
    let mut out = Vec::new();
    for v in result.as_array().into_iter().flatten() {
        walk(v, "", &mut out);
    }
    out
}

/// One step of a `WorkspaceEdit`, in the order the server gave them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    Edit(String, Vec<LspEdit>),
    Create {
        uri: String,
        overwrite: bool,
        ignore_if_exists: bool,
    },
    Rename {
        old: String,
        new: String,
        overwrite: bool,
        ignore_if_exists: bool,
    },
    Delete {
        uri: String,
        recursive: bool,
        ignore_if_not_exists: bool,
    },
}

/// Every change in a `WorkspaceEdit`: `documentChanges` (text edits and
/// file operations, in order) else `changes`.
pub fn changes(edit: &Value) -> Vec<Change> {
    let flag = |v: &Value, k: &str| {
        v.get("options")
            .and_then(|o| o.get(k))
            .and_then(Value::as_bool)
            .unwrap_or(false)
    };
    if let Some(docs) = edit.get("documentChanges").and_then(Value::as_array) {
        return docs
            .iter()
            .filter_map(|d| match d.get("kind").and_then(Value::as_str) {
                Some("create") => Some(Change::Create {
                    uri: d.get("uri")?.as_str()?.to_string(),
                    overwrite: flag(d, "overwrite"),
                    ignore_if_exists: flag(d, "ignoreIfExists"),
                }),
                Some("rename") => Some(Change::Rename {
                    old: d.get("oldUri")?.as_str()?.to_string(),
                    new: d.get("newUri")?.as_str()?.to_string(),
                    overwrite: flag(d, "overwrite"),
                    ignore_if_exists: flag(d, "ignoreIfExists"),
                }),
                Some("delete") => Some(Change::Delete {
                    uri: d.get("uri")?.as_str()?.to_string(),
                    recursive: flag(d, "recursive"),
                    ignore_if_not_exists: flag(d, "ignoreIfNotExists"),
                }),
                _ => {
                    let uri = d.get("textDocument")?.get("uri")?.as_str()?;
                    Some(Change::Edit(uri.to_string(), edits_of(d.get("edits")?)))
                }
            })
            .collect();
    }
    workspace_edits(edit)
        .into_iter()
        .map(|(u, e)| Change::Edit(u, e))
        .collect()
}

/// The label for a `CompletionItem`'s documentation, one line of it.
pub fn doc_note(item: &Value) -> Option<String> {
    let doc = item
        .get("documentation")
        .filter(|d| !d.is_null())
        .or_else(|| item.get("detail"))?;
    // Markdown and code fences read badly in a one-line popup, so strip
    // them rather than join the lines (hover_summary joins with spaces).
    fn text(v: &Value) -> String {
        match v {
            Value::String(s) => s.clone(),
            Value::Object(o) => o
                .get("value")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            Value::Array(a) => a.iter().map(text).collect::<Vec<_>>().join("\n"),
            _ => String::new(),
        }
    }
    let all = text(doc);
    let line = all
        .lines()
        .find(|l| !l.trim().is_empty() && !l.starts_with("```"))?
        .trim();
    (!line.is_empty()).then(|| line.to_string())
}

/// A code action's title, and the command the server wants run when the
/// action has no edit of its own.
pub fn code_action(action: &Value) -> Option<(String, Option<String>)> {
    let title = action.get("title").and_then(Value::as_str)?;
    let command = action
        .get("command")
        .and_then(|c| c.get("command"))
        .and_then(Value::as_str)
        .map(String::from);
    Some((title.to_string(), command))
}

pub const KINDS: &[&str] = &[
    "text",
    "method",
    "function",
    "constructor",
    "field",
    "variable",
    "class",
    "interface",
    "module",
    "property",
    "unit",
    "value",
    "enum",
    "keyword",
    "snippet",
    "color",
    "file",
    "reference",
    "folder",
    "enum member",
    "constant",
    "struct",
    "event",
    "operator",
    "type parameter",
];

/// A short margin (an icon-ish tag) for a completion item's kind, shown
/// before it. None when the item has no kind this knows.
pub fn completion_margin(item: &Value) -> Option<String> {
    let k = item.get("kind").and_then(Value::as_u64)?;
    let tag = match *KINDS.get(k.checked_sub(1)? as usize)? {
        "method" => "m",
        "function" => "f",
        "constructor" => "c",
        "field" => "·",
        "variable" => "v",
        "class" => "C",
        "interface" => "I",
        "module" => "M",
        "property" => "p",
        "unit" => "u",
        "value" => "=",
        "enum" => "E",
        "keyword" => "k",
        "snippet" => "s",
        "color" => "#",
        "file" => "F",
        "reference" => "r",
        "folder" => "/",
        "enum member" => "e",
        "constant" => "K",
        "struct" => "S",
        "event" => "V",
        "operator" => "o",
        "type parameter" => "T",
        _ => return None,
    };
    Some(tag.to_string())
}

/// Whether a completion item is deprecated: its `deprecated` flag or the
/// `Deprecated` tag (1).
pub fn completion_deprecated(item: &Value) -> bool {
    item.get("deprecated")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        || item
            .get("tags")
            .and_then(Value::as_array)
            .is_some_and(|t| t.iter().any(|v| v.as_u64() == Some(1)))
}

/// A completion item's documentation (a string, or MarkupContent's value).
pub fn completion_documentation(item: &Value) -> Option<String> {
    let text = match item.get("documentation")? {
        Value::String(s) => s.clone(),
        Value::Object(o) => o.get("value").and_then(Value::as_str)?.to_string(),
        _ => return None,
    };
    (!text.is_empty()).then_some(text)
}

/// A completion item as (text to insert, annotation). Snippets become
/// their plain label.
pub fn completion_item(item: &Value) -> (String, Option<String>) {
    let label = item.get("label").and_then(Value::as_str).unwrap_or("");
    let insert = item.get("insertText").and_then(Value::as_str);
    let snippet = item.get("insertTextFormat").and_then(Value::as_i64) == Some(2);
    let text_edit = item
        .get("textEdit")
        .and_then(|e| e.get("newText"))
        .and_then(Value::as_str);
    let word = match text_edit.or(insert) {
        // A snippet's placeholders would be left as text: the word before
        // its first field is what gets typed (Eglot without yasnippet
        // doesn't ask servers for snippets at all).
        Some(t) if !snippet => t.to_string(),
        _ => {
            let f = item
                .get("filterText")
                .and_then(Value::as_str)
                .unwrap_or(label);
            f.split('(').next().unwrap_or(f).to_string()
        }
    };
    let detail = item
        .get("detail")
        .and_then(Value::as_str)
        .filter(|d| !d.is_empty())
        .map(String::from);
    let kind = item
        .get("kind")
        .and_then(Value::as_u64)
        .and_then(|k| KINDS.get(k as usize - 1))
        .map(|k| k.to_string());
    let note = detail.or(kind).map(|n| {
        if n.chars().count() > 40 {
            format!("{}...", n.chars().take(37).collect::<String>())
        } else {
            n
        }
    });
    (word.trim_start().to_string(), note)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn framing_round_trips_and_waits_for_whole_messages() {
        let a = json!({"jsonrpc": "2.0", "id": 1, "result": "é"});
        let mut input = frame(&a);
        input.extend(frame(&json!({"method": "x"})));
        let whole = input.len();
        input.extend_from_slice(b"Content-Length: 10\r\n\r\n{\"a\"");
        let (msgs, errors) = unframe(&mut input);
        assert_eq!(msgs, [a, json!({"method": "x"})]);
        assert!(errors.is_empty());
        assert_eq!(
            input.len(),
            b"Content-Length: 10\r\n\r\n{\"a\"".len(),
            "the partial one waits"
        );
        assert!(whole > 0);
        input.extend_from_slice(b":1}xx");
        // 10 bytes: {"a":1}xx is 9... plus one more byte to complete.
        input.push(b' ');
        let (msgs, errors) = unframe(&mut input);
        assert_eq!(msgs.len() + errors.len(), 1);
    }

    #[test]
    fn uris_and_columns() {
        assert_eq!(uri_of("/tmp/a b/é.py"), "file:///tmp/a%20b/%C3%A9.py");
        assert_eq!(path_of("file:///tmp/a%20b/%C3%A9.py"), "/tmp/a b/é.py");
        assert_eq!(utf16_col("a😀b", 2), 3);
        assert_eq!(char_col("a😀b", 3), 2);
        assert_eq!(char_col("abc", 10), 3);
    }

    #[test]
    fn locations_from_both_shapes() {
        let a = location(&json!({
            "uri": "file:///a.py",
            "range": {"start": {"line": 3, "character": 2}, "end": {"line": 3, "character": 9}},
        }))
        .unwrap();
        assert_eq!(a.uri, "file:///a.py");
        assert_eq!((a.line, a.character), (3, 2));
        assert_eq!(a.end.character, 9);

        let b = location(&json!({
            "targetUri": "file:///b.py",
            "targetSelectionRange": {
                "start": {"line": 1, "character": 0},
                "end": {"line": 1, "character": 4},
            },
        }))
        .unwrap();
        assert_eq!(
            (b.uri.clone(), b.line, b.character),
            ("file:///b.py".into(), 1, 0)
        );

        // A LocationLink with only a targetRange still yields a place to go.
        let c = location(&json!({
            "targetUri": "file:///c.py",
            "targetRange": {"start": {"line": 7, "character": 1}, "end": {"line": 7, "character": 2}},
        }))
        .unwrap();
        assert_eq!(c.line, 7);

        assert_eq!(location(&json!({"range": {}})), None);
        assert!(locations(&Value::Null).is_empty());
        assert_eq!(locations(&json!([a_ref(), b_ref()])).len(), 2);
        assert_eq!(locations(&a_ref()).len(), 1, "one location, not a list");
    }

    #[test]
    fn a_symbol_result_names_where_each_symbol_is() {
        let result = json!([
            {"name": "alpha", "kind": 12, "location": {"uri": "file:///a.rs",
             "range": {"start": {"line": 1, "character": 4}, "end": {"line": 1, "character": 9}}}},
            {"name": "beta", "kind": 12, "location": {"uri": "file:///b.rs",
             "range": {"start": {"line": 2, "character": 0}, "end": {"line": 2, "character": 1}}}},
        ]);
        let a = symbol_named(&json!([{"name": "alpha", "location": {"uri": "file:///a.py",
                "range": {"start": {"line": 3, "character": 2}, "end": {"line": 3, "character": 9}}}}]),
            "alpha")
        .unwrap();
        assert_eq!(
            (a.uri.as_str(), a.line, a.character),
            ("file:///a.py", 3, 2)
        );

        let b = symbol_named(&result, "beta").unwrap();
        assert_eq!((b.line, b.character), (2, 0));
        assert_eq!(symbol_named(&result, "missing"), None);
        assert_eq!(symbol_named(&Value::Null, "x"), None);
    }

    fn a_ref() -> Value {
        json!({"uri": "file:///a.py",
               "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}}})
    }
    fn b_ref() -> Value {
        json!({"targetUri": "file:///b.py",
               "targetSelectionRange": {"start": {"line": 5, "character": 0}, "end": {"line": 5, "character": 1}}})
    }

    #[test]
    fn workspace_edits_in_all_three_spellings() {
        let edit =
            json!({"start": {"line": 1, "character": 0}, "end": {"line": 1, "character": 2}});
        let changes = workspace_edits(&json!({
            "changes": {"file:///a.py": [ { "range": edit, "newText": "x" } ]},
        }));
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].0, "file:///a.py");
        assert_eq!(changes[0].1[0].text, "x");

        let doc_changes = workspace_edits(&json!({
            "documentChanges": [
                { "textDocument": {"uri": "file:///b.py"}, "edits": [ {"range": edit, "newText": "y"} ] },
                { "textDocument": {"uri": "file:///c.py"}, "edits": [] },
            ],
        }));
        assert_eq!(doc_changes.len(), 2);
        assert_eq!(doc_changes[0].1[0].text, "y");
        assert!(doc_changes[1].1.is_empty());

        // A file rename has no edits and must not panic or invent one.
        let renamed = workspace_edits(&json!({
            "documentChanges": [ { "kind": "rename", "oldUri": "file:///d.py", "newUri": "file:///e.py" } ],
        }));
        assert!(renamed.is_empty());
        assert!(workspace_edits(&json!({})).is_empty());
        // An edit without newText is dropped, not applied as empty text.
        let partial = workspace_edits(&json!({
            "changes": {"file:///a.py": [{"range": edit}]},
        }));
        assert!(partial[0].1.is_empty());
    }

    #[test]
    fn minimal_edit_keeps_the_surrounding_text() {
        let e = minimal_edit("hello world", "hello brave world").unwrap();
        assert_eq!(
            (e.start.character, e.end.character, e.text.as_str()),
            (6, 6, "brave ")
        );
        // A deletion: empty new text, a widened range.
        let d = minimal_edit("abcdef", "abef").unwrap();
        assert_eq!(
            (d.start.character, d.end.character, d.text.as_str()),
            (2, 4, "")
        );
        let i = minimal_edit("abc", "abcd").unwrap();
        assert_eq!(
            (i.start.character, i.end.character, i.text.as_str()),
            (3, 3, "d")
        );
        assert_eq!(
            minimal_edit("same", "same"),
            None,
            "no edit when nothing changed"
        );
        // Characters, not bytes: "a😀b" is three columns, so the insert
        // lands at 3, not at byte offset 5.
        // UTF-16 code units, as LSP counts: the emoji is two.
        let u = minimal_edit("a😀b", "a😀bc").unwrap();
        assert_eq!(
            (u.start.character, u.end.character, u.text.as_str()),
            (4, 4, "c")
        );
        let r = minimal_edit("a😀b", "a😃b").unwrap();
        assert_eq!(
            (r.start.character, r.end.character, r.text.as_str()),
            (1, 3, "😃")
        );
        // Lines and columns, not an offset into the whole file.
        let m = minimal_edit("one\ntwo\nthree\n", "one\ntWo\nthree\n").unwrap();
        assert_eq!(
            (m.start.line, m.start.character, m.end.line, m.end.character),
            (1, 1, 1, 2)
        );
        let j = minimal_edit("a\nb\n", "a b\n").unwrap();
        assert_eq!(
            (
                j.start.line,
                j.start.character,
                j.end.line,
                j.end.character,
                j.text.as_str()
            ),
            (0, 1, 1, 0, " ")
        );
    }

    #[test]
    fn signature_lines_mark_the_active_parameter() {
        let pyright = json!({"activeParameter": 1, "activeSignature": 0, "signatures": [
            {"activeParameter": 0, "label": "(name: str) -> str", "parameters": [{"label": [1, 10]}]}]});
        assert_eq!(
            signature_line(&pyright, "greet"),
            "greet: (\u{2039}name: str\u{203a}) -> str"
        );
        let strings = json!({"signatures": [{"label": "f(a, b)", "parameters": [{"label": "a"}, {"label": "b"}]}],
                             "activeParameter": 1});
        assert_eq!(signature_line(&strings, "f"), "f(a, \u{2039}b\u{203a})");
        assert_eq!(signature_line(&json!({"signatures": []}), "f"), "");
    }

    #[test]
    fn snippets_signatures_symbols_and_file_changes() {
        assert_eq!(
            expand_snippet("foo(${1:x}, ${2:y})$0"),
            ("foo(x, y)".into(), Some(9))
        );
        assert_eq!(
            expand_snippet("if $1 {\n\t$0\n}"),
            ("if  {\n\t\n}".into(), Some(7))
        );
        assert_eq!(
            expand_snippet("${1|one,two|} \\$x"),
            ("one $x".into(), Some(0))
        );
        assert_eq!(expand_snippet("vec![${1:${2:a}, b}]").0, "vec![a, b]");
        let sig = json!({"signatures": [{"label": "f(a: int, b: str)", "parameters": [
            {"label": [2, 8]}, {"label": "b: str"}]}], "activeParameter": 1});
        assert_eq!(
            signature_summary(&sig),
            "f(a: int, b: str)\n  a: int, \u{2039}b: str\u{203a}"
        );
        let ds = json!([{"name": "C", "kind": 5, "range": {"start": {"line": 0, "character": 0}, "end": {"line": 9, "character": 0}},
            "selectionRange": {"start": {"line": 0, "character": 6}, "end": {"line": 0, "character": 7}},
            "children": [{"name": "m", "kind": 6, "detail": "(self)", "range": {"start": {"line": 1, "character": 4}, "end": {"line": 2, "character": 0}},
            "selectionRange": {"start": {"line": 1, "character": 8}, "end": {"line": 1, "character": 9}}}]}]);
        let syms = symbols(&ds);
        assert_eq!(
            syms.iter()
                .map(|s| (s.name.as_str(), s.kind))
                .collect::<Vec<_>>(),
            [("C", "class"), ("C.m", "method")]
        );
        assert_eq!(
            syms[1].start,
            LspPos {
                line: 1,
                character: 8
            }
        );
        let si = json!([{"name": "f", "kind": 12, "containerName": "mod", "location": {"uri": "file:///a.py",
            "range": {"start": {"line": 3, "character": 4}, "end": {"line": 3, "character": 5}}}}]);
        assert_eq!(symbols(&si)[0].name, "mod.f");
        assert_eq!(symbols(&si)[0].uri.as_deref(), Some("file:///a.py"));
        let we = json!({"documentChanges": [
            {"kind": "rename", "oldUri": "file:///a.py", "newUri": "file:///b.py"},
            {"textDocument": {"uri": "file:///b.py", "version": 1}, "edits": [{"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 0}}, "newText": "x"}]},
            {"kind": "create", "uri": "file:///c.py", "options": {"ignoreIfExists": true}},
            {"kind": "delete", "uri": "file:///d", "options": {"recursive": true}}]});
        let c = changes(&we);
        assert_eq!(c.len(), 4);
        assert!(
            matches!(&c[0], Change::Rename { old, new, .. } if old.ends_with("a.py") && new.ends_with("b.py"))
        );
        assert!(matches!(&c[1], Change::Edit(u, e) if u.ends_with("b.py") && e.len() == 1));
        assert!(matches!(
            &c[2],
            Change::Create {
                ignore_if_exists: true,
                ..
            }
        ));
        assert!(matches!(
            &c[3],
            Change::Delete {
                recursive: true,
                ..
            }
        ));
    }

    #[test]
    fn docs_and_code_actions() {
        assert_eq!(
            doc_note(&json!({"documentation": {"kind": "markdown", "value": "Does it.\nMore."}})),
            Some("Does it.".into())
        );
        assert_eq!(
            doc_note(&json!({"detail": "fn f(x: u8)"})),
            Some("fn f(x: u8)".into())
        );
        assert_eq!(doc_note(&json!({})), None);
        assert_eq!(doc_note(&json!({"documentation": ""})), None);

        assert_eq!(
            code_action(&json!({"title": "Fix it", "command": {"command": "quickfix"}})),
            Some(("Fix it".into(), Some("quickfix".into())))
        );
        assert_eq!(
            code_action(&json!({"title": "Just an edit"})),
            Some(("Just an edit".into(), None))
        );
        assert_eq!(code_action(&json!({"kind": "quickfix"})), None);
    }

    #[test]
    fn hover_and_completion_items() {
        assert_eq!(
            hover_summary(
                &json!({"kind": "markdown", "value": "```python\n(function) def f()\n```\nDoes it."})
            ),
            "(function) def f()  Does it."
        );
        assert_eq!(
            hover_summary(&json!(["a", {"language": "x", "value": "b"}])),
            "a  b"
        );
        assert_eq!(
            completion_item(&json!({"label": "path", "kind": 6})),
            ("path".into(), Some("variable".into()))
        );
        assert_eq!(
            completion_item(
                &json!({"label": "f(x)", "insertText": "f(${1:x})", "insertTextFormat": 2, "detail": "fn f(x: u8)"})
            ),
            ("f".into(), Some("fn f(x: u8)".into()))
        );
        // A margin from the kind, and deprecation from the flag or tag.
        assert_eq!(completion_margin(&json!({"kind": 3})), Some("f".into()));
        assert_eq!(completion_margin(&json!({"kind": 6})), Some("v".into()));
        assert_eq!(completion_margin(&json!({"label": "x"})), None);
        assert!(!completion_deprecated(&json!({"label": "x"})));
        assert!(completion_deprecated(&json!({"deprecated": true})));
        assert!(completion_deprecated(&json!({"tags": [1]})));
        assert!(!completion_deprecated(&json!({"tags": [0]})));
        // Documentation as a plain string or a MarkupContent value; absent
        // or empty is none (a resolve reply fills it in later).
        assert_eq!(
            completion_documentation(&json!({"label": "x", "documentation": "Does it."})),
            Some("Does it.".into())
        );
        assert_eq!(
            completion_documentation(
                &json!({"documentation": {"kind": "markdown", "value": "```py\nx\n```"}})
            ),
            Some("```py\nx\n```".into())
        );
        assert_eq!(completion_documentation(&json!({"label": "x"})), None);
        assert_eq!(
            completion_documentation(&json!({"documentation": ""})),
            None
        );
    }
}
