# yadt

`yadt` — "Yet Another Diff Tool" — is a terminal diff viewer written in Rust. It compares
two files side by side, or two directory trees, giving in the terminal roughly what
[Meld](https://meldmerge.org/) gives in a GUI: mirrored panes, per-line and per-entry
status, word-level highlighting inside changed lines, and drill-down from a differing
directory entry into its file diff.

It is **read-only**. yadt never writes to the files it is comparing.

## Usage

```bash
yadt FILE_A FILE_B      # side-by-side file diff
yadt DIR_A DIR_B        # directory tree comparison, Enter opens a file diff
```

Both sides must be the same kind; a file against a directory is an error.

### File view

```
src/model.rs                              src/model.rs.orig
118 pub fn push_block(                 ▬   118 pub fn push_block(
119     &mut self,                         119     &mut self,
120     kind: RowKind,                 ▬   120     kind: RowType,
                                       ▶   121     extra: bool,
    ⋯ 194 identical lines              ⋯       ⋯ 194 identical lines
```

The centre column marks each row: `▬` changed, `◀` present only on the left, `▶` only on
the right, blank when the two sides agree. It carries the whole diff on its own, so the
view stays readable with colour off.

Both panes scroll together, because both render the same aligned row.

### Hex view

Two binaries open as hex, aligned by offset:

```
00000000 31 2e 34 2e 30 2d 62 65 74 61 32 2f  {25ac} 00000000 31 2e 34 2e 30 2d 62 65 74 61 33 2f
00000010 73 2f 61 70 70 2e 62 69 6e 00 00 00    00000010 73 2f 61 70 70 2e 62 69 6e 00 00 00
         {22ef} 1536 identical bytes            {22ef} 1536 identical bytes
```

### Directory view

```
release-1.4.0-beta2                       release-1.4.0-beta3
  changed.bin              4.0 KiB    ▬     changed.bin            4.1 KiB
  kindclash/                          ≠     kindclash                 10 B
  left_only.txt            2.0 KiB    ◀
▾ sub/                                ▬   ▾ sub/
    same.txt               820 B            same.txt                820 B
```

Each side shows its own tree, with a blank where a name is missing on that side, so
reading down one column tells you what that tree contains. `≠` is a name that is a
directory on one side and a file on the other; `↻` is a symlink cycle.

## Keys

**File view**

| Key | |
|---|---|
| `j` `k` / arrows | move down / up |
| `Ctrl-d` `Ctrl-u` | half page |
| `PgDn` `PgUp` `Space` `b` | page |
| `g` `G` | first / last row |
| `n` `p` | next / previous hunk |
| `h` `l` / arrows | scroll horizontally (`H` `L` by 8, `0` to reset) |
| `/` | search (smart-case) |
| `N` `P` | next / previous match |
| `z` | fold / unfold identical runs |
| `Enter` | expand the fold under the cursor |
| `Tab` | switch focused side |
| `?` | help |
| `Esc` `q` | back, or quit |

**Directory view**

| Key | |
|---|---|
| `j` `k` / arrows | move selection |
| `Enter` | open a file diff, or fold a directory |
| `l` `h` / arrows | expand / collapse |
| `n` `p` | next / previous difference |
| `f` | cycle filter: all → changes → differing |
| `r` | rescan |
| `?` | help |
| `Esc` `q` | quit |

## Options

| Flag | |
|---|---|
| `-C, --context N` | collapse identical runs, keeping `N` lines of context |
| `-w, --ignore-whitespace` | `exact` (default), `changes` (like `diff -b`), `all` (like `diff -w`) |
| `--algorithm` | `histogram` (default), `myers`, `patience`, `lcs` |
| `--tab-width N` | columns per tab stop (default 8) |
| `--hex-width N` | bytes per row when comparing two binaries (default 8) |
| `--max-file-size N` | refuse files larger than `N` MiB (default 64) |
| `--follow-symlinks` | follow links when comparing directories |
| `--no-inline` | skip word-level highlighting inside changed lines |
| `--no-color` | monochrome; `NO_COLOR` is honoured too |
| `-q, --quiet` | report via the exit code only, without starting the TUI |

Exit codes follow `diff(1)`: **0** identical, **1** differences found, **2** error. So
`yadt --quiet a b` is usable from a script.

## Behaviour worth knowing

- **Search** is smart-case: a lower-case query matches any case, a query with a capital
  matches exactly. `n`/`p` stay bound to hunks and matches are on `N`/`P`, so there are no
  modes -- both work at once. A hit hidden inside a fold expands it.
- **Whitespace.** `-w changes` matches GNU `diff -b`: a run of whitespace compares equal
  to any other run, and trailing whitespace is ignored, but whitespace appearing where
  there was none is still a difference. To ignore indentation added to a previously
  unindented line, use `-w all`, which matches `diff -w`. Both were checked case by case
  against GNU diff. Comparison runs on a normalised copy; what you see is always the
  original text, and word-level highlighting still points at the real bytes.
- **Line endings.** Terminators are stripped before diffing, so a CRLF file and its LF twin
  align as fully equal. yadt detects that separately and says
  `line endings differ: CRLF vs LF` rather than claiming the files match.
- **Binary files** are detected by a NUL byte in the first 8 KiB, and two of them open a
  side-by-side hex view: offset gutter, hex bytes, and a printable-ASCII pane that is
  dropped automatically when the terminal is too narrow. Only the bytes that actually
  differ are highlighted, in both the hex and the ASCII column. `--hex-width` sets the
  bytes per row.

  Hex rows are aligned **by offset**, so a byte inserted near the start shifts everything
  after it and every later row reads as changed. That is what `cmp` reports too. Folding
  and hunk navigation work over bytes exactly as over lines, so a firmware image with two
  changed fields collapses to those two hunks. Search does not work in the hex view and
  says so rather than failing quietly.

  UTF-16 is detected by its BOM and shown as hex rather than decoded.
- **Symlinks are not followed by default**, and are compared by their target. This differs
  from GNU `diff -r`, which follows. `--follow-symlinks` opts in, and reports a link back
  to an ancestor as a cycle rather than walking it.
- **A directory present on only one side is reported but not enumerated**, so a one-sided
  `node_modules/` costs one entry rather than fifty thousand.
- **Directory scanning runs on a worker thread**, so the tree fills progressively and `q`
  still responds while a large comparison is in flight.
- **Mouse capture is off**, deliberately: enabling it breaks the terminal's own text
  selection, and selecting a diff line to copy is a common thing to want.

## Building

The toolchain comes from Nix via [devenv](https://devenv.sh/); `direnv allow` (or
`devenv shell`) puts `cargo` on the path.

```bash
cargo build --release
cargo test
```
