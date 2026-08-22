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
use std::path::Path;

use anyhow::{Context, Result};
use arrow::csv::WriterBuilder as CsvWriterBuilder;
use arrow::datatypes::SchemaRef;
use arrow::record_batch::RecordBatch;
use flate2::Compression;
use flate2::write::GzEncoder;
use parquet::arrow::ArrowWriter;

/// How many rows are gathered and written at a time.
pub const BATCH: usize = 8192;

/// What a name says it should be written as.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Format {
    Csv,
    Tsv,
    Parquet,
}

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
            "" => anyhow::bail!("give the name an extension: .csv, .tsv, .parquet, or .csv.gz"),
            other => anyhow::bail!(
                ".{other} is not something this can write — .csv, .tsv, .parquet \
                 and .gz versions of the first two are"
            ),
        };
        if gzipped && format == Format::Parquet {
            anyhow::bail!("parquet holds its own compression, so .parquet.gz is not a thing");
        }
        Ok(Self { format, gzipped })
    }

    /// What to call this in a message.
    pub fn label(&self) -> String {
        let name = match self.format {
            Format::Csv => "csv",
            Format::Tsv => "tsv",
            Format::Parquet => "parquet",
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
        }
        Ok(())
    }
}

/// Throw away a file that was only half written. A cancelled or failed export
/// should leave nothing behind rather than something that looks like data.
pub fn abandon(path: &Path) {
    let _ = std::fs::remove_file(path);
}
