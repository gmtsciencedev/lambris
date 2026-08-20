use std::collections::{HashMap, VecDeque};
use std::fs::File;
use std::io::{BufRead, BufReader, Cursor, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use arrow::array::{
    make_comparator, Array, ArrayRef, BooleanArray, Date32Array, Float64Array, Int64Array,
    StringArray, TimestampMillisecondArray,
};
use arrow::compute::SortOptions;
use arrow::csv::reader::Format as CsvFormat;
use arrow::csv::ReaderBuilder as CsvReaderBuilder;
use arrow::datatypes::{DataType, Field, Schema, SchemaRef};
use arrow::record_batch::RecordBatch;
use arrow::util::display::{ArrayFormatter, FormatOptions};
// `DataType` is imported anonymously: it carries the cell accessors we need
// (`as_datetime`), and the name would collide with Arrow's own `DataType`.
use calamine::{open_workbook_auto, Data, DataType as _, Range, Reader};
use chrono::{NaiveDate, NaiveTime};
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use regex::Regex;

/// Rows per lazily-loaded chunk. Chunks are the unit of caching.
const CHUNK: usize = 8192;
/// Number of records to sample when inferring a CSV/TSV schema.
const CSV_INFER_ROWS: usize = 1000;
/// How many chunks to keep resident. Bounds memory to ~`CACHE_CHUNKS * CHUNK`
/// rows regardless of file size.
const CACHE_CHUNKS: usize = 32;

/// The backing store for a dataset, holding just enough to fetch any chunk on
/// demand — never the whole file's contents.
enum Backend {
    Parquet,
    /// Delimited text: the field separator plus byte offsets of every chunk's
    /// first row (`chunk_offsets[k]` = start of row `k * CHUNK`).
    Csv {
        delimiter: u8,
        chunk_offsets: Vec<u64>,
    },
    /// A fully-resident batch (e.g. a transposed view), served in chunks.
    Memory(Arc<RecordBatch>),
}

/// A lazily-loaded view of one table: a parquet or CSV/TSV file, or a single
/// worksheet of an Excel workbook. For the lazy formats only the schema, some
/// metadata, and a bounded LRU cache of decoded chunks live in memory and cells
/// are fetched on demand; a worksheet has no seekable structure, so it is
/// decoded once and served from memory. Everything above this layer is
/// format-agnostic.
pub struct Dataset {
    pub path: PathBuf,
    /// Name shown in the title bar (the file name, or a transposed label).
    pub label: String,
    backend: Backend,
    schema: SchemaRef,
    pub column_names: Vec<String>,
    pub column_types: Vec<String>,
    pub nrows: usize,
    pub ncols: usize,
    cache: Mutex<ChunkCache>,
}

impl Dataset {
    /// Autodetect the file format and open every table the file holds: one per
    /// worksheet for an Excel workbook, and exactly one for anything else.
    pub fn load_all(path: &Path) -> Result<Vec<Self>> {
        match detect_source(path)? {
            Source::Excel => load_workbook(path),
            source => Ok(vec![Self::from_source(path, source)?]),
        }
    }

    /// Open a file as a single table; a workbook yields its first sheet with
    /// data. Prefer [`Dataset::load_all`] when every sheet should be shown.
    // Used throughout the tests, where one table per file is the norm.
    #[allow(dead_code)]
    pub fn load(path: &Path) -> Result<Self> {
        let mut all = Self::load_all(path)?;
        if all.is_empty() {
            anyhow::bail!("{} holds no data", path.display());
        }
        Ok(all.remove(0))
    }

    /// Prepare a single-table format for lazy access. Reads only metadata
    /// (parquet) or builds a byte-offset index (CSV) — never the data.
    fn from_source(path: &Path, source: Source) -> Result<Self> {
        let (backend, schema, nrows) = match source {
            Source::Parquet => load_parquet_meta(path)?,
            Source::Delimited(delim) => load_csv_meta(path, delim)?,
            // Workbooks go through `load_workbook`: a sheet is fully decoded
            // into memory, so there is no lazy backend to set up here.
            Source::Excel => unreachable!("workbooks are opened per sheet"),
        };
        let column_names = schema.fields().iter().map(|f| f.name().clone()).collect();
        let column_types = schema
            .fields()
            .iter()
            .map(|f| f.data_type().to_string())
            .collect();
        let ncols = schema.fields().len();
        let label = file_label(path);
        Ok(Self {
            path: path.to_path_buf(),
            label,
            backend,
            schema,
            column_names,
            column_types,
            nrows,
            ncols,
            cache: Mutex::new(ChunkCache::new(CACHE_CHUNKS)),
        })
    }

    /// Build a dataset from an already-materialised batch (used for transpose).
    fn in_memory(batch: RecordBatch, path: PathBuf, label: String) -> Self {
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
            label,
            backend: Backend::Memory(Arc::new(batch)),
            schema,
            column_names,
            column_types,
            nrows,
            ncols,
            cache: Mutex::new(ChunkCache::new(CACHE_CHUNKS)),
        }
    }

    /// Build the transpose of `orig_rows` (in view order): the first column's
    /// values become the column headers, and the remaining columns become rows
    /// labelled by their name in a leading `field` column. Each record column's
    /// type is inferred from its values, so the result behaves like any table.
    pub fn transpose(&self, orig_rows: &[usize]) -> Result<Dataset> {
        // Original columns 1.. become rows; column 0 titles the records.
        let field_cols: Vec<usize> = (1..self.ncols).collect();
        let field_values: Vec<Vec<Option<String>>> = field_cols
            .iter()
            .map(|&c| self.cells(c, orig_rows))
            .collect::<Result<_>>()?;
        let titles = self.cells(0, orig_rows)?;

        let mut names = Vec::with_capacity(orig_rows.len() + 1);
        let mut columns: Vec<ArrayRef> = Vec::with_capacity(orig_rows.len() + 1);

        // Leading column: the original column names.
        names.push("field".to_string());
        let field_names: StringArray = field_cols
            .iter()
            .map(|&c| Some(self.column_names[c].as_str()))
            .collect();
        columns.push(Arc::new(field_names));

        // One column per record, titled by the first column's value.
        for (r, title) in titles.iter().enumerate() {
            let name = title
                .clone()
                .unwrap_or_else(|| format!("row{}", orig_rows[r] + 1));
            let values: Vec<Option<String>> =
                (0..field_cols.len()).map(|f| field_values[f][r].clone()).collect();
            names.push(name);
            columns.push(infer_array(&values));
        }

        let fields: Vec<Field> = names
            .iter()
            .zip(&columns)
            .map(|(n, a)| Field::new(n, a.data_type().clone(), true))
            .collect();
        let batch = RecordBatch::try_new(Arc::new(Schema::new(fields)), columns)
            .context("building transposed batch")?;
        Ok(Dataset::in_memory(
            batch,
            self.path.clone(),
            format!("{} ⇄ transposed", self.label),
        ))
    }

    fn num_chunks(&self) -> usize {
        self.nrows.div_ceil(CHUNK)
    }

    /// Fetch chunk `k` (rows `k*CHUNK..`), loading and caching it if needed.
    fn chunk(&self, k: usize) -> Result<Arc<RecordBatch>> {
        if let Some(batch) = self.cache.lock().unwrap().get(k) {
            return Ok(batch);
        }
        let batch = Arc::new(self.load_chunk(k)?);
        self.cache.lock().unwrap().put(k, batch.clone());
        Ok(batch)
    }

    fn load_chunk(&self, k: usize) -> Result<RecordBatch> {
        match &self.backend {
            Backend::Parquet => self.load_parquet_chunk(k),
            Backend::Csv {
                delimiter,
                chunk_offsets,
            } => self.load_csv_chunk(k, *delimiter, chunk_offsets),
            Backend::Memory(batch) => {
                let start = k * CHUNK;
                if start >= batch.num_rows() {
                    return Ok(RecordBatch::new_empty(self.schema.clone()));
                }
                let len = CHUNK.min(batch.num_rows() - start);
                Ok(batch.slice(start, len))
            }
        }
    }

    fn load_parquet_chunk(&self, k: usize) -> Result<RecordBatch> {
        let file = File::open(&self.path)?;
        let reader = ParquetRecordBatchReaderBuilder::try_new(file)
            .context("opening parquet")?
            .with_batch_size(CHUNK)
            .with_offset(k * CHUNK)
            .with_limit(CHUNK)
            .build()
            .context("building parquet reader")?;
        let mut batches = Vec::new();
        for batch in reader {
            batches.push(batch.context("decoding parquet chunk")?);
        }
        concat(&self.schema, &batches)
    }

    fn load_csv_chunk(&self, k: usize, delimiter: u8, offsets: &[u64]) -> Result<RecordBatch> {
        let Some(&start) = offsets.get(k) else {
            return Ok(RecordBatch::new_empty(self.schema.clone()));
        };
        let mut file = File::open(&self.path)?;
        file.seek(SeekFrom::Start(start))?;
        // Read exactly this chunk's byte span (to EOF for the last chunk).
        let bytes = match offsets.get(k + 1) {
            Some(&end) => {
                let mut buf = vec![0u8; (end - start) as usize];
                file.read_exact(&mut buf)?;
                buf
            }
            None => {
                let mut buf = Vec::new();
                file.read_to_end(&mut buf)?;
                buf
            }
        };
        // The slice starts on a record boundary, so parse it header-free with
        // the schema we already inferred.
        let format = CsvFormat::default()
            .with_header(false)
            .with_delimiter(delimiter);
        let reader = CsvReaderBuilder::new(self.schema.clone())
            .with_format(format)
            .with_batch_size(CHUNK)
            .build(Cursor::new(bytes))
            .context("building CSV chunk reader")?;
        let mut batches = Vec::new();
        for batch in reader {
            batches.push(batch.context("decoding CSV chunk")?);
        }
        concat(&self.schema, &batches)
    }

    /// Whether a column holds a numeric Arrow type (int, uint, float, decimal).
    pub fn is_numeric(&self, col: usize) -> bool {
        use DataType::*;
        matches!(
            self.schema.field(col).data_type(),
            Int8 | Int16
                | Int32
                | Int64
                | UInt8
                | UInt16
                | UInt32
                | UInt64
                | Float16
                | Float32
                | Float64
                | Decimal128(_, _)
                | Decimal256(_, _)
        )
    }

    // Exercised by tests as a public accessor; the UI now goes through `cells`.
    #[allow(dead_code)]
    pub fn is_null(&self, col: usize, row: usize) -> bool {
        self.chunk(row / CHUNK)
            .map(|b| b.column(col).is_null(row % CHUNK))
            .unwrap_or(false)
    }

    /// The full (untruncated) display value of a single cell; `None` if null.
    pub fn cell_display(&self, col: usize, row: usize) -> Result<Option<String>> {
        let batch = self.chunk(row / CHUNK)?;
        let array = batch.column(col);
        let off = row % CHUNK;
        if array.is_null(off) {
            return Ok(None);
        }
        let opts = FormatOptions::default().with_null("");
        let formatter = ArrayFormatter::try_new(array, &opts)
            .with_context(|| format!("formatting column {}", self.column_names[col]))?;
        Ok(Some(formatter.value(off).to_string()))
    }

    /// Display strings for `col` at the given original row indices (`None` =
    /// null). Groups the requests by chunk so each chunk is decoded once.
    pub fn cells(&self, col: usize, rows: &[usize]) -> Result<Vec<Option<String>>> {
        let mut out = vec![None; rows.len()];
        let mut by_chunk: HashMap<usize, Vec<usize>> = HashMap::new();
        for (i, &r) in rows.iter().enumerate() {
            by_chunk.entry(r / CHUNK).or_default().push(i);
        }
        let opts = FormatOptions::default().with_null("");
        for (k, positions) in by_chunk {
            let batch = self.chunk(k)?;
            let array = batch.column(col);
            let formatter = ArrayFormatter::try_new(array, &opts)
                .with_context(|| format!("formatting column {}", self.column_names[col]))?;
            for i in positions {
                let off = rows[i] % CHUNK;
                out[i] = (!array.is_null(off)).then(|| formatter.value(off).to_string());
            }
        }
        Ok(out)
    }

    /// Materialise one whole column into a single array (used for sorting).
    /// Returns `None` if `cancel` fires while reading chunks.
    fn full_column(&self, col: usize, cancel: &dyn Fn() -> bool) -> Result<Option<ArrayRef>> {
        let mut arrays: Vec<ArrayRef> = Vec::with_capacity(self.num_chunks());
        for k in 0..self.num_chunks() {
            if cancel() {
                return Ok(None);
            }
            arrays.push(self.chunk(k)?.column(col).clone());
        }
        let refs: Vec<&dyn Array> = arrays.iter().map(|a| a.as_ref()).collect();
        Ok(Some(arrow::compute::concat(&refs).context("concatenating column")?))
    }

    /// Order `rows` (original indices) by the values in `col`, using Arrow's
    /// type-aware comparator. Stable, so ties keep their prior order. Returns
    /// `None` if `cancel` fires while reading the column (the in-memory sort
    /// itself is fast and runs to completion).
    pub fn sort_indices(
        &self,
        rows: &[usize],
        col: usize,
        descending: bool,
        cancel: impl Fn() -> bool,
    ) -> Result<Option<Vec<usize>>> {
        let Some(array) = self.full_column(col, &cancel)? else {
            return Ok(None);
        };
        let cmp = make_comparator(array.as_ref(), array.as_ref(), SortOptions::default())
            .with_context(|| format!("sorting column {}", self.column_names[col]))?;
        let mut out = rows.to_vec();
        out.sort_by(|&a, &b| {
            let ord = cmp(a, b);
            if descending { ord.reverse() } else { ord }
        });
        Ok(Some(out))
    }

    /// Return the original row indices where any (non-null) cell matches `re`.
    /// Streams the file one chunk at a time; returns `None` if `cancel` fires.
    pub fn filter_rows(
        &self,
        re: &Regex,
        cancel: impl Fn() -> bool,
    ) -> Result<Option<Vec<usize>>> {
        let opts = FormatOptions::default().with_null("");
        let mut out = Vec::new();
        for k in 0..self.num_chunks() {
            if cancel() {
                return Ok(None);
            }
            let batch = self.chunk(k)?;
            let formatters: Vec<ArrayFormatter> = (0..self.ncols)
                .map(|c| ArrayFormatter::try_new(batch.column(c), &opts).map_err(Into::into))
                .collect::<Result<_>>()?;
            for r in 0..batch.num_rows() {
                let hit = (0..self.ncols).any(|c| {
                    !batch.column(c).is_null(r) && re.is_match(&formatters[c].value(r).to_string())
                });
                if hit {
                    out.push(k * CHUNK + r);
                }
            }
        }
        Ok(Some(out))
    }

    /// Find the next cell matching `re`, scanning the view starting just after
    /// `(start_row, start_col)` and wrapping around. The view has `view_len`
    /// rows and `orig(i)` maps a view row to its original dataset row — so the
    /// identity view needs no materialised index. When `scope` is `Some(col)`
    /// the search is confined to that single column; otherwise it sweeps every
    /// column in row-major order. Returns a `(view_row, col)` position.
    pub fn find_match(
        &self,
        re: &Regex,
        view_len: usize,
        orig: impl Fn(usize) -> usize,
        start_row: usize,
        start_col: usize,
        forward: bool,
        scope: Option<usize>,
        cancel: impl Fn() -> bool,
    ) -> Option<(usize, usize)> {
        if view_len == 0 || self.ncols == 0 {
            return None;
        }
        let matches = |vr: usize, c: usize| -> bool {
            matches!(self.cell_display(c, orig(vr)), Ok(Some(s)) if re.is_match(&s))
        };
        // Checking every cell would syscall too often; a stride keeps the
        // worst-case cancel latency to a few chunk decodes.
        const STRIDE: usize = 512;
        if let Some(col) = scope {
            let n = view_len;
            for i in 1..=n {
                if i % STRIDE == 0 && cancel() {
                    return None;
                }
                let vr = if forward {
                    (start_row + i) % n
                } else {
                    (start_row + n - i) % n
                };
                if matches(vr, col) {
                    return Some((vr, col));
                }
            }
            return None;
        }
        let total = view_len * self.ncols;
        let start = start_row * self.ncols + start_col;
        for i in 1..=total {
            if i % STRIDE == 0 && cancel() {
                return None;
            }
            let p = if forward {
                (start + i) % total
            } else {
                (start + total - i) % total
            };
            let (vr, c) = (p / self.ncols, p % self.ncols);
            if matches(vr, c) {
                return Some((vr, c));
            }
        }
        None
    }
}

