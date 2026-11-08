<p align="center"><img src="assets/logo-wordmark.svg" alt="vmac" width="420"></p>

<p align="center">A modal (evil-style) editor on GPUI whose behavior is written in Steel
Scheme and Lua, and can be redefined while it runs.</p>

<p align="center"><a href="https://super-yojan.dev/vmac/">Install and configure</a></p>

<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/screenshot-dark.png">
    <img src="assets/screenshot-light.png" alt="vmac with the file tree, a Rust file and the README side by side" width="900">
  </picture>
</p>

vmac was called gmacs. Plugins that use `gmacs.*` in Lua still work (it's an
alias of `vmac`), and `~/.config/gmacs/` is read while `~/.config/vmac/`
doesn't exist; move it there when you like.

## Install

vmac builds from source with Rust's cargo (get it from [rustup.rs](https://rustup.rs)).
It's tested on macOS.

```sh
curl -fsSL https://raw.githubusercontent.com/Super-Yojan/vmac/master/install.sh | sh
```

or, the same thing by hand:

```sh
cargo install --git https://github.com/Super-Yojan/vmac --tag v0.0.2 --locked
```

Then `vmac FILE`. Your config goes in `~/.config/vmac/` (`init.scm`,
`init.lua`, `plugins/`). From a checkout, `cargo run -- FILE` runs it with
live reload of `lisp/core/`.

**As a Mac app.** From a checkout, `scripts/make-app.sh` builds `vmac.app`
into `/Applications` (or the folder you name), with its icon, so it opens
from Spotlight, Launchpad and the Dock. Opened that way it starts on the
dashboard, in your home folder, with your login shell's `PATH` (so it finds
language servers, fish, `npx` and the agents). It needs `rsvg-convert` and
ImageMagick (`brew install librsvg imagemagick`). Run it again after
updating to rebuild the app.

## How it is split

| Layer        | Where                                                | What                                                                           |
|--------------|------------------------------------------------------|--------------------------------------------------------------------------------|
| Text, undo   | `src/buffer.rs`                                      | a rope, the point, undo snapshots                                              |
| Editor state | `src/editor.rs`                                      | buffers, windows and splits, LSP, the picker                                   |
| Primitives   | `src/lisp.rs`                                        | `insert`, `char-at`, `delete-region`, `find-file`, ... in both Scheme and Lua  |
| Window       | `src/shell.rs`                                       | draws windows, modelines, popups; sends every key to Scheme                    |
| The editor   | `lisp/core/*.scm`                                    | states, motions, operators, keymaps, picker, file tree, LSP commands, modeline |
| Plugins      | `~/.config/vmac/plugins/`                            | Scheme and Lua, loaded after the core                                          |
| Your config  | `~/.config/vmac/init.scm`, `~/.config/vmac/init.lua` | loaded last                                                                    |

The core is one file per area (`20-dispatch.scm`, `45-operators.scm`,
`75-file-tree.scm`, ...), loaded as one program in name order.

## Keys

- **Motions:** `h j k l`, `w b e` and `W B E`, `0 ^ $`, `gg G`, with counts (`3j`).
- **Operators:** `d c y > <`, followed by a motion (`dw`, `c$`, `y2j`, `dG`),
  a text object (`ciw`, `da(`, `yi"`), or themselves for lines (`dd`, `3yy`, `>>`).
  Shorthands `D C Y`. `cw` changes to the end of the word, like Vim.
