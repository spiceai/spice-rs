use arrow::array::{
    Array, ArrayRef, BinaryArray, BooleanArray, Float32Array, Float64Array, Int8Array,
    Int16Array, Int32Array, Int64Array, LargeBinaryArray, LargeStringArray, StringArray,
    UInt8Array, UInt16Array, UInt32Array, UInt64Array, new_null_array,
};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::error::ArrowError;
use arrow::record_batch::RecordBatch;
use snafu::{Snafu, ensure};
use std::sync::Arc;

#[derive(Debug, Snafu)]
pub enum QueryParameterError {
    #[snafu(display("Failed to construct query parameter batch: {source}"))]
    BatchCreation { source: ArrowError },

    #[snafu(display(
        "Query parameter arrays must contain exactly one value, got {array_length}"
    ))]
    InvalidArrayLength { array_length: usize },

    #[snafu(display("{message}"))]
    UnsupportedJsonParameter { message: String },
}

#[derive(Debug, Clone, Default)]
pub struct QueryParameters {
    values: Vec<QueryParameter>,
}

impl QueryParameters {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn push(mut self, value: impl Into<QueryParameter>) -> Self {
        self.values.push(value.into());
        self
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    pub fn into_record_batch(self) -> Result<Option<RecordBatch>, QueryParameterError> {
        if self.values.is_empty() {
            return Ok(None);
        }

        let field_arrays = self
            .values
            .into_iter()
            .enumerate()
            .map(|(index, value)| value.into_field_and_array(index))
            .collect::<Result<Vec<_>, _>>()?;

        let (fields, columns): (Vec<_>, Vec<_>) = field_arrays.into_iter().unzip();

        RecordBatch::try_new(Arc::new(Schema::new(fields)), columns)
            .map(Some)
            .map_err(|source| QueryParameterError::BatchCreation { source })
    }

    /// Encodes these parameters as the JSON array expected by the async
    /// `/v1/queries` API, i.e. positional bind values (`$1`, `$2`, ...).
    ///
    /// Unlike [`into_record_batch()`](Self::into_record_batch), which produces an
    /// Arrow batch for the Flight path, the HTTP async API accepts scalar bind
    /// values as a JSON array. Non-scalar parameters
    /// ([`QueryParameter::Array`]) and binary parameters have no JSON scalar
    /// representation and return [`QueryParameterError::UnsupportedJsonParameter`].
    ///
    /// # Errors
    ///
    /// Returns [`QueryParameterError::UnsupportedJsonParameter`] if any parameter
    /// cannot be represented as a JSON scalar (binary, array, or non-finite float).
    pub fn to_json_values(&self) -> Result<serde_json::Value, QueryParameterError> {
        let values = self
            .values
            .iter()
            .map(QueryParameter::to_json_value)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(serde_json::Value::Array(values))
    }
}

impl From<QueryParameter> for QueryParameters {
    fn from(value: QueryParameter) -> Self {
        Self {
            values: vec![value],
        }
    }
}

impl From<Vec<QueryParameter>> for QueryParameters {
    fn from(values: Vec<QueryParameter>) -> Self {
        Self { values }
    }
}

impl<const N: usize> From<[QueryParameter; N]> for QueryParameters {
    fn from(values: [QueryParameter; N]) -> Self {
        values.into_iter().collect()
    }
}

impl FromIterator<QueryParameter> for QueryParameters {
    fn from_iter<T: IntoIterator<Item = QueryParameter>>(iter: T) -> Self {
        Self {
            values: iter.into_iter().collect(),
        }
    }
}

#[derive(Debug, Clone)]
pub enum QueryParameter {
    Boolean(Option<bool>),
    Int8(Option<i8>),
    Int16(Option<i16>),
    Int32(Option<i32>),
    Int64(Option<i64>),
    UInt8(Option<u8>),
    UInt16(Option<u16>),
    UInt32(Option<u32>),
    UInt64(Option<u64>),
    Float32(Option<f32>),
    Float64(Option<f64>),
    Utf8(Option<String>),
    LargeUtf8(Option<String>),
    Binary(Option<Vec<u8>>),
    LargeBinary(Option<Vec<u8>>),
    Array(ArrayRef),
    Null(DataType),
}

impl QueryParameter {
    #[must_use]
    pub fn null(data_type: DataType) -> Self {
        Self::Null(data_type)
    }

