use arrow::array::{ArrayRef, Int32Array, StringArray};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use async_trait::async_trait;
use convergence::engine::{Engine, Portal, PreparedStatement, QueryResult};
use convergence::protocol::{ErrorResponse, FormatCode, RowDescription};
use convergence::server::{self, BindOptions};
use convergence_arrow::table::{record_batch_to_row_desc, record_batch_to_rows};
use sqlparser::ast::Statement;
use std::sync::Arc;
use tokio_postgres::{connect, NoTls};

struct ArrowPortal {
	batch: RecordBatch,
}

#[async_trait]
impl Portal for ArrowPortal {
	async fn fetch(&mut self) -> Result<QueryResult, ErrorResponse> {
		Ok(QueryResult {
			rows: record_batch_to_rows(&self.batch, FormatCode::Binary),
		})
	}

	fn row_desc(&self) -> RowDescription {
		record_batch_to_row_desc(&self.batch, FormatCode::Binary)
	}
}

struct ArrowEngine {
	batch: RecordBatch,
}

#[async_trait]
impl Engine for ArrowEngine {
	type PortalType = ArrowPortal;

	async fn new() -> Self {
		let int_col = Arc::new(Int32Array::from(vec![1, 2, 3])) as ArrayRef;
		let string_col = Arc::new(StringArray::from(vec!["a", "b", "c"])) as ArrayRef;

		let schema = Schema::new(vec![
			Field::new("int_col", DataType::Int32, true),
			Field::new("string_col", DataType::Utf8, true),
		]);

		Self {
			batch: RecordBatch::try_new(Arc::new(schema), vec![int_col, string_col]).expect("failed to create batch"),
		}
	}

	async fn prepare(&mut self, statement: Statement) -> Result<PreparedStatement, ErrorResponse> {
		Ok(PreparedStatement {
			statement,
			row_desc: record_batch_to_row_desc(&self.batch, FormatCode::Text),
		})
	}

	async fn create_portal(&mut self, _: &PreparedStatement, _: FormatCode) -> Result<Self::PortalType, ErrorResponse> {
		Ok(ArrowPortal {
			batch: self.batch.clone(),
		})
	}
}

#[tokio::test]
async fn basic_connection() {
	let _handle = server::run_background::<ArrowEngine>(BindOptions::new());

	let (client, conn) = connect("postgres://localhost:5432/test", NoTls)
		.await
		.expect("failed to init client");

	let _conn_handle = tokio::spawn(async move { conn.await.unwrap() });

	let rows = client.query("select 1", &[]).await.unwrap();
	let get_row = |idx: usize| {
		let row = &rows[idx];
		let cols: (i32, &str) = (row.get(0), row.get(1));
		cols
	};

	assert_eq!(get_row(0), (1, "a"));
	assert_eq!(get_row(1), (2, "b"));
	assert_eq!(get_row(2), (3, "c"));
}
