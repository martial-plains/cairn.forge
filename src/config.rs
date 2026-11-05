// SPDX-License-Identifier: GPL-3.0-or-later
//! Which servers to run, and what to tell them (Emacs's
//! `eglot-server-programs` and `eglot-workspace-configuration`).
//!
//! Defaults are built in. `~/.config/forge/cairn.json` adds to them, and a
//! project's own `.cairn.json` (at its root, Emacs's `.dir-locals.el`
//! equivalent) adds to that:
//!
//! ```json
//! {
//!   "autostart": true,
//!   "eldoc": true,
//!   "highlight-symbols": true,
//!   "server-programs": [
//!     { "modes": ["python-mode"], "language-id": "python",
//!       "command": ["pylsp"],
//!       "initialization-options": {} }
//!   ],
//!   "workspace-configuration": {
//!     "pylsp": { "plugins": { "pycodestyle": { "enabled": false } } }
//!   }
//! }
//! ```
//!
//! Server programs from the file are tried before the defaults (the first
//! one that starts wins, as Eglot tries its alternatives). The workspace
//! configuration answers the server's `workspace/configuration` requests
//! section by section, and goes with `didChangeConfiguration`.

use serde_json::{Map, Value, json};

#[derive(Debug, Clone, PartialEq)]
pub struct ServerSpec {
    pub modes: Vec<String>,
    pub language_id: String,
    /// The program and its arguments.
    pub command: Vec<String>,
    pub init_options: Value,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    /// Start servers for files as they open (Emacs's eglot-ensure on every
    /// mode with a server); off, only `M-x cairn` starts one.
    pub autostart: bool,
    /// Show documentation for what's at point when idle (eldoc).
    pub eldoc: bool,
    /// Highlight the other occurrences of the symbol at point.
    pub highlight: bool,
    pub servers: Vec<ServerSpec>,
    pub workspace: Value,
    /// Seconds idle before eldoc and highlighting.
    pub idle: f64,
    /// Lines kept in each server's events buffer.
    pub events_size: usize,
}

fn spec(modes: &[&str], language: &str, command: &[&str]) -> ServerSpec {
    ServerSpec {
        modes: modes.iter().map(|m| m.to_string()).collect(),
        language_id: language.into(),
        command: command.iter().map(|c| c.to_string()).collect(),
        init_options: Value::Null,
    }
}

/// The built-in servers, alternatives in the order they're tried.
pub fn default_servers() -> Vec<ServerSpec> {
    vec![
        spec(
            &["python-mode"],
            "python",
            &["pyright-langserver", "--stdio"],
        ),
        spec(&["python-mode"], "python", &["pylsp"]),
        spec(
            &["python-mode"],
            "python",
            &["basedpyright-langserver", "--stdio"],
        ),
        spec(&["python-mode"], "python", &["jedi-language-server"]),
        spec(&["python-mode"], "python", &["ruff", "server"]),
        spec(&["rust-mode"], "rust", &["rust-analyzer"]),
        spec(&["c-mode"], "c", &["clangd"]),
        spec(&["c-mode"], "c", &["ccls"]),
        spec(&["go-mode"], "go", &["gopls"]),
        spec(
            &["js-mode"],
            "javascript",
            &["typescript-language-server", "--stdio"],
        ),
        spec(&["js-mode"], "javascript", &["deno", "lsp"]),
        spec(
            &["sh-mode"],
            "shellscript",
            &["bash-language-server", "start"],
        ),
        spec(&["lua-mode"], "lua", &["lua-language-server"]),
        spec(
            &["json-mode"],
            "json",
            &["vscode-json-language-server", "--stdio"],
        ),
        spec(
            &["json-mode"],
            "json",
            &["vscode-json-languageserver", "--stdio"],
        ),
        spec(&["toml-mode"], "toml", &["taplo", "lsp", "stdio"]),
    ]
}

impl Default for Config {
    fn default() -> Self {
        Self {
            autostart: true,
            eldoc: true,
            highlight: true,
            servers: default_servers(),
            workspace: Value::Object(Map::new()),
            idle: 0.5,
            events_size: 2000,
        }
    }
}

/// `b` merged into `a`: objects key by key, anything else replaced.
pub fn merge(a: &mut Value, b: &Value) {
    match (a, b) {
        (Value::Object(x), Value::Object(y)) => {
            for (k, v) in y {
                merge(x.entry(k.clone()).or_insert(Value::Null), v);
            }
        }
        (a, b) => *a = b.clone(),
    }
}

