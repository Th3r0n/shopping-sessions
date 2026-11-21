//! The two Parquet tables: uncompressed, no dictionary, no statistics.

use std::fs::File;
use std::io::BufWriter;
use std::path::Path;
use std::sync::Arc;

use parquet::basic::Compression;
use parquet::data_type::{ByteArray, ByteArrayType, Int32Type, Int64Type};
use parquet::file::properties::{EnabledStatistics, WriterProperties};
use parquet::file::writer::SerializedFileWriter;
use parquet::schema::parser::parse_message_type;

pub const USER_SCHEMA: &str = "message user_daily {
    REQUIRED BYTE_ARRAY user_id (STRING);
    REQUIRED INT32 day (DATE);
    REQUIRED INT64 sessions;
    REQUIRED INT64 events;
    REQUIRED INT64 revenue_cents;
    OPTIONAL BYTE_ARRAY top_category (STRING);
}";

pub const CAMPAIGN_SCHEMA: &str = "message campaign_daily {
    REQUIRED BYTE_ARRAY campaign_id (STRING);
    REQUIRED BYTE_ARRAY channel (STRING);
    REQUIRED INT32 day (DATE);
    REQUIRED INT64 sessions;
    REQUIRED INT64 purchasing_sessions;
    REQUIRED INT64 revenue_cents;
    REQUIRED INT64 assisted_sessions;
}";

pub enum Column {
    Strings(Vec<ByteArray>),
    /// Values of the non-null rows, and one definition level per row.
    OptionalStrings(Vec<ByteArray>, Vec<i16>),
    Int32(Vec<i32>),
    Int64(Vec<i64>),
}

pub fn write(path: &Path, schema: &str, columns: Vec<Column>) -> parquet::errors::Result<()> {
    let schema = Arc::new(parse_message_type(schema)?);
    let props = Arc::new(
        WriterProperties::builder()
            .set_compression(Compression::UNCOMPRESSED)
            .set_dictionary_enabled(false)
            .set_statistics_enabled(EnabledStatistics::None)
            .build(),
    );
    let file = BufWriter::with_capacity(1 << 20, File::create(path)?);
    let mut writer = SerializedFileWriter::new(file, schema, props)?;
    let mut group = writer.next_row_group()?;
    for column in columns {
        let mut w = group.next_column()?.expect("schema and columns differ");
        match column {
            Column::Strings(values) => {
                w.typed::<ByteArrayType>().write_batch(&values, None, None)?;
            }
            Column::OptionalStrings(values, levels) => {
                w.typed::<ByteArrayType>().write_batch(&values, Some(&levels), None)?;
            }
            Column::Int32(values) => {
                w.typed::<Int32Type>().write_batch(&values, None, None)?;
            }
            Column::Int64(values) => {
                w.typed::<Int64Type>().write_batch(&values, None, None)?;
            }
        }
        w.close()?;
    }
    assert!(group.next_column()?.is_none(), "schema has more columns");
    group.close()?;
    writer.close()?; // writes the footer and flushes the BufWriter
    Ok(())
}
