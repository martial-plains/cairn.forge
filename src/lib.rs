// SPDX-License-Identifier: GPL-3.0-or-later
//! Cairn for Forge: a Language Server Protocol client that works the way
//! Emacs's Eglot does.
//!
//! A file whose major mode has a server starts that server for the
//! project (one per server and project root; `M-x cairn` starts one by
//! hand, and `"autostart": false` in cairn.json leaves it to that). The
//! mode line says `[cairn:project]`, with progress the server reports and
//! the file's diagnostic counts.
//!
//! - **Flymake**: diagnostics are decorations (the message shows with
//!   point on one); `M-x flymake-show-buffer-diagnostics` and
//!   `flymake-show-project-diagnostics` list them,
//!   `flymake-goto-next-error` / `-prev-error` move between them.
//! - **Xref**: `M-.` definitions, `M-?` references, `C-M-.` workspace
//!   symbols, `M-,` back and `C-M-,` forward again; several results go in
//!   `*xref*`. `cairn-find-declaration`, `-implementation`,
//!   `-typeDefinition` too.
//! - **Eldoc**: idle at a symbol, the echo area shows the signature of the
//!   call around point or the documentation of the symbol, and the other
//!   occurrences of the symbol are highlighted. `C-h .` (`eldoc`) asks at
//!   once, `M-x eldoc-doc-buffer` shows all of it in `*eldoc*`.
//! - **Editing**: `cairn-rename`, `cairn-format` (the region, or the
//!   buffer), `cairn-code-actions` and the shortcuts
//!   `cairn-code-action-organize-imports`, `-quickfix`, `-extract`,
//!   `-inline`, `-rewrite`. A server's workspace edits, file creations,
//!   renames and deletions included, are applied to buffers and left
//!   unsaved for review.
//! - **Completion**: a completion source (the corfu popup, or M-TAB).
//! - `M-g i` in a managed buffer is imenu from the server's symbols;
//!   `cairn-show-call-hierarchy` and `cairn-show-type-hierarchy` draw trees
//!   (TAB expands a node, RET visits it).
//! - `cairn-list-connections`, `cairn-events-buffer` (the JSON-RPC
//!   traffic), `cairn-reconnect`, `cairn-shutdown`, `cairn-shutdown-all`,
//!   `cairn-signal-didChangeConfiguration` and
//!   `cairn-show-workspace-configuration` manage the servers.
//!
//! The servers and their settings come from config.rs's defaults,
//! `~/.config/forge/cairn.json` and a project's `.cairn.json`. The old
//! `lsp-*` command names still work.

pub mod config;
pub mod protocol;

use config::Config;
use forge_plugin_sdk::{
    Buffer, CompletionRequest, Decoration, DetailedCandidate, Event, EventKind, Level, Position,
    Process, Scope, Selection, Span, answer_registration, bind_in_mode, close_overlay, command,
    completing_read, completion_source, current_hook, current_registration, decorations,
    detailed_candidate, environment, files, forge_plugin, keymap, log_at, message, minibuffer,
    modes, on, on_resolve_location, open_overlay, project_root, read_string, spawn, update_overlay,
    when_idle, xref,
};
use protocol::{
    LspEdit, LspLocation, LspPos, char_col, frame, path_of, unframe, uri_of, utf16_col,
};
use serde_json::{Value, json};
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};

type Reply = Box<dyn FnOnce(Value)>;

struct Server {
    key: String,
    /// The program and its arguments.
    command: Vec<String>,
    /// What it calls itself (serverInfo.name), else the program.
    name: String,
    root: String,
    process: Process,
    next_id: i64,
    pending: HashMap<i64, (String, Reply)>,
    /// Messages waiting for the server to be initialized.
    queue: Vec<Value>,
    input: Vec<u8>,
    ready: bool,
    stopping: bool,
    capabilities: Value,
    /// Work-done progress, by token: what to show.
    progress: BTreeMap<String, String>,
    /// The JSON-RPC traffic (cairn-events-buffer).
    events: VecDeque<String>,
    init_options: Value,
}

struct Doc {
    server: String,
    uri: String,
    language: String,
    version: i64,
    /// The buffer version the server has.
    synced: u64,
}

/// Completion items held back until the server has answered
/// `completionItem/resolve` for the ones that need it.
struct Resolving {
    req: CompletionRequest,
    span: Span,
    /// A candidate per item, with documentation filled in as it arrives.
    cands: Vec<DetailedCandidate>,
    /// Resolve replies still outstanding.
    remaining: usize,
}

/// A diagnostic, kept in the server's terms (it goes back with code
/// action requests) and with what the lists show.
#[derive(Clone, Debug)]
struct Diagnostic {
    raw: Value,
    start: LspPos,
    end: LspPos,
    /// 1 error, 2 warning, 3 information, 4 hint.
    severity: u64,
    message: String,
    source: String,
}

/// A place to go back to (xref's marker stack).
#[derive(Clone, Debug)]
struct Marker {
    buffer: u32,
    path: Option<String>,
    point: Position,
}

/// A node of a call or type hierarchy tree.
#[derive(Clone, Debug)]
struct Node {
    item: Value,
    depth: usize,
    expanded: bool,
    /// Where the call happens (for call hierarchies), else the item.
    at: Option<LspLocation>,
}

#[derive(Clone, Debug)]
struct Hierarchy {
    server: String,
    /// "incoming"/"outgoing" calls, or "supertypes"/"subtypes".
    direction: String,
    calls: bool,
    nodes: Vec<Node>,
}

#[derive(Default)]
struct State {
    config: Config,
    config_loaded: bool,
    /// Project roots whose .cairn.json was read: root -> its config.
    project_configs: HashMap<String, Config>,
    servers: HashMap<String, Server>,
    docs: HashMap<u32, Doc>,
    /// The text last sent for a document, for incremental `didChange`.
    synced_text: HashMap<u32, String>,
    /// Diagnostics by document URI, opened or not (project lists).
    diagnostics: HashMap<String, Vec<Diagnostic>>,
    /// Symbol highlights per buffer (merged with the diagnostics).
    highlights: HashMap<u32, Vec<Span>>,
    /// Programs that couldn't start (not retried every time).
    failed: HashSet<String>,
    /// xref's marker stack, and the places gone back from.
    back: Vec<Marker>,
    forward: Vec<Marker>,
    /// What each line of a list buffer (*xref*, diagnostics) points at.
    lists: HashMap<u32, Vec<Option<LspLocation>>>,
    hierarchies: HashMap<u32, Hierarchy>,
    /// The last place eldoc looked at, and what it showed.
    last_eldoc: Option<(u32, u64, u32, u32)>,
    eldoc_shown: Option<String>,
    /// The full documentation for *eldoc*.
    eldoc_doc: String,
    /// Completion items across servers waiting on `completionItem/resolve`.
    resolving: HashMap<u32, Resolving>,
    /// The host id of the overlay showing work-done progress.
    progress_overlay: Option<String>,
    /// Numbers server keys.
    serial: u64,
}

thread_local! {
    static STATE: RefCell<State> = RefCell::new(State::default());
}

fn with<R>(f: impl FnOnce(&mut State) -> R) -> R {
    STATE.with(|s| f(&mut s.borrow_mut()))
}

const EVENTS_LINE: usize = 4000;

// ------------------------------------------------------------- config

/// The user's configuration, read once (and again with
/// cairn-signal-didChangeConfiguration).
fn config() -> Config {
    let loaded = with(|s| s.config_loaded);
    if !loaded {
        let mut c = Config::default();
        if let Ok(text) = files::read("~/.config/forge/cairn.json")
            && let Err(e) = c.apply(&text)
        {
            message(&format!("~/.config/forge/cairn.json: {e}"));
        }
        with(|s| {
            s.config = c;
            s.config_loaded = true;
        });
    }
    with(|s| s.config.clone())
}

/// The configuration for a project: the user's, plus its `.cairn.json`.
fn project_config(root: &str) -> Config {
    if let Some(c) = with(|s| s.project_configs.get(root).cloned()) {
        return c;
    }
    let mut c = config();
    let file = format!("{}/.cairn.json", root.trim_end_matches('/'));
    if let Ok(text) = files::read(&file)
        && let Err(e) = c.apply(&text)
    {
        message(&format!("{file}: {e}"));
    }
    with(|s| s.project_configs.insert(root.to_string(), c.clone()));
    c
}

fn reload_config() {
    with(|s| {
        s.config_loaded = false;
        s.project_configs.clear();
    });
    let _ = config();
}

/// Whether `program` is found the way a process launch would find it.
fn program_on_path(program: &str) -> bool {
    if program.is_empty() {
        return false;
    }
    if program.contains('/') || program.contains('\\') {
        return files::exists(program);
    }
    let Ok(vars) = environment::variables() else {
        return false;
    };
    let Some(path) = vars
        .iter()
        .find(|(name, _)| name == "PATH")
        .map(|(_, value)| value.clone())
    else {
        return false;
    };
    let sep = environment::path_separator();
    let sep = sep.chars().next().unwrap_or(':');
    let windows = environment::system_type() == "windows-nt";
    let extensions: Vec<String> = if windows {
        vars.iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("PATHEXT"))
            .map(|(_, value)| value.clone())
            .unwrap_or_else(|| ".COM;.EXE;.BAT;.CMD".to_string())
            .split(';')
            .filter(|ext| !ext.is_empty())
            .map(str::to_string)
            .collect()
    } else {
        Vec::new()
    };
    for dir in path.split(sep).filter(|dir| !dir.is_empty()) {
        let base = format!("{}/{}", dir.trim_end_matches(['/', '\\']), program);
        if files::exists(&base) {
            return true;
        }
        if extensions
            .iter()
            .any(|ext| files::exists(&format!("{base}{ext}")))
        {
            return true;
        }
    }
    false
}

/// The findings of the server check, shared by `package-doctor` and
/// `M-x cairn-doctor`.
fn doctor_findings() -> Vec<Value> {
    let mut findings: Vec<Value> = Vec::new();
    let mut cfg = Config::default();
    let path = "~/.config/forge/cairn.json";
    match files::read(path) {
        Ok(text) => {
            if let Err(e) = cfg.apply(&text) {
                findings.push(json!({
                    "severity": "error",
                    "message": format!("{path}: {e}"),
                    "fix": "Fix the JSON (or remove the file), then run the check again.",
                }));
            }
        }
        Err(_) => {}
    }
    let mut modes: BTreeMap<String, Vec<&config::ServerSpec>> = BTreeMap::new();
    for spec in &cfg.servers {
        for mode in &spec.modes {
            modes.entry(mode.clone()).or_default().push(spec);
        }
    }
    let mut checked: HashMap<String, bool> = HashMap::new();
    for (mode, specs) in modes {
        let found = specs.iter().any(|spec| {
            spec.command.first().is_some_and(|program| {
                *checked
                    .entry(program.clone())
                    .or_insert_with(|| program_on_path(program))
            })
        });
        if !found {
            let tried: Vec<String> = specs
                .iter()
                .filter(|spec| !spec.command.is_empty())
                .map(|spec| spec.command.join(" "))
                .collect();
            findings.push(json!({
                "severity": "warning",
                "message": format!(
                    "{mode}: no language server found on PATH (tried {})",
                    tried.join(", ")
                ),
                "fix": "Install one of the servers, or add one to ~/.config/forge/cairn.json.",
            }));
        }
    }
    findings
}

/// The `[[doctor-checks]]` handler, and `M-x cairn-doctor` outside it.
fn doctor() -> Result<(), String> {
    let findings = doctor_findings();
    if current_registration().is_some() {
        let answer = serde_json::to_string(&findings).map_err(|e| e.to_string())?;
        return answer_registration(&answer);
    }
    if findings.is_empty() {
        message("[cairn] every mode has a language server on PATH");
    } else {
        for finding in findings {
            let text = finding["message"].as_str().unwrap_or("").to_string();
            let severity = finding["severity"].as_str().unwrap_or("warning");
            let level = if severity == "error" {
                Level::Error
            } else {
                Level::Warn
            };
            log_at(level, &text);
        }
    }
    Ok(())
}

// ------------------------------------------------------------ messaging

fn log_event(key: &str, dir: &str, msg: &Value) {
    let size = with(|s| s.config.events_size);
    let mut text = msg.to_string();
    if text.len() > EVENTS_LINE {
        let mut cut = EVENTS_LINE;
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        text.truncate(cut);
        text.push_str(" ...");
    }
    with(|s| {
        if let Some(sv) = s.servers.get_mut(key) {
            sv.events.push_back(format!("[{dir}] {text}"));
            while sv.events.len() > size.max(10) {
                sv.events.pop_front();
            }
        }
    });
}

fn send(key: &str, mut msg: Value) {
    msg["jsonrpc"] = json!("2.0");
    log_event(key, "client", &msg);
    let target = with(|s| s.servers.get(key).map(|sv| (sv.process, sv.name.clone())));
    if let Some((p, name)) = target
        && let Err(e) = p.write(&frame(&msg))
    {
        log_at(Level::Warn, &format!("{name}: {e}"));
    }
}

/// A notification; waits until the server is initialized.
fn notify(key: &str, method: &str, params: Value) {
    let msg = json!({ "method": method, "params": params });
    let now = with(|s| match s.servers.get_mut(key) {
        Some(sv) if sv.ready => true,
        Some(sv) => {
            sv.queue.push(msg.clone());
            false
        }
        None => false,
    });
    if now {
        send(key, msg);
    }
}

