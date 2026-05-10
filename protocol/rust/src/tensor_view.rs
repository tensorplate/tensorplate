// SPDX-License-Identifier: Apache-2.0
//
// V01-E02-F06: Rust mirror of `protocol/schemas/tensor_view.json` and the
// C++ `tensorplate::TensorView` value object.

use serde::{Deserialize, Serialize};

use crate::{DecodeError, ValidatePayload, SCHEMA_VERSION};

/// Element data type. Stable wire names match the JSON Schema and the C++
/// `tensorplate::DType` enum.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DType {
    #[default]
    Float32,
    Float16,
    Bfloat16,
    Int64,
    Int32,
    Int16,
    Int8,
    Uint8,
    Bool,
}

impl DType {
    /// Width in bytes of one element of this dtype.
    #[must_use]
    pub fn byte_width(self) -> u64 {
        match self {
            Self::Float32 | Self::Int32 => 4,
            Self::Float16 | Self::Bfloat16 | Self::Int16 => 2,
            Self::Int64 => 8,
            Self::Int8 | Self::Uint8 | Self::Bool => 1,
        }
    }

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Float32 => "float32",
            Self::Float16 => "float16",
            Self::Bfloat16 => "bfloat16",
            Self::Int64 => "int64",
            Self::Int32 => "int32",
            Self::Int16 => "int16",
            Self::Int8 => "int8",
            Self::Uint8 => "uint8",
            Self::Bool => "bool",
        }
    }
}

/// Memory layout. v0.1.0 supports row-major (C-contiguous) and column-major
/// (Fortran-contiguous) only; per-axis stride support is deferred.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Layout {
    #[default]
    RowMajor,
    ColMajor,
}

impl Layout {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::RowMajor => "row_major",
            Self::ColMajor => "col_major",
        }
    }
}

/// Mirror of `tensorplate::TensorView` (C++) and
/// `protocol/schemas/tensor_view.json`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TensorView {
    pub schema_version: String,
    pub dtype: DType,
    pub shape: Vec<i64>,
    #[serde(default, skip_serializing_if = "is_default_layout")]
    pub layout: Layout,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub byte_offset: u64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub byte_size: u64,
}

#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_default_layout(l: &Layout) -> bool {
    *l == Layout::RowMajor
}

#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_zero(v: &u64) -> bool {
    *v == 0
}

/// Errors raised by [`TensorView::new`]. Mirrors the C++
/// `TensorView::create` validation rules.
#[derive(Debug, thiserror::Error)]
pub enum TensorViewError {
    #[error("TensorView.shape must be rank >= 1")]
    EmptyShape,
    #[error("TensorView.shape entries must be >= 1; zero-volume tensors are not representable")]
    NonPositiveDim,
    #[error("TensorView.byte_size is smaller than product(shape) * dtype.byte_width()")]
    InsufficientByteSize,
    #[error("TensorView byte-size computation overflowed u64")]
    ByteSizeOverflow,
}

impl TensorView {
    /// Build and validate a TensorView at the v0.1 schema version.
    ///
    /// # Errors
    ///
    /// See [`TensorViewError`]. The same rules are applied by the C++
    /// `TensorView::create` factory.
    pub fn new(
        dtype: DType,
        shape: Vec<i64>,
        layout: Layout,
        byte_offset: u64,
        byte_size: u64,
    ) -> Result<Self, TensorViewError> {
        if shape.is_empty() {
            return Err(TensorViewError::EmptyShape);
        }
        let mut total: u64 = dtype.byte_width();
        for &d in &shape {
            if d <= 0 {
                return Err(TensorViewError::NonPositiveDim);
            }
            let ud = u64::try_from(d).map_err(|_| TensorViewError::ByteSizeOverflow)?;
            total = total
                .checked_mul(ud)
                .ok_or(TensorViewError::ByteSizeOverflow)?;
        }
        let resolved_size = if byte_size == 0 {
            total
        } else if byte_size < total {
            return Err(TensorViewError::InsufficientByteSize);
        } else {
            byte_size
        };
        Ok(Self {
            schema_version: SCHEMA_VERSION.to_string(),
            dtype,
            shape,
            layout,
            byte_offset,
            byte_size: resolved_size,
        })
    }

    /// Number of elements (product of shape).
    #[must_use]
    pub fn num_elements(&self) -> i64 {
        self.shape.iter().product()
    }
}

