//! Writing a view out as a file.
//!
//! What leaves is the view as arranged: the columns on display, in the order
//! they are displayed, with computed ones included and renamed ones renamed,
//! and the rows in the order they are shown. The *values* leave, not the
//! presentation — a cell clipped to `…` on screen is written in full.
//!
//! Everything here is fed a batch at a time and never holds the table, so a
//! filtered view of a file far too large to fit is written the same way as a
//! small one.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use arrow::array::{
    Array, ArrayRef, BooleanArray, Date32Array, Float64Array, StringArray,
    TimestampMillisecondArray,
};
use arrow::csv::WriterBuilder as CsvWriterBuilder;
use arrow::datatypes::{DataType, SchemaRef, TimeUnit};
use arrow::record_batch::RecordBatch;
use arrow::temporal_conversions::{date32_to_datetime, timestamp_ms_to_datetime};
use arrow::util::display::{ArrayFormatter, FormatOptions};
use flate2::Compression;
use flate2::write::GzEncoder;
use parquet::arrow::ArrowWriter;
use rust_xlsxwriter::{Format as CellFormat, Workbook, Worksheet};

/// How many rows are gathered and written at a time.
pub const BATCH: usize = 8192;

/// What a name says it should be written as.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Format {
    Csv,
    Tsv,
    Parquet,
    Xlsx,
}

/// Excel's own limits, and the reason `.xlsx` is the one format that can be
/// asked to hold more than it can. Both are Excel's, not this program's.
const XLSX_MAX_ROWS: usize = 1_048_576;
const XLSX_MAX_COLS: usize = 16_384;

/// A format, and whether it is to be gzipped on the way out.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Target {
    pub format: Format,
    pub gzipped: bool,
}

impl Target {
    /// How a name is written, worked out from its extension — the same way the
    /// reader works out how to read one, so there is no format to choose
    /// beyond the name itself.
    pub fn for_name(path: &Path) -> Result<Self> {
        let extension = |path: &Path| {
            path.extension()
                .and_then(|e| e.to_str())
                .map(|e| e.to_ascii_lowercase())
                .unwrap_or_default()
        };
        let last = extension(path);
        let gzipped = last == "gz";
        let inner = match gzipped {
            true => extension(Path::new(path.file_stem().unwrap_or_default())),
            false => last,
        };
        let format = match inner.as_str() {
            "csv" => Format::Csv,
            "tsv" | "tab" => Format::Tsv,
            "parquet" => Format::Parquet,
            "xlsx" => Format::Xlsx,
            "" => anyhow::bail!(
                "give the name an extension: .csv, .tsv, .parquet, .xlsx, or .csv.gz"
            ),
            other => anyhow::bail!(
                ".{other} is not something this can write — .csv, .tsv, .parquet, \
                 .xlsx and .gz versions of the first two are"
            ),
        };
        if gzipped && format == Format::Parquet {
            anyhow::bail!("parquet holds its own compression, so .parquet.gz is not a thing");
        }
        if gzipped && format == Format::Xlsx {
            anyhow::bail!("an xlsx file is already a zip, so .xlsx.gz is not a thing");
        }
        Ok(Self { format, gzipped })
    }

    /// Whether a view of this size can be written at all.
    ///
    /// Only Excel has a ceiling, and it is checked before anything is created
    /// rather than found on the way through: a sheet that stopped at the
    /// millionth row would be a complete-looking file with the rest of the
    /// data missing, which is the worst thing an export can leave behind.
    pub fn holds(&self, rows: usize, cols: usize) -> Result<()> {
        if self.format != Format::Xlsx {
            return Ok(());
        }
        // The column names take a row of their own.
        if rows + 1 > XLSX_MAX_ROWS {
            anyhow::bail!(
                "{rows} rows: a sheet holds {}, and the column names take one of them",
                XLSX_MAX_ROWS - 1
            );
        }
        if cols > XLSX_MAX_COLS {
            anyhow::bail!("{cols} columns: a sheet holds {XLSX_MAX_COLS}");
        }
        Ok(())
    }

    /// What to call this in a message.
    pub fn label(&self) -> String {
        let name = match self.format {
            Format::Csv => "csv",
            Format::Tsv => "tsv",
            Format::Parquet => "parquet",
            Format::Xlsx => "xlsx",
        };
        match self.gzipped {
            true => format!("{name}.gz"),
            false => name.to_string(),
        }
    }
}

/// A file being written to, a batch at a time.
pub struct Export {
    sink: Sink,
}

enum Sink {
    /// Delimited text, with the header written before the first batch.
    Text {
        out: Plain,
        delimiter: u8,
        started: bool,
    },
    Parquet(Box<ArrowWriter<File>>),
    /// A workbook, built in a temp file of its own and saved at the end.
    Xlsx(Box<Book>),
}