fn request(key: &str, method: &str, params: Value, reply: impl FnOnce(Value) + 'static) {
    let msg = with(|s| {
        let sv = s.servers.get_mut(key)?;
        sv.next_id += 1;
        let id = sv.next_id;
        sv.pending.insert(id, (method.to_string(), Box::new(reply)));
        let msg = json!({ "id": id, "method": method, "params": params });
        if sv.ready || method == "initialize" {
            Some(msg)
        } else {
            sv.queue.push(msg);
            None
        }
    });
    if let Some(msg) = msg {
        send(key, msg);
    }
}

fn buffer_for_uri(uri: &str) -> Option<Buffer> {
    let id = with(|s| s.docs.iter().find(|(_, d)| d.uri == uri).map(|(id, _)| *id))?;
    Buffer::from_id(id).filter(|b| b.alive())
}

fn server_name(key: &str) -> String {
    with(|s| s.servers.get(key).map(|sv| sv.name.clone())).unwrap_or_default()
}

/// Does the server say it can do `capability` (a key of its
/// ServerCapabilities, true or an options object)?
fn can(key: &str, capability: &str) -> bool {
    with(|s| {
        s.servers.get(key).is_some_and(|sv| {
            let c = &sv.capabilities[capability];
            !(c.is_null() || c == &Value::Bool(false))
        })
    })
}

fn require(key: &str, capability: &str, what: &str) -> Result<(), String> {
    if can(key, capability) {
        Ok(())
    } else {
        Err(format!("{} can't {what}", server_name(key)))
    }
}

/// Whether the server fills completion items in only when asked
/// (`completionProvider.resolveProvider`): rust-analyzer and clangd leave
/// the documentation out of the list and send it with the resolve reply.
fn resolves_completions(key: &str) -> bool {
    with(|s| {
        s.servers.get(key).is_some_and(|sv| {
            sv.capabilities["completionProvider"]["resolveProvider"] == json!(true)
        })
    })
}

// ---------------------------------------------------------- diagnostics

fn publish_diagnostics(params: &Value) {
    let Some(uri) = params["uri"].as_str() else {
        return;
    };
    let mut kept = Vec::new();
    for d in params["diagnostics"].as_array().into_iter().flatten() {
        let pos = |p: &Value| LspPos {
            line: p["line"].as_u64().unwrap_or(0) as u32,
            character: p["character"].as_u64().unwrap_or(0) as u32,
        };
        let msg = d["message"].as_str().unwrap_or("").to_string();
        kept.push(Diagnostic {
            raw: d.clone(),
            start: pos(&d["range"]["start"]),
            end: pos(&d["range"]["end"]),
            severity: d["severity"].as_u64().unwrap_or(1),
            message: msg,
            source: d["source"].as_str().unwrap_or("").to_string(),
        });
    }
    with(|s| {
        if kept.is_empty() {
            s.diagnostics.remove(uri);
        } else {
            s.diagnostics.insert(uri.to_string(), kept);
        }
    });
    if let Some(buf) = buffer_for_uri(uri) {
        redecorate(&buf);
        update_mode_line(&buf);
    }
    refresh_diagnostic_lists();
}

fn face_of(severity: u64) -> &'static str {
    match severity {
        2 => "warning",
        3 | 4 => "info",
        _ => "error",
    }
}

/// A diagnostic's range as editor positions in `buf` (a point gets one
/// character, so it shows).
fn diag_span(buf: &Buffer, d: &Diagnostic) -> Span {
    let start = to_forge(buf, &d.start);
    let mut end = to_forge(buf, &d.end);
    if end.line == start.line && end.column <= start.column {
        end.column = start.column + 1;
    }
    Span { start, end }
}

fn diag_text(d: &Diagnostic) -> String {
    let first = d.message.lines().next().unwrap_or("");
    if d.source.is_empty() {
        first.to_string()
    } else {
        format!("{}: {first}", d.source)
    }
}

/// Set the plugin's decorations on a buffer: diagnostics, then symbol
/// highlights.
fn redecorate(buf: &Buffer) {
    if !buf.alive() {
        return;
    }
    let uri = with(|s| s.docs.get(&buf.id()).map(|d| d.uri.clone()));
    let diags = uri
        .and_then(|u| with(|s| s.diagnostics.get(&u).cloned()))
        .unwrap_or_default();
    let mut decos: Vec<Decoration> = diags
        .iter()
        .map(|d| Decoration {
            span: diag_span(buf, d),
            face: face_of(d.severity).into(),
            message: Some(diag_text(d)),
        })
        .collect();
    let hl = with(|s| s.highlights.get(&buf.id()).cloned()).unwrap_or_default();
    decos.extend(hl.into_iter().map(|span| Decoration {
        span,
        face: "symbol-highlight".into(),
        message: None,
    }));
    if let Err(e) = decorations::set(buf, &decos) {
        log_at(Level::Warn, &e);
    }
}

fn counts(uri: &str) -> (usize, usize, usize) {
    with(|s| {
        let ds = s.diagnostics.get(uri);
        let n =
            |f: &dyn Fn(u64) -> bool| ds.map_or(0, |v| v.iter().filter(|d| f(d.severity)).count());
        (n(&|x| x <= 1), n(&|x| x == 2), n(&|x| x >= 3))
    })
}

/// `[cairn:project]`, the server's progress, and the diagnostics count.
fn update_mode_line(buf: &Buffer) {
    let Some((key, uri)) = with(|s| {
        s.docs
            .get(&buf.id())
            .map(|d| (d.server.clone(), d.uri.clone()))
    }) else {
        buf.set_mode_line_info(None);
        return;
    };
    let (root, ready, progress) = with(|s| match s.servers.get(&key) {
        Some(sv) => (
            sv.root.clone(),
            sv.ready,
            sv.progress.values().last().cloned(),
        ),
        None => (String::new(), false, None),
    });
    let project = root
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or("")
        .to_string();
    let mut text = format!("[cairn:{project}");
    if !ready {
        text.push_str(" starting");
    }
    if let Some(p) = progress {
        text.push(' ');
        text.push_str(&p);
    }
    text.push(']');
    let (e, w, i) = counts(&uri);
    if e + w + i > 0 {
        text.push_str(&format!(" {e}E {w}W"));
        if i > 0 {
            text.push_str(&format!(" {i}I"));
        }
    }
    buf.set_mode_line_info(Some(&text));
}

fn update_mode_lines_for(key: &str) {
    let ids: Vec<u32> = with(|s| {
        s.docs
            .iter()
            .filter(|(_, d)| d.server == key)
            .map(|(id, _)| *id)
            .collect()
    });
    for id in ids {
        if let Some(b) = Buffer::from_id(id).filter(|b| b.alive()) {
            update_mode_line(&b);
        }
    }
}

// --------------------------------------------------- server requests

/// Apply a `WorkspaceEdit`: text edits go into buffers (files not open are
/// visited and left modified, for review), file operations happen on
/// disk. Returns (edits applied, problems).
fn apply_workspace_edit(edit: &Value) -> (usize, Vec<String>) {
    let (mut applied, mut problems) = (0, Vec::new());
    for change in protocol::changes(edit) {
        match change {
            protocol::Change::Edit(uri, edits) => {
                if edits.is_empty() {
                    continue;
                }
                match Buffer::visit(&path_of(&uri)) {
                    Ok(buf) => {
                        let (n, p) = apply_edits(&buf, edits);
                        applied += n;
                        problems.extend(p);
                        attach(&buf);
                    }
                    Err(e) => problems.push(e),
                }
            }
            protocol::Change::Create {
                uri,
                overwrite,
                ignore_if_exists,
            } => {
                let p = path_of(&uri);
                if files::exists(&p) && (ignore_if_exists || !overwrite) {
                    continue;
                }
                match files::create(&p, overwrite) {
                    Ok(()) => applied += 1,
                    Err(e) => problems.push(e),
                }
            }
            protocol::Change::Rename {
                old,
                new,
                overwrite,
                ignore_if_exists,
            } => {
                let (a, b) = (path_of(&old), path_of(&new));
                if files::exists(&b) && ignore_if_exists && !overwrite {
                    continue;
                }
                // The servers think in documents: close the old one, open
                // the new.
                let id = with(|s| s.docs.iter().find(|(_, d)| d.uri == old).map(|(id, _)| *id));
                if let Some(id) = id {
                    detach(id);
                }
                match files::rename(&a, &b, overwrite) {
                    Ok(()) => {
                        applied += 1;
                        if let Some(buf) = id.and_then(Buffer::from_id) {
                            attach(&buf);
                        }
                    }
                    Err(e) => problems.push(e),
                }
            }
            protocol::Change::Delete {
                uri,
                recursive,
                ignore_if_not_exists,
            } => {
                let p = path_of(&uri);
                if !files::exists(&p) && ignore_if_not_exists {
                    continue;
                }
                let id = with(|s| s.docs.iter().find(|(_, d)| d.uri == uri).map(|(id, _)| *id));
                if let Some(id) = id {
                    detach(id);
                }
                match files::delete(&p, recursive) {
                    Ok(()) => applied += 1,
                    Err(e) => problems.push(e),
                }
            }
        }
    }
    (applied, problems)
}

fn reply_ok(key: &str, id: &Value, result: Value) {
    send(key, json!({ "id": id, "result": result }));
}

fn on_server_request(key: &str, msg: &Value) {
    let method = msg["method"].as_str().unwrap_or("");
    let id = msg["id"].clone();
    let params = &msg["params"];
    match method {
        "workspace/applyEdit" => {
            let (n, problems) = apply_workspace_edit(&params["edit"]);
            if problems.is_empty() {
                log_at(
                    Level::Info,
                    &format!("applied {n} server-requested edit(s)"),
                );
                reply_ok(key, &id, json!({ "applied": true }));
            } else {
                for p in &problems {
                    log_at(Level::Warn, &format!("server edit: {p}"));
                }
                reply_ok(
                    key,
                    &id,
                    json!({ "applied": false, "failureReason": problems.join("; ") }),
                );
            }
        }
        "workspace/configuration" => {
            let root = with(|s| s.servers.get(key).map(|sv| sv.root.clone())).unwrap_or_default();
            let settings = config::settings(&project_config(&root), &root);
            let items: Vec<Value> = params["items"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|item| config::section(&settings, item["section"].as_str().unwrap_or("")))
                .collect();
            reply_ok(key, &id, Value::Array(items));
        }
        "workspace/workspaceFolders" => {
            let root = with(|s| s.servers.get(key).map(|sv| sv.root.clone())).unwrap_or_default();
            reply_ok(
                key,
                &id,
                json!([{ "uri": uri_of(&root), "name": root.rsplit('/').next().unwrap_or("") }]),
            );
        }
        "client/registerCapability" | "client/unregisterCapability" => {
            reply_ok(key, &id, Value::Null)
        }
        "window/workDoneProgress/create" => reply_ok(key, &id, Value::Null),
        "window/showMessageRequest" => {
            let text = params["message"].as_str().unwrap_or("").to_string();
            let actions: Vec<String> = params["actions"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|a| a["title"].as_str().map(String::from))
                .collect();
            if actions.is_empty() {
                message(&format!("{}: {text}", server_name(key)));
                return reply_ok(key, &id, Value::Null);
            }
            let k = key.to_string();
            let prompt = format!("[cairn] {}: {text} ", server_name(key));
            let titles = actions.clone();
            completing_read(&prompt, &actions, true, move |answer| {
                let chosen = answer.filter(|a| titles.contains(a));
                reply_ok(
                    &k,
                    &id,
                    chosen.map_or(Value::Null, |t| json!({ "title": t })),
                );
            });
        }
        "window/showDocument" => {
            let uri = params["uri"].as_str().unwrap_or("");
            if params["external"].as_bool() == Some(true) || !uri.starts_with("file://") {
                message(&format!("[cairn] The server wants you to open {uri}"));
                return reply_ok(key, &id, json!({ "success": false }));
            }
            let loc = params
                .get("selection")
                .and_then(|r| protocol::location(&json!({ "uri": uri, "range": r })));
            let ok = match loc {
                Some(l) => go_to(&l, false).is_ok(),
                None => Buffer::visit(&path_of(uri)).map(|b| b.show()).is_ok(),
            };
            reply_ok(key, &id, json!({ "success": ok }));
        }
        _ => {
            let error = json!({ "code": -32601, "message": format!("not supported: {method}") });
            send(key, json!({ "id": id, "error": error }));
        }
    }
}

