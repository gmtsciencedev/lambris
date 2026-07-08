use std::fs::File;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use arrow::array::ArrayRef;
use arrow::record_batch::RecordBatch;
use arrow::util::display::{ArrayFormatter, FormatOptions};
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;

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
}
