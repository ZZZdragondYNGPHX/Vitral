//! CPU tensor helpers shared by the two Qwen exports.
use crate::{error::VitralError, onnx_env::get_ort_env};
use ort::{
    memory::Allocator,
    session::Session,
    value::{DynTensor, DynTensorValueType, DynValue, Tensor, TensorElementType},
};
use std::path::Path;
pub(super) fn error(e: impl std::fmt::Display) -> VitralError {
    VitralError::ModelUnavailable(format!("Qwen ONNX: {e}"))
}
pub(super) fn session(path: &Path) -> Result<Session, VitralError> {
    let env = get_ort_env().ok_or_else(|| error("ONNX Runtime unavailable"))?;
    Session::builder(env)
        .map_err(error)?
        .with_intra_threads(1)
        .map_err(error)?
        .with_inter_threads(1)
        .map_err(error)?
        .commit_from_file(path)
        .map_err(error)
}
pub(super) fn ids(
    shape: impl Into<ort::value::Shape>,
    values: Vec<i64>,
) -> Result<DynValue, VitralError> {
    Tensor::from_array((shape.into(), values))
        .map(|t| t.into_dyn())
        .map_err(error)
}
pub(super) fn zeros(dtype: TensorElementType, shape: Vec<i64>) -> Result<DynValue, VitralError> {
    if !matches!(
        dtype,
        TensorElementType::Float32 | TensorElementType::Float16 | TensorElementType::Uint16
    ) {
        return Err(error("unsupported KV type"));
    }
    if shape.iter().any(|&d| d < 0) {
        return Err(error("unresolved KV shape"));
    }
    DynTensor::new(&Allocator::default(), dtype, shape)
        .map(|t| t.into_dyn())
        .map_err(error)
}
pub(super) fn half_to_float(h: u16) -> f32 {
    let sign = if h & 0x8000 == 0 { 1.0 } else { -1.0 };
    let exp = (h >> 10) & 31;
    let mant = h & 1023;
    match exp {
        0 => sign * (mant as f32) * 2f32.powi(-24),
        31 => {
            if mant == 0 {
                sign * f32::INFINITY
            } else {
                f32::NAN
            }
        }
        _ => sign * (1.0 + mant as f32 / 1024.0) * 2f32.powi(i32::from(exp) - 15),
    }
}
pub(super) fn greedy(value: &DynValue, previous: &[i64], penalty: f32) -> Result<i64, VitralError> {
    let t = value
        .view()
        .downcast::<DynTensorValueType>()
        .map_err(error)?;
    let shape = t.shape();
    let vocab = *shape.last().ok_or_else(|| error("empty logits"))? as usize;
    if shape.len() != 3 || shape[0] != 1 || vocab == 0 || shape[1] < 1 {
        return Err(error("invalid logits shape"));
    }
    let offset = (shape[1] as usize - 1) * vocab;
    if !t.memory_info().is_cpu_accessible() {
        return Err(error("logits must be CPU-accessible"));
    }
    let mut scores = match t.data_type() {
        TensorElementType::Float32 => {
            value.try_extract_tensor::<f32>().map_err(error)?.1[offset..offset + vocab].to_vec()
        }
        TensorElementType::Float16 | TensorElementType::Uint16 => {
            // The shape bounds the slice; the session output is CPU-accessible.
            let row = unsafe {
                std::slice::from_raw_parts(t.data_ptr().cast::<u16>().add(offset), vocab)
            };
            row.iter().map(|&h| half_to_float(h)).collect()
        }
        _ => return Err(error("unsupported logits type")),
    };
    let mut seen = std::collections::HashSet::new();
    for &id in previous.iter().rev().take(64) {
        if seen.insert(id) {
            if let Some(score) = usize::try_from(id).ok().and_then(|id| scores.get_mut(id)) {
                *score = if *score < 0.0 {
                    *score * penalty
                } else {
                    *score / penalty
                };
            }
        }
    }
    scores
        .iter()
        .enumerate()
        .filter(|(_, score)| score.is_finite())
        .max_by(|(ai, a), (bi, b)| a.total_cmp(b).then_with(|| bi.cmp(ai)))
        .map(|(id, _)| id as i64)
        .ok_or_else(|| error("no finite logits"))
}
/// The ASR export returns only new KV positions, in [B,T,H,D] order.
#[cfg(feature = "native-voice")]
pub(super) fn apply_delta(
    cache: &mut DynValue,
    delta: &DynValue,
    position: usize,
    count: usize,
) -> Result<(), VitralError> {
    let src = delta
        .view()
        .downcast::<DynTensorValueType>()
        .map_err(error)?;
    let dst = cache
        .view_mut()
        .downcast::<DynTensorValueType>()
        .map_err(error)?;
    if !src.memory_info().is_cpu_accessible() || !dst.memory_info().is_cpu_accessible() {
        return Err(error("KV tensors must be CPU-accessible"));
    }
    let s = src.shape();
    let d = dst.shape();
    if s.len() != 4
        || d.len() != 4
        || s[0] != 1
        || d[0] != 1
        || s[2..] != d[2..]
        || src.data_type() != dst.data_type()
        || count > s[1] as usize
        || position + count > d[1] as usize
    {
        return Err(error("invalid KV delta shape or position"));
    }
    let width = (d[2] * d[3]) as usize;
    let bytes = dst
        .data_type()
        .byte_size(width)
        .ok_or_else(|| error("invalid KV element type"))?;
    if count > 0 {
        // Bounds and element types checked above; both are distinct CPU allocations.
        unsafe {
            std::ptr::copy_nonoverlapping(
                src.data_ptr().cast::<u8>(),
                dst.data_ptr().cast_mut().cast::<u8>().add(position * bytes),
                count * bytes,
            );
        }
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(feature = "native-voice")]
    #[test]
    fn kv_delta_and_fp16_logits() {
        let _env = crate::onnx_env::get_ort_env().unwrap();
        let mut c = zeros(TensorElementType::Float32, vec![1, 4, 2, 1]).unwrap();
        let delta = Tensor::from_array(([1, 2, 2, 1], vec![1.0f32, 2.0, 3.0, 4.0]))
            .unwrap()
            .into_dyn();
        apply_delta(&mut c, &delta, 1, 2).unwrap();
        assert_eq!(
            c.try_extract_tensor::<f32>().unwrap().1,
            &[0., 0., 1., 2., 3., 4., 0., 0.]
        );
        assert!(apply_delta(&mut c, &delta, 3, 2).is_err());
        let mut logits = zeros(TensorElementType::Float16, vec![1, 1, 3])
            .unwrap()
            .downcast::<DynTensorValueType>()
            .unwrap();
        unsafe {
            std::ptr::copy_nonoverlapping(
                [0x3c00u16, 0x4000, 0xbc00].as_ptr(),
                logits.data_ptr_mut().cast::<u16>(),
                3,
            );
        }
        assert_eq!(greedy(&logits.into_dyn(), &[], 1.0).unwrap(), 1);
    }
    #[test]
    fn half_values() {
        assert_eq!(half_to_float(0x3c00), 1.0);
        assert_eq!(half_to_float(0xc000), -2.0);
        assert!(half_to_float(0x7c00).is_infinite());
        assert_eq!(half_to_float(1), 2f32.powi(-24));
    }
}