    /// Converts this scalar parameter into a JSON value for the async
    /// `/v1/queries` API. A `NULL` parameter maps to JSON `null`.
    ///
    /// # Errors
    ///
    /// Returns [`QueryParameterError::UnsupportedJsonParameter`] for parameters
    /// with no JSON scalar representation: binary ([`QueryParameter::Binary`],
    /// [`QueryParameter::LargeBinary`]), arrays ([`QueryParameter::Array`]), and
    /// non-finite floats (`NaN`/`Inf`).
    pub fn to_json_value(&self) -> Result<serde_json::Value, QueryParameterError> {
        use serde_json::Value;

        let value = match self {
            Self::Boolean(v) => (*v).map_or(Value::Null, Value::Bool),
            Self::Int8(v) => (*v).map_or(Value::Null, Value::from),
            Self::Int16(v) => (*v).map_or(Value::Null, Value::from),
            Self::Int32(v) => (*v).map_or(Value::Null, Value::from),
            Self::Int64(v) => (*v).map_or(Value::Null, Value::from),
            Self::UInt8(v) => (*v).map_or(Value::Null, Value::from),
            Self::UInt16(v) => (*v).map_or(Value::Null, Value::from),
            Self::UInt32(v) => (*v).map_or(Value::Null, Value::from),
            Self::UInt64(v) => (*v).map_or(Value::Null, Value::from),
            Self::Float32(v) => match v {
                Some(f) => Self::finite_json_number(f64::from(*f))?,
                None => Value::Null,
            },
            Self::Float64(v) => match v {
                Some(f) => Self::finite_json_number(*f)?,
                None => Value::Null,
            },
            Self::Utf8(v) | Self::LargeUtf8(v) => v.clone().map_or(Value::Null, Value::String),
            Self::Null(_) => Value::Null,
            Self::Binary(_) | Self::LargeBinary(_) => {
                return Err(QueryParameterError::UnsupportedJsonParameter {
                    message: "binary parameters are not supported by the async /v1/queries API"
                        .to_string(),
                });
            }
            Self::Array(_) => {
                return Err(QueryParameterError::UnsupportedJsonParameter {
                    message: "array parameters are not supported by the async /v1/queries API; \
                              use scalar bind values"
                        .to_string(),
                });
            }
        };

        Ok(value)
    }

    fn finite_json_number(value: f64) -> Result<serde_json::Value, QueryParameterError> {
        serde_json::Number::from_f64(value)
            .map(serde_json::Value::Number)
            .ok_or_else(|| QueryParameterError::UnsupportedJsonParameter {
                message: format!("non-finite float parameter ({value}) cannot be encoded as JSON"),
            })
    }

    pub fn array<A>(array: A) -> Result<Self, QueryParameterError>
    where
        A: Array + 'static,
    {
        Self::array_ref(Arc::new(array) as ArrayRef)
    }

    pub fn array_ref(array: ArrayRef) -> Result<Self, QueryParameterError> {
        ensure!(
            array.len() == 1,
            InvalidArrayLengthSnafu {
                array_length: array.len(),
            }
        );
        Ok(Self::Array(array))
    }

    fn into_field_and_array(self, index: usize) -> Result<(Field, ArrayRef), QueryParameterError> {
        let field_name = format!("${}", index + 1);
        let data_type = self.data_type();
        let array = self.into_array();
        ensure!(
            array.len() == 1,
            InvalidArrayLengthSnafu {
                array_length: array.len(),
            }
        );
        Ok((Field::new(&field_name, data_type, true), array))
    }

    fn data_type(&self) -> DataType {
        match self {
            Self::Boolean(_) => DataType::Boolean,
            Self::Int8(_) => DataType::Int8,
            Self::Int16(_) => DataType::Int16,
            Self::Int32(_) => DataType::Int32,
            Self::Int64(_) => DataType::Int64,
            Self::UInt8(_) => DataType::UInt8,
            Self::UInt16(_) => DataType::UInt16,
            Self::UInt32(_) => DataType::UInt32,
            Self::UInt64(_) => DataType::UInt64,
            Self::Float32(_) => DataType::Float32,
            Self::Float64(_) => DataType::Float64,
            Self::Utf8(_) => DataType::Utf8,
            Self::LargeUtf8(_) => DataType::LargeUtf8,
            Self::Binary(_) => DataType::Binary,
            Self::LargeBinary(_) => DataType::LargeBinary,
            Self::Array(array) => array.data_type().clone(),
            Self::Null(data_type) => data_type.clone(),
        }
    }

