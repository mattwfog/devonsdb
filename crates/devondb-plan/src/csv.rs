//! Streaming RFC 4180 CSV reader for `COPY` (`docs/SCALE.md` §5.2).
//!
//! Header-mapped by exact column name; non-String fields parse with the
//! text language's own literal grammar so the shell and the loader can
//! never drift. Yields schema-ordered `Vec<Value>` rows.

use std::{
    fs::File,
    io::{BufRead, BufReader},
};

use crate::text::parser::parse_value_literal;
use devondb_types::{
    DevonError, DevonResult,
    logical_type::LogicalType,
    schema::{Column, NodeTableSchema, suggestion_suffix},
    value::Value,
};

/// A streaming, schema-aware reader for the `COPY` CSV dialect.
pub struct CsvReader {
    reader: BufReader<File>,
    columns: Vec<Column>,
    file_to_schema: Vec<usize>,
    line: usize,
    done: bool,
}

impl CsvReader {
    /// Opens `path` and validates its mandatory header against `schema`.
    ///
    /// Header errors are returned here, before any data row is yielded.
    pub fn open(path: &str, schema: &NodeTableSchema) -> DevonResult<Self> {
        let mut csv = Self {
            reader: BufReader::new(File::open(path)?),
            columns: schema.columns().to_vec(),
            file_to_schema: Vec::new(),
            line: 1,
            done: false,
        };
        let header = csv
            .read_record()?
            .ok_or_else(|| invalid_argument("CSV file requires a header row"))?;
        csv.file_to_schema = map_header(header, &csv.columns)?;
        Ok(csv)
    }

    fn read_record(&mut self) -> DevonResult<Option<CsvRecord>> {
        let mut builder = RecordBuilder::new(self.line);
        loop {
            let Some(byte) = self.read_byte()? else {
                return builder.finish_eof();
            };
            if let Some(record) = builder.consume(byte, &mut self.line)? {
                return Ok(Some(record));
            }
        }
    }

    fn read_byte(&mut self) -> std::io::Result<Option<u8>> {
        let byte = self.reader.fill_buf()?.first().copied();
        if byte.is_some() {
            self.reader.consume(1);
        }
        Ok(byte)
    }

    fn row_from_record(&self, record: CsvRecord) -> DevonResult<Vec<Value>> {
        if record.fields.len() != self.columns.len() {
            return Err(invalid_argument(format!(
                "CSV line {} has {} fields; expected {}",
                record.line,
                record.fields.len(),
                self.columns.len()
            )));
        }
        let mut row = vec![Value::Null; self.columns.len()];
        for (file_index, field) in record.fields.into_iter().enumerate() {
            let schema_index = self.file_to_schema[file_index];
            row[schema_index] = field_value(field, &self.columns[schema_index], file_index + 1)?;
        }
        Ok(row)
    }
}

impl Iterator for CsvReader {
    type Item = DevonResult<Vec<Value>>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        let record = match self.read_record() {
            Ok(Some(record)) => record,
            Ok(None) => {
                self.done = true;
                return None;
            }
            Err(error) => {
                self.done = true;
                return Some(Err(error));
            }
        };
        let row = self.row_from_record(record);
        if row.is_err() {
            self.done = true;
        }
        Some(row)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum FieldState {
    Start,
    Unquoted,
    Quoted,
    AfterQuote,
    RecordCr,
}

struct RecordBuilder {
    fields: Vec<CsvField>,
    bytes: Vec<u8>,
    state: FieldState,
    record_line: usize,
    field_line: usize,
    quoted: bool,
    quoted_previous_cr: bool,
    saw_byte: bool,
}

impl RecordBuilder {
    fn new(line: usize) -> Self {
        Self {
            fields: Vec::new(),
            bytes: Vec::new(),
            state: FieldState::Start,
            record_line: line,
            field_line: line,
            quoted: false,
            quoted_previous_cr: false,
            saw_byte: false,
        }
    }

    fn consume(&mut self, byte: u8, line: &mut usize) -> DevonResult<Option<CsvRecord>> {
        self.saw_byte = true;
        match self.state {
            FieldState::Start => self.consume_start(byte, line),
            FieldState::Unquoted => self.consume_unquoted(byte, line),
            FieldState::Quoted => self.consume_quoted(byte, line),
            FieldState::AfterQuote => self.consume_after_quote(byte, line),
            FieldState::RecordCr => self.consume_record_cr(byte, line),
        }
    }