/// A workbook being assembled. Boxed, since it is much the largest thing a
/// sink can be. The worksheet is reached through the workbook each time rather
/// than held, since it borrows from it.
struct Book {
    book: Workbook,
    path: PathBuf,
    /// The next row to write. Row 0 holds the column names.
    row: u32,
    date: CellFormat,
    datetime: CellFormat,
}

/// Where the bytes go: straight to the file, or through gzip on the way.
enum Plain {
    Raw(BufWriter<File>),
    Gzip(Box<GzEncoder<BufWriter<File>>>),
}

impl Write for Plain {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            Plain::Raw(out) => out.write(buf),
            Plain::Gzip(out) => out.write(buf),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Plain::Raw(out) => out.flush(),
            Plain::Gzip(out) => out.flush(),
        }
    }
}

impl Plain {
    /// Close it properly. A gzip stream needs its ending written, which is not
    /// something to leave to a destructor that cannot report failure.
    fn finish(self) -> Result<()> {
        match self {
            Plain::Raw(mut out) => out.flush().context("finishing the file")?,
            Plain::Gzip(out) => {
                let mut out = out.finish().context("finishing the compressed file")?;
                out.flush().context("finishing the file")?;
            }
        }
        Ok(())
    }
}

impl Export {
    /// Start writing `path`, which must not exist yet as far as this is
    /// concerned — the caller decides what to do about that.
    pub fn create(path: &Path, target: Target, schema: SchemaRef) -> Result<Self> {
        if target.format == Format::Xlsx {
            // No file yet: a workbook is assembled in a temp file of its own
            // and only written out by `finish`, so an export given up on
            // leaves nothing at the name at all.
            let mut book = Workbook::new();
            let sheet = book.add_worksheet_with_constant_memory();
            for (col, field) in schema.fields().iter().enumerate() {
                sheet.write_string(0, col as u16, field.name())?;
            }
            return Ok(Self {
                sink: Sink::Xlsx(Box::new(Book {
                    book,
                    path: path.to_path_buf(),
                    row: 1,
                    date: CellFormat::new().set_num_format("yyyy\\-mm\\-dd"),
                    datetime: CellFormat::new().set_num_format("yyyy\\-mm\\-dd hh:mm:ss"),
                })),
            });
        }
        let file = File::create(path).with_context(|| format!("creating {}", path.display()))?;
        let sink = match target.format {
            Format::Parquet => Sink::Parquet(Box::new(
                ArrowWriter::try_new(file, schema, None).context("starting the parquet file")?,
            )),
            format => {
                let out = BufWriter::new(file);
                Sink::Text {
                    out: match target.gzipped {
                        true => Plain::Gzip(Box::new(GzEncoder::new(out, Compression::default()))),
                        false => Plain::Raw(out),
                    },
                    delimiter: match format {
                        Format::Tsv => b'\t',
                        _ => b',',
                    },
                    started: false,
                }
            }
        };
        Ok(Self { sink })
    }

    pub fn write(&mut self, batch: &RecordBatch) -> Result<()> {
        match &mut self.sink {
            Sink::Parquet(writer) => writer.write(batch).context("writing rows")?,
            Sink::Xlsx(book) => {
                // Each column is worked out once for the whole batch, then the
                // cells are written a row at a time: constant memory mode
                // flushes a row as soon as the next one is started, so going
                // column by column would be writing backwards.
                let columns: Vec<Cells> =
                    batch.columns().iter().map(Cells::of).collect::<Result<_>>()?;
                let Book {
                    row, date, datetime, ..
                } = &**book;
                let (start, date, datetime) = (*row, date.clone(), datetime.clone());
                let sheet = book.book.worksheet_from_index(0)?;
                for i in 0..batch.num_rows() {
                    for (col, cells) in columns.iter().enumerate() {
                        cells.write(sheet, start + i as u32, col as u16, i, &date, &datetime)?;
                    }
                }
                book.row += batch.num_rows() as u32;
            }
            Sink::Text {
                out,
                delimiter,
                started,
            } => {
                // A fresh writer per batch, borrowing the sink, so the sink can
                // still be closed properly afterwards. The header goes with the
                // first batch and not with the rest.
                let mut writer = CsvWriterBuilder::new()
                    .with_header(!*started)
                    .with_delimiter(*delimiter)
                    .build(&mut *out);
                writer.write(batch).context("writing rows")?;
                *started = true;
            }
        }
        Ok(())
    }

    /// Close the file, which for parquet is where most of it gets written.
    pub fn finish(self) -> Result<()> {
        match self.sink {
            Sink::Parquet(writer) => {
                writer.close().context("finishing the parquet file")?;
            }
            Sink::Text { out, .. } => out.finish()?,
            Sink::Xlsx(mut book) => {
                let path = book.path.clone();
                book.book
                    .save(&path)
                    .with_context(|| format!("writing {}", path.display()))?;
            }
        }
        Ok(())
    }
}

