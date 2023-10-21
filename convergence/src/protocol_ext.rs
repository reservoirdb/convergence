//! Contains extensions that make working with the Postgres protocol simpler or more efficient.

use crate::{protocol::{ConnectionCodec, FormatCode, ProtocolError, RowDescription}, to_wire::{Writer, ToWire}};
use bytes::{BufMut, Bytes, BytesMut};
use chrono::{NaiveDate, NaiveDateTime, NaiveTime};
use tokio_util::codec::Encoder;

/// Supports batched rows for e.g. returning portal result sets.
///
/// NB: this struct only performs limited validation of column consistency across rows.
pub struct DataRowBatch {
	format_code: FormatCode,
	num_cols: usize,
	num_rows: usize,
	data: BytesMut,
	row: BytesMut,
}

impl DataRowBatch {
	/// Creates a new row batch using the given format code
	pub fn new(format_code: FormatCode) -> Self {
		Self {
			format_code,
			num_cols: 0,
			num_rows: 0,
			data: BytesMut::new(),
			row: BytesMut::new(),
		}
	}

	/// Creates a [DataRowBatch] from the given [RowDescription].
	pub fn from_row_desc(desc: &RowDescription) -> Self {
		Self {
			format_code: desc.format_code,
			num_cols: desc.fields.len(),
			num_rows: 0,
			data: BytesMut::new(),
			row: BytesMut::new(),
		}
	}

	/// Starts writing a new row.
	///
	/// Returns a [DataRowWriter] that is responsible for the actual value encoding.
	pub fn create_row(&mut self) -> DataRowWriter {
		self.num_rows += 1;
		DataRowWriter::new(self)
	}

	/// Specify the number of columns
	/// Allows for column count to be changed after creation.
	/// Here be dragons, if you have started writing data you will have a terrible time.
	pub fn set_num_cols(&mut self, num_cols: usize) {
		self.num_cols = num_cols;
	}

	/// Returns the number of columns currently written to this batch.
	pub fn num_cols(&self) -> usize {
		self.num_cols
	}

	/// Returns the number of rows currently written to this batch.
	pub fn num_rows(&self) -> usize {
		self.num_rows
	}

	/// Returns a clone of the raw row bytes
	/// Only used for testing
	pub fn data(&self) -> Bytes {
		Bytes::from(self.data.clone())
	}
}

macro_rules! primitive_write {
	($name: ident, $type: ident) => {
		#[allow(missing_docs)]
		pub fn $name(&mut self, val: $type) {
			match self.parent.format_code {
				FormatCode::Text => self.write_value(&val.to_string().into_bytes()),
				FormatCode::Binary => self.write_value(&val.to_be_bytes()),
			};
		}
	};
}

/// Temporarily leased from a [DataRowBatch] to encode a single row.
pub struct DataRowWriter<'a> {
	current_col: usize,
	parent: &'a mut DataRowBatch,
}

impl Writer for DataRowWriter<'_> {
	fn write<T>(&mut self, val: T)
	where
		T: ToWire {
		match self.parent.format_code {
			FormatCode::Text => self.write_value(&val.to_binary()),
			FormatCode::Binary => self.write_value(&val.to_text()),
		};
	}
}

impl<'a> DataRowWriter<'a> {
	fn new(parent: &'a mut DataRowBatch) -> Self {
		parent.row.put_i16(parent.num_cols as i16);
		Self { current_col: 0, parent }
	}

	fn write_value(&mut self, data: &[u8]) {
		assert!(
			self.current_col < self.parent.num_cols,
			"tried to write more columns than specified in row description"
		);
		self.current_col += 1;
		self.parent.row.put_i32(data.len() as i32);
		self.parent.row.put_slice(data);
	}

	/// Writes a null value for the next column.
	pub fn write_null(&mut self) {
		self.current_col += 1;
		self.parent.row.put_i32(-1);
	}

	/// Writes raw bytes for the next column.
	pub fn write_bytes(&mut self, data: Bytes) {
		self.current_col += 1;
		self.parent.row.put_i32(data.len() as i32);
		self.parent.row.put_slice(data.as_ref());
	}

	/// Writes a string value for the next column.
	pub fn write_string(&mut self, val: &str) {
		self.write_value(val.as_bytes());
	}

	/// Writes a bool value for the next column.
	pub fn write_bool(&mut self, val: bool) {
		match self.parent.format_code {
			FormatCode::Text => self.write_value(if val { "t" } else { "f" }.as_bytes()),
			FormatCode::Binary => {
				self.current_col += 1;
				self.parent.row.put_u8(val as u8);
			}
		};
	}

	fn pg_date_epoch() -> NaiveDate {
		NaiveDate::from_ymd_opt(2000, 1, 1).expect("failed to create pg date epoch")
	}

	fn pg_timestamp_epoch() -> NaiveDateTime {
		Self::pg_date_epoch()
			.and_hms_opt(0, 0, 0)
			.expect("failed to create pg timestamp epoch")
	}