/// A tiny LRU cache of decoded chunks keyed by chunk index.
struct ChunkCache {
    capacity: usize,
    map: HashMap<usize, Arc<RecordBatch>>,
    /// Recency order; front is least-recently-used.
    order: VecDeque<usize>,
}

impl ChunkCache {
    fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            map: HashMap::new(),
            order: VecDeque::new(),
        }
    }

    fn get(&mut self, k: usize) -> Option<Arc<RecordBatch>> {
        let batch = self.map.get(&k)?.clone();
        self.touch(k);
        Some(batch)
    }

    fn put(&mut self, k: usize, batch: Arc<RecordBatch>) {
        if self.map.insert(k, batch).is_none() && self.map.len() > self.capacity {
            if let Some(evict) = self.order.pop_front() {
                self.map.remove(&evict);
            }
        }
        self.touch(k);
    }

    fn touch(&mut self, k: usize) {
        if let Some(pos) = self.order.iter().position(|&x| x == k) {
            self.order.remove(pos);
        }
        self.order.push_back(k);
    }
}

/// The kind of file behind a path, chosen by autodetection.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Source {
    Parquet,
    /// Delimited text with the given field separator (`,` or `\t`).
    Delimited(u8),
    /// An Excel (or OpenDocument) workbook, read one sheet at a time.
    Excel,
}