/// Throw away a file that was only half written. A cancelled or failed export
/// should leave nothing behind rather than something that looks like data.
pub fn abandon(path: &Path) {
    let _ = std::fs::remove_file(path);
}

/// One column of a batch, worked out once and then written cell by cell.
///
/// Excel holds a type per cell rather than per column, and that is the whole
/// point of writing xlsx at all: a sample id or a gene name written as a string
/// stays a string, where the same value in a csv is re-guessed on import and
/// can come back as a date or with its leading zeros gone.
enum Cells {
    Text(StringArray),
    /// Excel keeps every number as a float, so that is what is worked out here.
    /// An integer too big for a float is beyond what a sheet can hold either
    /// way; nothing is gained by writing it more precisely.
    Numbers(Float64Array),
    Bools(BooleanArray),
    Days(Date32Array),
    Stamps(TimestampMillisecondArray),
}

impl Cells {
    fn of(array: &ArrayRef) -> Result<Self> {
        let cast = |to| arrow::compute::cast(array, &to).context("preparing a column");
        let pull = |array: ArrayRef| -> Result<Self> {
            match array.data_type() {
                DataType::Float64 => Ok(Self::Numbers(down(&array))),
                DataType::Date32 => Ok(Self::Days(down(&array))),
                _ => Ok(Self::Stamps(down(&array))),
            }
        };
        match array.data_type() {
            DataType::Boolean => Ok(Self::Bools(down(array))),
            DataType::Utf8 => Ok(Self::Text(down(array))),
            DataType::LargeUtf8 | DataType::Utf8View => Ok(Self::Text(down(&cast(DataType::Utf8)?))),
            DataType::Int8
            | DataType::Int16
            | DataType::Int32
            | DataType::Int64
            | DataType::UInt8
            | DataType::UInt16
            | DataType::UInt32
            | DataType::UInt64
            | DataType::Float16
            | DataType::Float32
            | DataType::Float64
            | DataType::Decimal128(..)
            | DataType::Decimal256(..) => pull(cast(DataType::Float64)?),
            DataType::Date32 | DataType::Date64 => pull(cast(DataType::Date32)?),
            DataType::Timestamp(..) => {
                pull(cast(DataType::Timestamp(TimeUnit::Millisecond, None))?)
            }
            // A time of day, a duration, a list, anything nested: written as it
            // is shown on screen. A string is never re-read as something else,
            // so this is the safe end to fall off.
            _ => Ok(Self::Text(shown(array)?)),
        }
    }

    fn write(
        &self,
        sheet: &mut Worksheet,
        row: u32,
        col: u16,
        i: usize,
        date: &CellFormat,
        datetime: &CellFormat,
    ) -> Result<()> {
        // A gap is left as an empty cell rather than written as anything, so it
        // stays a gap and not a zero or an empty string.
        match self {
            Self::Text(a) if a.is_valid(i) => sheet.write_string(row, col, a.value(i))?,
            Self::Numbers(a) if a.is_valid(i) => sheet.write_number(row, col, a.value(i))?,
            Self::Bools(a) if a.is_valid(i) => sheet.write_boolean(row, col, a.value(i))?,
            Self::Days(a) if a.is_valid(i) => match date32_to_datetime(a.value(i)) {
                Some(when) => sheet.write_datetime_with_format(row, col, when.date(), date)?,
                None => sheet.write_blank(row, col, date)?,
            },
            Self::Stamps(a) if a.is_valid(i) => match timestamp_ms_to_datetime(a.value(i)) {
                Some(when) => sheet.write_datetime_with_format(row, col, when, datetime)?,
                None => sheet.write_blank(row, col, datetime)?,
            },
            _ => sheet,
        };
        Ok(())
    }
}

/// A cast array as its own type. Infallible by construction: the cast just
/// above asked for exactly this.
fn down<T: 'static + Clone>(array: &ArrayRef) -> T {
    array
        .as_any()
        .downcast_ref::<T>()
        .expect("cast to the type just asked for")
        .clone()
}

/// A column as it appears on screen, for the types Excel has nothing of its
/// own for. Gaps stay gaps rather than becoming the word for one.
fn shown(array: &ArrayRef) -> Result<StringArray> {
    let options = FormatOptions::default();
    let formatter = ArrayFormatter::try_new(array.as_ref(), &options)
        .context("preparing a column")?;
    let cells: Vec<Option<String>> = (0..array.len())
        .map(|i| match array.is_valid(i) {
            true => Some(formatter.value(i).to_string()),
            false => None,
        })
        .collect();
    Ok(StringArray::from(cells))
}
