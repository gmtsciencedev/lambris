# lambris

A terminal parquet file viewer, in the manner of [csvlens](https://github.com/YS-L/csvlens).

Opens a `.parquet` file and lets you scroll around it in a TUI — a header row with
column names, a row-number gutter, a status bar showing the selected cell's column
name and Arrow type, and truncation of wide values. Null values render as a
highlighted `NA`. Supports regex search (`/`) and row filtering (`&`).

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
| `/` | Search (regex, case-insensitive); jumps to the first match |
| `n` / `N` | Jump to next / previous search match |
| `&` | Filter rows to those with a cell matching a regex |
| `Esc` | Clear search, then filter, then quit |
| `q` / `Ctrl-c` | Quit |

While typing a search or filter, `Enter` commits and `Esc` cancels. Submitting an
empty query clears that search/filter.

## Scope

Core viewer plus regex search and row filtering. Nulls are shown as a highlighted
`NA`. Column freeze and sorting are not implemented yet.

## How it works

The whole file is read into memory and its row groups concatenated into a single
Arrow `RecordBatch`, so cell access is an O(1) index into one array per column.
Cells are formatted on demand for the visible window using Arrow's `ArrayFormatter`,
and column widths are computed from the visible rows each frame.

## Development

```sh
cargo test    # headless render + navigation tests (build their own fixture)
cargo build
```