/// A ZIP container: xlsx, xlsm and ods all start with it.
const ZIP_MAGIC: &[u8] = b"PK\x03\x04";
/// The OLE2 compound-file header of legacy `.xls` workbooks.
const OLE2_MAGIC: &[u8] = &[0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1];

/// Autodetect the format from the file's magic number, falling back to the
/// extension. Parquet carries `PAR1`; workbooks are either a ZIP container
/// (xlsx/xlsm/ods) or the OLE2 one (xls), and since neither can be delimited
/// text they are handed to the workbook reader — which reports clearly if the
/// container turns out to hold something else. Everything else is read as
/// delimited text, with the separator taken from the extension
/// (`.tsv`/`.tab` → tab) or sniffed from the first line.
fn detect_source(path: &Path) -> Result<Source> {
    let mut file =
        File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut magic = [0u8; 8];
    let read = file.read(&mut magic).unwrap_or(0);
    let magic = &magic[..read];
    if magic.starts_with(b"PAR1") {
        return Ok(Source::Parquet);
    }
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|s| s.to_ascii_lowercase())
        .unwrap_or_default();
    let is_workbook_ext = matches!(
        ext.as_str(),
        "xlsx" | "xlsm" | "xlsb" | "xlam" | "xla" | "xls" | "ods"
    );
    if is_workbook_ext || magic.starts_with(ZIP_MAGIC) || magic.starts_with(OLE2_MAGIC) {
        return Ok(Source::Excel);
    }
    let delimiter = match ext.as_str() {
        "tsv" | "tab" => b'\t',
        "csv" => b',',
        _ => sniff_delimiter(path)?,
    };
    Ok(Source::Delimited(delimiter))
}

