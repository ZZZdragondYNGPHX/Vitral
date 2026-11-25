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
    kv_byte_len(dtype, &shape)?;
    DynTensor::new(&Allocator::default(), dtype, shape)
        .map(|t| t.into_dyn())
        .map_err(error)
}

fn kv_byte_len(dtype: TensorElementType, shape: &[i64]) -> Result<usize, VitralError> {
    let width = match dtype {
        TensorElementType::Float32 => 4usize,
        TensorElementType::Float16 | TensorElementType::Uint16 => 2,
        _ => return Err(error("unsupported KV type")),
    };
    shape
        .iter()
        .try_fold(width, |bytes, &dim| {
            bytes.checked_mul(usize::try_from(dim).ok()?)
        })
        .filter(|&bytes| bytes <= isize::MAX as usize)
        .ok_or_else(|| error("invalid KV shape or byte length"))
}

#[cfg(feature = "native-onnx")]
pub(super) struct KvSnapshot {
    dtype: TensorElementType,
    shape: ort::value::Shape,
    bytes: Vec<u8>,
}

#[cfg(feature = "native-onnx")]
fn cpu_kv_tensor(value: &DynValue) -> Result<(ort::value::DynTensorRef<'_>, usize), VitralError> {
    let tensor = value
        .view()
        .downcast::<DynTensorValueType>()
        .map_err(error)?;
    if !tensor.memory_info().is_cpu_accessible() {
        return Err(error("KV tensors must be CPU-accessible"));
    }
    let len = kv_byte_len(*tensor.data_type(), tensor.shape())?;
    Ok((tensor, len))
}

#[cfg(feature = "native-onnx")]
pub(super) fn kv_tensor_byte_len(value: &DynValue) -> Result<usize, VitralError> {
    cpu_kv_tensor(value).map(|(_, len)| len)
}

#[cfg(feature = "native-onnx")]
pub(super) fn snapshot_kv_tensor(value: &DynValue) -> Result<KvSnapshot, VitralError> {
    let (tensor, len) = cpu_kv_tensor(value)?;
    let mut bytes = Vec::new();
    if len > 0 {
        let src = tensor.data_ptr().cast::<u8>();
        if src.is_null() {
            return Err(error("missing KV tensor data"));
        }
        bytes.try_reserve_exact(len).map_err(error)?;
        // ONNX owns the checked CPU tensor. The reserved byte buffer is disjoint.
        unsafe {
            std::ptr::copy_nonoverlapping(src, bytes.as_mut_ptr(), len);
            bytes.set_len(len);
        }
    }
    Ok(KvSnapshot {
        dtype: *tensor.data_type(),
        shape: tensor.shape().clone(),
        bytes,
    })
}