fn on_progress(key: &str, params: &Value) {
    let token = match &params["token"] {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    let v = &params["value"];
    let kind = v["kind"].as_str().unwrap_or("");
    with(|s| {
        let Some(sv) = s.servers.get_mut(key) else {
            return;
        };
        if kind == "end" {
            sv.progress.remove(&token);
            return;
        }
        let prev = sv.progress.get(&token).cloned().unwrap_or_default();
        let title = v["title"]
            .as_str()
            .map(String::from)
            .unwrap_or_else(|| prev.split(':').next().unwrap_or("").to_string());
        let mut text = title;
        if let Some(m) = v["message"].as_str().filter(|m| !m.is_empty()) {
            text = format!("{text}: {m}");
        }
        if let Some(p) = v["percentage"].as_u64() {
            text = format!("{text} {p}%");
        }
        sv.progress.insert(token, text);
    });
    update_mode_lines_for(key);
    refresh_progress_overlay();
}

/// What every server currently reports, one row per token.
fn progress_rows() -> Vec<String> {
    with(|s| {
        let mut rows = Vec::new();
        for sv in s.servers.values() {
            for text in sv.progress.values() {
                rows.push(format!("{}: {text}", sv.name));
            }
        }
        rows
    })
}

/// Show the servers' work-done progress in the declared overlay, or take
/// the overlay down when nothing is in flight.
fn refresh_progress_overlay() {
    let rows = progress_rows();
    if rows.is_empty() {
        if let Some(id) = with(|s| s.progress_overlay.take()) {
            let _ = close_overlay(&id);
        }
        return;
    }
    let row_refs: Vec<&str> = rows.iter().map(String::as_str).collect();
    let open = with(|s| s.progress_overlay.clone());
    match open {
        Some(id) => {
            if update_overlay(&id, &row_refs).is_err() {
                // Forge closed it (C-g or overlay-close) without our hook
                // having cleared the id yet: open it again.
                with(|s| s.progress_overlay = None);
                refresh_progress_overlay();
            }
        }
        None => {
            if let Ok(id) = open_overlay("progress", &row_refs) {
                with(|s| s.progress_overlay = Some(id));
            }
        }
    }
}

/// The `overlay.closed` observer: forget the progress overlay's id when the
/// host takes it down.
fn progress_overlay_closed() -> Result<(), String> {
    let Some(call) = current_hook() else {
        return Ok(());
    };
    let payload: Value = serde_json::from_str(&call.payload).unwrap_or(Value::Null);
    let id = payload["id"].as_str().unwrap_or("");
    with(|s| {
        if s.progress_overlay.as_deref() == Some(id) {
            s.progress_overlay = None;
        }
    });
    Ok(())
}

fn handle(key: &str, msg: Value) {
    log_event(key, "server", &msg);
    let has_id = msg.get("id").is_some_and(|i| !i.is_null());
    match msg.get("method").and_then(Value::as_str) {
        Some(_) if has_id => on_server_request(key, &msg),
        Some("textDocument/publishDiagnostics") => publish_diagnostics(&msg["params"]),
        Some("$/progress") => on_progress(key, &msg["params"]),
        Some("window/showMessage") => {
            let t = msg["params"]["type"].as_u64().unwrap_or(4);
            let text = msg["params"]["message"].as_str().unwrap_or("");
            if t <= 3 {
                message(&format!("[cairn] {}: {text}", server_name(key)));
            }
            log_at(Level::Info, &format!("{}: {text}", server_name(key)));
        }
        Some("window/logMessage") => {
            log_at(
                Level::Debug,
                &format!(
                    "{}: {}",
                    server_name(key),
                    msg["params"]["message"].as_str().unwrap_or("")
                ),
            );
        }
        Some(_) => {}
        None if has_id => {
            let id = msg["id"].as_i64().unwrap_or(-1);
            let reply = with(|s| s.servers.get_mut(key).and_then(|sv| sv.pending.remove(&id)));
            if let Some(err) = msg.get("error").filter(|e| !e.is_null()) {
                let text = err["message"].as_str().unwrap_or("error");
                // A request the server dropped because things moved on
                // isn't worth a message.
                let code = err["code"].as_i64().unwrap_or(0);
                let method = reply.as_ref().map(|(m, _)| m.clone()).unwrap_or_default();
                if code != -32800 && code != -32801 {
                    log_at(
                        Level::Warn,
                        &format!("{} {method}: {text}", server_name(key)),
                    );
                    if !method.starts_with("textDocument/documentHighlight")
                        && !method.starts_with("textDocument/hover")
                        && !method.starts_with("textDocument/signatureHelp")
                    {
                        message(&format!("[cairn] {}: {text}", server_name(key)));
                    }
                }
                // A failed resolve must still unblock the completion list
                // it was holding back (the reply gets a null result).
                if method == "completionItem/resolve"
                    && let Some((_, reply)) = reply
                {
                    reply(Value::Null);
                }
            } else if let Some((_, reply)) = reply {
                reply(msg.get("result").cloned().unwrap_or(Value::Null));
            }
        }
        None => {}
    }
}

fn on_output(key: &str, data: &[u8]) {
    let (msgs, errors) = with(|s| match s.servers.get_mut(key) {
        Some(sv) => {
            sv.input.extend_from_slice(data);
            unframe(&mut sv.input)
        }
        None => (Vec::new(), Vec::new()),
    });
    for e in errors {
        log_at(Level::Warn, &format!("cairn: {e}"));
    }
    for m in msgs {
        handle(key, m);
    }
}

// ------------------------------------------------------------ servers

/// The project root for a file: what Forge says, else its directory.
fn root_of(path: &str) -> String {
    project_root(path).unwrap_or_else(|| {
        path.rsplit_once('/')
            .map_or(".".into(), |(d, _)| d.to_string())
    })
}

/// Start (or find) a server for `mode` in `root`, trying the configured
/// alternatives in order. The key, and the document's languageId.
fn start(mode: &str, root: &str) -> Option<(String, String)> {
    let cfg = project_config(root);
    let running = with(|s| {
        s.servers
            .iter()
            .find(|(_, sv)| {
                sv.root == root
                    && cfg
                        .servers_for(mode)
                        .iter()
                        .any(|sp| sp.command == sv.command)
            })
            .map(|(k, _)| k.clone())
    });
    if let Some(k) = running {
        let lang = cfg
            .servers_for(mode)
            .first()
            .map(|s| s.language_id.clone())
            .unwrap_or_default();
        return Some((k, lang));
    }
    for spec in cfg.servers_for(mode) {
        let program = spec.command[0].clone();
        if with(|s| s.failed.contains(&program)) {
            continue;
        }
        // Unique per process: a restarted server's predecessor exits
        // after it has started, and must not take it along.
        let serial = with(|s| {
            s.serial += 1;
            s.serial
        });
        let key = format!("{}\0{root}\0{serial}", spec.command.join(" "));
        let args: Vec<String> = spec.command[1..].to_vec();
        let (k1, k2) = (key.clone(), key.clone());
        let process = spawn(
            &program,
            &args,
            Some(root),
            move |data| on_output(&k1, data),
            move |code| server_exited(&k2, code),
        );
        let process = match process {
            Ok(p) => p,
            Err(e) => {
                with(|s| s.failed.insert(program.clone()));
                log_at(Level::Info, &format!("cairn: can't start {program}: {e}"));
                continue;
            }
        };
        with(|s| {
            s.servers.insert(
                key.clone(),
                Server {
                    key: key.clone(),
                    command: spec.command.clone(),
                    name: program.clone(),
                    root: root.into(),
                    process,
                    next_id: 0,
                    pending: HashMap::new(),
                    queue: Vec::new(),
                    input: Vec::new(),
                    ready: false,
                    stopping: false,
                    capabilities: Value::Null,
                    progress: BTreeMap::new(),
                    events: VecDeque::new(),
                    init_options: spec.init_options.clone(),
                },
            )
        });
        initialize(&key, root, &cfg);
        return Some((key, spec.language_id.clone()));
    }
    None
}

fn server_exited(key: &str, code: Option<i32>) {
    let server = with(|s| {
        let sv = s.servers.remove(key);
        let ids: Vec<u32> = s
            .docs
            .iter()
            .filter(|(_, d)| d.server == key)
            .map(|(id, _)| *id)
            .collect();
        for id in &ids {
            s.docs.remove(id);
            s.synced_text.remove(id);
        }
        (sv, ids)
    });
    let (sv, ids) = server;
    for id in ids {
        if let Some(b) = Buffer::from_id(id).filter(|b| b.alive()) {
            b.set_mode_line_info(None);
            let _ = decorations::set(&b, &[]);
        }
    }
    if let Some(sv) = sv.filter(|sv| !sv.stopping) {
        let how = code.map(|c| format!(" with code {c}")).unwrap_or_default();
        message(&format!(
            "[cairn] {} exited{how}; M-x cairn-reconnect starts it again",
            sv.name
        ));
        log_at(Level::Warn, &format!("{} exited{how}", sv.name));
    }
}

fn client_capabilities() -> Value {
    json!({
        "general": { "positionEncodings": ["utf-16"] },
        "workspace": {
            "configuration": true,
            "workspaceFolders": true,
            "applyEdit": true,
            "workspaceEdit": {
                "documentChanges": true,
                "resourceOperations": ["create", "rename", "delete"],
                "failureHandling": "abort",
            },
            "symbol": { "dynamicRegistration": false },
            "didChangeConfiguration": { "dynamicRegistration": false },
            "executeCommand": { "dynamicRegistration": false },
        },
        "window": {
            "workDoneProgress": true,
            "showMessage": { "messageActionItem": { "additionalPropertiesSupport": false } },
            "showDocument": { "support": true },
        },
        "textDocument": {
            "synchronization": { "didSave": true, "willSave": false, "willSaveWaitUntil": false },
            "publishDiagnostics": { "relatedInformation": false, "codeDescriptionSupport": false },
            "hover": { "contentFormat": ["plaintext", "markdown"] },
            "definition": { "linkSupport": true },
            "declaration": { "linkSupport": true },
            "implementation": { "linkSupport": true },
            "typeDefinition": { "linkSupport": true },
            "references": {},
            "documentHighlight": {},
            "documentSymbol": { "hierarchicalDocumentSymbolSupport": true },
            "rename": { "prepareSupport": false },
            "codeAction": {
                "codeActionLiteralSupport": {
                    "codeActionKind": {
                        "valueSet": [
                            "", "quickfix", "refactor", "refactor.extract", "refactor.inline",
                            "refactor.rewrite", "source", "source.organizeImports", "source.fixAll",
                        ],
                    },
                },
                "isPreferredSupport": true,
                "resolveSupport": { "properties": ["edit", "command"] },
                "dataSupport": true,
            },
            "formatting": {},
            "rangeFormatting": {},
            "signatureHelp": {
                "signatureInformation": {
                    "parameterInformation": { "labelOffsetSupport": true },
                    "documentationFormat": ["plaintext", "markdown"],
                    "activeParameterSupport": true,
                },
            },
            // No snippets: like Eglot without yasnippet, servers send plain
            // text to insert.
            "completion": { "completionItem": { "snippetSupport": false } },
            "callHierarchy": {},
            "typeHierarchy": {},
        },
    })
}

fn initialize(key: &str, root: &str, cfg: &Config) {
    let name = root.trim_end_matches('/').rsplit('/').next().unwrap_or("");
    let init_options =
        with(|s| s.servers.get(key).map(|sv| sv.init_options.clone())).unwrap_or(Value::Null);
    let init_options = if init_options.is_null() {
        // lua-language-server reads Forge's API settings from here too.
        json!({ "Lua": config::lua_settings(root) })
    } else {
        init_options
    };
    let params = json!({
        "processId": null,
        "rootUri": uri_of(root),
        "rootPath": root,
        "workspaceFolders": [{ "uri": uri_of(root), "name": name }],
        "clientInfo": { "name": "Cairn (Forge)" },
        "initializationOptions": init_options,
        "capabilities": client_capabilities(),
    });
    let k = key.to_string();
    let settings = config::settings(cfg, root);
    request(key, "initialize", params, move |result| {
        let server_name = result["serverInfo"]["name"].as_str().map(String::from);
        let queued = with(|s| {
            s.servers.get_mut(&k).map(|sv| {
                sv.ready = true;
                sv.capabilities = result["capabilities"].clone();
                if let Some(n) = server_name {
                    sv.name = n;
                }
                std::mem::take(&mut sv.queue)
            })
        });
        send(&k, json!({ "method": "initialized", "params": {} }));
        send(
            &k,
            json!({ "method": "workspace/didChangeConfiguration", "params": { "settings": settings } }),
        );
        for m in queued.into_iter().flatten() {
            send(&k, m);
        }
        let (name, root) = with(|s| {
            s.servers
                .get(&k)
                .map(|sv| (sv.name.clone(), sv.root.clone()))
        })
        .unwrap_or_default();
        message(&format!(
            "[cairn] Connected! Server `{name}' now managing buffers in project `{}'",
            root.rsplit('/').next().unwrap_or("")
        ));
        update_mode_lines_for(&k);
    });
}

/// Manage a buffer: start its server if need be, and open the document.
/// `force`: even with autostart off (M-x cairn).
fn attach_with(buf: &Buffer, force: bool) -> Result<(), String> {
    if !buf.alive() || with(|s| s.docs.contains_key(&buf.id())) {
        return Ok(());
    }
    let Some(path) = buf.path() else {
        return Err("this buffer isn't visiting a file".into());
    };
    let mode = buf.major_mode();
    let root = root_of(&path);
    let cfg = project_config(&root);
    if !cfg.handles(&mode) {
        return Err(format!(
            "no server for {mode} (see ~/.config/forge/cairn.json)"
        ));
    }
    if !cfg.autostart && !force {
        return Ok(());
    }
    let Some((key, language)) = start(&mode, &root) else {
        let tried: Vec<String> = cfg
            .servers_for(&mode)
            .iter()
            .map(|s| s.command[0].clone())
            .collect();
        return Err(format!(
            "no server for {mode} could start (tried {})",
            tried.join(", ")
        ));
    };
    let uri = uri_of(&path);
    with(|s| {
        s.docs.insert(
            buf.id(),
            Doc {
                server: key.clone(),
                uri: uri.clone(),
                language: language.clone(),
                version: 1,
                synced: buf.version(),
            },
        )
    });
    let text = buf.text();
    let doc = json!({ "uri": uri, "languageId": language, "version": 1, "text": text });
    notify(&key, "textDocument/didOpen", json!({ "textDocument": doc }));
    with(|s| s.synced_text.insert(buf.id(), text));
    if let Err(e) = keymap::bind(&Scope::Buffer(buf.id()), "M-g i", "cairn-imenu") {
        log_at(Level::Warn, &format!("cairn: M-g i: {e}"));
    }
    update_mode_line(buf);
    redecorate(buf);
    Ok(())
}

fn attach(buf: &Buffer) {
    if let Err(e) = attach_with(buf, false)
        && buf.path().is_some()
        && config().handles(&buf.major_mode())
    {
        log_at(Level::Info, &format!("cairn: {e}"));
    }
}

fn detach(id: u32) {
    let doc = with(|s| {
        s.synced_text.remove(&id);
        s.highlights.remove(&id);
        s.docs.remove(&id)
    });
    if let Some(d) = doc {
        notify(
            &d.server,
            "textDocument/didClose",
            json!({ "textDocument": { "uri": d.uri } }),
        );
    }
    if let Some(b) = Buffer::from_id(id).filter(|b| b.alive()) {
        b.set_mode_line_info(None);
        let _ = decorations::set(&b, &[]);
        let _ = keymap::unbind(&Scope::Buffer(id), "M-g i");
    }
}

/// Send the buffer's text if the server hasn't seen this version: just
/// the part that changed, with its range.
fn sync(buf: &Buffer) {
    let text = buf.text();
    let change = with(|s| {
        let d = s.docs.get_mut(&buf.id())?;
        if d.synced == buf.version() {
            return None;
        }
        let old = s
            .synced_text
            .get(&buf.id())
            .cloned()
            .unwrap_or_else(|| text.clone());
        d.version += 1;
        d.synced = buf.version();
        s.synced_text.insert(buf.id(), text.clone());
        Some((
            d.server.clone(),
            d.uri.clone(),
            d.version,
            content_change(&old, &text),
        ))
    });
    if let Some((key, uri, version, change)) = change {
        if with(|s| s.highlights.remove(&buf.id()).is_some()) {
            redecorate(buf);
        }
        let params = json!({
            "textDocument": { "uri": uri, "version": version },
            "contentChanges": [change],
        });
        notify(&key, "textDocument/didChange", params);
    }
}

/// The one `contentChanges` entry taking `old` to `new`.
fn content_change(old: &str, new: &str) -> Value {
    match protocol::minimal_edit(old, new) {
        Some(e) => json!({
            "range": {
                "start": { "line": e.start.line, "character": e.start.character },
                "end": { "line": e.end.line, "character": e.end.character },
            },
            "text": e.text,
        }),
        None => json!({ "text": new }),
    }
}

fn buffer_of(e: &Event) -> Option<Buffer> {
    Buffer::from_id(forge_plugin_sdk::event_buffer(e))
}

fn shutdown(key: &str) {
    let ids: Vec<u32> = with(|s| {
        s.docs
            .iter()
            .filter(|(_, d)| d.server == key)
            .map(|(id, _)| *id)
            .collect()
    });
    for id in ids {
        detach(id);
    }
    let found = with(|s| {
        s.servers.get_mut(key).map(|sv| {
            sv.stopping = true;
        })
    });
    if found.is_some() {
        let k = key.to_string();
        request(key, "shutdown", Value::Null, move |_| {
            send(&k, json!({ "method": "exit" }));
        });
    }
}

fn kill_server(key: &str) {
    if let Some(p) = with(|s| s.servers.get(key).map(|sv| sv.process)) {
        p.kill();
    }
}

// ----------------------------------------------------------- positions

fn current_doc() -> Result<(Buffer, String, String), String> {
    let b = Buffer::current();
    match with(|s| {
        s.docs
            .get(&b.id())
            .map(|d| (d.server.clone(), d.uri.clone()))
    }) {
        Some((key, uri)) => {
            sync(&b);
            Ok((b, key, uri))
        }
        None => Err(format!(
            "Cairn isn't managing this buffer ({}); M-x cairn starts it",
            b.major_mode()
        )),
    }
}

fn position_of_range(b: &Buffer, p: Position) -> Value {
    json!({ "line": p.line, "character": utf16_col(&b.line(p.line), p.column) })
}

fn position_of(b: &Buffer) -> Value {
    position_of_range(b, b.point())
}

fn doc_position(b: &Buffer, uri: &str) -> Value {
    json!({ "textDocument": { "uri": uri }, "position": position_of(b) })
}

/// The identifier around column `column`, as (start, end) columns.
fn word_in(line: &str, column: u32) -> Option<(u32, u32)> {
    let chars: Vec<char> = line.chars().collect();
    let ident = |c: char| c.is_alphanumeric() || c == '_';
    let col = (column as usize).min(chars.len());
    let on_word = col < chars.len() && ident(chars[col]) || col > 0 && ident(chars[col - 1]);
    if !on_word {
        return None;
    }
    let mut start = col;
    while start > 0 && ident(chars[start - 1]) {
        start -= 1;
    }
    let mut end = col;
    while end < chars.len() && ident(chars[end]) {
        end += 1;
    }
    Some((start as u32, end as u32))
}

fn word_at(b: &Buffer) -> Option<(Position, Position)> {
    let p = b.point();
    let (start, end) = word_in(&b.line(p.line), p.column)?;
    Some((
        Position {
            line: p.line,
            column: start,
        },
        Position {
            line: p.line,
            column: end,
        },
    ))
}

fn to_forge(b: &Buffer, p: &LspPos) -> Position {
    Position {
        line: p.line,
        column: char_col(&b.line(p.line), p.character),
    }
}

// ----------------------------------------------------------------- xref

fn here() -> Marker {
    let b = Buffer::current();
    Marker {
        buffer: b.id(),
        path: b.path(),
        point: b.point(),
    }
}

fn go_marker(m: &Marker) -> Result<(), String> {
    let b = match Buffer::from_id(m.buffer).filter(|b| b.alive()) {
        Some(b) => b,
        None => match &m.path {
            Some(p) => Buffer::visit(p)?,
            None => return Err("that buffer is gone".into()),
        },
    };
    b.show();
    b.set_point(m.point);
    Ok(())
}

/// Visit a location, remembering where we were (`push`) so M-, comes back.
fn go_to(loc: &LspLocation, push: bool) -> Result<(), String> {
    let from = here();
    let target = Buffer::visit(&path_of(&loc.uri))?;
    target.show();
    let p = to_forge(&target, &loc.start);
    target.set_point(p);
    attach(&target);
    if push {
        with(|s| {
            s.back.push(from);
            s.forward.clear();
        });
    }
    Ok(())
}

/// A line of a file, for listing a location (without visiting it).
fn line_of(path: &str, line: u32) -> String {
    if let Some(b) = Buffer::all()
        .into_iter()
        .find(|b| b.path().as_deref() == Some(path))
    {
        return b.line(line);
    }
    files::read(path)
        .ok()
        .and_then(|t| t.lines().nth(line as usize).map(String::from))
        .unwrap_or_default()
}

fn define_list_modes() {
    for (mode, keys) in [
        (
            "xref-mode",
            vec![
                ("RET", "xref-goto-xref"),
                ("o", "xref-show-location-at-point"),
                ("n", "xref-next-line"),
                ("p", "xref-prev-line"),
            ],
        ),
        (
            "flymake-diagnostics-buffer-mode",
            vec![
                ("RET", "xref-goto-xref"),
                ("SPC", "xref-show-location-at-point"),
                ("n", "xref-next-line"),
                ("p", "xref-prev-line"),
                ("g", "flymake-refresh-diagnostics-buffer"),
            ],
        ),
        (
            "cairn-hierarchy-mode",
            vec![
                ("RET", "xref-goto-xref"),
                ("TAB", "cairn-hierarchy-expand"),
                ("n", "xref-next-line"),
                ("p", "xref-prev-line"),
            ],
        ),
        (
            "cairn-list-connections-mode",
            vec![("k", "cairn-shutdown"), ("g", "cairn-list-connections")],
        ),
        ("cairn-events-mode", vec![("g", "cairn-events-buffer")]),
    ] {
        let _ = modes::define_major_mode(mode, Some("special-mode"), &[]);
        for (k, c) in keys {
            let _ = bind_in_mode(mode, k, c);
        }
    }
}

/// Fill a list buffer: each line with the location it points at.
fn show_list(
    name: &str,
    mode: &str,
    lines: Vec<(String, Option<LspLocation>)>,
    faces: Vec<(u32, u32, u32, &'static str)>,
    select: bool,
) -> Buffer {
    let b = match Buffer::find(name) {
        Some(b) => b,
        None => Buffer::create(name),
    };
    let _ = b.set_major_mode(mode);
    let _ = b.set_option("read-only", "true");
    let text: String = lines.iter().map(|(l, _)| format!("{l}\n")).collect();
    b.replace(
        Span {
            start: Position { line: 0, column: 0 },
            end: Position {
                line: u32::MAX,
                column: u32::MAX,
            },
        },
        &text,
    );
    let decos: Vec<Decoration> = faces
        .into_iter()
        .map(|(l, a, z, f)| Decoration {
            span: Span {
                start: Position { line: l, column: a },
                end: Position { line: l, column: z },
            },
            face: f.into(),
            message: None,
        })
        .collect();
    let _ = decorations::set(&b, &decos);
    let first = lines.iter().position(|(_, l)| l.is_some()).unwrap_or(0) as u32;
    with(|s| {
        s.lists
            .insert(b.id(), lines.into_iter().map(|(_, l)| l).collect())
    });
    if select {
        b.show();
    }
    b.set_point(Position {
        line: first,
        column: 0,
    });
    b
}

/// `*xref*`: locations grouped by file, each with its line.
fn show_xrefs(title: &str, locs: Vec<LspLocation>) {
    let mut by_file: BTreeMap<String, Vec<LspLocation>> = BTreeMap::new();
    for l in locs {
        by_file.entry(path_of(&l.uri)).or_default().push(l);
    }
    let mut lines = vec![(title.to_string(), None)];
    let mut faces = vec![(0, 0, title.chars().count() as u32, "function")];
    // File names relative to the current project, as xref shows them.
    let root = Buffer::current()
        .path()
        .map(|p| root_of(&p))
        .unwrap_or_default();
    for (path, mut locs) in by_file {
        locs.sort_by_key(|l| (l.line, l.character));
        let shown = relative(&path, &root);
        faces.push((lines.len() as u32, 0, shown.chars().count() as u32, "type"));
        lines.push((shown, None));
        for l in locs {
            let num = format!("{:>6}: ", l.line + 1);
            let text = line_of(&path, l.line).trim_end().to_string();
            faces.push((
                lines.len() as u32,
                0,
                num.chars().count() as u32,
                "constant",
            ));
            lines.push((format!("{num}{text}"), Some(l)));
        }
    }
    let from = here();
    with(|s| {
        s.back.push(from);
        s.forward.clear();
    });
    show_list("*xref*", "xref-mode", lines, faces, true);
}

/// `path` relative to `root` when it's under it.
fn relative(path: &str, root: &str) -> String {
    let root = root.trim_end_matches('/');
    match path.strip_prefix(root).and_then(|r| r.strip_prefix('/')) {
        Some(r) if !root.is_empty() => r.to_string(),
        _ => path.to_string(),
    }
}

/// Jump to one location, or list several. Also publishes the locations as
/// cross-references (`xref.publish`), so consult-xref can offer them.
fn jump_or_list(what: &str, locs: Vec<LspLocation>) {
    let name = {
        let b = Buffer::current();
        match word_at(&b) {
            Some((s, e)) => b.text_in(Span { start: s, end: e }),
            None => what.to_string(),
        }
    };
    let refs: Vec<xref::Reference> = locs
        .iter()
        .map(|l| xref::Reference {
            name: name.clone(),
            kind: what.to_string(),
            path: path_of(&l.uri),
            line: l.line + 1,
            column: l.character + 1,
        })
        .collect();
    xref::publish(&refs);
    match locs.len() {
        0 => message(&format!("No {what} found")),
        1 => {
            if let Err(e) = go_to(&locs[0], true) {
                message(&e);
            }
        }
        n => show_xrefs(&format!("{n} {what}"), locs),
    }
}

fn find_locations(method: &str, capability: &str, what: &'static str) -> Result<(), String> {
    let (b, key, uri) = current_doc()?;
    require(&key, capability, &format!("find {what}"))?;
    let mut params = doc_position(&b, &uri);
    if method == "textDocument/references" {
        params["context"] = json!({ "includeDeclaration": true });
    }
    request(&key, method, params, move |result| {
        jump_or_list(what, protocol::locations(&result));
    });
    Ok(())
}

fn list_location_at_point() -> Option<LspLocation> {
    let b = Buffer::current();
    with(|s| {
        s.lists
            .get(&b.id())
            .and_then(|l| l.get(b.point().line as usize).cloned().flatten())
    })
}

// -------------------------------------------------------------- flymake

fn diagnostics_lines(
    uris: &[String],
) -> (
    Vec<(String, Option<LspLocation>)>,
    Vec<(u32, u32, u32, &'static str)>,
) {
    let mut lines = Vec::new();
    let mut faces = Vec::new();
    for uri in uris {
        let ds = with(|s| s.diagnostics.get(uri).cloned()).unwrap_or_default();
        if ds.is_empty() {
            continue;
        }
        let path = path_of(uri);
        if uris.len() > 1 {
            faces.push((lines.len() as u32, 0, path.chars().count() as u32, "type"));
            lines.push((path.clone(), None));
        }
        let mut ds = ds;
        ds.sort_by_key(|d| (d.start.line, d.start.character));
        for d in ds {
            let kind = match d.severity {
                1 => "error",
                2 => "warning",
                3 => "note",
                _ => "hint",
            };
            let head = format!(
                "{:>5}:{:<3} {kind:<8}",
                d.start.line + 1,
                d.start.character + 1
            );
            let text = format!("{head} {}", diag_text(&d));
            faces.push((
                lines.len() as u32,
                10,
                10 + kind.len() as u32,
                face_of(d.severity),
            ));
            lines.push((
                text,
                Some(LspLocation {
                    uri: uri.clone(),
                    line: d.start.line,
                    character: d.start.character,
                    start: d.start.clone(),
                    end: d.end.clone(),
                }),
            ));
        }
    }
    (lines, faces)
}

/// The URIs a diagnostics buffer covers: one file, or a server's project.
fn diag_buffer_uris(name: &str) -> Vec<String> {
    with(|s| {
        if let Some(path) = name
            .strip_prefix("*Flymake diagnostics for `")
            .and_then(|n| n.strip_suffix("'*"))
        {
            return s
                .diagnostics
                .keys()
                .filter(|u| path_of(u) == path)
                .cloned()
                .collect();
        }
        if let Some(root) = name
            .strip_prefix("*Flymake diagnostics for project ")
            .and_then(|n| n.strip_suffix('*'))
        {
            let mut v: Vec<String> = s
                .diagnostics
                .keys()
                .filter(|u| path_of(u).starts_with(root))
                .cloned()
                .collect();
            v.sort();
            return v;
        }
        Vec::new()
    })
}

fn refresh_diagnostic_lists() {
    for b in Buffer::all() {
        let name = b.name();
        if name.starts_with("*Flymake diagnostics for ") {
            let (lines, faces) = diagnostics_lines(&diag_buffer_uris(&name));
            let point = b.point();
            let lines = if lines.is_empty() {
                vec![("No diagnostics".to_string(), None)]
            } else {
                lines
            };
            show_list(
                &name,
                "flymake-diagnostics-buffer-mode",
                lines,
                faces,
                false,
            );
            b.set_point(point);
        }
    }
}

fn show_diagnostics(name: String, uris: Vec<String>) {
    let (lines, faces) = diagnostics_lines(&uris);
    let lines = if lines.is_empty() {
        vec![("No diagnostics".to_string(), None)]
    } else {
        lines
    };
    show_list(&name, "flymake-diagnostics-buffer-mode", lines, faces, true);
}

fn goto_diagnostic(forward: bool, n: i64) -> Result<(), String> {
    let b = Buffer::current();
    let uri = with(|s| s.docs.get(&b.id()).map(|d| d.uri.clone()))
        .ok_or("No diagnostics here: Cairn isn't managing this buffer")?;
    let mut ds = with(|s| s.diagnostics.get(&uri).cloned()).unwrap_or_default();
    if ds.is_empty() {
        message("No diagnostics");
        return Ok(());
    }
    let mut spans: Vec<(Position, String)> = ds
        .drain(..)
        .map(|d| (diag_span(&b, &d).start, diag_text(&d)))
        .collect();
    spans.sort_by_key(|(p, _)| (p.line, p.column));
    let p = b.point();
    let key = |q: &Position| (q.line, q.column);
    let mut target = None;
    let mut from = key(&p);
    for _ in 0..n.max(1) {
        let next = if forward {
            spans.iter().find(|(q, _)| key(q) > from)
        } else {
            spans.iter().rev().find(|(q, _)| key(q) < from)
        };
        match next {
            Some(x) => {
                from = key(&x.0);
                target = Some(x.clone());
            }
            None => break,
        }
    }
    match target {
        Some((q, text)) => {
            b.set_point(q);
            message(&text);
        }
        None => message(if forward {
            "No more diagnostics"
        } else {
            "No earlier diagnostics"
        }),
    }
    Ok(())
}

// ----------------------------------------------------------------- eldoc

fn diagnostic_at(b: &Buffer) -> Option<String> {
    let uri = with(|s| s.docs.get(&b.id()).map(|d| d.uri.clone()))?;
    let p = b.point();
    let ds = with(|s| s.diagnostics.get(&uri).cloned())?;
    ds.iter()
        .find(|d| {
            let sp = diag_span(b, d);
            (sp.start.line, sp.start.column) <= (p.line, p.column)
                && (p.line, p.column) < (sp.end.line, sp.end.column)
        })
        .map(diag_text)
}

/// Show `text` in the echo area if nothing else is there (or eldoc put
/// what is there).
fn eldoc_message(text: &str) {
    let current = minibuffer::current_message();
    let ours = with(|s| s.eldoc_shown.clone());
    if current.is_some() && current != ours {
        return;
    }
    message(text);
    with(|s| s.eldoc_shown = Some(text.to_string()));
}

/// The name before the parentheses point is in (for the echo area).
fn callee(b: &Buffer) -> String {
    let p = b.point();
    let line: Vec<char> = b.line(p.line).chars().collect();
    let mut depth = 0i32;
    let mut i = (p.column as usize).min(line.len());
    while i > 0 {
        i -= 1;
        match line[i] {
            ')' => depth += 1,
            '(' if depth == 0 => {
                let end = i;
                let mut start = end;
                while start > 0 && (line[start - 1].is_alphanumeric() || line[start - 1] == '_') {
                    start -= 1;
                }
                return line[start..end].iter().collect();
            }
            '(' => depth -= 1,
            _ => {}
        }
    }
    String::new()
}

/// Is point inside the parentheses of a call?
fn in_call(b: &Buffer) -> bool {
    let p = b.point();
    let line: Vec<char> = b.line(p.line).chars().collect();
    let mut depth = 0i32;
    for c in line[..(p.column as usize).min(line.len())].iter().rev() {
        match c {
            ')' => depth += 1,
            '(' if depth == 0 => return true,
            '(' => depth -= 1,
            _ => {}
        }
    }
    false
}

/// Documentation for what's at point (`force`: asked for, so say so when
/// there is none, and don't mind what the echo area shows).
fn eldoc(force: bool) {
    let Ok((b, key, uri)) = current_doc() else {
        if force {
            message("Cairn isn't managing this buffer");
        }
        return;
    };
    let p = b.point();
    let spot = (b.id(), b.version(), p.line, p.column);
    if !force && with(|s| s.last_eldoc == Some(spot)) {
        return;
    }
    with(|s| s.last_eldoc = Some(spot));
    let params = doc_position(&b, &uri);
    // Signature help inside a call, else the symbol's documentation.
    let word = word_at(&b);
    let on_word = word.is_some();
    // On a function name that's being called, its documentation; on an
    // argument, the signature of the call.
    let on_callee =
        word.is_some_and(|(_, end)| b.line(end.line).chars().nth(end.column as usize) == Some('('));
    let use_sig = !on_callee && in_call(&b) && can(&key, "signatureHelpProvider");
    if diagnostic_at(&b).is_some() && !force {
        // Flymake's message is what shows here.
    } else if use_sig {
        let k2 = key.clone();
        let p2 = params.clone();
        let callee = callee(&b);
        request(&key, "textDocument/signatureHelp", params, move |result| {
            if !force && !still_at(spot) {
                return;
            }
            let text = protocol::signature_line(&result, &callee);
            if text.is_empty() {
                if on_word {
                    eldoc_hover(&k2, p2, force, spot);
                }
                return;
            }
            with(|s| s.eldoc_doc = text.clone());
            if force {
                message(&text);
            } else {
                eldoc_message(&text);
            }
        });
    } else if on_word || force {
        eldoc_hover(&key, params, force, spot);
    }
    if !force {
        highlight(&b, &key, &uri);
    }
}

/// Is point still where eldoc asked about?
fn still_at(spot: (u32, u64, u32, u32)) -> bool {
    let c = Buffer::current();
    let p = c.point();
    (c.id(), c.version(), p.line, p.column) == spot
}

fn eldoc_hover(key: &str, params: Value, force: bool, spot: (u32, u64, u32, u32)) {
    if !can(key, "hoverProvider") {
        if force {
            message("No documentation here");
        }
        return;
    }
    request(key, "textDocument/hover", params, move |result| {
        if !force && !still_at(spot) {
            return;
        }
        let full = result
            .get("contents")
            .map(protocol::markup_text)
            .unwrap_or_default();
        with(|s| s.eldoc_doc = full);
        let text = result
            .get("contents")
            .map(protocol::hover_summary)
            .unwrap_or_default();
        if text.is_empty() {
            if force {
                message("No documentation here");
            }
        } else if force {
            message(&text);
        } else {
            eldoc_message(&text);
        }
    });
}

/// documentHighlight: the other occurrences of the symbol at point.
fn highlight(b: &Buffer, key: &str, uri: &str) {
    if !with(|s| s.config.highlight) || !can(key, "documentHighlightProvider") {
        return;
    }
    let id = b.id();
    if word_at(b).is_none() {
        if with(|s| s.highlights.remove(&id).is_some()) {
            redecorate(b);
        }
        return;
    }
    let version = b.version();
    request(
        key,
        "textDocument/documentHighlight",
        doc_position(b, uri),
        move |result| {
            let Some(b) = Buffer::from_id(id).filter(|b| b.alive() && b.version() == version)
            else {
                return;
            };
            let spans: Vec<Span> = result
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|h| protocol::location(&json!({ "uri": "", "range": h["range"] })))
                .map(|l| Span {
                    start: to_forge(&b, &l.start),
                    end: to_forge(&b, &l.end),
                })
                .collect();
            // One occurrence is just the one at point: nothing to show.
            let spans = if spans.len() < 2 { Vec::new() } else { spans };
            with(|s| s.highlights.insert(id, spans));
            redecorate(&b);
        },
    );
}

fn show_doc_buffer(text: &str) {
    let b = match Buffer::find("*eldoc*") {
        Some(b) => b,
        None => Buffer::create("*eldoc*"),
    };
    let _ = b.set_major_mode("special-mode");
    let _ = b.set_option("read-only", "true");
    b.replace(
        Span {
            start: Position { line: 0, column: 0 },
            end: Position {
                line: u32::MAX,
                column: u32::MAX,
            },
        },
        text,
    );
    b.set_point(Position { line: 0, column: 0 });
    b.show();
}

// -------------------------------------------------------------- editing

/// Apply text edits to one buffer, back to front. Returns (applied,
/// problems).
fn apply_edits(buf: &Buffer, mut edits: Vec<LspEdit>) -> (usize, Vec<String>) {
    let mut problems = Vec::new();
    edits.sort_by_key(|e| std::cmp::Reverse((e.start.line, e.start.character)));
    let total = edits.len();
    for e in edits {
        let start = to_forge(buf, &e.start);
        let end = to_forge(buf, &e.end);
        if end.line < start.line || (end.line == start.line && end.column < start.column) {
            problems.push(format!("inverted range at line {}", start.line + 1));
            continue;
        }
        buf.replace(Span { start, end }, &e.text);
    }
    (total - problems.len(), problems)
}

fn diagnostics_at(id: u32, line: u32) -> Vec<Value> {
    let uri = with(|s| s.docs.get(&id).map(|d| d.uri.clone()));
    let Some(uri) = uri else { return Vec::new() };
    with(|s| {
        s.diagnostics
            .get(&uri)
            .map(|ds| {
                ds.iter()
                    .filter(|d| d.start.line <= line && line <= d.end.line)
                    .map(|d| d.raw.clone())
                    .collect()
            })
            .unwrap_or_default()
    })
}

fn pick(prompt: &str, labels: &[String], f: impl FnOnce(usize) + 'static) {
    let owned: Vec<String> = labels.to_vec();
    let answers = owned.clone();
    completing_read(prompt, &owned, true, move |answer| {
        if let Some(a) = answer.and_then(|a| answers.iter().position(|l| *l == a)) {
            f(a);
        }
    });
}

/// Run a code action: resolve it if the server wants that, then apply its
/// edit and run its command.
fn run_action(key: String, action: Value) {
    let needs_resolve = action.get("edit").is_none()
        && action.get("command").map_or(true, |c| !c.is_object())
        && action.get("data").is_some()
        && with(|s| {
            s.servers.get(&key).is_some_and(|sv| {
                sv.capabilities["codeActionProvider"]["resolveProvider"] == json!(true)
            })
        });
    if needs_resolve {
        let k = key.clone();
        return request(&key, "codeAction/resolve", action, move |resolved| {
            perform_action(&k, &resolved)
        });
    }
    perform_action(&key, &action);
}

fn perform_action(key: &str, action: &Value) {
    let title = action["title"].as_str().unwrap_or("action").to_string();
    if let Some(edit) = action.get("edit").filter(|e| !e.is_null()) {
        let (n, problems) = apply_workspace_edit(edit);
        for p in &problems {
            log_at(Level::Warn, &format!("cairn-code-actions: {p}"));
        }
        let failed = if problems.is_empty() {
            String::new()
        } else {
            format!(" ({} failed)", problems.len())
        };
        message(&format!("{title}: {n} change(s){failed}"));
    }
    let cmd = if action["command"].is_string() {
        action.clone()
    } else {
        action["command"].clone()
    };
    if let Some(name) = cmd["command"].as_str() {
        let params = json!({ "command": name, "arguments": cmd["arguments"].as_array().cloned().unwrap_or_default() });
        request(key, "workspace/executeCommand", params, |_| {});
    }
}

/// Ask for code actions (of `only` kinds) over the region or point's line,
/// then let the user pick (or, with exactly one of a given kind, run it).
fn code_actions(only: Option<&'static str>) -> Result<(), String> {
    let (b, key, uri) = current_doc()?;
    require(&key, "codeActionProvider", "do code actions")?;
    let (start, end) = match b.region() {
        Some(r) => (r.start, r.end),
        None => {
            let p = b.point();
            let len = b.line(p.line).chars().count() as u32;
            (
                Position {
                    line: p.line,
                    column: p.column,
                },
                Position {
                    line: p.line,
                    column: len.max(p.column),
                },
            )
        }
    };
    let mut context = json!({ "diagnostics": diagnostics_at(b.id(), start.line) });
    if let Some(k) = only {
        context["only"] = json!([k]);
    }
    let params = json!({
        "textDocument": { "uri": uri },
        "range": { "start": position_of_range(&b, start), "end": position_of_range(&b, end) },
        "context": context,
    });
    request(
        &key.clone(),
        "textDocument/codeAction",
        params,
        move |result| {
            let actions: Vec<Value> = result
                .as_array()
                .into_iter()
                .flatten()
                .filter(|a| a.get("disabled").is_none())
                .filter(|a| {
                    only.is_none_or(|k| {
                        a["kind"]
                            .as_str()
                            .is_none_or(|ak| ak == k || ak.starts_with(&format!("{k}.")))
                    })
                })
                .cloned()
                .collect();
            if actions.is_empty() {
                return message(
                    match only {
                        Some(k) => format!("No \"{k}\" code actions here"),
                        None => "No code actions here".into(),
                    }
                    .as_str(),
                );
            }
            // Eglot runs a lone action of the asked-for kind straight away.
            if only.is_some() && actions.len() == 1 {
                return run_action(key, actions[0].clone());
            }
            let labels: Vec<String> = actions
                .iter()
                .map(|a| {
                    let t = a["title"].as_str().unwrap_or("?");
                    if a["isPreferred"] == json!(true) {
                        format!("{t} (preferred)")
                    } else {
                        t.to_string()
                    }
                })
                .collect();
            pick("Code action: ", &labels, move |i| {
                run_action(key, actions[i].clone())
            });
        },
    );
    Ok(())
}

fn format(b: Buffer, key: String, uri: String) -> Result<(), String> {
    let tab_size = b
        .get_option("indent-width")
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(4);
    let insert_spaces = b.get_option("indent-tabs").as_deref() != Some("true");
    let options = json!({ "tabSize": tab_size, "insertSpaces": insert_spaces });
    let (method, params) = match b.region() {
        Some(r) => {
            require(&key, "documentRangeFormattingProvider", "format a region")?;
            (
                "textDocument/rangeFormatting",
                json!({ "textDocument": { "uri": uri }, "options": options,
                        "range": { "start": position_of_range(&b, r.start), "end": position_of_range(&b, r.end) } }),
            )
        }
        None => {
            require(&key, "documentFormattingProvider", "format")?;
            (
                "textDocument/formatting",
                json!({ "textDocument": { "uri": uri }, "options": options }),
            )
        }
    };
    request(&key, method, params, move |result| {
        let edits = protocol::text_edits(&result);
        if edits.is_empty() {
            return message("Nothing to format");
        }
        let (n, problems) = apply_edits(&b, edits);
        for p in &problems {
            log_at(Level::Warn, &format!("cairn-format: {p}"));
        }
        message(&format!(
            "Formatted {n} span(s); not saved{}",
            if problems.is_empty() {
                String::new()
            } else {
                format!(", {} failed", problems.len())
            }
        ));
    });
    Ok(())
}

fn rename() -> Result<(), String> {
    let (b, key, uri) = current_doc()?;
    require(&key, "renameProvider", "rename")?;
    let (start, end) = word_at(&b).unwrap_or_else(|| (b.point(), b.point()));
    let old = b.text_in(Span { start, end });
    let params = json!({ "textDocument": { "uri": uri }, "position": position_of_range(&b, start), "newName": "" });
    let prompt = if old.is_empty() {
        "Rename to: ".to_string()
    } else {
        format!("Rename `{old}' to: ")
    };
    // Like Eglot: nothing to edit, the old name as the default.
    read_string(&prompt, "", move |answer| {
        let Some(new_name) = answer.filter(|n| !n.is_empty()) else {
            return;
        };
        if new_name == old {
            return message(&format!("{old} is already its name"));
        }
        let mut params = params;
        params["newName"] = json!(new_name);
        request(&key, "textDocument/rename", params, move |result| {
            if result.is_null() {
                return message("The server can't rename that");
            }
            let (n, problems) = apply_workspace_edit(&result);
            for p in &problems {
                log_at(Level::Warn, &format!("cairn-rename: {p}"));
            }
            message(&format!(
                "Renamed {n} place{}{}",
                if n == 1 { "" } else { "s" },
                if problems.is_empty() {
                    String::new()
                } else {
                    format!(" ({} failed)", problems.len())
                }
            ));
        });
    });
    Ok(())
}

// ------------------------------------------------------------ hierarchy

fn hierarchy_label(item: &Value) -> String {
    let name = item["name"].as_str().unwrap_or("?");
    let kind = protocol::symbol_kind(item["kind"].as_u64());
    let detail = item["detail"]
        .as_str()
        .filter(|d| !d.is_empty())
        .map(|d| format!(" {d}"))
        .unwrap_or_default();
    let file = item["uri"].as_str().map(path_of).unwrap_or_default();
    let file = file.rsplit('/').next().unwrap_or("").to_string();
    let line = item["selectionRange"]["start"]["line"]
        .as_u64()
        .unwrap_or(0)
        + 1;
    format!("{name} ({kind}){detail}  {file}:{line}")
}

fn item_location(item: &Value) -> Option<LspLocation> {
    protocol::location(&json!({ "uri": item["uri"], "range": item["selectionRange"] }))
}

fn render_hierarchy(id: u32) {
    let Some(h) = with(|s| s.hierarchies.get(&id).cloned()) else {
        return;
    };
    let root = h
        .nodes
        .first()
        .map(|n| n.item["name"].as_str().unwrap_or("?").to_string())
        .unwrap_or_default();
    let title = match h.direction.as_str() {
        "incoming" => format!("Calls to {root}"),
        "outgoing" => format!("Calls from {root}"),
        "supertypes" => format!("Supertypes of {root}"),
        _ => format!("Subtypes of {root}"),
    };
    let mut lines = vec![(title.clone(), None)];
    let mut faces = vec![(0, 0, title.chars().count() as u32, "function")];
    for n in &h.nodes {
        let mark = if n.expanded { "-" } else { "+" };
        let text = format!(
            "{}{mark} {}",
            "  ".repeat(n.depth),
            hierarchy_label(&n.item)
        );
        let at = n.at.clone().or_else(|| item_location(&n.item));
        let indent = (n.depth * 2 + 2) as u32;
        let name_len = n.item["name"].as_str().map_or(0, |s| s.chars().count()) as u32;
        faces.push((lines.len() as u32, indent, indent + name_len, "function"));
        lines.push((text, at));
    }
    let name = Buffer::from_id(id)
        .map(|b| b.name())
        .unwrap_or_else(|| "*CAIRN hierarchy*".into());
    show_list(&name, "cairn-hierarchy-mode", lines, faces, false);
}

/// The children of a hierarchy item, then `then` with them.
fn hierarchy_children(
    key: &str,
    calls: bool,
    direction: &str,
    item: Value,
    then: impl FnOnce(Vec<(Value, Option<LspLocation>)>) + 'static,
) {
    let method = if calls {
        format!("callHierarchy/{direction}Calls")
    } else {
        format!("typeHierarchy/{direction}")
    };
    let dir = direction.to_string();
    let parent_uri = item["uri"].clone();
    request(key, &method, json!({ "item": item }), move |result| {
        let kids = result
            .as_array()
            .into_iter()
            .flatten()
            .map(|r| {
                if !calls {
                    return (r.clone(), None);
                }
                let node = if dir == "incoming" {
                    r["from"].clone()
                } else {
                    r["to"].clone()
                };
                // Where the call is: fromRanges are in the caller, which
                // is the node for incoming calls and the parent for
                // outgoing ones.
                let uri = if dir == "incoming" {
                    node["uri"].clone()
                } else {
                    parent_uri.clone()
                };
                let at = r["fromRanges"]
                    .as_array()
                    .and_then(|rs| rs.first())
                    .and_then(|range| protocol::location(&json!({ "uri": uri, "range": range })));
                (node, at)
            })
            .collect();
        then(kids);
    });
}

fn show_hierarchy(calls: bool) -> Result<(), String> {
    let (b, key, uri) = current_doc()?;
    let (cap, prepare) = if calls {
        ("callHierarchyProvider", "textDocument/prepareCallHierarchy")
    } else {
        ("typeHierarchyProvider", "textDocument/prepareTypeHierarchy")
    };
    require(
        &key,
        cap,
        if calls {
            "show call hierarchies"
        } else {
            "show type hierarchies"
        },
    )?;
    let k = key.clone();
    request(&key, prepare, doc_position(&b, &uri), move |result| {
        let Some(item) = result.as_array().and_then(|a| a.first()).cloned() else {
            return message("Nothing here to show a hierarchy for");
        };
        let directions: Vec<String> = if calls {
            vec!["incoming".into(), "outgoing".into()]
        } else {
            vec!["supertypes".into(), "subtypes".into()]
        };
        let k2 = k.clone();
        let labels = directions.clone();
        pick("Direction: ", &labels, move |i| {
            let dir = directions[i].clone();
            let name = format!("*CAIRN {} hierarchy*", if calls { "call" } else { "type" });
            let b = match Buffer::find(&name) {
                Some(b) => b,
                None => Buffer::create(&name),
            };
            let id = b.id();
            with(|s| {
                s.hierarchies.insert(
                    id,
                    Hierarchy {
                        server: k2.clone(),
                        direction: dir.clone(),
                        calls,
                        nodes: vec![Node {
                            item: item.clone(),
                            depth: 0,
                            expanded: false,
                            at: None,
                        }],
                    },
                )
            });
            render_hierarchy(id);
            b.show();
            expand_node(id, 0);
        });
    });
    Ok(())
}

fn expand_node(id: u32, index: usize) {
    let Some(h) = with(|s| s.hierarchies.get(&id).cloned()) else {
        return;
    };
    let Some(node) = h.nodes.get(index).cloned() else {
        return;
    };
    if node.expanded {
        // Collapse: drop the deeper nodes after it.
        with(|s| {
            if let Some(h) = s.hierarchies.get_mut(&id) {
                let end = (index + 1..h.nodes.len())
                    .find(|&j| h.nodes[j].depth <= node.depth)
                    .unwrap_or(h.nodes.len());
                h.nodes.drain(index + 1..end);
                h.nodes[index].expanded = false;
            }
        });
        return render_hierarchy(id);
    }
    hierarchy_children(
        &h.server,
        h.calls,
        &h.direction,
        node.item.clone(),
        move |kids| {
            with(|s| {
                if let Some(h) = s.hierarchies.get_mut(&id) {
                    if index >= h.nodes.len() {
                        return;
                    }
                    h.nodes[index].expanded = true;
                    let depth = h.nodes[index].depth + 1;
                    let new: Vec<Node> = kids
                        .into_iter()
                        .map(|(item, at)| Node {
                            item,
                            depth,
                            expanded: false,
                            at,
                        })
                        .collect();
                    h.nodes.splice(index + 1..index + 1, new);
                }
            });
            render_hierarchy(id);
            if let Some(b) = Buffer::from_id(id) {
                b.set_point(Position {
                    line: index as u32 + 1,
                    column: 0,
                });
            }
        },
    );
}

// --------------------------------------------------------------- imenu

fn imenu() -> Result<(), String> {
    let (b, key, uri) = current_doc()?;
    require(&key, "documentSymbolProvider", "list symbols")?;
    let id = b.id();
    request(
        &key,
        "textDocument/documentSymbol",
        json!({ "textDocument": { "uri": uri } }),
        move |result| {
            let syms = protocol::symbols(&result);
            if syms.is_empty() {
                return message("No symbols");
            }
            // Plain names, as imenu shows them; the kind only tells repeats
            // apart.
            let mut labels: Vec<String> = syms.iter().map(|s| s.name.clone()).collect();
            for i in 0..labels.len() {
                if syms.iter().filter(|t| t.name == syms[i].name).count() > 1 {
                    labels[i] = format!(
                        "{} ({}, line {})",
                        syms[i].name,
                        syms[i].kind,
                        syms[i].start.line + 1
                    );
                }
            }
            pick("Index item: ", &labels, move |i| {
                let Some(b) = Buffer::from_id(id) else { return };
                let from = here();
                b.show();
                b.set_point(to_forge(&b, &syms[i].start));
                with(|s| {
                    s.back.push(from);
                    s.forward.clear();
                });
            });
        },
    );
    Ok(())
}

fn workspace_symbols() -> Result<(), String> {
    let (b, key, _) = current_doc()?;
    require(
        &key,
        "workspaceSymbolProvider",
        "search the workspace's symbols",
    )?;
    let initial = word_at(&b)
        .map(|(a, z)| b.text_in(Span { start: a, end: z }))
        .unwrap_or_default();
    read_string(
        "Search for pattern (workspace symbols): ",
        &initial,
        move |answer| {
            let Some(q) = answer.filter(|q| !q.is_empty()) else {
                return;
            };
            request(
                &key,
                "workspace/symbol",
                json!({ "query": q }),
                move |result| {
                    let locs: Vec<LspLocation> = protocol::symbols(&result)
                        .into_iter()
                        .filter_map(|s| {
                            let uri = s.uri?;
                            Some(LspLocation {
                                uri,
                                line: s.start.line,
                                character: s.start.character,
                                start: s.start.clone(),
                                end: s.start,
                            })
                        })
                        .collect();
                    jump_or_list("symbols", locs);
                },
            );
        },
    );
    Ok(())
}

// ---------------------------------------------------------- connections

fn list_connections() {
    let rows: Vec<(String, String)> = with(|s| {
        let mut v: Vec<(String, String)> = s
            .servers
            .values()
            .map(|sv| {
                let n = s.docs.values().filter(|d| d.server == sv.key).count();
                let state = if sv.ready { "" } else { " (starting)" };
                let progress = sv
                    .progress
                    .values()
                    .last()
                    .map(|p| format!(" [{p}]"))
                    .unwrap_or_default();
                (
                    sv.key.clone(),
                    format!(
                        "{:<24} {:<30} {n} buffer{}{state}{progress}",
                        sv.name,
                        sv.root,
                        if n == 1 { "" } else { "s" }
                    ),
                )
            })
            .collect();
        v.sort_by(|a, b| a.1.cmp(&b.1));
        v
    });
    let mut lines = vec![(format!("{:<24} {:<30} Buffers", "Server", "Project"), None)];
    lines.extend(rows.iter().map(|(_, l)| (l.clone(), None)));
    if rows.is_empty() {
        lines.push(("No servers running".into(), None));
    }
    let b = show_list(
        "*CAIRN connections*",
        "cairn-list-connections-mode",
        lines,
        vec![(0, 0, 60, "type")],
        true,
    );
    with(|s| s.lists.insert(b.id(), Vec::new()));
    CONNECTION_ROWS.with(|r| *r.borrow_mut() = rows.into_iter().map(|(k, _)| k).collect());
}

thread_local! {
    /// The servers listed in *CAIRN connections*, by line (from line 1).
    static CONNECTION_ROWS: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
}

/// The server for the current buffer, or the one at point in the
/// connections list.
fn current_server() -> Result<String, String> {
    let b = Buffer::current();
    if b.name() == "*CAIRN connections*" {
        let line = b.point().line as usize;
        let row = CONNECTION_ROWS.with(|r| r.borrow().get(line.wrapping_sub(1)).cloned());
        if let Some(k) = row.filter(|k| with(|s| s.servers.contains_key(k))) {
            return Ok(k);
        }
    }
    if let Some(k) = with(|s| s.docs.get(&b.id()).map(|d| d.server.clone())) {
        return Ok(k);
    }
    if let Some(name) = b
        .name()
        .strip_prefix("*CAIRN events: ")
        .and_then(|n| n.strip_suffix('*'))
    {
        if let Some(k) = with(|s| {
            s.servers
                .values()
                .find(|sv| format!("{} {}", sv.name, sv.root) == name)
                .map(|sv| sv.key.clone())
        }) {
            return Ok(k);
        }
    }
    let keys: Vec<String> = with(|s| s.servers.keys().cloned().collect());
    match keys.as_slice() {
        [one] => Ok(one.clone()),
        [] => Err("No language servers running".into()),
        _ => Err(
            "Several servers run: use this in a buffer one manages (or M-x cairn-list-connections)"
                .into(),
        ),
    }
}

fn events_buffer(key: &str) {
    let (title, lines) = with(|s| {
        s.servers
            .get(key)
            .map(|sv| {
                (
                    format!("*CAIRN events: {} {}*", sv.name, sv.root),
                    sv.events.iter().cloned().collect::<Vec<_>>(),
                )
            })
            .unwrap_or_default()
    });
    let rows: Vec<(String, Option<LspLocation>)> = lines.into_iter().map(|l| (l, None)).collect();
    let faces: Vec<(u32, u32, u32, &'static str)> = rows
        .iter()
        .enumerate()
        .map(|(i, (l, _))| {
            (
                i as u32,
                0,
                l.find(']').map_or(0, |e| e + 1) as u32,
                if l.starts_with("[client]") {
                    "keyword"
                } else {
                    "string"
                },
            )
        })
        .collect();
    let b = show_list(&title, "cairn-events-mode", rows, faces, true);
    let last = b.line_count().saturating_sub(1);
    b.set_point(Position {
        line: last,
        column: 0,
    });
}

// ----------------------------------------------------------------- init

fn reconnect(key: &str) {
    let (root, buffers): (String, Vec<u32>) = with(|s| {
        (
            s.servers
                .get(key)
                .map(|sv| sv.root.clone())
                .unwrap_or_default(),
            s.docs
                .iter()
                .filter(|(_, d)| d.server == key)
                .map(|(id, _)| *id)
                .collect(),
        )
    });
    shutdown(key);
    kill_server(key);
    with(|s| {
        s.servers.remove(key);
        s.failed.clear();
        s.project_configs.remove(&root);
    });
    for id in buffers {
        if let Some(b) = Buffer::from_id(id).filter(|b| b.alive()) {
            let _ = attach_with(&b, true);
        }
    }
    message("[cairn] Reconnecting");
}

/// Where the selected completion is defined (corfu's M-g). LSP has no
/// position-free definition request, so this asks `workspace/symbol` for
/// the name and reports the first place the server says it is defined.
fn resolve_location(sel: &Selection) {
    let name = sel.text.clone();
    let (_, key, _) = match current_doc() {
        Ok(v) => v,
        Err(e) => return message(&e),
    };
    if let Err(e) = require(&key, "workspaceSymbolProvider", "find a definition") {
        return message(&e);
    }
    request(
        &key,
        "workspace/symbol",
        json!({ "query": name }),
        move |result| match protocol::symbol_named(&result, &name) {
            Some(loc) => {
                let path = path_of(&loc.uri);
                let root = Buffer::current()
                    .path()
                    .map(|p| root_of(&p))
                    .unwrap_or_default();
                message(&format!(
                    "{name} ({}:{}:{})",
                    relative(&path, &root),
                    loc.line + 1,
                    loc.character + 1
                ));
            }
            None => message(&format!("No location for {name}")),
        },
    );
}

fn init() -> Result<(), String> {
    define_list_modes();

    on(EventKind::BufferOpened, |e| {
        if let Some(b) = buffer_of(e) {
            attach(&b);
        }
    });
    on(EventKind::ModeChanged, |e| {
        let Some(b) = buffer_of(e) else { return };
        let managed_lang = with(|s| s.docs.get(&b.id()).map(|d| d.language.clone()));
        let handles = config().handles(&b.major_mode());
        if managed_lang.is_some() && !handles {
            detach(b.id());
        }
        attach(&b);
    });
    on(EventKind::BufferChanged, |e| {
        if let Some(b) = buffer_of(e) {
            sync(&b);
        }
    });
    on(EventKind::BufferSaved, |e| {
        let Some(b) = buffer_of(e) else { return };
        if let Some((key, uri)) = with(|s| {
            s.docs
                .get(&b.id())
                .map(|d| (d.server.clone(), d.uri.clone()))
        }) {
            sync(&b);
            notify(
                &key,
                "textDocument/didSave",
                json!({ "textDocument": { "uri": uri } }),
            );
        }
    });
    on(EventKind::BufferKilled, |e| {
        detach(forge_plugin_sdk::event_buffer(e))
    });

    let modes: Vec<String> = config::default_servers()
        .iter()
        .flat_map(|s| s.modes.clone())
        .collect();
    completion_source("cairn", &modes, |req| {
        let Some(b) = Buffer::from_id(req.buffer) else {
            return;
        };
        let here = Span {
            start: req.point,
            end: req.point,
        };
        let Some((key, uri)) = with(|s| {
            s.docs
                .get(&b.id())
                .map(|d| (d.server.clone(), d.uri.clone()))
        }) else {
            return req.deliver(here, &[]);
        };
        sync(&b);
        let line = b.line(req.point.line);
        let chars: Vec<char> = line.chars().collect();
        let mut start = (req.point.column as usize).min(chars.len());
        while start > 0 && (chars[start - 1].is_alphanumeric() || chars[start - 1] == '_') {
            start -= 1;
        }
        let params = json!({
            "textDocument": { "uri": uri },
            "position": { "line": req.point.line, "character": utf16_col(&line, req.point.column) },
        });
        let server = key.clone();
        request(&server, "textDocument/completion", params, move |result| {
            let mut items: Vec<Value> = match result {
                Value::Array(a) => a,
                Value::Object(mut o) => match o.remove("items") {
                    Some(Value::Array(a)) => a,
                    _ => Vec::new(),
                },
                _ => Vec::new(),
            };
            items.sort_by_key(|v| {
                v["sortText"]
                    .as_str()
                    .or(v["label"].as_str())
                    .unwrap_or("")
                    .to_string()
            });
            items.truncate(200);
            let cands: Vec<DetailedCandidate> = items
                .iter()
                .map(|v| {
                    let (t, a) = protocol::completion_item(v);
                    detailed_candidate(
                        t,
                        a.as_deref(),
                        protocol::completion_margin(v).as_deref(),
                        protocol::completion_deprecated(v),
                        protocol::completion_documentation(v).as_deref(),
                        None,
                    )
                })
                .collect();
            let span = Span {
                start: Position {
                    line: req.point.line,
                    column: start as u32,
                },
                end: req.point,
            };
            // Ask the server for the documentation of the items that came
            // without it, then hand them all to Forge together. Only items
            // carrying `data` can gain anything from a resolve.
            let missing: Vec<usize> = if resolves_completions(&key) {
                items
                    .iter()
                    .enumerate()
                    .filter(|(i, v)| cands[*i].documentation.is_none() && v.get("data").is_some())
                    .map(|(i, _)| i)
                    .collect()
            } else {
                Vec::new()
            };
            if missing.is_empty() {
                req.deliver_detailed(span, &cands);
                return;
            }
            let rid = with(|s| {
                s.serial += 1;
                s.serial as u32
            });
            let remaining = missing.len();
            with(|s| {
                s.resolving.insert(
                    rid,
                    Resolving {
                        req,
                        span,
                        cands,
                        remaining,
                    },
                );
            });
            for i in missing {
                let item = items[i].clone();
                request(&key, "completionItem/resolve", item, move |result| {
                    let doc = protocol::completion_documentation(&result);
                    let done = with(|s| {
                        let Some(r) = s.resolving.get_mut(&rid) else {
                            return false;
                        };
                        if let Some(d) = doc {
                            r.cands[i].documentation = Some(d);
                        }
                        r.remaining -= 1;
                        r.remaining == 0
                    });
                    if done && let Some(r) = with(|s| s.resolving.remove(&rid)) {
                        r.req.deliver_detailed(r.span, &r.cands);
                    }
                });
            }
        });
    })?;
    on_resolve_location(resolve_location);

    command(
        "cairn-doctor",
        "Check that the language servers Cairn may start are installed.",
        |_| doctor(),
    )?;
    command(
        "cairn-progress-closed",
        "Forget a progress overlay that Forge closed.",
        |_| progress_overlay_closed(),
    )?;

    // --- servers
    command(
        "cairn",
        "Manage this buffer with a language server (start one for the project if need be).",
        |_| {
            let b = Buffer::current();
            if with(|s| s.docs.contains_key(&b.id())) {
                let key = current_server()?;
                message(&format!(
                    "[cairn] {} already manages this buffer (M-x cairn-reconnect restarts it)",
                    server_name(&key)
                ));
                return Ok(());
            }
            with(|s| s.failed.clear());
            attach_with(&b, true)
        },
    )?;
    command(
        "cairn-reconnect",
        "Restart the current buffer's language server, keeping its buffers.",
        |_| {
            let key = current_server()?;
            reconnect(&key);
            Ok(())
        },
    )?;
    command(
        "cairn-shutdown",
        "Shut the current buffer's language server down.",
        |_| {
            let key = current_server()?;
            let name = server_name(&key);
            shutdown(&key);
            message(&format!("[cairn] Asking {name} politely to terminate"));
            Ok(())
        },
    )?;
    command(
        "cairn-shutdown-all",
        "Shut every language server down.",
        |_| {
            let keys: Vec<String> = with(|s| s.servers.keys().cloned().collect());
            let n = keys.len();
            for k in keys {
                shutdown(&k);
            }
            message(&format!(
                "[cairn] Shutting down {n} server{}",
                if n == 1 { "" } else { "s" }
            ));
            Ok(())
        },
    )?;
    command(
        "cairn-list-connections",
        "List the language servers, their projects and buffers (k shuts the one at point down).",
        |_| {
            list_connections();
            Ok(())
        },
    )?;
    command(
        "cairn-events-buffer",
        "Show the JSON-RPC messages between Forge and the current buffer's server.",
        |_| {
            let key = current_server()?;
            events_buffer(&key);
            Ok(())
        },
    )?;
    command(
        "cairn-forget-pending-continuations",
        "Forget the requests still waiting for the server's answers.",
        |_| {
            let key = current_server()?;
            let n = with(|s| {
                s.servers.get_mut(&key).map_or(0, |sv| {
                    let n = sv.pending.len();
                    sv.pending.clear();
                    n
                })
            });
            message(&format!("[cairn] Forgot {n} pending request(s)"));
            Ok(())
        },
    )?;
    command(
        "cairn-clear-status",
        "Clear the progress the server reported (from the mode line).",
        |_| {
            let key = current_server()?;
            with(|s| s.servers.get_mut(&key).map(|sv| sv.progress.clear()));
            update_mode_lines_for(&key);
            Ok(())
        },
    )?;
    command(
        "cairn-signal-didChangeConfiguration",
        "Read cairn.json (and the project's .cairn.json) again and send the settings to the server.",
        |_| {
            let key = current_server()?;
            reload_config();
            let root = with(|s| s.servers.get(&key).map(|sv| sv.root.clone())).unwrap_or_default();
            let settings = config::settings(&project_config(&root), &root);
            notify(
                &key,
                "workspace/didChangeConfiguration",
                json!({ "settings": settings }),
            );
            message(&format!(
                "[cairn] Sent the configuration to {}",
                server_name(&key)
            ));
            Ok(())
        },
    )?;
    command(
        "cairn-show-workspace-configuration",
        "Show the settings the server gets (cairn.json and .cairn.json).",
        |_| {
            let b = Buffer::current();
            let root = with(|s| {
                s.docs
                    .get(&b.id())
                    .and_then(|d| s.servers.get(&d.server))
                    .map(|sv| sv.root.clone())
            })
            .or_else(|| b.path().map(|p| root_of(&p)))
            .unwrap_or_default();
            let settings = config::settings(&project_config(&root), &root);
            show_doc_buffer(&serde_json::to_string_pretty(&settings).unwrap_or_default());
            Ok(())
        },
    )?;

    // --- xref
    command(
        "xref-find-definitions",
        "Go to the definition of the identifier at point (M-.); several go in *xref*.",
        |_| {
            find_locations(
                "textDocument/definition",
                "definitionProvider",
                "definitions",
            )
        },
    )?;
    command(
        "xref-find-references",
        "List the references to the identifier at point (M-?).",
        |_| {
            find_locations(
                "textDocument/references",
                "referencesProvider",
                "references",
            )
        },
    )?;
    command(
        "cairn-find-declaration",
        "Go to the declaration of the identifier at point.",
        |_| {
            find_locations(
                "textDocument/declaration",
                "declarationProvider",
                "declarations",
            )
        },
    )?;
    command(
        "cairn-find-implementation",
        "Go to the implementations of the identifier at point.",
        |_| {
            find_locations(
                "textDocument/implementation",
                "implementationProvider",
                "implementations",
            )
        },
    )?;
    command(
        "cairn-find-typeDefinition",
        "Go to the definition of the type of the identifier at point.",
        |_| {
            find_locations(
                "textDocument/typeDefinition",
                "typeDefinitionProvider",
                "type definitions",
            )
        },
    )?;
    command(
        "xref-find-apropos",
        "Find symbols in the workspace matching a pattern (C-M-.).",
        |_| workspace_symbols(),
    )?;
    command(
        "xref-go-back",
        "Go back to where you were before the last jump (M-,).",
        |_| {
            let Some(m) = with(|s| s.back.pop()) else {
                message("Marker stack is empty");
                return Ok(());
            };
            let now = here();
            go_marker(&m)?;
            with(|s| s.forward.push(now));
            Ok(())
        },
    )?;
    command(
        "xref-go-forward",
        "Go forward again to where M-, came back from (C-M-,).",
        |_| {
            let Some(m) = with(|s| s.forward.pop()) else {
                message("No next location");
                return Ok(());
            };
            let now = here();
            go_marker(&m)?;
            with(|s| s.back.push(now));
            Ok(())
        },
    )?;
    command("xref-goto-xref", "Visit the location on this line.", |_| {
        let loc = list_location_at_point().ok_or("Nothing on this line")?;
        go_to(&loc, false)
    })?;
    command(
        "xref-show-location-at-point",
        "Show the location on this line, staying in the list.",
        |_| {
            let loc = list_location_at_point().ok_or("Nothing on this line")?;
            let list = Buffer::current();
            go_to(&loc, false)?;
            list.show();
            Ok(())
        },
    )?;
    command(
        "xref-next-line",
        "Move to the next location in the list.",
        |_| list_move(true),
    )?;
    command(
        "xref-prev-line",
        "Move to the previous location in the list.",
        |_| list_move(false),
    )?;

    // --- eldoc
    command(
        "eldoc",
        "Show the documentation (or signature) of the thing at point (C-h .).",
        |_| {
            eldoc(true);
            Ok(())
        },
    )?;
    command(
        "eldoc-doc-buffer",
        "Show all of the documentation of the thing at point in *eldoc*.",
        |_| {
            let (b, key, uri) = current_doc()?;
            require(&key, "hoverProvider", "show documentation")?;
            request(
                &key,
                "textDocument/hover",
                doc_position(&b, &uri),
                |result| {
                    let text = result
                        .get("contents")
                        .map(protocol::markup_text)
                        .unwrap_or_default();
                    if text.is_empty() {
                        message("No documentation here");
                    } else {
                        show_doc_buffer(&text);
                    }
                },
            );
            Ok(())
        },
    )?;
    let idle = config().idle;
    when_idle(
        "cairn--on-idle",
        "Cairn's idle work: eldoc and symbol highlighting.",
        idle,
        || {
            if config().eldoc {
                eldoc(false);
            }
        },
    )?;

    // --- flymake
    command(
        "flymake-show-buffer-diagnostics",
        "List this buffer's diagnostics.",
        |_| {
            let b = Buffer::current();
            let uri = with(|s| s.docs.get(&b.id()).map(|d| d.uri.clone()))
                .ok_or("Cairn isn't managing this buffer")?;
            show_diagnostics(
                format!("*Flymake diagnostics for `{}'*", path_of(&uri)),
                vec![uri],
            );
            Ok(())
        },
    )?;
    command(
        "flymake-show-project-diagnostics",
        "List the diagnostics of every file in the project.",
        |_| {
            let key = current_server()?;
            let root = with(|s| s.servers.get(&key).map(|sv| sv.root.clone())).unwrap_or_default();
            let name = format!("*Flymake diagnostics for project {root}*");
            show_diagnostics(name.clone(), diag_buffer_uris(&name));
            Ok(())
        },
    )?;
    command(
        "flymake-refresh-diagnostics-buffer",
        "Redraw the diagnostics list.",
        |_| {
            refresh_diagnostic_lists();
            Ok(())
        },
    )?;
    command(
        "flymake-goto-next-error",
        "Move to the next diagnostic in this buffer.",
        |n| goto_diagnostic(true, n.unwrap_or(1)),
    )?;
    command(
        "flymake-goto-prev-error",
        "Move to the previous diagnostic in this buffer.",
        |n| goto_diagnostic(false, n.unwrap_or(1)),
    )?;

    // --- editing
    command(
        "cairn-rename",
        "Rename the symbol at point everywhere the server knows it.",
        |_| rename(),
    )?;
    command(
        "cairn-format",
        "Format the region with the server, or the buffer without one.",
        |_| {
            let (b, key, uri) = current_doc()?;
            format(b, key, uri)
        },
    )?;
    command(
        "cairn-format-buffer",
        "Format the whole buffer with the server.",
        |_| format_buffer(),
    )?;
    command(
        "cairn-code-actions",
        "Offer the code actions at point or in the region, and run the one picked.",
        |_| code_actions(None),
    )?;
    command(
        "cairn-code-action-organize-imports",
        "Organize the imports.",
        |_| code_actions(Some("source.organizeImports")),
    )?;
    command(
        "cairn-code-action-quickfix",
        "Fix the problem at point.",
        |_| code_actions(Some("quickfix")),
    )?;
    command(
        "cairn-code-action-extract",
        "Extract the region (a function, a variable).",
        |_| code_actions(Some("refactor.extract")),
    )?;
    command(
        "cairn-code-action-inline",
        "Inline the thing at point.",
        |_| code_actions(Some("refactor.inline")),
    )?;
    command(
        "cairn-code-action-rewrite",
        "Rewrite the code at point.",
        |_| code_actions(Some("refactor.rewrite")),
    )?;
    command(
        "cairn-imenu",
        "Jump to a symbol of this buffer, from the server's list (M-g i here).",
        |_| imenu(),
    )?;
    command(
        "cairn-show-call-hierarchy",
        "Show the calls to (or from) the function at point, as a tree.",
        |_| show_hierarchy(true),
    )?;
    command(
        "cairn-show-type-hierarchy",
        "Show the supertypes (or subtypes) of the type at point, as a tree.",
        |_| show_hierarchy(false),
    )?;
    command(
        "cairn-hierarchy-expand",
        "Expand or collapse the hierarchy node at point.",
        |_| {
            let b = Buffer::current();
            let line = b.point().line as usize;
            if line == 0 {
                return Ok(());
            }
            expand_node(b.id(), line - 1);
            Ok(())
        },
    )?;
    command(
        "cairn-signature-help",
        "Show the signature of the call around point.",
        |_| signature_help(),
    )?;

    // --- the old lsp-* names
    let alias = |old: &str, new: &str, run: fn() -> Result<(), String>| {
        command(
            old,
            &format!("Alias for {new} (the Cairn name)."),
            move |_| run(),
        )
    };
    alias("lsp-hover", "eldoc", || {
        eldoc(true);
        Ok(())
    })?;
    alias("lsp-find-definition", "xref-find-definitions", || {
        find_locations(
            "textDocument/definition",
            "definitionProvider",
            "definitions",
        )
    })?;
    alias("lsp-references", "xref-find-references", || {
        find_locations(
            "textDocument/references",
            "referencesProvider",
            "references",
        )
    })?;
    alias("lsp-rename", "cairn-rename", rename)?;
    alias("lsp-code-actions", "cairn-code-actions", || {
        code_actions(None)
    })?;
    alias("lsp-format", "cairn-format-buffer", format_buffer)?;
    alias("lsp-signature-help", "cairn-signature-help", signature_help)?;
    alias("lsp-servers", "cairn-list-connections", || {
        list_connections();
        Ok(())
    })?;
    command(
        "lsp-restart",
        "Restart every language server (cairn-reconnect for all of them).",
        |_| {
            let keys: Vec<String> = with(|s| s.servers.keys().cloned().collect());
            for k in keys {
                reconnect(&k);
            }
            Ok(())
        },
    )?;

    for (keys, cmd) in [
        ("M-.", "xref-find-definitions"),
        ("M-?", "xref-find-references"),
        ("M-,", "xref-go-back"),
        ("C-M-,", "xref-go-forward"),
        ("C-M-.", "xref-find-apropos"),
        ("C-h .", "eldoc"),
    ] {
        let _ = keymap::bind(&Scope::Global, keys, cmd);
    }

    for b in Buffer::all() {
        attach(&b);
    }
    Ok(())
}

fn format_buffer() -> Result<(), String> {
    let (b, key, uri) = current_doc()?;
    b.set_mark(None);
    format(b, key, uri)
}

fn signature_help() -> Result<(), String> {
    let (b, key, uri) = current_doc()?;
    require(&key, "signatureHelpProvider", "show signatures")?;
    let name = callee(&b);
    request(
        &key,
        "textDocument/signatureHelp",
        doc_position(&b, &uri),
        move |result| {
            let text = protocol::signature_line(&result, &name);
            message(if text.is_empty() {
                "No signature here"
            } else {
                &text
            });
        },
    );
    Ok(())
}

fn list_move(forward: bool) -> Result<(), String> {
    let b = Buffer::current();
    let rows = with(|s| s.lists.get(&b.id()).cloned()).unwrap_or_default();
    let line = b.point().line as usize;
    let next = if forward {
        (line + 1..rows.len()).find(|&i| rows[i].is_some())
    } else {
        (0..line).rev().find(|&i| rows[i].is_some())
    };
    match next {
        Some(i) => b.set_point(Position {
            line: i as u32,
            column: 0,
        }),
        None => message(if forward {
            "No next location"
        } else {
            "No previous location"
        }),
    }
    Ok(())
}

forge_plugin!(init);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn word_in_spans_the_whole_identifier() {
        let line = "let total_count = 1;";
        assert_eq!(word_in(line, 6), Some((4, 15)));
        assert_eq!(word_in(line, 4), Some((4, 15)));
        assert_eq!(word_in(line, 15), Some((4, 15)), "just past the end");
        assert_eq!(word_in(line, 3), Some((0, 3)));
        assert_eq!(word_in(line, 16), None);
        assert_eq!(word_in("", 0), None);
        assert_eq!(word_in("let \u{e9}x = 1;", 5), Some((4, 6)));
    }

    #[test]
    fn content_changes_carry_lines_and_utf16_columns() {
        let one = content_change("abc", "abXc");
        assert_eq!(one["range"]["start"], json!({"line": 0, "character": 2}));
        assert_eq!(one["text"], "X");
        let multi = content_change("one\ntwo\n", "one\nTWO\n");
        assert_eq!(multi["range"]["start"], json!({"line": 1, "character": 0}));
        assert_eq!(multi["range"]["end"], json!({"line": 1, "character": 3}));
        assert_eq!(multi["text"], "TWO");
        assert_eq!(content_change("same", "same")["text"], "same");
    }

    #[test]
    fn severities_and_faces() {
        assert_eq!(face_of(1), "error");
        assert_eq!(face_of(2), "warning");
        assert_eq!(face_of(4), "info");
        let d = Diagnostic {
            raw: Value::Null,
            start: LspPos {
                line: 0,
                character: 0,
            },
            end: LspPos {
                line: 0,
                character: 1,
            },
            severity: 1,
            message: "bad thing\nmore".into(),
            source: "pyright".into(),
        };
        assert_eq!(diag_text(&d), "pyright: bad thing");
    }

    #[test]
    fn relative_paths() {
        assert_eq!(relative("/p/src/a.rs", "/p"), "src/a.rs");
        assert_eq!(relative("/p/src/a.rs", "/p/"), "src/a.rs");
        assert_eq!(relative("/pq/a.rs", "/p"), "/pq/a.rs");
        assert_eq!(relative("/x/a.rs", ""), "/x/a.rs");
    }

    #[test]
    fn hierarchy_labels() {
        let item = json!({"name": "f", "kind": 12, "detail": "def f()", "uri": "file:///p/a.py",
                          "selectionRange": {"start": {"line": 4, "character": 4}, "end": {"line": 4, "character": 5}}});
        assert_eq!(hierarchy_label(&item), "f (function) def f()  a.py:5");
        assert_eq!(item_location(&item).unwrap().line, 4);
    }
}