/// The file name, shown in the title bar and the tab strip.
fn file_label(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// Guess the delimiter of an unknown text file from its first non-comment
/// line: tab if tabs outnumber commas, else comma.
fn sniff_delimiter(path: &Path) -> Result<u8> {
    let mut reader = BufReader::new(File::open(path)?);
    loop {
        let mut line = String::new();
        let n = reader
            .read_line(&mut line)
            .with_context(|| format!("reading {}", path.display()))?;
        if n == 0 {
            return Ok(b','); // empty or all-comment file; harmless default
        }
        if line.starts_with('#') {
            continue;
        }
        let tabs = line.matches('\t').count();
        let commas = line.matches(',').count();
        return Ok(if tabs > commas { b'\t' } else { b',' });
    }
}

/// Read parquet schema and row count from the footer, without decoding data.
fn load_parquet_meta(path: &Path) -> Result<(Backend, SchemaRef, usize)> {
    let file = File::open(path)?;
    let builder = ParquetRecordBatchReaderBuilder::try_new(file)
        .with_context(|| format!("reading parquet metadata from {}", path.display()))?;
    let schema = builder.schema().clone();
    let nrows = builder.metadata().file_metadata().num_rows() as usize;
    Ok((Backend::Parquet, schema, nrows))
}

/// Infer a CSV/TSV schema from a sample, then scan the file to build the
/// chunk byte-offset index and count rows. Files that begin with `#` comment
/// lines take a dedicated path (see [`load_csv_meta_commented`]).
fn load_csv_meta(path: &Path, delimiter: u8) -> Result<(Backend, SchemaRef, usize)> {
    if starts_with_comment(path)? {
        return load_csv_meta_commented(path, delimiter);
    }
    let format = CsvFormat::default()
        .with_header(true)
        .with_delimiter(delimiter);
    let infer_file = File::open(path)?;
    let (schema, _) = format
        .infer_schema(BufReader::new(infer_file), Some(CSV_INFER_ROWS))
        .with_context(|| format!("inferring schema from {}", path.display()))?;
    let (chunk_offsets, nrows) = build_csv_index(path)?;
    Ok((
        Backend::Csv {
            delimiter,
            chunk_offsets,
        },
        Arc::new(schema),
        nrows,
    ))
}

/// Handle files with a leading `#` comment block (MetaPhlAn and friends).
/// Works out where the real header and data start, then infers column types
/// from the data alone — the comment lines never reach the chunk reader.
fn load_csv_meta_commented(path: &Path, delimiter: u8) -> Result<(Backend, SchemaRef, usize)> {
    let (data_start, names) = analyze_comment_header(path, delimiter)?;
    let (chunk_offsets, nrows) = build_index_from(path, data_start)?;

    // Types come from the data rows only (header, if any, is already excluded).
    let types = if nrows == 0 {
        vec![DataType::Utf8; names.len()]
    } else {
        infer_types_from(path, delimiter, data_start)?
    };
    // The inferred column count is authoritative; pad/truncate names to match.
    let fields: Vec<Field> = types
        .iter()
        .enumerate()
        .map(|(i, ty)| {
            let name = names
                .get(i)
                .cloned()
                .unwrap_or_else(|| format!("column_{}", i + 1));
            Field::new(name, ty.clone(), true)
        })
        .collect();
    let schema = Arc::new(Schema::new(fields));
    Ok((
        Backend::Csv {
            delimiter,
            chunk_offsets,
        },
        schema,
        nrows,
    ))
}

fn starts_with_comment(path: &Path) -> Result<bool> {
    let mut byte = [0u8; 1];
    let n = File::open(path)?.read(&mut byte).unwrap_or(0);
    Ok(n == 1 && byte[0] == b'#')
}

/// Find where the data begins in a commented file and what the column names
/// are. Two conventions are supported: the header is either the last `#` line
/// (when, stripped of `#`, its field count matches the first data row — the
/// MetaPhlAn style) or the first non-`#` line (a pure comment preamble).
fn analyze_comment_header(path: &Path, delimiter: u8) -> Result<(u64, Vec<String>)> {
    let delim = delimiter as char;
    let count_fields = |s: &str| s.split(delim).count();
    let split_header = |s: &str| -> Vec<String> {
        s.split(delim)
            .map(|f| f.trim().trim_matches('"').to_string())
            .collect()
    };

    let mut reader = BufReader::new(File::open(path)?);
    let mut offset: u64 = 0;
    let mut last_comment: Option<String> = None;
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line)?;
        if n == 0 {
            // No data line at all: fall back to the last comment as header.
            let names = last_comment
                .as_deref()
                .map(|c| split_header(c.strip_prefix('#').unwrap_or(c)))
                .unwrap_or_default();
            return Ok((offset, names));
        }
        let trimmed = line.trim_end_matches(['\n', '\r']);
        if trimmed.starts_with('#') {
            last_comment = Some(trimmed.to_string());
            offset += n as u64;
            continue;
        }
        // First non-comment line: header or first data row?
        if let Some(comment) = &last_comment {
            let stripped = comment.strip_prefix('#').unwrap_or(comment);
            if count_fields(stripped) > 1 && count_fields(stripped) == count_fields(trimmed) {
                // MetaPhlAn style: this line is data, the header was the comment.
                return Ok((offset, split_header(stripped)));
            }
        }
        // Pure preamble: this line is the header; data starts after it.
        return Ok((offset + n as u64, split_header(trimmed)));
    }
}

