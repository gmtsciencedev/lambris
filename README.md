# lambris

A terminal viewer for parquet, CSV/TSV, and Excel files, in the manner of
[csvlens](https://github.com/YS-L/csvlens).

Opens one or more data files — each in its own tab — and lets you scroll around
them in a TUI: a header row with column names, a row-number gutter, a status bar
showing the selected cell's column name and Arrow type, and truncation of wide
values. Null values render as a highlighted `NA`. Supports global regex search
(`/`), column-scoped search (`-`), and row filtering (`&`). Press `?` for the
full key reference.

## Usage

```sh
cargo run --release -- path/to/file.parquet

# Several files at once, one tab each; Tab switches between them.
cargo run --release -- first.parquet second.csv third.tsv

# A workbook opens one tab per sheet.
cargo run --release -- book.xlsx

# Treat the first row as data rather than column names.
cargo run --release -- --no-header data.csv

# Open files as they come, ignoring any arrangement saved with `w`.
cargo run --release -- --no-pattern data.csv
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
| `=` | Summary line: total, mean, sd, mean±sd — hold it to remove (see [Summary line](#summary-line)) |
| `w` | Remember this arrangement for the file, or for a glob of files (see [Patterns](#patterns)) |
| `z` / `Z` | Undo / redo the last change to the view (see [Undo](#undo)) |
| `s` | Sort by the selected column: cycles ascending → descending → unsorted |
| `S` | Sort by *part* of the selected column (see [Sorting by part of a column](#sorting-by-part-of-a-column)) |
| `f` | Freeze columns `0..=selected` (pinned while scrolling); press again to unfreeze |
| `(` / `)` | Aim the next column command at every column to the right / left (see [Several columns at once](#several-columns-at-once)) |
| `x` | Hide the selected column |
| `r` | Set this column's width; `( r` evens out a whole block (`%` fits the values) |
| `[` / `]` (or `Shift-←`/`Shift-→`) | Move the selected column left / right |
| `u` | Put every hidden column back, in the file's own order |
| `t` | Transpose the table (first column becomes the headers); `t`/`Esc` to return |
| `J` | Join two tabs on a key column: `Enter` on each side (see [Join](#join)) |
| `Tab` / `Shift-Tab` | Switch to the next / previous tab (wraps around) |
| `o` | Open another file in a new tab; at the prompt `Tab` browses the folder |
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

At the `o` prompt, `Tab` lists the folder of the file you are looking at, over
the table:

```
┌ /data/cohort ─────────────────────┐
│ nested/                           │
│ alpha.csv                         │
│ beta.tsv                          │
└ 1/3 ──────────────────────────────┘
open ▏
```

Directories come first and are marked with a `/`. `Tab` again (or `↑`/`↓`) walks
the list, `Enter` steps into a directory or opens a file, and `Esc` puts the
listing away without leaving the prompt. Typing narrows the list as you go, and
`Tab` on a unique match completes it outright — so `bet` + `Tab` fills in
`beta.tsv` and `Enter` opens it. Hidden files stay hidden until you type a
leading `.`.

Paths are taken relative to the folder of the file on screen — the one `Tab`
lists — so a bare name means what the listing shows. `~/` expands, and absolute
paths work as usual.

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

## Undo

`z` steps back through changes to the view and `Z` steps forward again, each
saying what it did (`undid sort`, `redid filter`). It covers the changes that
have no obvious way back:

- sorting, including a keyed `S` sort — `s` only cycles ascending → descending →
  off, and cannot clear a keyed sort at all
- filters and searches, including clearing them
- hiding, moving and resizing columns — and `u`, which throws away every one of
  those arrangements in a single press
- numeric styles, decimals, and frozen columns

One press puts back a whole `u`, or a whole resize, since a resize is recorded
as the one change it looks like rather than one per keypress. Restoring a filter
or a sort is instant: the row order is *remembered*, not recomputed, so undoing
a filter over a large file costs nothing. The cursor goes back to where it was
too — undo puts the view back as it stood, not as you left it.

Changes that already reverse themselves are deliberately left out, since a
second press of the same key is the way back: `t` leaves a transposed view, `T`
toggles the header reading, `H` again puts a promoted header back, and `Ctrl-w`
closes a tab that `o` or `J` opened. History is per tab, so undoing in one tab
never disturbs another.

Note that `T` and `H` re-read the file, which starts the tab from a fresh view —
so they clear its history along with its filter and sort. Undo cannot reach back
past one of those.

A cancelled change leaves nothing behind — abandoning a resize with `Esc`, or
interrupting a slow sort with `Ctrl-C`, does not consume an undo step. The
history keeps the last 32 changes, and drops the oldest early if they are
holding on to too many rows between them.

## Summary line

`=` puts a line at the foot of the table, below the rows and staying put as they
scroll:

```
#  sample reads depth
1  S1     1000  10.5
2  S2     3000  20.25
3  S3     2000  NA
Σμ        6000  15.38
```

It opens on **auto**: a total for each numeric column, except where a column is
being read on a log scale (`%`), which is averaged instead — a total of
log-scaled measurements rarely means anything.

After that, `=` moves the **selected column** on through `total`, `mean`, `sd`
and `mean±sd`, and round to auto again, so different columns can show different
things. `( =` takes a whole block of columns at once and keeps the aim while you
cycle, which is how to move them all together. Turning the line on and putting it
away are about the line rather than any one column, so they take the whole of it
either way: the first `=` turns it on and **holding `=` down** removes it. The
gutter marks what the selected column is showing (`Σμ`, `Σ`, `μ`, `σ`, `±`) and
the status bar names it.

- Only numeric columns get a figure; the rest are left blank, and nulls are not
  counted rather than counting as zero.
- It counts **the rows on display**, so a filter changes the totals — and a sort
  does not, since it only changes their order.
- `sd` is the sample standard deviation (the `n − 1` one, as spreadsheets and R
  use), and needs at least two values.
- A column widens if it has to, so `mean±sd` arrives whole rather than clipped to
  something that reads like a total.
- A column with fixed decimals (`<`/`>`) has its figure shown to the same
  number, so the line matches the column above it.

The figures are worked out once for the rows on display and kept, so moving
through the cycle costs nothing; they are worked out again when a filter changes
what is counted. They are read a chunk at a time and never build a row list, so
summarising an unfiltered file of any size costs memory-free passes rather than
an index. The line is part of an arrangement, so `w` remembers it.

## Patterns

A tuned view is worth keeping. `w` remembers the current arrangement and it is
applied the next time that file is opened — the status bar says which pattern
matched. `--no-pattern` opens files as they come.

What is remembered: which columns are shown and in what order, which are hidden,
widths, numeric styles, the sort (including a keyed `S` sort), the frozen prefix,
the row filter, the row-number gutter, the summary line and what each column
shows on it, and **how the top of the file is read** —
so a spreadsheet with two junk rows above its real header opens correctly for
good, rather than needing `H` every time.

Everything is stored by **column name**, never by position:

```json
{
  "bind": "*_stats.csv",
  "columns": ["sample", "label", "depth"],
  "hidden": ["junk"],
  "widths": { "label": 7 },
  "numeric": { "depth": { "align": true, "log": true } },
  "sort": { "column": "sample", "descending": false,
            "key": { "from": 1, "to": 2, "method": "nat" } },
  "frozen_through": "sample",
  "header": { "skip": 0, "named": true },
  "row_numbers": true,
  "summary": "auto",
  "summaries": { "reads": "mean" }
}
```

That is what makes a pattern survive the file changing under it. Reopen a file
whose columns have moved, one has gone and another is new, and the saved names
are placed in the saved order, the ones that are gone are skipped, and a column
the pattern has never heard of stays **visible at the end** — a new column
appears rather than vanishing because a pattern written before it did not mention
it. The frozen prefix is named by its last column for the same reason.

The pattern is derived from the live view at the moment you press `w`. There is
no running record of "column X belongs at position Y" to keep in step as you
work, and so nothing that can drift out of step with what you are looking at.

### What it is tied to

The prompt opens pre-filled with the file's **name** — not its full path, so a
pattern follows a file that gets regenerated in another directory — and you can
edit it to anything, including a glob:

- `run1_stats.tsv` — that name, wherever it lives
- `*_stats.tsv` — every file whose name ends that way
- `/data/cohort/*.tsv` — a binding containing `/` is matched against the whole
  path instead, for when one directory should be treated differently

An exact name beats a glob, and among globs the longest binding wins, so
`*_stats.tsv` takes precedence over a blanket `*.tsv`. Submitting an empty
binding forgets the pattern for the current file. A workbook is remembered per
sheet, so each sheet of one file can be arranged differently.

### Where it lives

`$LAMBRIS_CONFIG` if set, otherwise `$XDG_CONFIG_HOME/lambris/patterns.json`,
otherwise `~/.config/lambris/patterns.json` — one path on every unix-ish system,
which is where command-line tools tend to settle. The file is pretty-printed
JSON and meant to be readable: editing it by hand, or copying a pattern between
machines, is expected. Every field is optional, so a hand-written pattern can set
one thing and leave the rest alone. An unreadable file is treated as no
patterns — a viewer should still open the file you asked for.

Patterns belong to files, so a join or a transposed view cannot have one; `w`
there says so rather than saving something that could never be matched again.
The search is left out too: it moves the cursor, which makes it navigation rather
than part of an arrangement. A pattern is the starting point for a view, not a
change to walk back from, so `z` will not undo it.

## Sorting by part of a column

`s` sorts by a whole column. `S` sorts by a slice of it — `sort -k 1.10,1.11`,
except you pick the characters by eye instead of counting them.

Press `S` on a column and the arrows move the **start** of the slice; `Enter`,
and they move the **end**; `Enter` again, and you choose how to compare it. The
slice is drawn into *every* row of the column at once and moves as you press, so
the offsets are judged against the whole column rather than one value:

```
1  SMP_2024_07_a        chars 10-11
2  SMP_2024_03_b               ▔▔
3  SHORT                ← too short: no key here
```

The end edge **opens at the far side of the field**, so taking everything from
the start onwards needs no arrows at all: `S` `Enter` `Enter` `v` sorts by the
whole value, naturally — which is most of what this is used for. Walk the end
back with `←` only when you want less than that.

`j`/`k` scroll while you choose, so the offsets can be checked further down the
table, and `Esc` gives up at any point.

Three ways to compare, all ascending (`s` afterwards cycles the direction,
keeping the slice):

| Key | Method | |
| --- | --- | --- |
| `a` | alphabetic | character by character |
| `n` | numeric | parsed as a number; anything that won't parse sorts last |
| `v` | natural | digits inside text compared as numbers, so `chr2` comes before `chr10` |

**Natural** is there because the other two both get identifiers wrong:
alphabetically `chr10` precedes `chr2`, and numerically neither parses at all.
It is `sort -V`'s behaviour. The other methods `sort(1)` offers — month,
human-numeric, random — have no place in a viewer, so they aren't here.

A row whose field is too short to reach the slice has no key and sorts **last**
ascending, whichever method is chosen, rather than being treated as an empty
string. Offsets are characters, shown 1-based and inclusive (`id[10-11]` in the
status bar) to match `sort -k`.

## Columns

`x` hides the column under the cursor, `[` and `]` move it left and right
(`Shift-←`/`Shift-→` do the same where the terminal sends them), `r` sets widths
(`%` inside a resize fits the values), and `u` puts everything back in the file's
own order. The status bar
counts what is hidden.

Nothing is deleted — this is the *view*'s column order, held per tab, so the
file is untouched and `u` always gets you back. It earns its keep after a join,
where two tables' columns arrive together and only a few of them matter.

### Several columns at once

A column command normally acts on the column under the cursor. `(` aims the next
one at **this column and every column to its right**, and `)` at **this column
and everything to its left**. The covered headers are marked and the bottom line
names the range while the aim is pending:

```
#  name a   b     c
 columns 2-4  the next column command takes all of them · r % < > = x
```

It applies to `r` (width), `%` (numeric styling), `<`/`>` (decimals), `=`
(summary) and `x` (hide). A scoped command works out what the **selected**
column should become and gives every covered column the same — the point of
asking for a block is to even it out, not to nudge each column from wherever it
happened to be. Columns that hold no numbers are passed over by the numeric ones.

The aim lasts for a **run** of column commands, so `( % > =` all land on the
same block and `( = = =` cycles it without pressing `(` again. Anything else — a
movement, `Esc` — drops it, so it can never quietly apply to a command typed much
later. `r` is the exception: a resize is a whole interaction rather than a
repeatable keypress, so it spends the aim on the way in.

Sorting deliberately has no scoped form: sorting by several columns
is a *composite key* — by A, then by B within it — which is a different thing
from doing one command to several columns.

### Widths

Columns size themselves to their name and contents, up to 40 characters. `r`
sets the selected column's width by hand, and `( r` does the same for that column
and every column to its right. Both the arrows and `h`/`l`/`j`/`k` widen and narrow —
neither pair is obviously the right one for a width, so both work. `Enter` keeps
what you set, `Esc` puts back the widths you started with, and `u` forgets widths
along with order and visibility.

A scoped resize **evens the block out**: every column it covers takes the *same*
width, so a narrow column becomes wider rather than shrinking by the same amount
as its neighbours. That is what makes it useful on a run of similar columns — and
why it snaps them together the moment you press `r`, before you adjust anything:

```
a_long_name v                 x   y        as loaded
a_long_name v           x           y      ( r  — all four at 11
a_long_name   v             x             y     →→  — all four at 13
```

Two exceptions to "one width":

- `%` fits each column to **its own values, ignoring the column name**, which is
  often the longer of the two. Over a block that leaves the columns at different
  widths — each as wide as its data needs. A name that no longer fits is not
  lost: the status bar shows it whenever the cursor is on that column.
- After `%` the columns hold their own sizes and an adjustment moves them all by
  the same amount, rather than flattening them again. `0` returns to sizing by
  name and content, and to evening out.

A width set by hand runs from a single character up to 200 — the point of
widening by hand is to see a long value, so it is not held to the 40 that
content-derived widths are.

### When something doesn't fit

A clipped cell ends in `…`, which tells you something is missing but not what.
So when the cell under the cursor is cut short, the **status bar shows it in
full** in whatever room is left over, and the column's name follows if there is
still space:

```
 row 1/2  col 1/2   = this is a really rather long cell value  a_very_long_col…
```

Content comes first and keeps the room: on a narrow terminal the column name
gives way rather than the value. Info mode (`i`) already spells out the name,
type and full value on its own line, so while it is on the status bar stops
repeating them.

Because it is what you are looking at, the rest of the viewer follows it:
searching and filtering consider only the columns on display (so a hit can never
put the cursor on a hidden column), and **transpose and join take the columns you
can see, in the order you put them**. Hide the noise, then join, and the result
has only the columns you kept.

`Ctrl-<`/`Ctrl->` would have been the obvious pair for moving, but terminals
cannot distinguish Ctrl from shifted punctuation — most send nothing at all for
`Ctrl-<` — so `[`/`]` are used instead.

## Join

`J` starts a small wizard: go to the column holding the key — `Tab` switches
tabs, `h`/`l` move across columns — and press `Enter`; then do the same on the
other side and press `Enter` again. The bottom line says which step you are on
and names the column you picked. `Esc` backs out at any point.

The result opens as a new tab, labelled `left ⋈ right`:

```
#  sample depth label            row 1/3  col 1/3   3 rows · 2 matched, 1 unmatched
1  S1     10    control
2  S2     20    treated
3  S3     30    NA
```

It is a **left join**: every row of the first side is kept, and one that matched
nothing carries `NA` across the second side's columns — so what *didn't* match
stays visible instead of silently disappearing. A key appearing several times on
the right multiplies the left row, as a join does. The status line reports how
many rows matched and how many didn't.

- The second side's **key column is dropped**, since it would repeat the first
  one exactly. Any other name clash gets a `_2` suffix.
- Keys are compared as the **trimmed text you see**, so a number in one file
  matches the same number stored as text in another. A blank key matches
  nothing — pairing rows on "no value" is never what you meant.
- **Column types survive**, so a joined numeric column still sorts numerically
  and takes `%`.
- Each side contributes **the rows and columns it is currently showing**, so a
  filter, a sort, a transposed view or a hidden column all carry through — you
  can transpose a sheet and join what is on screen, or hide the columns you
  don't want before joining rather than after.
- Closing a tab mid-wizard keeps a pick pointing at the same table; closing the
  picked tab itself cancels the join rather than quietly aiming it elsewhere.

A join is the one operation that cannot stream: matching keys means holding both
key columns, and the result can address any row of either side. Both sides are
therefore materialised, and each is capped at **200 000 rows** — enough that the
columns stay in the chunk cache. Past that it declines rather than trying; a long
join can be abandoned with `Esc`/`Ctrl-C` like any other heavy operation.

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

Core viewer plus regex search, row filtering, type-aware sorting (whole column
or a slice of one), column freeze, hiding, reordering and resizing columns,
undo/redo over all of it, a per-column summary line, saved per-file
arrangements, a transposed view, joins between tabs, and tabs over several open
files or workbook sheets. Nulls are shown as a highlighted `NA`. Sort
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