- **Text objects** (`i` inner, `a` around): `w W`, `( ) b`, `[ ]`, `{ } B`,
  `< >`, `" ' ``, `p` (paragraph).
- **Visual:** `v` characters, `V` lines; move with any motion, select with a
  text object (`viw`, `va{`), `o` swaps ends, then `d x c y > < p`; `ESC` leaves.
- **Visual block:** `C-v`, then any motion (`$` reaches every line's end), `o`
  and `O` swap corners; `d x y` cut or copy the rectangle (`p`/`P` paste it
  back as a column), `I`/`A` type before/after it on every line, `c` changes it.
- **Repeat:** `.` repeats the last change: an operator (`dw`, `ci"foo`), an
  insert session, `x`, `p`, a visual change. A count replaces the original
  (`3.`).
- **Find a character:** `f t F T` on the line, `;` and `,` to repeat; they
  work after operators (`dt)`, `cf,`) and in visual mode.
- **Search:** `/` and `?` (regex, smartcase, wraps around), `n N`, `*` and `#`
  for the word under the cursor. Matches highlight as you type and stay
  highlighted until `ESC` or `:noh`. Operators take them too (`d/foo`, `dn`).
- **File tree:** `SPC o p` opens and closes a sidebar with the project's
  folders, opened down to the current file (.gitignore'd files hidden). Every
  motion works in it (`j k gg G /`); `RET`/`o` open a file or toggle a folder,
  `l`/`h` open/close a folder (`h` on a closed one goes to its parent), `a` adds
  a file (end with `/` for a folder), `r` renames, `d` deletes (asks first),
  `R` refreshes, `q` closes. `C-w h`/`C-w l` move between it and your files.
- **Find file:** `SPC f f` or `SPC SPC` opens a telescope-style picker over the
  project (the git repo of the current file; .gitignore respected): type to
  filter fuzzily, `C-n`/`C-p`, `C-j`/`C-k` or arrows to move, `RET` to open.
  The selected file is previewed. Like telescope, `ESC` goes to normal mode:
  `j k` (with counts), `gg G`, `C-d C-u` move through the results, every
  other Vim key edits the query (`x`, `cw`, `0`, `u`), `i a I A` type again,
  and `q` or `ESC` closes.
- **Scrolling:** `C-d`/`C-u` half a window, `C-f`/`C-b` a page, `C-e`/`C-y`
  a line, `zz zt zb` to put the cursor's line in the middle, top or bottom.
- **Buffers:** every file you open stays open. `SPC ,` or `SPC b b` picks
  one, `]b [b` (or `SPC b n/p`, `:bn :bp`) cycle, `SPC b k` or `:bd` closes
  (`:bd!` discards changes), `:b NAME`, `SPC b N` a new scratch buffer.
- **Windows:** `C-w v` splits beside, `C-w s` below (or `SPC w v/s`, `:vs FILE`,
  `:sp FILE`); `C-w h/j/k/l` moves, `C-w w` cycles, `C-w c` closes, `C-w o`
  keeps only this one. Each window has its own cursor. `:q` closes a window
  and quits from the last; `:qa` quits.
- **Editing:** `i a I A o O`, `x`, `p P`, `u`, `C-r`. Insert: type; `ESC` back.
- **System clipboard:** yanks and deletes also copy to it, and `p P` paste
  what was copied last anywhere (text copied in another app wins over an
  older yank). Cmd-V pastes it at the cursor (normal, insert, prompts, the
  picker, terminals); Cmd-C copies the visual selection.
  `(set-clipboard-sync #f)` keeps yanks and `p` to vmac's own register.
- **Ex:** `:w`, `:q`, `:wq`, `:e FILE`, `:eval EXPR`, or `:any-command-name`.
- **Leader:** `SPC :` run a command by name, `SPC ;` evaluate Scheme,
  `SPC f f` find file, `SPC f s` save, `SPC q q` quit.

Your own pickers use the same panel:

```scheme
(pick "Theme" (list "light" "dark") "" (lambda (choice) (message choice)))
```

## Syntax highlighting

tree-sitter, for Rust, Python, JavaScript, TypeScript/TSX, Go, C, Bash, JSON
and TOML (picked by file extension; the modeline shows which). Colors come
from `(syntax-theme)` in Scheme: capture names to faces.

```scheme
;; Comments in yellow, strings in red; everything else as the core says.
(define (syntax-theme)
  (list (list "comment" 'yellow) (list "string" 'red)
        (list "keyword" 'magenta) (list "function" 'blue) (list "type" 'cyan)))
```

## Language servers (LSP)

Opening a file starts its language's server for the project (from
`(lsp-servers)` in `lisp/core/80-lsp.scm`: rust-analyzer, pyright, typescript-language-server,
clangd, gopls). Errors and warnings get colored squiggles; the one under the
cursor shows in the echo area, the modeline counts them (`E2 W1`), `]d`/`[d`
jump between them and `SPC c x` lists them in the picker. `gd` goes to the
definition (opening its file), `C-o` comes back, and `K` shows the
documentation for what's under the cursor in a popup (any key closes it).
`gr` lists references in the picker (previewing each line). Typing a word in
insert state pops up completions that narrow as you type: `C-n`/`C-p` move,
`RET`/`TAB` accept, `ESC` closes; `.` and `:` ask afresh. `SPC c r` renames
what's under the cursor everywhere (starting from its current name), and
`SPC c a` lists fixes and refactorings for the cursor or a visual selection,
and `SPC c f` formats the file (or, in visual state, the selected lines). `SPC c R` (or `:lsp-restart`) restarts
the language servers, and tries again those that failed to start (after you
install one, say).
Edits to other files open them unsaved, so you can review before `:w`. A server that fails
to start or stops says why in the echo area. Set `VMAC_LSP_LOG=/tmp/lsp.log`
to log every message to and from the servers.

```scheme
;; init.scm: use pylsp instead of pyright.
(define (lsp-servers)
  (list (list "python" "pylsp") (list "rust" "rust-analyzer")))
```

## Terminal

`SPC o t` opens a terminal below (or closes it, from the terminal; the
running one is reused), `SPC o T` one in this window, `:term CMD` runs a
command. Terminals run fish (as a login shell) when it's installed, else
`$SHELL`; `:terminal-shell /bin/zsh`, or `(set-terminal-shell ...)` /
`vmac.set_terminal_shell(...)` in your config, changes it. It's a buffer: in insert state every key goes to the program
(`ESC` returns to normal state, `C-\` sends the program an `ESC`); in normal
state its screen is text, so motions, search and yank work, and `p` pastes
into the program. Local terminals run on Ghostty's terminal engine
([qwertty-term-vt](https://github.com/joshka/qwertty-term)): colors (256 and
true color), 2,000 lines of scrollback in the buffer (`gg`, `/` reach back
through it), text re-wrapped on resize, and answers to programs' queries.

Where the program runs is a **backend**. The built-in one is `local` (a
PTY). Plugins add others in Lua:

```lua
vmac.terminal_backend("name", {
  -- "" = a shell. Return a handle (then send/read are polled), or attach:
  -- { handle = h, command = "tool attach h" } runs that command on a local
  -- PTY, which carries keys, screen and size (tmux, ssh, docker exec, hc...).
  start = function(command, cwd, rows, cols) return handle end,
  send  = function(handle, text, key) end,   -- text typed, or a key: enter, ctrl+c, up
  read  = function(handle) return screen_text end,
  alive = function(handle) return true end,  -- optional
  kill  = function(handle) end,              -- optional (attached ones too)
})
```

`:terminal-backend NAME` (or `vmac.set_terminal_backend`) picks the
default. `examples/plugins/herdr-terminal` runs terminals in herdr-core
(`hc run`) and attaches with herdr's own client (`hc attach`), so they
outlive vmac while keys, size and colors work as in a local terminal.
`vmac.exec(cmd, args)` runs a program and returns its output, for backends
built on a command line tool.

## Dashboard and projects

Started without a file, vmac opens on a dashboard: find a file, reopen a
recent one, switch project, open a folder, ask the agent, open your config.
Each line's key runs it (or `RET` on it), and the projects you opened lately
are listed below, `1` to `9`. `SPC b h` or `:dashboard` shows it again.

A project is a folder with `.git` in it; vmac remembers the projects and
files you open (in `~/.local/state/vmac`).

- `SPC p p` switches project: pick one (those you opened, then repositories
  it finds in `~/git`, `~/code`, `~/projects`, `~/src`, `~/dev`, `~/work`),
  and the file picker opens in it. It becomes the working folder, for `:e`,
  terminals and the agent.
- `SPC p a` (or `:project PATH`) opens any folder by path; `SPC p d` forgets
  one.
- `SPC f r` reopens a recent file; `SPC f P` opens your config.

The dashboard is Scheme: redefine `dashboard-banner` or `dashboard-items`
in your config, e.g.
`(define (dashboard-items) (cons (list "t" "Terminal" 'terminal-toggle) (default-dashboard-items)))`,
or `(define dashboard-on-start #f)` to start on a scratch buffer.

## Coding agent

vmac talks to coding agents over the
[Agent Client Protocol](https://agentclientprotocol.com) (ACP), the protocol
Zed uses, so it drives the real Claude Code, Codex or Gemini CLI rather than
an agent of its own. It works in the current file's project.

`SPC a a` opens the `*agent*` buffer on the right with the cursor in its
prompt, below a rule. The prompt is ordinary text: type, and use any Vim key
on it; `RET` in normal state sends it (`C-RET` or `Cmd-RET` from insert
state). Above the rule the conversation streams in, wrapped to the window:
your prompts, its replies as Markdown, its thinking in italics, its plan as a
checklist, and each tool call on one line (`▸ ✓ Edit src/lsp.rs`). You can
move through, search and yank the conversation, not change it; `i`, `a` or
`o` there jump down to the prompt.

- **On a tool call**, `RET` or `TAB` (or `za`) opens it: the files it touched
  and its output, an edit as the lines it changed. `RET` on a file opens it in
  the other window at that line.
- **`SPC a s`** asks about the current line, or the visual selection: it sends
  your question with the file, the line numbers and the code.
- **`@` attaches a file.** Typing `@` at the start of a word in the prompt
  opens the file picker; the chosen path goes in the prompt (`@src/lib.rs`),
  and the file's text is sent along: your buffer's, saved or not. (Agents
  that only take links to files get a link.)
- **`SPC a e`** asks about the current file's problems: the language
  server's errors and warnings, with the file. The question defaults to
  "Fix these problems."
- **Commands it runs show in vmac terminals** (`*agent: cargo test*`). A
  running tool call shows the command's latest line; opened, its last lines,
  and `RET` there opens the terminal. Stopping the agent stops them.
- It reads files **through your buffers**, so it sees edits you haven't saved.
- **Its edits wait for your review.** They're made at once and saved (so it
  can build and test them), in the file's buffer, which keeps the text from
  before. Changed lines get a green band, and a red rule marks where lines
  were deleted. `]h` and `[h` move between changes; `ga` accepts the one under
  the cursor, `gx` rejects it (the old lines come back and are saved; `u`
  undoes that). `SPC a r` lists every change as a diff in `*agent-review*`,
  where the same keys work and `RET` goes to one; `SPC a A` and `SPC a X`
  accept or reject them all. `*agent*` counts what's left.
  `*agent-review*` is drawn with gpui-kit's Diff (word-level changes, line
  numbers, a header per file); Vim keys move through it, and the line under
  the cursor is selected in it.
- **Comment, and let it revise.** `gc` comments on the line under the
  cursor, in the review or in any file (`c` in the review, or the Diff's `+`
  beside a hovered line). Comments show under their line in the review and
  as a yellow mark in the file; `gC` deletes them. `SPC a S` (`S` in the
  review) sends them all to the agent, each with the code it's on, asking
  it to revise its changes by them. This works with ACP and CLI agents alike. To accept edits
  as they're made: `(set-agent-review #f)` or `vmac.set_agent_review(false)`.
- **When it needs permission** (to edit, to run a command, to start coding
  after a plan), `*agent*` shows the question with what it's about (the
  plan, the command, the diff) and the choices, and puts the cursor on it so
  you can read first. Press a choice's number, or `RET` on it; `SPC a p`
  picks from a list. While you're typing, vmac only tells you it's asking.
- **Past sessions:** `SPC a h` reopens one of this project's, `SPC a H`
  one from any project (also on the dashboard, `s`). With Claude Code these
  are all its sessions, from the CLI and the desktop app too: vmac reads
  them from `~/.claude/projects`, titled as Claude titled them, newest
  first. Other agents list their own. Picking one starts the agent in that
  session's folder and replays the conversation into `*agent*`, to carry on.
- `SPC a c` (or `C-c` in `*agent*`) cancels the run, `SPC a n` starts a new
  conversation, `SPC a k` stops the agent, `SPC a o` shows `*agent*`.
  `:agent PROMPT` asks from the command line.

**Two kinds of agent.** ACP agents (`claude`, `codex`, `gemini`) answer
in `*agent*`, as above. CLI agents run their own interface in a terminal
beside your code, exactly as in your shell: `claude-cli`, `codex-cli`,
`gemini-cli` are built in, and you can add any (pi, aider...):

```lua
vmac.agent_backend("pi", { terminal = "pi" })
vmac.agent_backend("claude-cli", { terminal = "claude", resume = "claude --resume {id}" })
```

`SPC a b` picks either kind, and the agent keys work with both: `SPC a a`
opens it, `SPC a t` shows or hides its pane, `SPC a s` and `SPC a e` send
it your question with the file and line numbers (a CLI agent reads the
file itself), `SPC a h` reopens a session (with `resume`). A CLI agent
writes to disk itself, so vmac reloads the open files it changes and keeps
what they were: its changes wait for review too (`ga`, `gx`, `SPC a r`).
Open files that change on disk otherwise (git, another editor) reload too,
unless you have unsaved edits in them.

**Which agent.** `claude` (Claude Code through
[claude-code-acp](https://www.npmjs.com/package/@zed-industries/claude-code-acp),
installed or run with `npx`, using your `claude` login) is the default;
`codex` (codex-acp) and `gemini` (Gemini CLI) are built in too. `SPC a b` or
`:agent-use NAME` picks one; `SPC a n` restarts with it. Add your own:

```lua
vmac.agent_backend("my-agent", { command = "my-agent --acp" })
vmac.agent_use("my-agent")
```

or `(agent-define "my-agent" "my-agent --acp")` in init.scm.
`:agent-command CMD` runs a one-off command line.

**Rules and hooks.** Before asking you for permission, vmac asks a rule,
with the tool call's title and kind (`read`, `edit`, `delete`, `move`,
`search`, `execute`, `fetch`, ...). It returns `"allow"` (this once),
`"always"`, `"reject"` or `"ask"`:

```lua
-- Let it read and search freely; never delete.
vmac.hooks.agent_permission = function(title, kind)
  if kind == "read" or kind == "search" then return "allow" end
  if kind == "delete" then return "reject" end
end -- nil: leave it to Scheme, which asks
vmac.hooks.agent_tool_call = function(title, kind, status) end
vmac.hooks.agent_turn_end = function(reason)   -- end_turn, cancelled, error...
  vmac.exec("osascript", { "-e", 'display notification "Agent done" with title "vmac"' })
end
```

In Scheme, redefine `(agent-permission-rule title kind)`,
`(agent-on-tool-call title kind status)` and `(agent-on-turn-end reason)`.

`VMAC_AGENT_LOG=/tmp/agent.log` logs the traffic. Claude Code won't start
from a vmac launched inside a Claude Code session (it refuses to nest); start
vmac from a plain terminal.

## Plugins

Plugins live in `~/.config/vmac/plugins/`: a folder with an `init.scm`
and/or `init.lua` (Lua can `require` modules from the folder or its `lua/`),
or a single `name.scm` / `name.lua`. They load after the core and before
your init files, so your config can change anything a plugin sets. A plugin
that fails to load is skipped, with its error shown; the rest still load.
Editing a plugin's files reloads it.

- `SPC h p` (or `:plugins`) lists them with their status; `RET` turns one on
  or off (a `disabled` file in its folder).
- `:plugin-install URL` clones one with git (in the background, then reloads),
  `:plugin-update` pulls them all, `:plugin-remove NAME` deletes one.

`examples/plugins/` has `word-count` (Lua, `SPC t w`), `zen` (Scheme,
`SPC t z`) and `herdr-terminal` (a terminal backend). To try them:
`cp -r examples/plugins/* ~/.config/vmac/plugins/`

A plugin is just config that can be shared: in Scheme, `defcommand` and
`define-key`; in Lua, `vmac.command` and `vmac.map`. To publish one, put
its folder in a git repository.

## Markdown

`.md` files are styled in place, still fully editable: headings are larger,
bold and in the UI's sans-serif font, with `**bold**`, `*italic*`,
`` `code` ``, links, quotes, lists and code blocks each styled.

**Two ways to read it.** Outside insert state the markup is hidden, so it
reads like the rendered page: headings without `#`, bold and italic without
their stars, code without backticks, links as their text, bullets, rules and
table borders drawn as lines. In insert state everything shows, as typed.
`SPC t m` (or `(set-markdown-conceal #f)`) shows the markup all the time.
Or `SPC m p` opens a preview on the right, rendered by gpui-kit (headings,
code blocks, tables, images and HTML like a web page), following the
Markdown file you're in as you type; `SPC m p` again closes it.
`(set-markdown-preview #t)` opens it from the start.

Tables work like Org mode's: leaving insert state in one aligns it (every
column padded to its widest cell, the `|---|` row redrawn, `:---:` alignment
kept), `TAB`/`S-TAB` move between cells as they align, `TAB` in the last cell
adds a row, and `SPC m a` aligns the table under the cursor.

The styles are faces, defined in Scheme with attributes:

```scheme
;; (name key value ...): color, background, scale, weight, slant, underline, family.
;; set-faces! puts yours in front of the defaults.
(set-faces! (append (heading-faces 1 2.2 'magenta)   ; a bigger, magenta h1
                    (list (list "md-bold" 'weight 'bold 'color 'yellow))))
```

## Themes

`SPC h t` (or `:theme NAME`) picks a color theme: Catppuccin (Latte, Frappé,
Macchiato, Mocha), Tokyo Night, Gruvbox, Solarized, Everforest, Ayu, Flexoki,
Molokai and more, 36 built in (from
[gpui-component](https://github.com/longbridge/gpui-component/tree/main/themes),
Apache-2.0). Syntax and Markdown colors are the theme's own, so everything
follows. Add your own as `~/.config/vmac/themes/*.json`, in the same format.

vmac follows the system's light or dark mode, switching when it does, with
the themes you chose for each:

```lua
vmac.set_themes("Catppuccin Latte", "Catppuccin Mocha")   -- light, dark
-- or one theme, always: vmac.set_theme("Tokyo Night")
```

`:theme dark` (or `light`, or `system` to follow it again) pins a mode.

## Font

vmac starts with an installed Nerd Font (JetBrains Mono, FiraCode, Hack,
Iosevka, ...), so icons from eza, starship and the like show; without one
it uses the theme's monospace font. `:font NAME` changes it, `:fonts` or
`SPC h F` picks from the fonts installed, and `(set-font "...")` or
`vmac.set_font("...")` sets it from your config.

## Customizing

Newer features are written in Lua, in `lua/core/` (built in, loaded
before plugins and your config, so either can change them); the older core
is Scheme, in `lisp/core/`. Both call the same primitives.

The core, init.scm and init.lua are watched: save any of them and vmac
reloads, keeping your buffers. If a save doesn't load, vmac shows the error and keeps running
the last version that did. Or evaluate code live with `SPC ;`.

```scheme
;; ~/.config/vmac/init.scm
(defcommand (insert-date) (insert "2026-11-07"))
(define-key normal-map "SPC i d" 'insert-date)

;; A motion works for moving, in visual mode and after every operator.
(define-motion 'next-blank-line 'linewise
  (lambda (p) (let loop ((l (+ (pos->line p) 1)))
                (cond ((>= l (line-count)) (line->pos (- (line-count) 1)))
                      ((empty-line? (line->pos l)) (line->pos l))
                      (else (loop (+ l 1)))))))
(define-key normal-map "}" (lambda () (move-by 'next-blank-line)))
(define-key operator-map "}" (operator-motion 'next-blank-line))

;; Show only the state and the file in the modeline.
(define (modeline)
  (list (list (if (inserting?) "I" "N") 'accent)
        (list (buffer-name) 'strong)))
```

### In Lua

Lua sits beside the Scheme, not instead of it: write either, or both.
`~/.config/vmac/init.lua` gets the same primitives in the `vmac` table
(Scheme's names with `-` as `_`: `vmac.buffer_name()`, `vmac.pos_to_line(p)`),
and its commands and keys mix with Scheme's. See `examples/init.lua`:

```lua
vmac.command("insert-date", function() vmac.insert(os.date("%Y-%m-%d")) end)
vmac.map("normal", "SPC i d", "insert-date")       -- or a function
vmac.hooks.modeline = function()                   -- replaces Scheme's
  return { { vmac.buffer_name(), "strong" }, { vmac.buffer_language(), "dim" } }
end
```

`vmac.hooks` can replace `modeline`, `lsp_servers` and `syntax_theme`.
`vmac.run("name")` runs any command and `vmac.scheme(code)` any Scheme; both
happen right after the current command (Lua can't call into Scheme midway
through one). `:lua 1 + 2` evaluates Lua; `print` shows in the echo area.

**In Scheme, define commands with `defcommand`, not `define`.** Steel resolves calls
between Scheme functions when it compiles them, so redefining a plain
function later doesn't change what code loaded earlier calls. `defcommand`
also registers the command by name, and keys look it up on every press, so
redefining a command changes its keys at once.

## Not yet

Registers beyond one, marks, macros, `:s`
substitution, resizing splits, incremental re-parsing (the whole file is
parsed again after each change), more languages, a custom text `Element`
(lines are plain divs now), UI trees described from Scheme beyond the
modeline and picker.

## License

vmac is licensed under either of [MIT](LICENSE-MIT) or
[Apache-2.0](LICENSE-APACHE), at your option.