/// Infer column data types from the data rows starting at `data_start`.
fn infer_types_from(path: &Path, delimiter: u8, data_start: u64) -> Result<Vec<DataType>> {
    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(data_start))?;
    let format = CsvFormat::default()
        .with_header(false)
        .with_delimiter(delimiter);
    let (schema, _) = format
        .infer_schema(BufReader::new(file), Some(CSV_INFER_ROWS))
        .with_context(|| format!("inferring types from {}", path.display()))?;
    Ok(schema
        .fields()
        .iter()
        .map(|f| f.data_type().clone())
        .collect())
}

/// Build the chunk byte-offset index for a headed file: skip the header row,
/// then index the data rows that follow.
fn build_csv_index(path: &Path) -> Result<(Vec<u64>, usize)> {
    let data_start = first_record_end(path)?;
    build_index_from(path, data_start)
}

/// Byte offset just past the first record (quote-aware), i.e. the start of the
/// second line — or EOF if the file has no line break.
fn first_record_end(path: &Path) -> Result<u64> {
    let mut reader = BufReader::new(File::open(path)?);
    let mut buf = [0u8; 64 * 1024];
    let mut pos: u64 = 0;
    let mut in_quotes = false;
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            return Ok(pos);
        }
        for &b in &buf[..n] {
            pos += 1;
            match b {
                b'"' => in_quotes = !in_quotes,
                b'\n' if !in_quotes => return Ok(pos),
                _ => {}
            }
        }
    }
}

