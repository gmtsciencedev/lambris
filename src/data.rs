use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use arrow::array::{make_comparator, ArrayRef};
use arrow::compute::SortOptions;
use arrow::csv::reader::Format as CsvFormat;
use arrow::csv::ReaderBuilder as CsvReaderBuilder;
use arrow::datatypes::SchemaRef;
use arrow::record_batch::RecordBatch;
use arrow::util::display::{ArrayFormatter, FormatOptions};
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use regex::Regex;

/// How the number of records to sample when inferring a CSV/TSV schema.
const CSV_INFER_ROWS: usize = 1000;

/// The kind of file behind a path, chosen by autodetection.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Source {
    Parquet,
    /// Delimited text with the given field separator (`,` or `\t`).
    Delimited(u8),
}

/// A file loaded fully into memory as a single record batch, plus the metadata
/// lambris needs to render it (column names, types, dimensions). Parquet and
/// CSV/TSV both funnel into the same Arrow representation, so everything above
/// this layer is format-agnostic.
pub struct Dataset {
    pub path: PathBuf,
    batch: RecordBatch,
    pub column_names: Vec<String>,
    pub column_types: Vec<String>,
    pub nrows: usize,
    pub ncols: usize,
}

impl Dataset {
    /// Autodetect the file format and load it into a single record batch.
    pub fn load(path: &Path) -> Result<Self> {
        let batch = match detect_source(path)? {
            Source::Parquet => read_parquet(path)?,
            Source::Delimited(delim) => read_delimited(path, delim)?,
        };
        Ok(Self::from_batch(path.to_path_buf(), batch))
    }

    /// Build the dataset (and its cached metadata) from a loaded batch.
    fn from_batch(path: PathBuf, batch: RecordBatch) -> Self {
        let schema = batch.schema();
        let column_names = schema.fields().iter().map(|f| f.name().clone()).collect();
        let column_types = schema
            .fields()
            .iter()
            .map(|f| f.data_type().to_string())
            .collect();
        let nrows = batch.num_rows();
        let ncols = batch.num_columns();
        Self {
            path,
            batch,
            column_names,
            column_types,
            nrows,
            ncols,
        }
    }

    pub fn column(&self, col: usize) -> &ArrayRef {
        self.batch.column(col)
    }

    pub fn is_null(&self, col: usize, row: usize) -> bool {
        self.column(col).is_null(row)
    }

    /// Order `rows` (original indices) by the values in `col`, using Arrow's
    /// type-aware comparator. Stable, so ties keep their prior order.
    pub fn sort_indices(
        &self,
        rows: &[usize],
        col: usize,
        descending: bool,
    ) -> Result<Vec<usize>> {
        let array = self.column(col);
        let cmp = make_comparator(array.as_ref(), array.as_ref(), SortOptions::default())
            .with_context(|| format!("sorting column {}", self.column_names[col]))?;
        let mut out = rows.to_vec();
        out.sort_by(|&a, &b| {
            let ord = cmp(a, b);
            if descending { ord.reverse() } else { ord }
        });
        Ok(out)
    }

    /// The full (untruncated) display value of a single cell; `None` if null.
    pub fn cell_display(&self, col: usize, row: usize) -> Result<Option<String>> {
        if self.is_null(col, row) {
            return Ok(None);
        }
        let formatter = &self.formatters(&[col])?[0];
        Ok(Some(formatter.value(row).to_string()))
    }

