use arrow::array::{
    ArrayRef, BinaryArray, BooleanArray, Float32Array, Float64Array, Int8Array, Int16Array,
    Int32Array, Int64Array, LargeBinaryArray, LargeStringArray, StringArray, UInt8Array,
    UInt16Array, UInt32Array, UInt64Array, new_null_array,
};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::error::ArrowError;
use arrow::record_batch::RecordBatch;
use snafu::Snafu;
use std::sync::Arc;

#[derive(Debug, Snafu)]
pub enum QueryParameterError {
    #[snafu(display("Failed to construct query parameter batch: {source}"))]
    BatchCreation { source: ArrowError },
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

        let (fields, columns): (Vec<_>, Vec<_>) = self
            .values
            .into_iter()
            .enumerate()
            .map(|(index, value)| value.into_field_and_array(index))
            .unzip();

        RecordBatch::try_new(Arc::new(Schema::new(fields)), columns)
            .map(Some)
            .map_err(|source| QueryParameterError::BatchCreation { source })
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
    Null(DataType),
}

impl QueryParameter {
    #[must_use]
    pub fn null(data_type: DataType) -> Self {
        Self::Null(data_type)
    }

    fn into_field_and_array(self, index: usize) -> (Field, ArrayRef) {
        let field_name = format!("${}", index + 1);
        let data_type = self.data_type();
        let array = self.into_array();
        (Field::new(&field_name, data_type, true), array)
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
            Self::Null(data_type) => new_null_array(&data_type, 1),
        }
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
    use arrow::array::{Int32Array, StringArray};

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
    fn test_query_parameters_empty_batch() {
        let batch = QueryParameters::new()
            .into_record_batch()
            .expect("empty parameter batch should succeed");
        assert!(batch.is_none());
    }

    #[test]
    fn test_query_parameters_typed_null() {
        let batch = QueryParameters::new()
            .push(QueryParameter::null(DataType::Int32))
            .into_record_batch()
            .expect("typed null batch should be created")
            .expect("typed null batch should not be empty");

        assert_eq!(batch.schema().field(0).data_type(), &DataType::Int32);
        assert!(batch.column(0).is_null(0));
    }
}