/// Scan data rows starting at `data_start`, recording the byte offset of every
/// chunk boundary (each `CHUNK`-th row) and the total row count. The scan is
/// quote-aware: a `"` toggles quoting (so a `""` escape flips back), and only
/// newlines outside quotes end a record — matching RFC 4180.
fn build_index_from(path: &Path, data_start: u64) -> Result<(Vec<u64>, usize)> {
    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(data_start))?;
    let mut reader = BufReader::new(file);
    let mut buf = [0u8; 64 * 1024];
    let mut pos = data_start;
    let mut in_quotes = false;
    let mut rows: usize = 0;
    let mut record_has_content = false;
    let mut offsets = vec![data_start]; // offsets[0] = start of row 0
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        for &b in &buf[..n] {
            pos += 1;
            match b {
                b'"' => {
                    in_quotes = !in_quotes;
                    record_has_content = true;
                }
                b'\n' if !in_quotes => {
                    rows += 1;
                    // `pos` is now the start of the next row.
                    if rows % CHUNK == 0 {
                        offsets.push(pos);
                    }
                    record_has_content = false;
                }
                b'\r' => {}
                _ => record_has_content = true,
            }
        }
    }
    // A final record with no trailing newline still counts.
    if record_has_content {
        rows += 1;
    }
    Ok((offsets, rows))
}