    fn consume_start(&mut self, byte: u8, line: &mut usize) -> DevonResult<Option<CsvRecord>> {
        match byte {
            b',' => self.finish_and_start_field(*line).map(|()| None),
            b'"' => {
                self.quoted = true;
                self.state = FieldState::Quoted;
                Ok(None)
            }
            b'\n' => self.finish_lf_record(line).map(Some),
            b'\r' => {
                self.state = FieldState::RecordCr;
                Ok(None)
            }
            other => {
                self.bytes.push(other);
                self.state = FieldState::Unquoted;
                Ok(None)
            }
        }
    }

    fn consume_unquoted(&mut self, byte: u8, line: &mut usize) -> DevonResult<Option<CsvRecord>> {
        match byte {
            b',' => self.finish_and_start_field(*line).map(|()| None),
            b'\n' => self.finish_lf_record(line).map(Some),
            b'\r' => {
                self.state = FieldState::RecordCr;
                Ok(None)
            }
            b'"' => Err(self.error("a quote may appear only at the start of a field")),
            other => {
                self.bytes.push(other);
                Ok(None)
            }
        }
    }

    fn consume_quoted(&mut self, byte: u8, line: &mut usize) -> DevonResult<Option<CsvRecord>> {
        match byte {
            b'"' => {
                self.quoted_previous_cr = false;
                self.state = FieldState::AfterQuote;
            }
            b'\r' => {
                self.bytes.push(byte);
                *line += 1;
                self.quoted_previous_cr = true;
            }
            b'\n' => {
                self.bytes.push(byte);
                if !self.quoted_previous_cr {
                    *line += 1;
                }
                self.quoted_previous_cr = false;
            }
            other => {
                self.bytes.push(other);
                self.quoted_previous_cr = false;
            }
        }
        Ok(None)
    }

    fn consume_after_quote(
        &mut self,
        byte: u8,
        line: &mut usize,
    ) -> DevonResult<Option<CsvRecord>> {
        match byte {
            b'"' => {
                self.bytes.push(b'"');
                self.state = FieldState::Quoted;
                Ok(None)
            }
            b',' => self.finish_and_start_field(*line).map(|()| None),
            b'\n' => self.finish_lf_record(line).map(Some),
            b'\r' => {
                self.state = FieldState::RecordCr;
                Ok(None)
            }
            _ => Err(self.error("unexpected character after closing quote")),
        }
    }

    fn consume_record_cr(&mut self, byte: u8, line: &mut usize) -> DevonResult<Option<CsvRecord>> {
        if byte != b'\n' {
            return Err(self.error("record-ending CR must be followed by LF"));
        }
        let record = self.finish_record()?;
        *line += 1;
        Ok(Some(record))
    }

    fn finish_lf_record(&mut self, line: &mut usize) -> DevonResult<CsvRecord> {
        let record = self.finish_record()?;
        *line += 1;
        Ok(record)
    }

    fn finish_and_start_field(&mut self, line: usize) -> DevonResult<()> {
        self.finish_field()?;
        self.state = FieldState::Start;
        self.field_line = line;
        Ok(())
    }

    fn finish_field(&mut self) -> DevonResult<()> {
        let column = self.fields.len() + 1;
        let bytes = std::mem::take(&mut self.bytes);
        let text = String::from_utf8(bytes).map_err(|error| {
            csv_field_error(
                self.field_line,
                column,
                format!("field is not UTF-8: {error}"),
            )
        })?;
        self.fields.push(CsvField {
            text,
            quoted: self.quoted,
            line: self.field_line,
        });
        self.quoted = false;
        Ok(())
    }

    fn finish_record(&mut self) -> DevonResult<CsvRecord> {
        self.finish_field()?;
        Ok(CsvRecord {
            fields: std::mem::take(&mut self.fields),
            line: self.record_line,
        })
    }

    fn finish_eof(&mut self) -> DevonResult<Option<CsvRecord>> {
        if !self.saw_byte {
            return Ok(None);
        }
        match self.state {
            FieldState::Quoted => Err(self.error("unterminated quoted field")),
            FieldState::RecordCr => Err(self.error("record-ending CR must be followed by LF")),
            _ => self.finish_record().map(Some),
        }
    }