impl Config {
    /// Read a config file's JSON on top of this.
    pub fn apply(&mut self, json: &str) -> Result<(), String> {
        let v: Value = serde_json::from_str(json).map_err(|e| format!("bad JSON: {e}"))?;
        let Value::Object(o) = v else {
            return Err("the file should hold a JSON object".into());
        };
        let flag = |k: &str| o.get(k).and_then(Value::as_bool);
        if let Some(b) = flag("autostart") {
            self.autostart = b;
        }
        if let Some(b) = flag("eldoc") {
            self.eldoc = b;
        }
        if let Some(b) = flag("highlight-symbols") {
            self.highlight = b;
        }
        if let Some(n) = o.get("idle-delay").and_then(Value::as_f64) {
            self.idle = n.max(0.05);
        }
        if let Some(n) = o.get("events-buffer-size").and_then(Value::as_u64) {
            self.events_size = n as usize;
        }
        if let Some(w) = o.get("workspace-configuration") {
            merge(&mut self.workspace, w);
        }
        if let Some(list) = o.get("server-programs") {
            let Value::Array(list) = list else {
                return Err("server-programs should be a list".into());
            };
            let mut mine = Vec::new();
            for (i, s) in list.iter().enumerate() {
                let strings = |k: &str| -> Vec<String> {
                    match s.get(k) {
                        Some(Value::String(one)) => vec![one.clone()],
                        Some(Value::Array(a)) => a
                            .iter()
                            .filter_map(Value::as_str)
                            .map(String::from)
                            .collect(),
                        _ => Vec::new(),
                    }
                };
                let modes = strings("modes");
                let command = strings("command");
                if modes.is_empty() || command.is_empty() {
                    return Err(format!("server-programs[{i}] needs modes and a command"));
                }
                let language_id = s
                    .get("language-id")
                    .and_then(Value::as_str)
                    .map(String::from)
                    .unwrap_or_else(|| modes[0].trim_end_matches("-mode").to_string());
                mine.push(ServerSpec {
                    modes,
                    language_id,
                    command,
                    init_options: s
                        .get("initialization-options")
                        .cloned()
                        .unwrap_or(Value::Null),
                });
            }
            mine.extend(std::mem::take(&mut self.servers));
            self.servers = mine;
        }
        Ok(())
    }

    /// The servers for a major mode, in the order to try them.
    pub fn servers_for(&self, mode: &str) -> Vec<&ServerSpec> {
        self.servers
            .iter()
            .filter(|s| s.modes.iter().any(|m| m == mode))
            .collect()
    }

    pub fn handles(&self, mode: &str) -> bool {
        self.servers
            .iter()
            .any(|s| s.modes.iter().any(|m| m == mode))
    }

    /// Every program a server could run (for the manifest's permission).
    pub fn programs(&self) -> Vec<String> {
        let mut v: Vec<String> = self.servers.iter().map(|s| s.command[0].clone()).collect();
        v.sort();
        v.dedup();
        v
    }
}

/// The settings for a root: the configured workspace configuration, plus
/// what Forge's own Lua files need from lua-language-server.
pub fn settings(cfg: &Config, root: &str) -> Value {
    let mut s = json!({ "Lua": lua_settings(root) });
    merge(&mut s, &cfg.workspace);
    s
}

/// One `section` (dotted) of the settings; null if there is none.
pub fn section(settings: &Value, section: &str) -> Value {
    if section.is_empty() {
        return settings.clone();
    }
    let mut v = settings;
    for key in section.split('.') {
        match v.get(key) {
            Some(next) => v = next,
            None => return Value::Null,
        }
    }
    v.clone()
}

/// lua-language-server, for Forge's own init.rhai and plugins: the
/// `forge` global, Lua 5.4, and the API definitions in `ROOT/lua` and
/// `~/.config/forge/lua` (which Forge keeps current).
pub fn lua_settings(root: &str) -> Value {
    json!({
        "runtime": { "version": "Lua 5.4" },
        "diagnostics": { "globals": ["forge"] },
        "workspace": {
            "checkThirdParty": false,
            // The project's own `lua/` and, for any Lua file anywhere, the
            // definitions Forge installs in the default config directory.
            "library": [format!("{}/lua", root.trim_end_matches('/')), "~/.config/forge/lua"],
        },
        "completion": {
            "autoRequire": false,
            "callSnippet": "Disable",
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn files_add_servers_in_front_and_merge_settings() {
        let mut c = Config::default();
        c.apply(
            r#"{"autostart": false,
                "server-programs": [{"modes": "python-mode", "command": ["pylsp", "-v"]}],
                "workspace-configuration": {"pylsp": {"plugins": {"a": {"enabled": false}}}}}"#,
        )
        .unwrap();
        assert!(!c.autostart);
        let py = c.servers_for("python-mode");
        assert_eq!(py[0].command, ["pylsp", "-v"]);
        assert_eq!(py[0].language_id, "python");
        assert_eq!(
            py[1].command[0], "pyright-langserver",
            "the defaults come after"
        );
        // A project's file merges into the user's.
        c.apply(r#"{"workspace-configuration": {"pylsp": {"plugins": {"b": {"enabled": true}}}}}"#)
            .unwrap();
        let s = settings(&c, "/p");
        assert_eq!(section(&s, "pylsp.plugins.a.enabled"), json!(false));
        assert_eq!(section(&s, "pylsp.plugins.b.enabled"), json!(true));
        assert_eq!(section(&s, "Lua.runtime.version"), json!("Lua 5.4"));
        assert_eq!(section(&s, "nope"), Value::Null);
        assert!(c.apply("[1]").is_err());
        assert!(c.apply(r#"{"server-programs": [{"modes": []}]}"#).is_err());
        assert!(c.programs().contains(&"pylsp".to_string()));
    }
}