/// Read a workbook and turn every sheet that holds data into its own dataset,
/// labelled `file[sheet]`. A sheet is decoded in full (xlsx is a zipped XML
/// stream, so there is no random access to seek by row range — and Excel caps a
/// sheet at ~1M rows), which is exactly what [`Backend::Memory`] serves.
/// Completely blank sheets are skipped rather than opened as empty tables.
fn load_workbook(path: &Path) -> Result<Vec<Dataset>> {
    let mut workbook = open_workbook_auto(path)
        .with_context(|| format!("opening workbook {}", path.display()))?;
    let label = file_label(path);
    let mut sheets = Vec::new();
    for name in workbook.sheet_names() {
        let range = workbook
            .worksheet_range(&name)
            .with_context(|| format!("reading sheet {name} of {}", path.display()))?;
        if range.is_empty() {
            continue;
        }
        let batch = sheet_batch(&range)
            .with_context(|| format!("reading sheet {name} of {}", path.display()))?;
        if batch.num_columns() == 0 {
            continue;
        }
        sheets.push(Dataset::in_memory(
            batch,
            path.to_path_buf(),
            format!("{label}[{name}]"),
        ));
    }
    if sheets.is_empty() {
        anyhow::bail!("no sheet with data in {}", path.display());
    }
    Ok(sheets)
}

/// Turn one worksheet into a record batch: the first row of the used range
/// gives the column names, the rest is data, and each column is typed from the
/// values Excel reported (see [`sheet_array`]).
fn sheet_batch(range: &Range<Data>) -> Result<RecordBatch> {
    let mut rows = range.rows();
    let Some(header) = rows.next() else {
        return Ok(RecordBatch::new_empty(Arc::new(Schema::empty())));
    };
    let names: Vec<String> = header
        .iter()
        .enumerate()
        .map(|(i, cell)| {
            cell_text(cell)
                .filter(|name| !name.is_empty())
                .unwrap_or_else(|| format!("column_{}", i + 1))
        })
        .collect();
    let body: Vec<&[Data]> = rows.collect();

    let columns: Vec<ArrayRef> = (0..names.len())
        .map(|c| {
            let values: Vec<&Data> = body
                .iter()
                .map(|row| row.get(c).unwrap_or(&Data::Empty))
                .collect();
            sheet_array(&values)
        })
        .collect();
    let fields: Vec<Field> = names
        .iter()
        .zip(&columns)
        .map(|(n, a)| Field::new(n, a.data_type().clone(), true))
        .collect();
    RecordBatch::try_new(Arc::new(Schema::new(fields)), columns)
        .context("building sheet batch")
}

/// Build one column from its cells, keeping the type Excel gave them so that
/// sorting and numeric styling behave: a column of integers stays Int64, mixed
/// integers and floats become Float64, booleans stay boolean, and dates become
/// a real date (or timestamp, when any cell carries a time of day). Anything
/// textual or mixed falls back to the same string inference the CSV and
/// transposed paths use, so numbers stored as text still sort numerically.
/// Blank cells and Excel error cells (`#REF!`, `#DIV/0!`) become nulls.
fn sheet_array(values: &[&Data]) -> ArrayRef {
    let (mut ints, mut floats, mut bools, mut dates, mut texts) = (0, 0, 0, 0, 0);
    for value in values {
        match value {
            Data::Int(_) => ints += 1,
            Data::Float(_) => floats += 1,
            Data::Bool(_) => bools += 1,
            Data::DateTime(_) | Data::DateTimeIso(_) => dates += 1,
            Data::String(_) | Data::DurationIso(_) => texts += 1,
            Data::Error(_) | Data::Empty => {}
        }
    }
    let present = ints + floats + bools + dates + texts;
    if present > 0 && texts == 0 {
        if dates == present {
            // Falls through to text if none of the cells actually converts.
            if let Some(array) = sheet_dates(values) {
                return array;
            }
        } else if bools == present {
            let a: BooleanArray = values
                .iter()
                .map(|v| match v {
                    Data::Bool(b) => Some(*b),
                    _ => None,
                })
                .collect();
            return Arc::new(a);
        } else if ints + floats == present {
            let numbers: Vec<Option<f64>> = values
                .iter()
                .map(|v| match v {
                    Data::Int(i) => Some(*i as f64),
                    Data::Float(f) => Some(*f),
                    _ => None,
                })
                .collect();
            // Excel has no integer type — xlsx reports every number as a float
            // — so a column of whole numbers becomes Int64, which is what Excel
            // itself displays. Only genuinely fractional columns stay Float64.
            if numbers.iter().flatten().copied().all(is_whole) {
                let a: Int64Array = numbers.iter().map(|v| v.map(|v| v as i64)).collect();
                return Arc::new(a);
            }
            let a: Float64Array = numbers.into_iter().collect();
            return Arc::new(a);
        }
        // Anything else is a genuine mix (dates beside numbers, say), which
        // falls through to the text path rather than nulling half the column.
    }
    let text: Vec<Option<String>> = values.iter().map(|v| cell_text(v)).collect();
    infer_array(&text)
}

