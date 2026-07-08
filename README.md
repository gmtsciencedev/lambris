# lambris

A terminal viewer for parquet, CSV, and TSV files, in the manner of
[csvlens](https://github.com/YS-L/csvlens).

Opens a data file and lets you scroll around it in a TUI — a header row with
column names, a row-number gutter, a status bar showing the selected cell's column
name and Arrow type, and truncation of wide values. Null values render as a
highlighted `NA`. Supports global regex search (`/`), column-scoped search
(`-`), and row filtering (`&`).

## Usage

```sh
cargo run --release -- path/to/file.parquet
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
| `i` | Toggle info mode — the bottom line shows the selected column's name, Arrow type, and the full (untruncated) cell value |
| `Esc` | Cancel a running operation; otherwise clear search, then filter, then quit |
| `q` / `Ctrl-c` | Cancel a running operation; otherwise quit |

While a heavy operation is running (sorting, filtering, or searching a large
file), `Esc` or `Ctrl-C` aborts it and leaves the previous state untouched.

The bottom line shows the main commands by default; `i` swaps it for the column
info view.

While typing a search or filter, `Enter` commits and `Esc` cancels. Submitting an
empty query clears that search/filter.

## Formats

The format is autodetected:

- **Parquet** — recognised by its `PAR1` magic number.
- **CSV / TSV** — anything else is read as delimited text. The delimiter comes
  from the extension (`.tsv`/`.tab` → tab, `.csv` → comma) or, for unknown
  extensions, is sniffed from the first non-comment line. The first row is
  treated as a header, and column types are inferred from a sample of the data.

### Comment lines

Files that begin with `#` comment lines (MetaPhlAn and other bioinformatics
tools) are handled automatically:

- A leading block of `#` lines is skipped.
- If the **last** `#` line has the same number of columns as the data (e.g.
  MetaPhlAn's `#clade_name<TAB>…`), it is used as the header. Otherwise the
  comment block is treated as pure preamble and the first non-`#` line is the
  header.

The detection is most reliable for TSV, where comment/command lines rarely
contain tabs; a CSV comment that happens to have the same comma count as the
data could be misread as a header.

## Scope

Core viewer plus regex search, row filtering, type-aware sorting, and column
freeze. Nulls are shown as a highlighted `NA`. Sort composes with the active
filter, and the cursor stays on the same record across a re-sort.

## Big files

Data is loaded lazily so memory stays bounded regardless of file size:

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

## License

Licensed under the [MIT License](LICENSE).
