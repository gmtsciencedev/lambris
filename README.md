# lambris

A terminal viewer for parquet, CSV/TSV, and Excel files, in the manner of
[csvlens](https://github.com/YS-L/csvlens).

Opens one or more data files — each in its own tab — and lets you scroll around
them in a TUI — a header row with
column names, a row-number gutter, a status bar showing the selected cell's column
name and Arrow type, and truncation of wide values. Null values render as a
highlighted `NA`. Supports global regex search (`/`), column-scoped search
(`-`), and row filtering (`&`).

## Usage

```sh
cargo run --release -- path/to/file.parquet

# Several files at once, one tab each; Tab switches between them.
cargo run --release -- first.parquet second.csv third.tsv

# A workbook opens one tab per sheet.
cargo run --release -- book.xlsx

# Treat the first row as data rather than column names.
cargo run --release -- --no-header data.csv
```

### Keys

| Key | Action |
| --- | --- |
| `j` / `k` / `↓` / `↑` | Move down / up one row |
| `h` / `l` / `←` / `→` | Move left / right one column |
| `Ctrl-d` / `Ctrl-u` | Half page down / up |
| `Ctrl-f` / `Ctrl-b` / `PgDn` / `PgUp` | Page down / up |
| `g` / `G` | Jump to first / last row |
| `0` / `$` | Jump to first / last column |
| `:` | Go to a row number (1-based, uses original row numbers under a filter) |
| `/` | Global search across all columns (regex, case-insensitive); jumps to the first match |
| `-` | Column search — same as `/` but confined to the selected column |
| `n` / `N` | Jump to next / previous search match (within scope) |
| `&` | Filter rows to those with a cell matching a regex |
| `s` | Sort by the selected column: cycles ascending → descending → unsorted |
| `f` | Freeze columns `0..=selected` (pinned while scrolling); press again to unfreeze |
| `t` | Transpose the table (first column becomes the headers); `t`/`Esc` to return |
| `Tab` / `Shift-Tab` | Switch to the next / previous tab (wraps around) |
| `o` | Open another file in a new tab (prompts for a path; `~/` is expanded) |
| `Ctrl-w` | Close the current tab; closing the last one quits |
| `%` | Numeric column: toggle decimal-point alignment + colouring by log magnitude |
| `<` / `>` | Decrease / increase displayed decimals (also aligns on the dot, no colouring) |
| `?` | Full key reference (`j`/`k` scrolls, `?`/`Esc`/`q` closes) |
| `T` | Toggle whether the first row is column names or data (re-reads the file) |
| `H` | Make the selected row the header, dropping the rows above it; `H` again undoes it |
| `#` | Show/hide the row-number gutter |
| `i` | Toggle info mode — the bottom line shows the selected column's name, Arrow type, and the full (untruncated) cell value |
| `Esc` | Cancel a running operation; otherwise clear search, then filter, then quit |
| `q` / `Ctrl-c` | Cancel a running operation; otherwise quit (every tab) |

While a heavy operation is running (sorting, filtering, or searching a large
file), `Esc` or `Ctrl-C` aborts it and leaves the previous state untouched.

The bottom line shows a handful of commands by default; `?` opens the full key
reference over the table, and `i` swaps the bottom line for the column info
view.

While typing a search or filter, `Enter` commits and `Esc` cancels. Submitting an
empty query clears that search/filter.

## Tabs

Every file passed on the command line opens in its own tab (an Excel workbook
opens one per sheet), and `o` opens another one at any time (a path that fails to load leaves the tabs untouched
and reports why). `Tab` and `Shift-Tab` cycle through them, `Ctrl-w` closes the
current one, and closing the last tab quits.

Each tab holds its **own** view state — cursor, search, filter, sort, frozen
columns, numeric styles, and its own stack of transposed views — so switching
away and back returns to exactly what you left. A tab's state is untouched by
anything you do in another tab.

With more than one file open, the title line becomes the tab strip
(`1:first.parquet  2:second.csv`), with the active tab highlighted; the row and
column counts stay in the status bar below. When the tabs don't all fit, the
strip scrolls so the active one is always visible and `‹`/`›` mark the hidden
ones.

Since a transposed view is itself just a table on that tab's stack, transposing
in one tab leaves the others alone, and `t`/`Esc` pops only that tab's view
rather than closing the tab.

## The first row

The first row is read as column names — which is what a spreadsheet or a CSV
almost always holds — and there is no guessing: nothing scans the file trying to
decide. When that is wrong, say so:

- `--no-header` at startup, for every file opened, or
- `T` in the viewer, which re-reads the current tab the other way.

Without a header the first row becomes an ordinary data row and columns are
named `column_1`, `column_2`, … The status bar shows `no header` while that is
in effect.

And when the header isn't the first row at all — a spreadsheet with a title and
a provenance row above it, or a CSV exported the same way — put the cursor on
the real header row and press `H`. That row becomes the header and everything
above it is dropped, which also fixes the column *types*: a numeric column that
was reading as text because of the junk above it becomes numeric again. Press
`H` again to put the header back at the top. The status bar shows `header@3` for
a header promoted to row 3.

```
#  exported by hand                   #  id  name  score
1  note             2026  -     →     1  1   alpha 3
2  id               name  score       2  2   beta  4
3  1                alpha 3
```

Because all of this changes the schema — the names, and the type of any column
the header row joins — `T` and `H` re-read the file and the tab starts from a
fresh view, dropping any cursor position, filter or sort. They apply per tab, so
one sheet of a workbook can be read differently from the others. They do nothing
on parquet, which carries its own column names, and ask you to leave a
transposed view first.

## Formats

The format is autodetected from the file's magic number, falling back to the
extension:

- **Parquet** — recognised by its `PAR1` magic number.
- **Excel / OpenDocument** — `.xlsx`, `.xlsm`, `.xlsb`, `.xls` and `.ods`, or
  any file whose container says so (a ZIP header for the modern formats, the
  OLE2 one for legacy `.xls`) even when the extension doesn't. Since neither
  container can be delimited text, such a file is always read as a workbook,
  and reports clearly if it turns out to hold something else.
- **CSV / TSV** — anything else is read as delimited text. The delimiter comes
  from the extension (`.tsv`/`.tab` → tab, `.csv` → comma) or, for unknown
  extensions, is sniffed from the first non-comment line. The first row is
  treated as a header, and column types are inferred from a sample of the data.

### Excel workbooks

Each worksheet becomes its own tab, labelled `book.xlsx[Sheet2]`, so every
command applies to a sheet exactly as it would to a CSV. The first row of a
sheet's used range is the header (see [The first row](#the-first-row) to turn
that off); blank header cells get `column_N`. Sheets with no cells at all are
skipped rather than opened as empty tabs.