/// Whether a float is a whole number small enough to hold exactly in an i64.
fn is_whole(v: f64) -> bool {
    // 2^53 — beyond this, f64 can no longer represent every integer.
    v.fract() == 0.0 && v.abs() <= 9_007_199_254_740_992.0
}

/// A column of date cells as a `Date32` array, or a millisecond timestamp one
/// when any cell carries a time of day. `None` if no cell converts at all
/// (e.g. a column of durations), leaving the caller to fall back to text.
fn sheet_dates(values: &[&Data]) -> Option<ArrayRef> {
    let stamps: Vec<Option<chrono::NaiveDateTime>> =
        values.iter().map(|v| v.as_datetime()).collect();
    if stamps.iter().all(Option::is_none) {
        return None;
    }
    if stamps
        .iter()
        .flatten()
        .all(|d| d.time() == NaiveTime::MIN)
    {
        let epoch = NaiveDate::from_ymd_opt(1970, 1, 1)?;
        let a: Date32Array = stamps
            .iter()
            .map(|d| d.map(|d| d.date().signed_duration_since(epoch).num_days() as i32))
            .collect();
        return Some(Arc::new(a));
    }
    let a: TimestampMillisecondArray = stamps
        .iter()
        .map(|d| d.map(|d| d.and_utc().timestamp_millis()))
        .collect();
    Some(Arc::new(a))
}

/// One cell as display text, trimmed. `None` for blanks and error cells — the
/// viewer shows those as `NA`, like any other null. Dates are rendered in ISO
/// form so a mixed column stays readable instead of showing Excel's serial.
fn cell_text(cell: &Data) -> Option<String> {
    let text = match cell {
        Data::Empty | Data::Error(_) => return None,
        Data::String(s) => s.trim().to_string(),
        Data::Int(i) => i.to_string(),
        Data::Float(f) => f.to_string(),
        Data::Bool(b) => b.to_string(),
        Data::DateTime(_) | Data::DateTimeIso(_) => match cell.as_datetime() {
            Some(d) if d.time() == NaiveTime::MIN => d.format("%Y-%m-%d").to_string(),
            Some(d) => d.format("%Y-%m-%d %H:%M:%S").to_string(),
            None => cell.to_string(),
        },
        Data::DurationIso(s) => s.trim().to_string(),
    };
    (!text.is_empty()).then_some(text)
}

/// Build a typed array from string values, inferring Int64 → Float64 → Utf8
/// from what parses. Blank/whitespace values become nulls. This gives each
/// transposed record column a real type so sorting and numeric styling work.
fn infer_array(values: &[Option<String>]) -> ArrayRef {
    let cleaned: Vec<Option<String>> = values
        .iter()
        .map(|v| {
            v.as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        })
        .collect();
    let present: Vec<&String> = cleaned.iter().filter_map(|v| v.as_ref()).collect();

    if !present.is_empty() && present.iter().all(|s| s.parse::<i64>().is_ok()) {
        let a: Int64Array = cleaned
            .iter()
            .map(|v| v.as_ref().map(|s| s.parse::<i64>().unwrap()))
            .collect();
        return Arc::new(a);
    }
    if !present.is_empty() && present.iter().all(|s| s.parse::<f64>().is_ok()) {
        let a: Float64Array = cleaned
            .iter()
            .map(|v| v.as_ref().map(|s| s.parse::<f64>().unwrap()))
            .collect();
        return Arc::new(a);
    }
    let a: StringArray = cleaned.iter().map(|v| v.as_deref()).collect();
    Arc::new(a)
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