    fn into_array(self) -> ArrayRef {
        match self {
            Self::Boolean(value) => Arc::new(BooleanArray::from(vec![value])) as ArrayRef,
            Self::Int8(value) => Arc::new(Int8Array::from(vec![value])) as ArrayRef,
            Self::Int16(value) => Arc::new(Int16Array::from(vec![value])) as ArrayRef,
            Self::Int32(value) => Arc::new(Int32Array::from(vec![value])) as ArrayRef,
            Self::Int64(value) => Arc::new(Int64Array::from(vec![value])) as ArrayRef,
            Self::UInt8(value) => Arc::new(UInt8Array::from(vec![value])) as ArrayRef,
            Self::UInt16(value) => Arc::new(UInt16Array::from(vec![value])) as ArrayRef,
            Self::UInt32(value) => Arc::new(UInt32Array::from(vec![value])) as ArrayRef,
            Self::UInt64(value) => Arc::new(UInt64Array::from(vec![value])) as ArrayRef,
            Self::Float32(value) => Arc::new(Float32Array::from(vec![value])) as ArrayRef,
            Self::Float64(value) => Arc::new(Float64Array::from(vec![value])) as ArrayRef,
            Self::Utf8(value) => Arc::new(StringArray::from(vec![value.as_deref()])) as ArrayRef,
            Self::LargeUtf8(value) => {
                Arc::new(LargeStringArray::from(vec![value.as_deref()])) as ArrayRef
            }
            Self::Binary(value) => Arc::new(BinaryArray::from(vec![value.as_deref()])) as ArrayRef,
            Self::LargeBinary(value) => {
                Arc::new(LargeBinaryArray::from(vec![value.as_deref()])) as ArrayRef
            }
            Self::Array(array) => array,
            Self::Null(data_type) => new_null_array(&data_type, 1),
        }
    }
}

impl TryFrom<ArrayRef> for QueryParameter {
    type Error = QueryParameterError;