Column types come from what Excel reported, so sorting and numeric styling
behave:

- Whole numbers become `Int64`. Excel has no integer type — xlsx reports every
  number as a float — so an id column shows `1`, not `1.0`; only genuinely
  fractional columns stay `Float64`.
- Date-formatted cells become real dates (`Date32`), or timestamps when any
  cell carries a time of day. They sort chronologically, and `%` correctly
  declines them as non-numeric.
- Booleans stay boolean.
- Blank cells and Excel **error** cells (`#DIV/0!`, `#REF!`, …) read as nulls,
  shown as the usual highlighted `NA`.
- A textual or genuinely mixed column (dates beside numbers, say) falls back to
  the same string inference the CSV and transposed paths use — so numbers
  stored as text still sort numerically, and nothing in the column is dropped.

A worksheet is decoded in full rather than lazily: xlsx is a zipped XML stream
with no way to seek to a row range, and Excel caps a sheet at ~1M rows anyway.
It is then served from memory through the same chunk interface as every other
format.

### Comment lines

Files that begin with `#` comment lines (MetaPhlAn and other bioinformatics
tools) are handled automatically:

- A leading block of `#` lines is skipped (with `--no-header`/`T`/`H` too — the
  comment block never counts as a row).
- If the **last** `#` line has the same number of columns as the data (e.g.
  MetaPhlAn's `#clade_name<TAB>…`), it is used as the header. Otherwise the
  comment block is treated as pure preamble and the first non-`#` line is the
  header.

The detection is most reliable for TSV, where comment/command lines rarely
contain tabs; a CSV comment that happens to have the same comma count as the
data could be misread as a header.

## Scope

Core viewer plus regex search, row filtering, type-aware sorting, column
freeze, a transposed view, and tabs over several open files or workbook sheets. Nulls are shown as a highlighted `NA`. Sort
composes with the active filter, and the cursor stays on the same record across
a re-sort.

Transpose builds the actual transposed table and shows it as a normal table, so
**every command works exactly as usual** — `s` sorts the selected (now-transposed)
column, `%`/`<`/`>` format it, and filter/search/freeze all apply. The first
column's values become the column headers (its name labels the leading `field`
column), and each transposed column's type is inferred from its values, so
sorting and numeric styling behave correctly. Requires at least two columns, and
is capped at the first few thousand records so a huge file can't create an
unbounded number of columns (a note shows when truncated).

### Numeric columns

On a numeric column, `%` switches to a numeric display: values are aligned on
the decimal point and coloured by the base-10 log of their magnitude (cool for
small values, warm for large), which makes the shape of the data pop out. `<`
and `>` set a fixed number of decimals (and turn on alignment without the
colouring). The status bar shows the active style (e.g. `num.3 log`).

## Big files

Data is loaded lazily so memory stays bounded regardless of file size (Excel
excepted — a worksheet is fully resident, see above):

- The file is read in **chunks** of 8192 rows, decoded on demand and kept in a
  small **LRU cache** — only the rows you're near are resident.
- Parquet chunks are read directly by row range (skipping row groups); CSV/TSV
  chunks are read by seeking into a compact byte-offset index built at open.
- The unfiltered, unsorted view stores **no** per-row index (it's the identity
  `0..n`), so scrolling a billion-row file allocates nothing for bookkeeping.
- Filtering and search **stream** the file a chunk at a time. Sorting reads the
  one sort column into memory (unavoidable — the result is a full ordering).

Everything above the loader (sorting, filtering, search, freeze, null handling)
operates on Arrow arrays and is format-agnostic. Cells are formatted on demand
for the visible window using Arrow's `ArrayFormatter`.

> Note: CSV/TSV are indexed with a single fast scan when the file is opened, so
> there's a brief pause on very large text files before the viewer appears.

## Development

```sh
cargo test    # headless render + navigation tests (build their own fixture)
cargo build
```

### Installing

`cargo install --path .` drops the binary in `~/.cargo/bin`. If you copy it
somewhere yourself instead, **replace the old one rather than overwriting it**:

```sh
rm -f /usr/local/bin/lambris && cp target/release/lambris /usr/local/bin/
```

macOS enforces code signatures on every binary, and an installed copy can end
up in a state where the kernel rejects it: it is `Killed: 9` (SIGKILL) on *every*
run — including `--version`, which opens no file at all — even though the file is
byte-for-byte identical to a `target/release/lambris` that runs fine. Overwriting
the installed binary in place is what triggered it here.

If it happens, `rm` the binary and copy it again (a fresh file, not an overwrite),
or repair it in place with `codesign -f -s - <path>`. The giveaway is that the
same bytes run from one path but not another — if `--version` is killed, the
problem is the binary, not the file you were trying to open.

## License

Licensed under the [MIT License](LICENSE).
