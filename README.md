# rex-typst

A live Typst preview in a [Rex](https://www.superlogical.com/rex) pane beside your editor.
The pages are drawn in the pane with the kitty graphics protocol, so there's no browser,
no Quick Look, and nothing to serve over the network. It works the same when the Rex
server is on another machine.

- **Live as you type** in Neovim: unsaved changes compile straight from the buffer.
  Other editors update on save.
- **Click to jump**: click anything in the preview and the editor jumps to its source,
  and takes the focus.
- **Whole documents**: open it from any chapter and it previews the document that
  includes it, found from your `#include`s without any setup.
- **Go to errors**: `e` jumps the editor to each compile error in turn.
- **Follows your cursor** in Neovim, optionally keeping it centered in the preview.
- **Zoom** in and out, and scroll sideways when the page is wider than the pane.
- **Incremental**: the Typst compiler (0.15) is built in and keeps running, so a change
  recompiles only what it touched, and only redrawn pages are sent to the terminal.
- Compile errors show under the preview and in Rex's status.

## Install

There are two parts: the `rex-typst` binary, which goes on the machine your Rex sessions
run on, and a small Lua module for your Rex config.

**1. Get Rust 1.92 or newer**, if you don't have it:

```sh
brew install rust        # or: curl https://sh.rustup.rs -sSf | sh
rustc --version          # 1.92 or newer
```

**2. Clone and build** (the first build takes a few minutes). Clone it wherever you
keep code; the commands below run from inside the clone, so the location doesn't matter:

```sh
git clone https://github.com/CiaranMcDonnell/rex-typst.git
cd rex-typst
cargo install --path . --root ~/.local     # installs ~/.local/bin/rex-typst
```

`~/.local/bin` has to be on your login shell's `PATH`; check with `which rex-typst`. If it
isn't, add `export PATH="$HOME/.local/bin:$PATH"` to your login profile (`~/.zprofile`
for zsh, `~/.bash_profile` for bash), or set `bin` (below) to the binary's full path.

**3. Link the Rex module** next to your `init.lua`. Still inside the clone:

```sh
mkdir -p ~/.config/rex
ln -s "$PWD/rex/rex-typst.lua" ~/.config/rex/rex-typst.lua
```

**4. Turn it on.** Add this to `~/.config/rex/init.lua`:

```lua
require("rex-typst").setup{}
rex.bind("ctrl+space>t", "typst_toggle")
```

then apply it:

```sh
rex config check && rex config reload
```

**Updating**, from inside the clone:

```sh
git pull && cargo install --path . --root ~/.local
```

The Lua module is a symlink, so `git pull` updates it; run `rex config reload` if it changed.

**Remote sessions:** if your Rex server runs on another machine, do steps 1–3 there; the
preview is drawn by your Rex app over the same connection as everything else.

## Use

Open a `.typ` file in a Rex pane and press your key. The preview opens on the right
without taking focus. It previews the file open in the editor, or with another editor
the `.typ` file it was started with, else `main.typ` or the only `.typ` file in the
directory. Pass `file=` to choose: `rex do typst_toggle file=chapters/intro.typ`.

Open it from a chapter and it previews the whole document: rex-typst reads the
`#include`s and `#import`s in the project to find the file at the top, and following the
cursor and clicking still work in every file it includes. Includes built from computed
paths can't be followed, so pass `file=` for those.

Actions, also in the command palette under **Typst**:

| Action | |
| --- | --- |
| `typst_toggle` | open or close the preview (a floating one hides and shows) |
| `typst_open`, `typst_close` | |
| `typst_next_page`, `typst_prev_page` | scroll the preview from the editor |
| `typst_fit` | fit the page width, or whole pages |
| `typst_dark` | invert the colors |
| `typst_dark_images` | invert images too in the dark preview, or let them keep their colors |
| `typst_follow` | follow the cursor on or off |
| `typst_center` | keep the cursor centered in the preview, on or off |
| `typst_goto_error` | jump the editor to the next error, or warning when there are none |
| `typst_zoom_in`, `typst_zoom_out`, `typst_zoom_reset` | zoom the preview |

To drive the preview from the editor, bind the actions you use, for example:

```lua
rex.bind("ctrl+space>=", "typst_zoom_in")
rex.bind("ctrl+space>-", "typst_zoom_out")
rex.bind("ctrl+space>0", "typst_zoom_reset")
rex.bind("ctrl+space>c", "typst_center")
rex.bind("ctrl+space>e", "typst_goto_error")
```

Keys in the preview pane:

| Key | |
| --- | --- |
| `j` `k`, `↑` `↓`, mouse wheel | scroll |
| `h` `l`, shift-wheel, trackpad | scroll sideways when zoomed (`←` `→` too, else they turn pages) |
| `+` `-`, ctrl-wheel | zoom in, out |
| `0` | reset the zoom |
| `ctrl-d` `ctrl-u`, `space` `b` | half page, page |
| `n` `p` | next, previous page |
| `g` `G` | first, last page |
| click | jump the editor to that spot |
| `e` | jump the editor to the next error (or warning) |
| `w` | fit width / whole pages (resets the zoom) |
| `i` | dark (inverted) preview |
| `I` | invert images too, or let them keep their colors |
| `f` | follow the cursor on or off |
| `c` | keep the cursor centered on or off |
| `r` | recompile everything and redraw every page |
| `q` | close |

## Options

```lua
require("rex-typst").setup{
  bin = "rex-typst",          -- path to the binary on the session's machine
  placement = "split",        -- "split", "float" or "tab"
  width = 0.45,               -- share of the editor's width, for "split"
  bounds = { 0.5, 0.04, 0.48, 0.92 },  -- for "float"
  invert = false,             -- start dark
  invert_images = false,      -- invert images too when dark; by default they keep their colors
  fit_page = false,           -- start fitting whole pages
  follow = true,              -- scroll with the editor's cursor
  center = false,             -- keep the cursor in the middle of the preview
  focus_editor = true,        -- focus the editor after a click or `e` jumps it
  image_mb = 40,              -- megabytes of rendered pages to keep in the pane
  args = {},                  -- extra arguments, such as { "--root", "/path" }
}
```

The project root is `$TYPST_ROOT`, else the nearest directory above the file with
`typst.toml` or `.git` (but not your home directory), else the file's directory.

The document a file belongs to is the one that includes or imports it, directly or
through other files, and that nothing else includes. When several do, `main.typ` wins,
then the one nearest the root. Finding it parses only the files that mention `include`
or `import`, and compiles nothing; projects with more than 5,000 Typst files are left
alone.

## Without Rex

`rex-typst FILE` runs in any terminal with kitty graphics (kitty, Ghostty, WezTerm).
Pass `--nvim SOCKET` to link it to a Neovim started with `nvim --listen SOCKET`.

## How it works

The binary embeds the Typst compiler through `typst`, `typst-kit` (files, fonts,
packages) and `typst-ide` (click and cursor mapping). It watches the directories of the
files the last compile read. Each page is rendered at the pane's pixel size, padded to
whole cells, compressed, and uploaded once; scrolling only moves placements, showing the
visible part of each page. Pages are kept in the terminal up to `image_mb`; past that the
ones furthest from the view are freed and rendered again when they come back, since
terminals (Rex's iPad app among them) limit how much image data a pane can hold.

With Neovim, rex-typst keeps one msgpack-RPC connection to its socket and installs
autocmds that push the cursor, the unsaved text of `.typ` buffers, and saves as they
happen, so nothing polls while you work. It reconnects if Neovim restarts.