    fn error(&self, problem: &str) -> DevonError {
        csv_field_error(self.field_line, self.fields.len() + 1, problem)
    }
}

struct CsvRecord {
    fields: Vec<CsvField>,
    line: usize,
}

struct CsvField {
    text: String,
    quoted: bool,
    line: usize,
}

fn map_header(header: CsvRecord, columns: &[Column]) -> DevonResult<Vec<usize>> {
    let mut seen = vec![false; columns.len()];
    let mut file_to_schema = Vec::with_capacity(header.fields.len());
    for (file_index, field) in header.fields.into_iter().enumerate() {
        let Some(schema_index) = columns.iter().position(|column| column.name == field.text) else {
            let suffix = suggestion_suffix(
                &field.text,
                columns.iter().map(|column| column.name.as_str()),
            );
            return Err(csv_field_error(
                field.line,
                file_index + 1,
                format!("unknown header column `{}`{suffix}", field.text),
            ));
        };
        if seen[schema_index] {
            return Err(csv_field_error(
                field.line,
                file_index + 1,
                format!("duplicate header column `{}`", field.text),
            ));
        }
        seen[schema_index] = true;
        file_to_schema.push(schema_index);
    }
    if let Some(index) = seen.iter().position(|present| !present) {
        return Err(invalid_argument(format!(
            "CSV header is missing column `{}`",
            columns[index].name
        )));
    }
    Ok(file_to_schema)
}

fn field_value(field: CsvField, column: &Column, file_column: usize) -> DevonResult<Value> {
    if field.text.is_empty() && !field.quoted {
        return Ok(Value::Null);
    }
    if column.ty == LogicalType::String {
        return Ok(Value::String(field.text));
    }
    if field.text.is_empty() {
        return Err(csv_field_error(
            field.line,
            file_column,
            format!("quoted empty field is invalid for column `{}`", column.name),
        ));
    }
    let value = parse_value_literal(&field.text).map_err(|error| {
        csv_field_error(
            field.line,
            file_column,
            format!("invalid literal for column `{}`: {error}", column.name),
        )
    })?;
    if !value.matches_type(&column.ty) {
        return Err(csv_field_error(
            field.line,
            file_column,
            format!(
                "value {value} for column `{}` does not match expected type {}",
                column.name, column.ty
            ),
        ));
    }
    Ok(value)
}

fn csv_field_error(line: usize, column: usize, problem: impl AsRef<str>) -> DevonError {
    invalid_argument(format!(
        "CSV line {line}, column {column}: {}",
        problem.as_ref()
    ))
}

fn invalid_argument(context: impl Into<String>) -> DevonError {
    DevonError::InvalidArgument {
        context: context.into(),
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::CsvReader;
    use devondb_types::{
        DevonError,
        logical_type::LogicalType,
        schema::{Column, NodeTableSchema},
        value::Value,
    };
    use tempfile::NamedTempFile;

    #[test]
    fn permuted_header_yields_schema_order() {
        let schema = schema(vec![
            column("id", LogicalType::Int64, true),
            column("name", LogicalType::String, false),
            column("active", LogicalType::Bool, false),
        ]);
        let file = csv_file(b"name,active,id\nAda,true,1\n");
        let rows = rows(&file, &schema);
        assert_eq!(
            rows,
            vec![vec![
                Value::Int64(1),
                Value::String("Ada".into()),
                Value::Bool(true),
            ]]
        );
    }

    #[test]
    fn quoted_field_unescapes_comma_quote_and_newline() {
        let schema = schema(vec![
            column("id", LogicalType::Int64, true),
            column("note", LogicalType::String, false),
        ]);
        let file = csv_file(b"id,note\n1,\"comma, quote \"\"yes\"\"\nand newline\"\n");
        let rows = rows(&file, &schema);
        assert_eq!(
            rows[0][1],
            Value::String("comma, quote \"yes\"\nand newline".into())
        );
    }

    #[test]
    fn crlf_records_are_accepted() {
        let schema = schema(vec![
            column("id", LogicalType::Int64, true),
            column("name", LogicalType::String, false),
        ]);
        let file = csv_file(b"id,name\r\n1,Ada\r\n2,Grace\r\n");
        assert_eq!(rows(&file, &schema).len(), 2);
    }

    #[test]
    fn unquoted_empty_is_null_and_quoted_empty_is_empty_string() {
        let schema = schema(vec![
            column("id", LogicalType::Int64, true),
            column("missing", LogicalType::String, false),
            column("empty", LogicalType::String, false),
            column("score", LogicalType::Int64, false),
        ]);
        let file = csv_file(b"id,missing,empty,score\n1,,\"\",\n");
        assert_eq!(
            rows(&file, &schema),
            vec![vec![
                Value::Int64(1),
                Value::Null,
                Value::String(String::new()),
                Value::Null,
            ]]
        );
    }

    #[test]
    fn quoted_empty_non_string_names_line_and_column() {
        let schema = schema(vec![column("id", LogicalType::Int64, true)]);
        let file = csv_file(b"id\n\"\"\n");
        let error = row_error(&file, &schema);
        assert!(error.contains("line 2"), "{error}");
        assert!(error.contains("column 1"), "{error}");
        assert!(error.contains("quoted empty"), "{error}");
    }

    #[test]
    fn unknown_missing_and_duplicate_header_columns_are_rejected() {
        let schema = schema(vec![
            column("id", LogicalType::Int64, true),
            column("name", LogicalType::String, false),
        ]);

        let unknown = open_error(&csv_file(b"id,nane\n"), &schema);
        assert!(
            unknown.contains("unknown header column `nane`"),
            "{unknown}"
        );
        assert!(unknown.contains("did you mean `name`"), "{unknown}");

        let missing = open_error(&csv_file(b"id\n"), &schema);
        assert!(missing.contains("missing column `name`"), "{missing}");

        let duplicate = open_error(&csv_file(b"id,id\n"), &schema);
        assert!(
            duplicate.contains("duplicate header column `id`"),
            "{duplicate}"
        );
    }

    #[test]
    fn row_arity_error_names_record_line() {
        let schema = schema(vec![
            column("id", LogicalType::Int64, true),
            column("name", LogicalType::String, false),
        ]);
        let error = row_error(&csv_file(b"id,name\n1\n"), &schema);
        assert!(error.contains("line 2"), "{error}");
        assert!(error.contains("1 fields; expected 2"), "{error}");
    }

    #[test]
    fn bad_literal_names_one_based_line_and_file_column() {
        let schema = schema(vec![
            column("id", LogicalType::Int64, true),
            column("active", LogicalType::Bool, false),
        ]);
        let error = row_error(&csv_file(b"id,active\n1,truth\n"), &schema);
        assert!(error.contains("line 2"), "{error}");
        assert!(error.contains("column 2"), "{error}");
        assert!(error.contains("invalid literal"), "{error}");
    }

    #[test]
    fn vector_and_bool_use_text_literal_grammar() {
        let schema = schema(vec![
            column("id", LogicalType::Int64, true),
            column("embedding", LogicalType::Vector { dim: 2 }, false),
            column("active", LogicalType::Bool, false),
        ]);
        let file = csv_file(b"id,embedding,active\n1,\"[0.1, -0.2]\",true\n");
        assert_eq!(
            rows(&file, &schema),
            vec![vec![
                Value::Int64(1),
                Value::Vector(vec![0.1, -0.2]),
                Value::Bool(true),
            ]]
        );
    }

    #[test]
    fn fifty_thousand_rows_stream_to_completion() {
        let schema = schema(vec![
            column("id", LogicalType::Int64, true),
            column("name", LogicalType::String, false),
        ]);
        let mut file = NamedTempFile::new().unwrap();
        writeln!(file, "id,name").unwrap();
        for id in 0..50_000_i64 {
            writeln!(file, "{id},person-{id}").unwrap();
        }
        file.flush().unwrap();

        let mut reader = open(&file, &schema);
        for id in 0..50_000_i64 {
            let row = reader.next().unwrap().unwrap();
            assert_eq!(row[0], Value::Int64(id));
            assert_eq!(row[1], Value::String(format!("person-{id}")));
        }
        assert!(reader.next().is_none());
    }

    fn column(name: &str, ty: LogicalType, primary_key: bool) -> Column {
        Column {
            name: name.into(),
            ty,
            primary_key,
        }
    }

    fn schema(columns: Vec<Column>) -> NodeTableSchema {
        NodeTableSchema::new("People".into(), columns).unwrap()
    }

    fn csv_file(contents: &[u8]) -> NamedTempFile {
        let mut file = NamedTempFile::new().unwrap();
        file.write_all(contents).unwrap();
        file.flush().unwrap();
        file
    }

    fn open(file: &NamedTempFile, schema: &NodeTableSchema) -> CsvReader {
        CsvReader::open(file.path().to_str().unwrap(), schema).unwrap()
    }

    fn rows(file: &NamedTempFile, schema: &NodeTableSchema) -> Vec<Vec<Value>> {
        open(file, schema).collect::<Result<Vec<_>, _>>().unwrap()
    }

    fn open_error(file: &NamedTempFile, schema: &NodeTableSchema) -> String {
        match CsvReader::open(file.path().to_str().unwrap(), schema) {
            Ok(_) => panic!("CSV header should be rejected"),
            Err(DevonError::InvalidArgument { context }) => context,
            Err(error) => panic!("expected InvalidArgument, got {error}"),
        }
    }

    fn row_error(file: &NamedTempFile, schema: &NodeTableSchema) -> String {
        let error = open(file, schema).next().unwrap().unwrap_err();
        let DevonError::InvalidArgument { context } = error else {
            panic!("expected InvalidArgument, got {error}");
        };
        context
    }
}