    fn try_from(value: ArrayRef) -> Result<Self, Self::Error> {
        Self::array_ref(value)
    }
}

impl From<bool> for QueryParameter {
    fn from(value: bool) -> Self {
        Self::Boolean(Some(value))
    }
}

impl From<Option<bool>> for QueryParameter {
    fn from(value: Option<bool>) -> Self {
        Self::Boolean(value)
    }
}

impl From<i8> for QueryParameter {
    fn from(value: i8) -> Self {
        Self::Int8(Some(value))
    }
}

impl From<Option<i8>> for QueryParameter {
    fn from(value: Option<i8>) -> Self {
        Self::Int8(value)
    }
}

impl From<i16> for QueryParameter {
    fn from(value: i16) -> Self {
        Self::Int16(Some(value))
    }
}

impl From<Option<i16>> for QueryParameter {
    fn from(value: Option<i16>) -> Self {
        Self::Int16(value)
    }
}

impl From<i32> for QueryParameter {
    fn from(value: i32) -> Self {
        Self::Int32(Some(value))
    }
}

impl From<Option<i32>> for QueryParameter {
    fn from(value: Option<i32>) -> Self {
        Self::Int32(value)
    }
}

impl From<i64> for QueryParameter {
    fn from(value: i64) -> Self {
        Self::Int64(Some(value))
    }
}

impl From<Option<i64>> for QueryParameter {
    fn from(value: Option<i64>) -> Self {
        Self::Int64(value)
    }
}

impl From<u8> for QueryParameter {
    fn from(value: u8) -> Self {
        Self::UInt8(Some(value))
    }
}

impl From<Option<u8>> for QueryParameter {
    fn from(value: Option<u8>) -> Self {
        Self::UInt8(value)
    }
}

impl From<u16> for QueryParameter {
    fn from(value: u16) -> Self {
        Self::UInt16(Some(value))
    }
}

impl From<Option<u16>> for QueryParameter {
    fn from(value: Option<u16>) -> Self {
        Self::UInt16(value)
    }
}

impl From<u32> for QueryParameter {
    fn from(value: u32) -> Self {
        Self::UInt32(Some(value))
    }
}

impl From<Option<u32>> for QueryParameter {
    fn from(value: Option<u32>) -> Self {
        Self::UInt32(value)
    }
}

impl From<u64> for QueryParameter {
    fn from(value: u64) -> Self {
        Self::UInt64(Some(value))
    }
}

impl From<Option<u64>> for QueryParameter {
    fn from(value: Option<u64>) -> Self {
        Self::UInt64(value)
    }
}

impl From<f32> for QueryParameter {
    fn from(value: f32) -> Self {
        Self::Float32(Some(value))
    }
}

impl From<Option<f32>> for QueryParameter {
    fn from(value: Option<f32>) -> Self {
        Self::Float32(value)
    }
}

impl From<f64> for QueryParameter {
    fn from(value: f64) -> Self {
        Self::Float64(Some(value))
    }
}

impl From<Option<f64>> for QueryParameter {
    fn from(value: Option<f64>) -> Self {
        Self::Float64(value)
    }
}

impl From<String> for QueryParameter {
    fn from(value: String) -> Self {
        Self::Utf8(Some(value))
    }
}

impl From<Option<String>> for QueryParameter {
    fn from(value: Option<String>) -> Self {
        Self::Utf8(value)
    }
}

impl<'a> From<&'a str> for QueryParameter {
    fn from(value: &'a str) -> Self {
        Self::Utf8(Some(value.to_string()))
    }
}

impl<'a> From<Option<&'a str>> for QueryParameter {
    fn from(value: Option<&'a str>) -> Self {
        Self::Utf8(value.map(str::to_string))
    }
}

impl From<Vec<u8>> for QueryParameter {
    fn from(value: Vec<u8>) -> Self {
        Self::Binary(Some(value))
    }
}

impl From<Option<Vec<u8>>> for QueryParameter {
    fn from(value: Option<Vec<u8>>) -> Self {
        Self::Binary(value)
    }
}

impl<'a> From<&'a [u8]> for QueryParameter {
    fn from(value: &'a [u8]) -> Self {
        Self::Binary(Some(value.to_vec()))
    }
}

impl<'a> From<Option<&'a [u8]>> for QueryParameter {
    fn from(value: Option<&'a [u8]>) -> Self {
        Self::Binary(value.map(<[u8]>::to_vec))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow::array::{
        ArrayRef, BinaryArray, BinaryViewArray, BooleanArray, Date32Array, Date64Array,
        Decimal128Array, Float32Array, Float64Array, Int8Array, Int16Array, Int32Array,
        Int32DictionaryArray, Int64Array, LargeBinaryArray, LargeStringArray, StringArray,
        StringViewArray, TimestampNanosecondArray, UInt8Array, UInt16Array, UInt32Array,
        UInt64Array, new_null_array,
    };
    use arrow::datatypes::{IntervalUnit, TimeUnit, UnionFields, UnionMode};
    use std::sync::Arc;

    #[test]
    fn to_json_values_encodes_scalars() {
        let params = QueryParameters::new()
            .push("active")
            .push(5_i64)
            .push(1.5_f64)
            .push(true);
        assert_eq!(
            params.to_json_values().expect("scalars encode"),
            serde_json::json!(["active", 5, 1.5, true])
        );
    }

    #[test]
    fn to_json_values_encodes_null_binding() {
        let params = QueryParameters::new().push(None::<i32>);
        assert_eq!(
            params.to_json_values().expect("null encodes"),
            serde_json::json!([null])
        );
    }

    #[test]
    fn to_json_values_empty_is_empty_array() {
        assert_eq!(
            QueryParameters::new()
                .to_json_values()
                .expect("empty encodes"),
            serde_json::json!([])
        );
    }

    #[test]
    fn to_json_values_rejects_binary() {
        let params = QueryParameters::new().push(vec![1_u8, 2, 3]);
        assert!(matches!(
            params.to_json_values(),
            Err(QueryParameterError::UnsupportedJsonParameter { .. })
        ));
    }

    #[test]
    fn to_json_values_rejects_array() {
        let array =
            QueryParameter::array(Int32Array::from(vec![1])).expect("single-element array param");
        assert!(matches!(
            QueryParameters::from(array).to_json_values(),
            Err(QueryParameterError::UnsupportedJsonParameter { .. })
        ));
    }

    #[test]
    fn to_json_values_rejects_non_finite_float() {
        let params = QueryParameters::new().push(f64::NAN);
        assert!(matches!(
            params.to_json_values(),
            Err(QueryParameterError::UnsupportedJsonParameter { .. })
        ));
    }

    fn batch_from(params: QueryParameters) -> RecordBatch {
        params
            .into_record_batch()
            .expect("parameter batch should be created")
            .expect("parameter batch should not be empty")
    }

    fn full_arrow_data_types() -> Vec<DataType> {
        let list_field = Arc::new(Field::new_list_field(DataType::Int32, true));
        let large_list_field = Arc::new(Field::new_list_field(DataType::Int64, true));
        let fixed_size_list_field = Arc::new(Field::new_list_field(DataType::Int16, true));
        let struct_fields = vec![
            Arc::new(Field::new("int_field", DataType::Int32, true)),
            Arc::new(Field::new("text_field", DataType::Utf8, true)),
        ];
        let union_fields = UnionFields::try_new(
            vec![0, 1],
            vec![
                Field::new("union_int", DataType::Int32, true),
                Field::new("union_text", DataType::Utf8, true),
            ],
        )
        .expect("union fields should be valid");
        let map_entries = Arc::new(Field::new(
            "entries",
            DataType::Struct(
                vec![
                    Arc::new(Field::new("key", DataType::Utf8, false)),
                    Arc::new(Field::new("value", DataType::Int32, true)),
                ]
                .into(),
            ),
            false,
        ));
        let run_end_type = Arc::new(Field::new("run_ends", DataType::Int32, false));
        let run_value_type = Arc::new(Field::new("values", DataType::Utf8, true));

        vec![
            DataType::Null,
            DataType::Boolean,
            DataType::Int8,
            DataType::Int16,
            DataType::Int32,
            DataType::Int64,
            DataType::UInt8,
            DataType::UInt16,
            DataType::UInt32,
            DataType::UInt64,
            DataType::Float16,
            DataType::Float32,
            DataType::Float64,
            DataType::Timestamp(TimeUnit::Second, None),
            DataType::Timestamp(TimeUnit::Millisecond, Some("+00:00".into())),
            DataType::Timestamp(TimeUnit::Microsecond, None),
            DataType::Timestamp(TimeUnit::Nanosecond, Some("+00:00".into())),
            DataType::Date32,
            DataType::Date64,
            DataType::Time32(TimeUnit::Second),
            DataType::Time32(TimeUnit::Millisecond),
            DataType::Time64(TimeUnit::Microsecond),
            DataType::Time64(TimeUnit::Nanosecond),
            DataType::Duration(TimeUnit::Second),
            DataType::Duration(TimeUnit::Millisecond),
            DataType::Duration(TimeUnit::Microsecond),
            DataType::Duration(TimeUnit::Nanosecond),
            DataType::Interval(IntervalUnit::YearMonth),
            DataType::Interval(IntervalUnit::DayTime),
            DataType::Interval(IntervalUnit::MonthDayNano),
            DataType::Binary,
            DataType::FixedSizeBinary(4),
            DataType::LargeBinary,
            DataType::BinaryView,
            DataType::Utf8,
            DataType::LargeUtf8,
            DataType::Utf8View,
            DataType::List(Arc::clone(&list_field)),
            DataType::ListView(Arc::clone(&list_field)),
            DataType::FixedSizeList(Arc::clone(&fixed_size_list_field), 2),
            DataType::LargeList(Arc::clone(&large_list_field)),
            DataType::LargeListView(Arc::clone(&large_list_field)),
            DataType::Struct(struct_fields.clone().into()),
            DataType::Union(union_fields.clone(), UnionMode::Sparse),
            DataType::Union(union_fields, UnionMode::Dense),
            DataType::Dictionary(Box::new(DataType::Int32), Box::new(DataType::Utf8)),
            DataType::Decimal32(5, 2),
            DataType::Decimal64(10, 2),
            DataType::Decimal128(20, 2),
            DataType::Decimal256(40, 2),
            DataType::Map(map_entries, false),
            DataType::RunEndEncoded(run_end_type, run_value_type),
        ]
    }

    fn array_parameter(array: ArrayRef) -> QueryParameter {
        QueryParameter::array_ref(array).expect("array-backed parameter should be valid")
    }

    #[test]
    fn test_query_parameters_build_record_batch() {
        let batch = QueryParameters::new()
            .push(1_i32)
            .push(1.5_f64)
            .push("taxi")
            .into_record_batch()
            .expect("parameter batch should be created")
            .expect("parameter batch should not be empty");

        assert_eq!(batch.schema().field(0).name(), "$1");
        assert_eq!(batch.schema().field(1).name(), "$2");
        assert_eq!(batch.schema().field(2).name(), "$3");

        let int_values = batch
            .column(0)
            .as_any()
            .downcast_ref::<Int32Array>()
            .expect("$1 should be Int32");
        assert_eq!(int_values.value(0), 1);

        let text_values = batch
            .column(2)
            .as_any()
            .downcast_ref::<StringArray>()
            .expect("$3 should be Utf8");
        assert_eq!(text_values.value(0), "taxi");
    }

    #[test]
    fn test_query_parameters_cover_supported_scalar_types() {
        let batch = batch_from(
            QueryParameters::new()
                .push(true)
                .push(-8_i8)
                .push(-16_i16)
                .push(-32_i32)
                .push(-64_i64)
                .push(8_u8)
                .push(16_u16)
                .push(32_u32)
                .push(64_u64)
                .push(3.25_f32)
                .push(6.5_f64)
                .push(String::from("utf8-owned"))
                .push(QueryParameter::LargeUtf8(Some("large-utf8".to_string())))
                .push(vec![1_u8, 2, 3])
                .push(QueryParameter::LargeBinary(Some(vec![4_u8, 5, 6]))),
        );

        let expected_types = vec![
            DataType::Boolean,
            DataType::Int8,
            DataType::Int16,
            DataType::Int32,
            DataType::Int64,
            DataType::UInt8,
            DataType::UInt16,
            DataType::UInt32,
            DataType::UInt64,
            DataType::Float32,
            DataType::Float64,
            DataType::Utf8,
            DataType::LargeUtf8,
            DataType::Binary,
            DataType::LargeBinary,
        ];
        let schema = batch.schema();

        for (index, data_type) in expected_types.iter().enumerate() {
            let expected_name = format!("${}", index + 1);
            let field = schema.field(index);
            assert_eq!(field.name(), expected_name.as_str());
            assert_eq!(field.data_type(), data_type);
        }

        assert!(batch
            .column(0)
            .as_any()
            .downcast_ref::<BooleanArray>()
            .expect("$1 should be Boolean")
            .value(0));
        assert_eq!(
            batch.column(1)
                .as_any()
                .downcast_ref::<Int8Array>()
                .expect("$2 should be Int8")
                .value(0),
            -8
        );
        assert_eq!(
            batch.column(2)
                .as_any()
                .downcast_ref::<Int16Array>()
                .expect("$3 should be Int16")
                .value(0),
            -16
        );
        assert_eq!(
            batch.column(3)
                .as_any()
                .downcast_ref::<Int32Array>()
                .expect("$4 should be Int32")
                .value(0),
            -32
        );
        assert_eq!(
            batch.column(4)
                .as_any()
                .downcast_ref::<Int64Array>()
                .expect("$5 should be Int64")
                .value(0),
            -64
        );
        assert_eq!(
            batch.column(5)
                .as_any()
                .downcast_ref::<UInt8Array>()
                .expect("$6 should be UInt8")
                .value(0),
            8
        );
        assert_eq!(
            batch.column(6)
                .as_any()
                .downcast_ref::<UInt16Array>()
                .expect("$7 should be UInt16")
                .value(0),
            16
        );
        assert_eq!(
            batch.column(7)
                .as_any()
                .downcast_ref::<UInt32Array>()
                .expect("$8 should be UInt32")
                .value(0),
            32
        );
        assert_eq!(
            batch.column(8)
                .as_any()
                .downcast_ref::<UInt64Array>()
                .expect("$9 should be UInt64")
                .value(0),
            64
        );
        assert_eq!(
            batch.column(9)
                .as_any()
                .downcast_ref::<Float32Array>()
                .expect("$10 should be Float32")
                .value(0),
            3.25
        );
        assert_eq!(
            batch.column(10)
                .as_any()
                .downcast_ref::<Float64Array>()
                .expect("$11 should be Float64")
                .value(0),
            6.5
        );
        assert_eq!(
            batch.column(11)
                .as_any()
                .downcast_ref::<StringArray>()
                .expect("$12 should be Utf8")
                .value(0),
            "utf8-owned"
        );
        assert_eq!(
            batch.column(12)
                .as_any()
                .downcast_ref::<LargeStringArray>()
                .expect("$13 should be LargeUtf8")
                .value(0),
            "large-utf8"
        );
        assert_eq!(
            batch.column(13)
                .as_any()
                .downcast_ref::<BinaryArray>()
                .expect("$14 should be Binary")
                .value(0),
            &[1_u8, 2, 3]
        );
        assert_eq!(
            batch.column(14)
                .as_any()
                .downcast_ref::<LargeBinaryArray>()
                .expect("$15 should be LargeBinary")
                .value(0),
            &[4_u8, 5, 6]
        );
    }

    #[test]
    fn test_query_parameter_array_backed_values_preserve_arrow_types() {
        let decimal = Decimal128Array::from(vec![Some(12_345_i128)])
            .with_precision_and_scale(5, 2)
            .expect("decimal array should accept precision and scale");
        let timestamp = TimestampNanosecondArray::from(vec![Some(1_234_567_890_i64)])
            .with_timezone_utc();
        let date = Date32Array::from(vec![Some(19_000)]);
        let date64 = Date64Array::from(vec![Some(1_728_000_000_i64)]);

        let batch = batch_from(
            QueryParameters::new()
                .push(QueryParameter::array(StringViewArray::from(vec![Some("view-value")]))
                    .expect("StringView parameter should be valid"))
                .push(QueryParameter::array(BinaryViewArray::from(vec![Some(b"view-bytes".as_slice())]))
                    .expect("BinaryView parameter should be valid"))
                .push(QueryParameter::array(decimal).expect("Decimal parameter should be valid"))
                .push(QueryParameter::array(timestamp).expect("Timestamp parameter should be valid"))
                .push(QueryParameter::array(date).expect("Date32 parameter should be valid"))
                .push(QueryParameter::array(date64).expect("Date64 parameter should be valid"))
                .push(
                    QueryParameter::array(
                        Int32DictionaryArray::from_iter(vec![Some("dictionary-value")]),
                    )
                    .expect("Dictionary parameter should be valid"),
                ),
        );

        assert_eq!(batch.schema().field(0).data_type(), &DataType::Utf8View);
        assert_eq!(batch.schema().field(1).data_type(), &DataType::BinaryView);
        assert_eq!(batch.schema().field(2).data_type(), &DataType::Decimal128(5, 2));
        assert_eq!(
            batch.schema().field(3).data_type(),
            &DataType::Timestamp(TimeUnit::Nanosecond, Some("+00:00".into()))
        );
        assert_eq!(batch.schema().field(4).data_type(), &DataType::Date32);
        assert_eq!(batch.schema().field(5).data_type(), &DataType::Date64);
        assert_eq!(
            batch.schema().field(6).data_type(),
            &DataType::Dictionary(Box::new(DataType::Int32), Box::new(DataType::Utf8))
        );

        assert_eq!(
            batch.column(0)
                .as_any()
                .downcast_ref::<StringViewArray>()
                .expect("$1 should be Utf8View")
                .value(0),
            "view-value"
        );
        assert_eq!(
            batch.column(1)
                .as_any()
                .downcast_ref::<BinaryViewArray>()
                .expect("$2 should be BinaryView")
                .value(0),
            b"view-bytes"
        );
        assert_eq!(
            batch.column(2)
                .as_any()
                .downcast_ref::<Decimal128Array>()
                .expect("$3 should be Decimal128")
                .value(0),
            12_345_i128
        );
        assert_eq!(
            batch.column(3)
                .as_any()
                .downcast_ref::<TimestampNanosecondArray>()
                .expect("$4 should be TimestampNanosecond")
                .value(0),
            1_234_567_890_i64
        );
        assert_eq!(
            batch.column(4)
                .as_any()
                .downcast_ref::<Date32Array>()
                .expect("$5 should be Date32")
                .value(0),
            19_000
        );
        assert_eq!(
            batch.column(5)
                .as_any()
                .downcast_ref::<Date64Array>()
                .expect("$6 should be Date64")
                .value(0),
            1_728_000_000_i64
        );
        let dictionary = batch
            .column(6)
            .as_any()
            .downcast_ref::<Int32DictionaryArray>()
            .expect("$7 should be Dictionary");
        assert_eq!(dictionary.keys().value(0), 0);
        let dictionary_values = dictionary
            .values()
            .as_any()
            .downcast_ref::<StringArray>()
            .expect("dictionary values should be Utf8");
        assert_eq!(dictionary_values.value(0), "dictionary-value");
    }

    #[test]
    fn test_query_parameter_array_backed_nulls_cover_entire_arrow_type_set() {
        for data_type in full_arrow_data_types() {
            let batch = batch_from(QueryParameters::from(array_parameter(new_null_array(
                &data_type,
                1,
            ))));

            assert_eq!(batch.num_rows(), 1);
            assert_eq!(batch.schema().field(0).data_type(), &data_type);
            assert_eq!(batch.column(0).data_type(), &data_type);
            assert_eq!(batch.column(0).len(), 1);
        }
    }

    #[test]
    fn test_query_parameter_array_requires_single_element() {
        let err = QueryParameter::array(StringViewArray::from(vec![Some("one"), Some("two")]))
            .expect_err("array-backed parameters should reject multi-value arrays");

        assert!(matches!(
            err,
            QueryParameterError::InvalidArrayLength { array_length: 2 }
        ));

        let err = QueryParameters::from(QueryParameter::Array(
            Arc::new(StringArray::from(vec![Some("one"), Some("two")])) as ArrayRef,
        ))
        .into_record_batch()
        .expect_err("direct array variant should also reject multi-value arrays");
        assert!(matches!(
            err,
            QueryParameterError::InvalidArrayLength { array_length: 2 }
        ));
    }

    #[test]
    fn test_query_parameters_option_and_borrowed_bindings() {
        let borrowed_bytes: &[u8] = b"borrowed-bytes";

        let batch = batch_from(
            QueryParameters::new()
                .push(Some(false))
                .push(None::<i8>)
                .push(Some(-16_i16))
                .push(None::<i32>)
                .push(Some(-64_i64))
                .push(None::<u8>)
                .push(Some(16_u16))
                .push(None::<u32>)
                .push(Some(64_u64))
                .push(None::<f32>)
                .push(Some(6.5_f64))
                .push(Some("borrowed-str"))
                .push(Some(String::from("owned-option")))
                .push(Some(borrowed_bytes))
                .push(Some(vec![9_u8, 8, 7])),
        );

        assert!(!batch
            .column(0)
            .as_any()
            .downcast_ref::<BooleanArray>()
            .expect("$1 should be Boolean")
            .value(0));
        assert!(batch.column(1).is_null(0));
        assert_eq!(
            batch.column(2)
                .as_any()
                .downcast_ref::<Int16Array>()
                .expect("$3 should be Int16")
                .value(0),
            -16
        );
        assert!(batch.column(3).is_null(0));
        assert_eq!(
            batch.column(4)
                .as_any()
                .downcast_ref::<Int64Array>()
                .expect("$5 should be Int64")
                .value(0),
            -64
        );
        assert!(batch.column(5).is_null(0));
        assert_eq!(
            batch.column(6)
                .as_any()
                .downcast_ref::<UInt16Array>()
                .expect("$7 should be UInt16")
                .value(0),
            16
        );
        assert!(batch.column(7).is_null(0));
        assert_eq!(
            batch.column(8)
                .as_any()
                .downcast_ref::<UInt64Array>()
                .expect("$9 should be UInt64")
                .value(0),
            64
        );
        assert!(batch.column(9).is_null(0));
        assert_eq!(
            batch.column(10)
                .as_any()
                .downcast_ref::<Float64Array>()
                .expect("$11 should be Float64")
                .value(0),
            6.5
        );
        assert_eq!(
            batch.column(11)
                .as_any()
                .downcast_ref::<StringArray>()
                .expect("$12 should be Utf8")
                .value(0),
            "borrowed-str"
        );
        assert_eq!(
            batch.column(12)
                .as_any()
                .downcast_ref::<StringArray>()
                .expect("$13 should be Utf8")
                .value(0),
            "owned-option"
        );
        assert_eq!(
            batch.column(13)
                .as_any()
                .downcast_ref::<BinaryArray>()
                .expect("$14 should be Binary")
                .value(0),
            borrowed_bytes
        );
        assert_eq!(
            batch.column(14)
                .as_any()
                .downcast_ref::<BinaryArray>()
                .expect("$15 should be Binary")
                .value(0),
            &[9_u8, 8, 7]
        );
    }

    #[test]
    fn test_query_parameters_collection_constructors() {
        assert!(QueryParameters::new().is_empty());
        assert!(!QueryParameters::new().push(1_i32).is_empty());

        let from_single = batch_from(QueryParameters::from(QueryParameter::from(11_i32)));
        assert_eq!(
            from_single
                .column(0)
                .as_any()
                .downcast_ref::<Int32Array>()
                .expect("single constructor should produce Int32")
                .value(0),
            11
        );

        let from_vec = batch_from(QueryParameters::from(vec![
            QueryParameter::from(12_i32),
            QueryParameter::from("vec-constructor"),
        ]));
        assert_eq!(
            from_vec
                .column(0)
                .as_any()
                .downcast_ref::<Int32Array>()
                .expect("vec constructor should produce Int32")
                .value(0),
            12
        );
        assert_eq!(
            from_vec
                .column(1)
                .as_any()
                .downcast_ref::<StringArray>()
                .expect("vec constructor should produce Utf8")
                .value(0),
            "vec-constructor"
        );

        let from_array = batch_from(QueryParameters::from([
            QueryParameter::from(13_i32),
            QueryParameter::from(14_i64),
        ]));
        assert_eq!(
            from_array
                .column(0)
                .as_any()
                .downcast_ref::<Int32Array>()
                .expect("array constructor should produce Int32")
                .value(0),
            13
        );
        assert_eq!(
            from_array
                .column(1)
                .as_any()
                .downcast_ref::<Int64Array>()
                .expect("array constructor should produce Int64")
                .value(0),
            14
        );

        let collected = batch_from(
            [QueryParameter::from(true), QueryParameter::from(15_u32)]
                .into_iter()
                .collect(),
        );
        assert!(
            collected
                .column(0)
                .as_any()
                .downcast_ref::<BooleanArray>()
                .expect("collected constructor should produce Boolean")
                .value(0)
        );
        assert_eq!(
            collected
                .column(1)
                .as_any()
                .downcast_ref::<UInt32Array>()
                .expect("collected constructor should produce UInt32")
                .value(0),
            15
        );
    }

    #[test]
    fn test_query_parameters_empty_batch() {
        let batch = QueryParameters::new()
            .into_record_batch()
            .expect("empty parameter batch should succeed");
        assert!(batch.is_none());
    }

    #[test]
    fn test_query_parameters_typed_nulls_preserve_explicit_types() {
        let batch = batch_from(
            QueryParameters::new()
                .push(QueryParameter::null(DataType::Int32))
                .push(QueryParameter::null(DataType::LargeUtf8))
                .push(QueryParameter::null(DataType::LargeBinary)),
        );

        assert_eq!(batch.schema().field(0).data_type(), &DataType::Int32);
        assert_eq!(batch.schema().field(1).data_type(), &DataType::LargeUtf8);
        assert_eq!(batch.schema().field(2).data_type(), &DataType::LargeBinary);
        assert!(batch.column(0).is_null(0));
        assert!(batch.column(1).is_null(0));
        assert!(batch.column(2).is_null(0));
    }
}