#[cfg(feature = "native-onnx")]
pub(super) fn restore_kv_tensor(snapshot: &KvSnapshot) -> Result<DynValue, VitralError> {
    let len = kv_byte_len(snapshot.dtype, &snapshot.shape)?;
    if len != snapshot.bytes.len() {
        return Err(error("invalid KV snapshot byte length"));
    }
    let mut tensor = DynTensor::new(
        &Allocator::default(),
        snapshot.dtype,
        snapshot.shape.clone(),
    )
    .map_err(error)?;
    if !tensor.memory_info().is_cpu_accessible() {
        return Err(error("KV tensors must be CPU-accessible"));
    }
    if len > 0 {
        let dst = tensor.data_ptr_mut().cast::<u8>();
        if dst.is_null() {
            return Err(error("missing KV tensor data"));
        }
        // The new tensor owns an independent CPU allocation of the checked length.
        unsafe { std::ptr::copy_nonoverlapping(snapshot.bytes.as_ptr(), dst, len) };
    }
    Ok(tensor.into_dyn())
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

fn greedy_row(scores: impl Iterator<Item = f32>, mut repeated: &[usize], penalty: f32) -> Option<i64> {
    scores
        .enumerate()
        .filter_map(|(id, mut score)| {
            if repeated.first() == Some(&id) {
                while repeated.first() == Some(&id) {
                    repeated = &repeated[1..];
                }
                score = if score < 0.0 { score * penalty } else { score / penalty };
            }
            score.is_finite().then_some((id, score))
        })
        .max_by(|(ai, a), (bi, b)| a.total_cmp(b).then_with(|| bi.cmp(ai)))
        .map(|(id, _)| id as i64)
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
    // Sort only the small repetition window to scan the vocabulary in one pass.
    let mut repeated = [0; 64];
    let mut count = 0;
    for id in previous.iter().rev().take(64).filter_map(|&id| usize::try_from(id).ok()) {
        if id < vocab {
            repeated[count] = id;
            count += 1;
        }
    }
    let repeated = &mut repeated[..count];
    repeated.sort_unstable();
    let best = match t.data_type() {
        TensorElementType::Float32 => {
            let (_, logits) = value.try_extract_tensor::<f32>().map_err(error)?;
            greedy_row(logits[offset..offset + vocab].iter().copied(), repeated, penalty)
        }
        TensorElementType::Float16 | TensorElementType::Uint16 => {
            // The shape bounds the slice; the session output is CPU-accessible.
            let row = unsafe {
                std::slice::from_raw_parts(t.data_ptr().cast::<u16>().add(offset), vocab)
            };
            greedy_row(row.iter().copied().map(half_to_float), repeated, penalty)
        }
        _ => return Err(error("unsupported logits type")),
    };
    best.ok_or_else(|| error("no finite logits"))
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

    #[cfg(feature = "native-onnx")]
    #[test]
    fn kv_snapshots_preserve_bytes_in_independent_cpu_tensors() {
        let _env = crate::onnx_env::get_ort_env().unwrap();
        for (dtype, bytes) in [
            (
                TensorElementType::Float32,
                [0x8000_0000_u32, 0x7fc0_1234, 0x3f80_0000]
                    .into_iter()
                    .flat_map(u32::to_ne_bytes)
                    .collect::<Vec<_>>(),
            ),
            (
                TensorElementType::Float16,
                [0x8000_u16, 0x7e12, 0x3c00]
                    .into_iter()
                    .flat_map(u16::to_ne_bytes)
                    .collect(),
            ),
            (
                TensorElementType::Uint16,
                [0x8001_u16, 0xffab, 0x3c00]
                    .into_iter()
                    .flat_map(u16::to_ne_bytes)
                    .collect(),
            ),
        ] {
            let mut source = zeros(dtype, vec![1, 1, 3, 1])
                .unwrap()
                .downcast::<DynTensorValueType>()
                .unwrap();
            unsafe {
                std::ptr::copy_nonoverlapping(
                    bytes.as_ptr(),
                    source.data_ptr_mut().cast(),
                    bytes.len(),
                );
            }
            let source = source.into_dyn();
            let snapshot = snapshot_kv_tensor(&source).unwrap();
            assert_eq!(snapshot.bytes, bytes);

            let restored = restore_kv_tensor(&snapshot).unwrap();
            let second = restore_kv_tensor(&snapshot).unwrap();
            let restored_snapshot = snapshot_kv_tensor(&restored).unwrap();
            assert_eq!(restored_snapshot.dtype, dtype);
            assert_eq!(&*restored_snapshot.shape, &[1, 1, 3, 1]);
            assert_eq!(restored_snapshot.bytes, bytes);
            let mut restored = restored
                .downcast::<DynTensorValueType>()
                .unwrap();
            unsafe {
                restored
                    .data_ptr_mut()
                    .cast::<u8>()
                    .write_bytes(0, bytes.len())
            };
            assert_eq!(snapshot_kv_tensor(&source).unwrap().bytes, bytes);
            assert_eq!(snapshot_kv_tensor(&second).unwrap().bytes, bytes);
            assert_eq!(snapshot.bytes, bytes);
        }
    }

    #[cfg(feature = "native-onnx")]
    #[test]
    fn kv_snapshot_checks_type_shape_and_length() {
        let _env = crate::onnx_env::get_ort_env().unwrap();
        assert!(snapshot_kv_tensor(&ids([1i64], vec![1]).unwrap()).is_err());
        assert!(kv_byte_len(TensorElementType::Float32, &[0, -1]).is_err());
        assert!(kv_byte_len(TensorElementType::Float32, &[i64::MAX, 2]).is_err());
        let empty = zeros(TensorElementType::Float16, vec![1, 1, 0, 2]).unwrap();
        let mut snapshot = snapshot_kv_tensor(&empty).unwrap();
        assert_eq!(
            kv_tensor_byte_len(&restore_kv_tensor(&snapshot).unwrap()).unwrap(),
            0
        );
        snapshot.bytes.push(0);
        assert!(restore_kv_tensor(&snapshot).is_err());
    }

    #[test]
    fn greedy_fp32_and_fp16_penalties_and_ties_match() {
        let _env = crate::onnx_env::get_ort_env().unwrap();
        for (row, half, expected) in [
            // Repeating token 0 twice still applies a single penalty, giving it a tie with 1.
            ([4.0, 2.0, 1.0], [0x4400, 0x4000, 0x3c00], 0),
            ([-1.0, -1.5, -2.0], [0xbc00, 0xbe00, 0xc000], 1),
            ([f32::INFINITY, f32::NAN, -2.0], [0x7c00, 0x7e00, 0xc000], 2),
        ] {
            let f32_logits = Tensor::from_array((
                [1, 2, 3],
                vec![100.0, 100.0, 100.0, row[0], row[1], row[2]],
            )).unwrap().into_dyn();
            let mut f16 = zeros(TensorElementType::Float16, vec![1, 2, 3])
                .unwrap()
                .downcast::<DynTensorValueType>()
                .unwrap();
            // Only the last row should determine the result.
            let raw = [0x5640u16, 0x5640, 0x5640, half[0], half[1], half[2]];
            unsafe {
                std::ptr::copy_nonoverlapping(raw.as_ptr(), f16.data_ptr_mut().cast::<u16>(), raw.len());
            }
            let f16_logits = f16.into_dyn();
            for logits in [&f32_logits, &f16_logits] {
                let mut previous = [-1; 65];
                previous[0] = 1; // Outside the 64-token window.
                previous[62] = i64::MAX;
                previous[63..].fill(0);
                assert_eq!(greedy(logits, &previous, 2.0).unwrap(), expected);
            }
        }
    }

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
