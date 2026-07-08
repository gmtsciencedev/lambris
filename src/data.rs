use std::fs::File;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use arrow::array::ArrayRef;
use arrow::record_batch::RecordBatch;
use arrow::util::display::{ArrayFormatter, FormatOptions};
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use regex::Regex;

/// A parquet file loaded fully into memory as a single record batch, plus the
/// metadata lambris needs to render it (column names, types, dimensions).
pub struct Dataset {
    pub path: PathBuf,
    batch: RecordBatch,
    pub column_names: Vec<String>,
    pub column_types: Vec<String>,
    pub nrows: usize,
    pub ncols: usize,
}

impl Dataset {
    /// Read every row group of `path` and concatenate the batches so cell
    /// access is a simple O(1) index into one array per column.
    pub fn load(path: &Path) -> Result<Self> {
        let file =
            File::open(path).with_context(|| format!("opening {}", path.display()))?;
        let builder = ParquetRecordBatchReaderBuilder::try_new(file)
            .with_context(|| format!("reading parquet metadata from {}", path.display()))?;
        let schema = builder.schema().clone();
        let reader = builder.build().context("building parquet reader")?;

        let mut batches = Vec::new();
        for batch in reader {
            batches.push(batch.context("decoding parquet batch")?);
        }
        let batch = arrow::compute::concat_batches(&schema, &batches)
            .context("concatenating parquet batches")?;

        let column_names = schema
            .fields()
            .iter()
            .map(|f| f.name().clone())
            .collect::<Vec<_>>();
        let column_types = schema
            .fields()
            .iter()
            .map(|f| f.data_type().to_string())
            .collect::<Vec<_>>();

        let nrows = batch.num_rows();
        let ncols = batch.num_columns();

        Ok(Self {
            path: path.to_path_buf(),
            batch,
            column_names,
            column_types,
            nrows,
            ncols,
        })
    }

    pub fn column(&self, col: usize) -> &ArrayRef {
        self.batch.column(col)
    }

    pub fn is_null(&self, col: usize, row: usize) -> bool {
        self.column(col).is_null(row)
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

    /// Find the next cell matching `re`, scanning the view `rows` in row-major
    /// order starting just after `(start_row, start_col)` and wrapping around.
    /// Returns a `(view_row, col)` position, where `view_row` indexes `rows`.
    pub fn find_match(
        &self,
        re: &Regex,
        rows: &[usize],
        start_row: usize,
        start_col: usize,
        forward: bool,
    ) -> Option<(usize, usize)> {
        if rows.is_empty() || self.ncols == 0 {
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