    /// Build one formatter per requested column, valid for the whole dataset.
    /// Formatters borrow their arrays, so we hand them back to the caller
    /// rather than storing them (the struct would otherwise be self-referential).
    pub fn formatters<'a>(&'a self, cols: &[usize]) -> Result<Vec<ArrayFormatter<'a>>> {
        let opts = FormatOptions::default().with_null("");
        cols.iter()
            .map(|&c| {
                ArrayFormatter::try_new(self.column(c), &opts)
                    .with_context(|| format!("formatting column {}", self.column_names[c]))
            })
            .collect()
    }

    /// Return the original row indices where any (non-null) cell matches `re`.
    pub fn filter_rows(&self, re: &Regex) -> Result<Vec<usize>> {
        let cols: Vec<usize> = (0..self.ncols).collect();
        let fmts = self.formatters(&cols)?;
        let mut out = Vec::new();
        for r in 0..self.nrows {
            let hit = (0..self.ncols).any(|c| {
                !self.is_null(c, r) && re.is_match(&fmts[c].value(r).to_string())
            });
            if hit {
                out.push(r);
            }
        }
        Ok(out)
    }

    /// Find the next cell matching `re`, scanning the view `rows` starting just
    /// after `(start_row, start_col)` and wrapping around. When `scope` is
    /// `Some(col)` the search is confined to that single column; otherwise it
    /// sweeps every column in row-major order. Returns a `(view_row, col)`
    /// position, where `view_row` indexes `rows`.
    pub fn find_match(
        &self,
        re: &Regex,
        rows: &[usize],
        start_row: usize,
        start_col: usize,
        forward: bool,
        scope: Option<usize>,
    ) -> Option<(usize, usize)> {
        if rows.is_empty() || self.ncols == 0 {
            return None;
        }
        if let Some(col) = scope {
            let fmt = &self.formatters(&[col]).ok()?[0];
            let n = rows.len();
            for i in 1..=n {
                let vr = if forward {
                    (start_row + i) % n
                } else {
                    (start_row + n - i) % n
                };
                let orig = rows[vr];
                if !self.is_null(col, orig) && re.is_match(&fmt.value(orig).to_string()) {
                    return Some((vr, col));
                }
            }
            return None;
        }
        let cols: Vec<usize> = (0..self.ncols).collect();
        let fmts = self.formatters(&cols).ok()?;
        let total = rows.len() * self.ncols;
        let start = start_row * self.ncols + start_col;
        for i in 1..=total {
            let p = if forward {
                (start + i) % total
            } else {
                (start + total - i) % total
            };
            let vr = p / self.ncols;
            let c = p % self.ncols;
            let orig = rows[vr];
            if !self.is_null(c, orig) && re.is_match(&fmts[c].value(orig).to_string()) {
                return Some((vr, c));
            }
        }
        None
    }
}

/// Autodetect the format: parquet files carry a `PAR1` magic number, so trust
/// that; otherwise treat the file as delimited text and pick the separator
/// from the extension (`.tsv`/`.tab` → tab) or by sniffing the first line.
fn detect_source(path: &Path) -> Result<Source> {
    let mut file =
        File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut magic = [0u8; 4];
    let read = file.read(&mut magic).unwrap_or(0);
    if read == 4 && &magic == b"PAR1" {
        return Ok(Source::Parquet);
    }

    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|s| s.to_ascii_lowercase())
        .unwrap_or_default();
    let delimiter = match ext.as_str() {
        "tsv" | "tab" => b'\t',
        "csv" => b',',
        _ => sniff_delimiter(path)?,
    };
    Ok(Source::Delimited(delimiter))
}

/// Guess the delimiter of an unknown text file from its header line: tab if
/// tabs outnumber commas, else comma.
fn sniff_delimiter(path: &Path) -> Result<u8> {
    let file = File::open(path)?;
    let mut header = String::new();
    BufReader::new(file)
        .read_line(&mut header)
        .with_context(|| format!("reading {}", path.display()))?;
    let tabs = header.matches('\t').count();
    let commas = header.matches(',').count();
    Ok(if tabs > commas { b'\t' } else { b',' })
}

/// Read every parquet row group and concatenate into one batch.
fn read_parquet(path: &Path) -> Result<RecordBatch> {
    let file = File::open(path)?;
    let builder = ParquetRecordBatchReaderBuilder::try_new(file)
        .with_context(|| format!("reading parquet metadata from {}", path.display()))?;
    let schema = builder.schema().clone();
    let reader = builder.build().context("building parquet reader")?;

    let mut batches = Vec::new();
    for batch in reader {
        batches.push(batch.context("decoding parquet batch")?);
    }
    concat(&schema, &batches)
}

/// Infer a schema from a sample of rows, then read the whole delimited file.
/// The first row is treated as a header.
fn read_delimited(path: &Path, delimiter: u8) -> Result<RecordBatch> {
    let format = CsvFormat::default()
        .with_header(true)
        .with_delimiter(delimiter);

    let infer_file = File::open(path)?;
    let (schema, _) = format
        .infer_schema(BufReader::new(infer_file), Some(CSV_INFER_ROWS))
        .with_context(|| format!("inferring schema from {}", path.display()))?;
    let schema = Arc::new(schema);

    let data_file = File::open(path)?;
    let reader = CsvReaderBuilder::new(schema.clone())
        .with_format(format)
        .build(BufReader::new(data_file))
        .context("building CSV reader")?;

    let mut batches = Vec::new();
    for batch in reader {
        batches.push(batch.context("decoding CSV batch")?);
    }
    concat(&schema, &batches)
}

/// Concatenate batches, yielding an empty batch (with the schema) if there are
/// none — `concat_batches` requires at least one input otherwise.
fn concat(schema: &SchemaRef, batches: &[RecordBatch]) -> Result<RecordBatch> {
    if batches.is_empty() {
        Ok(RecordBatch::new_empty(schema.clone()))
    } else {
        arrow::compute::concat_batches(schema, batches).context("concatenating batches")
    }
}
