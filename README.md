# lsp: Cairn, a language server client for Forge

A Language Server Protocol client that works like Emacs's Eglot. Open a
file whose mode has a server and that server starts for the project (one
per server program and project root). `M-x cairn` starts one by hand. The
mode line shows `[cairn:PROJECT]`, any progress the server reports, and
the file's Flymake counts (`2E 1W`).

## Servers

Alternatives are tried in order; the first that starts wins.

| Mode | Servers |
|------|---------|
| python-mode | `pyright-langserver --stdio`, `pylsp`, `basedpyright-langserver --stdio`, `jedi-language-server`, `ruff server` |
| rust-mode | `rust-analyzer` |
| c-mode | `clangd`, `ccls` |
| go-mode | `gopls` |
| js-mode | `typescript-language-server --stdio`, `deno lsp` |
| sh-mode | `bash-language-server start` |
| lua-mode | `lua-language-server` |
| json-mode | `vscode-json-language-server --stdio` |
| toml-mode | `taplo lsp stdio` |

`[package.metadata.forge.permissions]` in Cargo.toml lists the programs it
may start (the `processes` permission). Forge asks before granting it,
because a server runs outside the sandbox. A program you add in
cairn.json must be added there too.

## What it does

- **Flymake.** Diagnostics are decorations, and the message shows in the
  echo area while point is on one. `M-x flymake-show-buffer-diagnostics`
  and `flymake-show-project-diagnostics` list them (RET visits, `g`
  redraws). `flymake-goto-next-error` and `flymake-goto-prev-error` move
  between them.
- **Xref.**
  - `M-.` goes to a definition and `M-?` finds references. `C-M-.` finds
    workspace symbols.
  - `M-,` goes back and `C-M-,` goes forward again.
  - Several results go in `*xref*`, grouped by file with each line's text
    (RET visits, `o` shows, `n`/`p` move).
  - `cairn-find-declaration`, `cairn-find-implementation` and
    `cairn-find-typeDefinition` work the same way.
- **Eldoc.** When idle, the echo area shows the documentation of the
  symbol at point. On an argument of a call it shows the call's signature
  instead, with the current parameter marked `‹so›`. Other occurrences of
  the symbol get the `symbol-highlight` face. `C-h .` (`eldoc`) asks
  straight away, and `M-x eldoc-doc-buffer` shows the full documentation.
- **Editing.**
  - `cairn-rename` renames everywhere the server knows the symbol.
  - `cairn-format` formats the region, or the buffer when there is no
    region; `cairn-format-buffer` always formats the buffer.
  - `cairn-code-actions` offers what the server has. The shortcuts
    `cairn-code-action-organize-imports`, `-quickfix`, `-extract`,
    `-inline` and `-rewrite` run a lone matching action straight away.
- **Workspace edits** (rename, code actions, `workspace/applyEdit`) go
  into buffers, never straight to disk. Files that weren't open are
  visited and left unsaved for you to review. File creations, renames
  and deletions requested by the server happen as asked.
- **Completion.** It is a completion source, so the corfu popup or `M-TAB`
  offers the server's completions.
- **Navigation.**
  - `M-g i` in a managed buffer is imenu built from the server's symbols.
  - `cairn-show-call-hierarchy` and `cairn-show-type-hierarchy` draw trees
    (TAB expands a node, RET visits it).
- **Server messages.** Questions the server asks
  (`window/showMessageRequest`) are asked in the minibuffer.
  `window/showDocument` visits the file.
- **Managing servers.**
  - `cairn-list-connections` lists the servers (`k` shuts down the one at
    point).
  - `cairn-events-buffer` shows the JSON-RPC traffic.
  - `cairn-reconnect`, `cairn-shutdown` and `cairn-shutdown-all` restart
    or stop servers.
  - Also: `cairn-forget-pending-continuations`, `cairn-clear-status`.

The old `lsp-*` command names still work.

## Configuration

`~/.config/forge/cairn.json` sets options for every project. A project's
own `.cairn.json` at its root adds to it, playing the part of Emacs's
`.dir-locals.el`:

```json
{
  "autostart": true,
  "eldoc": true,
  "highlight-symbols": true,
  "idle-delay": 0.5,
  "events-buffer-size": 2000,
  "server-programs": [
    { "modes": ["python-mode"], "language-id": "python",
      "command": ["pylsp"], "initialization-options": {} }
  ],
  "workspace-configuration": {
    "pylsp": { "plugins": { "pycodestyle": { "enabled": false } } }
  }
}
```

- **`server-programs`** are tried before the built-in ones.
- **`workspace-configuration`** answers the server's
  `workspace/configuration` requests, section by section, and is sent with
  `didChangeConfiguration`.
- **Applying and checking.**
  - `M-x cairn-signal-didChangeConfiguration` rereads the files and sends
    the settings to the server.
  - `M-x cairn-show-workspace-configuration` shows what the server gets.
- **`autostart: false`** leaves starting servers to `M-x cairn`, like
  Emacs without `eglot-ensure`.

Forge's own Lua configuration gets lua-language-server settings: the
`forge` global, Lua 5.4, and the API definitions in the project's `lua/` and
in `~/.config/forge/lua/`. Forge writes and updates `forge.lua` there (and a
`.luarc.json`, if you have none) each time it starts.

## Building and testing

    cargo build --release --target wasm32-wasip2
    cp target/wasm32-wasip2/release/forge_lsp.wasm ~/.config/forge/plugins/lsp.wasm

`cargo test --lib` runs the unit tests: framing, UTF-16 columns,
incremental sync ranges, workspace edits, signatures, symbols and the
configuration. `sdks/test.sh lsp` builds the plugin and checks it end to
end against pyright: connection, mode line, Flymake, xref and going back,
eldoc, completion, references, rename. `sdks/test.sh cairn` runs only
those checks, on a plugin already built.

## Not yet

- Inlay hints. They need virtual text, which the display doesn't have.
- Snippets. Like Eglot without yasnippet, completions insert plain text.
- Semantic tokens.


## Install with use_package

Add this to `~/.config/forge/init.rhai`:

```rhai
use_package("lsp", #{
  vc: "https://github.com/martial-plains/forge-plugin-lsp",
  pin: "v0.1.0",
  commands: ["cairn", "xref-find-definitions"],
  grant: #{ processes: [
    "pyright-langserver", "pylsp", "basedpyright-langserver",
    "jedi-language-server", "ruff", "rust-analyzer", "clangd", "ccls",
    "gopls", "typescript-language-server", "deno", "bash-language-server",
    "lua-language-server", "vscode-json-language-server",
    "vscode-json-languageserver", "taplo",
  ] },
});
```

Remove the servers you do not want and add any program you add to `cairn.json`.
Forge asks before running the package build command the first time.

## Building

This plugin is built against the Forge plugin SDK (`forge-plugin-sdk`),
taken from the Forge repository named in `Cargo.toml` (change the `git =`
URL if Forge lives elsewhere).

    rustup target add wasm32-wasip2
    cargo build --release --target wasm32-wasip2

Copy the resulting `.wasm` to `~/.config/forge/plugins/`.

## License

GPL-3.0-or-later. See the license file in this directory.
