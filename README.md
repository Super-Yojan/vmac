# gmacs

A modal (evil-style) editor on GPUI whose behavior is written in Steel Scheme
and can be redefined while it runs.

```sh
cargo run -- path/to/file
```

## How it is split

| Layer | Where | What |
|---|---|---|
| Text, undo | `src/buffer.rs` | a rope, the point, undo snapshots |
| Editor state | `src/editor.rs` | buffers, windows and splits, LSP, the picker |
| Primitives | `src/lisp.rs` | `insert`, `char-at`, `delete-region`, `find-file`, ... in both Scheme and Lua |
| Window | `src/shell.rs` | draws windows, modelines, popups; sends every key to Scheme |
| The editor | `lisp/core/*.scm` | states, motions, operators, keymaps, picker, file tree, LSP commands, modeline |
| Plugins | `~/.config/gmacs/plugins/` | Scheme and Lua, loaded after the core |
| Your config | `~/.config/gmacs/init.scm`, `~/.config/gmacs/init.lua` | loaded last |

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
and `SPC c f` formats the file (or, in visual state, the selected lines).
Edits to other files open them unsaved, so you can review before `:w`. A server that fails
to start or stops says why in the echo area. Set `GMACS_LSP_LOG=/tmp/lsp.log`
to log every message to and from the servers.

```scheme
;; init.scm: use pylsp instead of pyright.
(define (lsp-servers)
  (list (list "python" "pylsp") (list "rust" "rust-analyzer")))
```

## Terminal

`SPC o t` opens a terminal below (or closes it, from the terminal; the
running one is reused), `SPC o T` one in this window, `:term CMD` runs a
command. It's a buffer: in insert state every key goes to the program
(`ESC` returns to normal state, `C-\` sends the program an `ESC`); in normal
state its screen is text, so motions, search and yank work, and `p` pastes
into the program. Colors carry over (256 and true color too).

Where the program runs is a **backend**. The built-in one is `local` (a
PTY). Plugins add others in Lua:

```lua
gmacs.terminal_backend("name", {
  start = function(command, cwd, rows, cols) return handle end,  -- "" = a shell
  send  = function(handle, text, key) end,   -- text typed, or a key: enter, ctrl+c, up
  read  = function(handle) return screen_text end,
  alive = function(handle) return true end,  -- optional
  kill  = function(handle) end,              -- optional
})
```

`:terminal-backend NAME` (or `gmacs.set_terminal_backend`) picks the
default. `examples/plugins/herdr-terminal` runs terminals in herdr-core
through `hc`, so they outlive gmacs; `hc keys` can't type every character,
so it sends what you type a line at a time (RET), which suits shells and
agents. `gmacs.exec(cmd, args)` runs a program and returns its output, for
backends built on a command line tool.

## Plugins

Plugins live in `~/.config/gmacs/plugins/`: a folder with an `init.scm`
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
`cp -r examples/plugins/* ~/.config/gmacs/plugins/`

A plugin is just config that can be shared: in Scheme, `defcommand` and
`define-key`; in Lua, `gmacs.command` and `gmacs.map`. To publish one, put
its folder in a git repository.

## Markdown

`.md` files are styled in place, still fully editable: headings are larger,
bold and in the UI's sans-serif font (the `#` marks stay, dimmed), with
`**bold**`, `*italic*`, `` `code` ``, links, quotes, lists and code blocks
each styled.

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

## Customizing

The core, init.scm and init.lua are watched: save any of them and gmacs
reloads, keeping your buffers. If a save doesn't load, gmacs shows the error and keeps running
the last version that did. Or evaluate code live with `SPC ;`.

```scheme
;; ~/.config/gmacs/init.scm
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
`~/.config/gmacs/init.lua` gets the same primitives in the `gmacs` table
(Scheme's names with `-` as `_`: `gmacs.buffer_name()`, `gmacs.pos_to_line(p)`),
and its commands and keys mix with Scheme's. See `examples/init.lua`:

```lua
gmacs.command("insert-date", function() gmacs.insert(os.date("%Y-%m-%d")) end)
gmacs.map("normal", "SPC i d", "insert-date")       -- or a function
gmacs.hooks.modeline = function()                   -- replaces Scheme's
  return { { gmacs.buffer_name(), "strong" }, { gmacs.buffer_language(), "dim" } }
end
```

`gmacs.hooks` can replace `modeline`, `lsp_servers` and `syntax_theme`.
`gmacs.run("name")` runs any command and `gmacs.scheme(code)` any Scheme; both
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
