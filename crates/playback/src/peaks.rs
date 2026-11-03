//! 波形峰值计算:把整段音频降采样为固定桶数的 min/max,供前端 canvas 绘制。
//! 导入时算一次,磁盘缓存;rayon 并行。

use rayon::prelude::*;

use crate::types::AudioData;

/// 每个像素桶的最小/最大值(归一化 -1..1)。
#[derive(Clone, Copy, serde::Serialize)]
pub struct PeakPair {
    pub min: f32,
    pub max: f32,
}

/// 计算 `buckets` 个桶的峰值。桶数任意;每桶取该区间所有声道采样的 min/max。
pub fn compute_peaks(data: &AudioData, buckets: usize) -> Vec<PeakPair> {
    let buckets = buckets.max(1);
    let total = data.samples.len();
    if total == 0 {
        return vec![PeakPair { min: 0.0, max: 0.0 }; buckets];
    }
    let chunk = total.div_ceil(buckets);
    (0..buckets)
        .into_par_iter()
        .map(|b| {
            let lo = b * chunk;
            let hi = ((b + 1) * chunk).min(total);
            if lo >= hi {
                return PeakPair { min: 0.0, max: 0.0 };
            }
            let mut mn = f32::MAX;
            let mut mx = f32::MIN;
            for s in &data.samples[lo..hi] {
                if *s < mn {
                    mn = *s;
                }
                if *s > mx {
                    mx = *s;
                }
            }
            PeakPair { min: mn, max: mx }
        })
        .collect()
}

/// 峰值缓存的磁盘读写(简单二进制:u32 桶数 + f32×2×桶)。
pub fn encode_peaks(peaks: &[PeakPair]) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + peaks.len() * 8);
    out.extend_from_slice(&(peaks.len() as u32).to_le_bytes());
    for p in peaks {
        out.extend_from_slice(&p.min.to_le_bytes());
        out.extend_from_slice(&p.max.to_le_bytes());
    }
    out
}

pub fn decode_peaks(bytes: &[u8]) -> Option<Vec<PeakPair>> {
    if bytes.len() < 4 {
        return None;
    }
    let n = u32::from_le_bytes(bytes[0..4].try_into().ok()?) as usize;
    if bytes.len() < 4 + n * 8 {
        return None;
    }
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let off = 4 + i * 8;
        let min = f32::from_le_bytes(bytes[off..off + 4].try_into().ok()?);
        let max = f32::from_le_bytes(bytes[off + 4..off + 8].try_into().ok()?);
        out.push(PeakPair { min, max });
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::AudioData;

    #[test]
    fn peaks_shape_and_values() {
        // 1000 帧方波:前半 +0.5,后半 -0.5
        let samples: Vec<f32> = (0..1000)
            .map(|i| if i < 500 { 0.5 } else { -0.5 })
            .collect();
        let data = AudioData::new(samples, 1000, 1);
        let peaks = compute_peaks(&data, 10);
        assert_eq!(peaks.len(), 10);
        assert!((peaks[0].max - 0.5).abs() < 1e-6);
        assert!((peaks[9].min + 0.5).abs() < 1e-6);
    }

    #[test]
    fn peaks_roundtrip() {
        let peaks = vec![PeakPair { min: -0.25, max: 0.75 }, PeakPair { min: 0.0, max: 0.1 }];
        let enc = encode_peaks(&peaks);
        let dec = decode_peaks(&enc).unwrap();
        assert_eq!(dec.len(), 2);
        assert!((dec[0].min - (-0.25)).abs() < 1e-7);
        assert!((dec[1].max - 0.1).abs() < 1e-7);
        assert!(decode_peaks(&[1, 2, 3]).is_none());
    }
}