	/// Writes a date value for the next column.
	pub fn write_date(&mut self, val: NaiveDate) {
		match self.parent.format_code {
			FormatCode::Binary => self.write_int4(val.signed_duration_since(Self::pg_date_epoch()).num_days() as i32),
			FormatCode::Text => self.write_string(&val.to_string()),
		}
	}

	/// Writes a time value for the next column.
	pub fn write_time(&mut self, val: NaiveTime) {
		match self.parent.format_code {
			FormatCode::Binary => {
				let delta = val.signed_duration_since(NaiveTime::from_hms_opt(0, 0, 0).unwrap());
				let time = delta.num_microseconds().unwrap_or(0);
				self.write_int8(time);
			}
			FormatCode::Text => self.write_string(&val.to_string()),
		}
	}

	/// Writes a timestamp value for the next column.
	pub fn write_timestamp(&mut self, val: NaiveDateTime) {
		match self.parent.format_code {
			FormatCode::Binary => {
				self.write_int8(
					val.signed_duration_since(Self::pg_timestamp_epoch())
						.num_microseconds()
						.unwrap(),
				);
			}
			FormatCode::Text => self.write_string(&val.to_string()),
		}
	}
	primitive_write!(write_char, i8);
	primitive_write!(write_int2, i16);
	primitive_write!(write_int4, i32);
	primitive_write!(write_int8, i64);
	primitive_write!(write_float4, f32);
	primitive_write!(write_float8, f64);
}

impl<'a> Drop for DataRowWriter<'a> {
	fn drop(&mut self) {
		assert_eq!(
			self.parent.num_cols, self.current_col,
			"dropped a row writer with an invalid number of columns"
		);

		self.parent.data.put_u8(b'D');
		self.parent.data.put_i32((self.parent.row.len() + 4) as i32);
		self.parent.data.extend(self.parent.row.split());
	}
}

impl Encoder<DataRowBatch> for ConnectionCodec {
	type Error = ProtocolError;

	fn encode(&mut self, item: DataRowBatch, dst: &mut BytesMut) -> Result<(), Self::Error> {
		dst.extend(item.data);
		Ok(())
	}
}

#[cfg(test)]
mod tests {
	use std::{convert::TryInto, mem};

	use crate::protocol::FormatCode;

	use super::DataRowBatch;

	// DataRow (B)
	// https://www.postgresql.org/docs/current/protocol-message-formats.html
	// Byte1('D')
	//     Identifies the message as a data row.
	// Int32
	//     Length of message contents in bytes, including self.
	// Int16
	//     The number of column values that follow (possibly zero).
	//
	// Next, the following pair of fields appear for each column:
	//
	// Int32
	//     The length of the column value, in bytes (this count does not include itself). Can be zero. As a special case, -1 indicates a NULL column value. No value bytes follow in the NULL case.
	// Byte * N
	//     The value of the column, in the format indicated by the associated format code. n is the above length.

	macro_rules! test_primitive_write {
		($name: ident, $type: ident) => {
			#[test]
			pub fn $name() {
				const EXPECTED_LEN: i32 = mem::size_of::<$type>() as i32;

				let mut batch = DataRowBatch::new(FormatCode::Binary);
				batch.set_num_cols(1);

				let mut row = batch.create_row();

				let expected_val = 42 as $type;
				let expected_columns = 1;
				let expected_id = b'D';

				row.$name(expected_val);
				drop(row); // Drop the row to write to the batch

				let message_id = 0..1;
				let message_len = 1..5;
				let column_count = 5..7;
				let len = 7..11;
				let val = 11..;

				// row.len + cols + val.len + val
				// i32 	   + i16  + i32     + val
				let expected_message_len = 4 + 2 + 4 + EXPECTED_LEN as i32;

				let bytes = batch.data();
				let bytes = bytes.as_ref();

				let data = &bytes[message_id];
				assert_eq!(data[0], expected_id);

				let data: [u8; 2] = bytes[column_count].try_into().expect("Expected i16");
				let data = i16::from_be_bytes(data);
				assert_eq!(data, expected_columns);

				let data: [u8; 4] = bytes[message_len].try_into().expect("Expected i16");
				let data = i32::from_be_bytes(data);
				assert_eq!(data, expected_message_len);

				let data: [u8; 4] = bytes[len].try_into().expect("Expected i32");
				let data = i32::from_be_bytes(data);
				assert_eq!(data, EXPECTED_LEN);

				let data: [u8; EXPECTED_LEN as usize] = bytes[val].try_into().expect("Expected $type");
				let data = $type::from_be_bytes(data);
				assert_eq!(data, expected_val);
			}
		};
	}

	test_primitive_write!(write_int2, i16);
	test_primitive_write!(write_int4, i32);
	test_primitive_write!(write_int8, i64);
	test_primitive_write!(write_float4, f32);
	test_primitive_write!(write_float8, f64);
}