impl ValidatePayload for TensorView {
    fn validate_payload(self) -> Result<Self, DecodeError> {
        Self::new(
            self.dtype,
            self.shape,
            self.layout,
            self.byte_offset,
            self.byte_size,
        )
        .map_err(|err| DecodeError::InvalidPayload(err.to_string()))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use super::{DType, Layout, TensorView, TensorViewError, SCHEMA_VERSION};
    use crate::decode_with_version_check;

    #[test]
    fn round_trip_preserves_fields() {
        let v = TensorView::new(DType::Float16, vec![1, 3, 224, 224], Layout::RowMajor, 0, 0)
            .expect("valid");
        let json = serde_json::to_string(&v).expect("serialize");
        let back: TensorView = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(v, back);
        assert_eq!(back.schema_version, SCHEMA_VERSION);
    }

    #[test]
    fn defaults_are_omitted_for_compactness() {
        let v = TensorView::new(DType::Float32, vec![10], Layout::RowMajor, 0, 0).expect("valid");
        let json = serde_json::to_string(&v).expect("serialize");
        // Defaults compress out: layout=row_major, byte_offset=0, but byte_size
        // is computed (not zero), so it appears.
        assert!(!json.contains("\"layout\""));
        assert!(!json.contains("\"byte_offset\""));
        // byte_size = 40 (10 * 4)
        assert!(json.contains("\"byte_size\":40"));
    }

    #[test]
    fn byte_size_is_computed_when_zero() {
        let v = TensorView::new(DType::Float32, vec![2, 3], Layout::RowMajor, 0, 0).expect("valid");
        assert_eq!(v.byte_size, 24);
        assert_eq!(v.num_elements(), 6);
    }

    #[test]
    fn explicit_byte_size_must_be_at_least_computed() {
        // Padding allowed (computed=24, allocated=64).
        let v = TensorView::new(DType::Float32, vec![2, 3], Layout::RowMajor, 0, 64).expect("ok");
        assert_eq!(v.byte_size, 64);

        // Underflow rejected.
        assert!(matches!(
            TensorView::new(DType::Float32, vec![2, 3], Layout::RowMajor, 0, 16),
            Err(TensorViewError::InsufficientByteSize)
        ));
    }

    #[test]
    fn empty_shape_is_rejected() {
        assert!(matches!(
            TensorView::new(DType::Float32, vec![], Layout::RowMajor, 0, 0),
            Err(TensorViewError::EmptyShape)
        ));
    }

    #[test]
    fn non_positive_dim_is_rejected() {
        assert!(matches!(
            TensorView::new(DType::Float32, vec![1, 0, 3], Layout::RowMajor, 0, 0),
            Err(TensorViewError::NonPositiveDim)
        ));
        assert!(matches!(
            TensorView::new(DType::Float32, vec![1, -1], Layout::RowMajor, 0, 0),
            Err(TensorViewError::NonPositiveDim)
        ));
    }

    #[test]
    fn dtype_byte_width_table_is_locked() {
        assert_eq!(DType::Float32.byte_width(), 4);
        assert_eq!(DType::Float16.byte_width(), 2);
        assert_eq!(DType::Bfloat16.byte_width(), 2);
        assert_eq!(DType::Int64.byte_width(), 8);
        assert_eq!(DType::Int32.byte_width(), 4);
        assert_eq!(DType::Int16.byte_width(), 2);
        assert_eq!(DType::Int8.byte_width(), 1);
        assert_eq!(DType::Uint8.byte_width(), 1);
        assert_eq!(DType::Bool.byte_width(), 1);
    }

    #[test]
    fn version_check_decoder_accepts_current_schema() {
        let json = format!(
            r#"{{"schema_version":"{SCHEMA_VERSION}","dtype":"float16","shape":[1,3,224,224]}}"#
        );
        let v: TensorView = decode_with_version_check(&json).expect("decode");
        assert_eq!(v.dtype, DType::Float16);
        assert_eq!(v.shape, vec![1, 3, 224, 224]);
        assert_eq!(v.layout, Layout::RowMajor);
        assert_eq!(v.byte_size, 3 * 224 * 224 * 2);
    }

    #[test]
    fn version_check_decoder_rejects_old_schema() {
        let json = r#"{"schema_version":"0.0","dtype":"float32","shape":[1]}"#;
        let err = decode_with_version_check::<TensorView>(json).expect_err("rejected");
        assert!(matches!(
            err,
            crate::DecodeError::UnsupportedSchemaVersion { .. }
        ));
    }

    #[test]
    fn version_check_decoder_rejects_current_schema_invalid_shape() {
        let json =
            format!(r#"{{"schema_version":"{SCHEMA_VERSION}","dtype":"float32","shape":[1,0]}}"#);
        let err = decode_with_version_check::<TensorView>(&json).expect_err("rejected");
        assert!(matches!(err, crate::DecodeError::InvalidPayload(_)));
    }
}
