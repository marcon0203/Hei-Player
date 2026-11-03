//! decode —— 把音频文件整段解码为 `AudioData`(交错 f32)。
//!
//! P0 策略:symphonia 纯 Rust 解码,主流量式格式(mp3/aac/flac/ogg/wav/m4a)全覆盖;
//! 解码是启动/导入时的一次性成本,不占实时路径。
//! 冷门格式兜底(ffmpeg 子进程转 wav)挂在 P1。

use std::fs::File;
use std::path::Path;
use std::sync::Arc;

use anyhow::{anyhow, bail, Context};
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{DecoderOptions, CODEC_TYPE_NULL};
use symphonia::core::errors::Error as SymError;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

use playback::AudioData;

/// 解码整个文件到内存。
pub fn load(path: &Path) -> anyhow::Result<Arc<AudioData>> {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("audio")
        .to_owned();

    let file = File::open(path).with_context(|| format!("打开失败: {}", path.display()))?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());

    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        // 返回值类型随版本可能是 () 或 Result,用 let _ 兼容
        let _ = hint.with_extension(&ext.to_ascii_lowercase());
    }

    let probed = symphonia::default::get_probe()
        .format(&hint, mss, &FormatOptions::default(), &MetadataOptions::default())
        .map_err(|e| anyhow!("探测容器失败({name}): {e}"))?;
    let mut format = probed.format;

    let track = format
        .tracks()
        .iter()
        .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
        .cloned()
        .ok_or_else(|| anyhow!("容器里没有可解码的音频轨({name})"))?;
    let track_id = track.id;

    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .map_err(|e| anyhow!("创建解码器失败({name}): {e}"))?;

    let mut samples: Vec<f32> = Vec::new();
    let mut reset_count = 0; // 防御:个别损坏文件可能无限 ResetRequired
    let mut sample_rate: Option<u32> = track.codec_params.sample_rate;
    let mut channels = track
        .codec_params
        .channels
        .map(|ch| ch.count())
        .unwrap_or(2);

    loop {
        let packet = match format.next_packet() {
            Ok(p) => p,
            Err(SymError::IoError(ref io)) if io.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(SymError::ResetRequired) => {
                reset_count += 1;
                if reset_count > 16 {
                    bail!("解码器反复重置,文件可能损坏({name})");
                }
                continue;
            }
            Err(e) => bail!("读包失败({name}): {e}"),
        };
        if packet.track_id() != track_id {
            continue;
        }

        match decoder.decode(&packet) {
            Ok(decoded) => {
                let spec = *decoded.spec();
                sample_rate = Some(spec.rate);
                channels = spec.channels.count();
                // 每包新建缓冲:P0 简化;导入路径可接受,后续可复用
                let mut sbuf = SampleBuffer::<f32>::new(decoded.capacity() as u64, spec);
                sbuf.copy_interleaved_ref(decoded);
                samples.extend_from_slice(sbuf.samples());
            }
            // 损坏帧跳过,保证"能播多少是多少"的现场容错语义
            Err(SymError::DecodeError(_)) | Err(SymError::ResetRequired) => continue,
            Err(e) => bail!("解码失败({name}): {e}"),
        }
    }

    if samples.is_empty() {
        bail!("没有解出任何采样({name})");
    }

    let rate = sample_rate.ok_or_else(|| anyhow!("未知采样率({name})"))?;
    Ok(Arc::new(AudioData::new(samples, rate, channels.max(1))))
}


/// 把交错 f32 写成 16-bit PCM WAV(导出素材 / 录音保存)。
pub fn write_wav(path: &Path, data: &AudioData) -> anyhow::Result<()> {
    let frames = data.total_frames();
    let ch = data.channels.max(1);
    let data_len = (frames * ch * 2) as u32;
    let mut wav: Vec<u8> = Vec::with_capacity(44 + data_len as usize);
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data_len).to_le_bytes());
    wav.extend_from_slice(b"WAVE");
    wav.extend_from_slice(b"fmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
    wav.extend_from_slice(&(ch as u16).to_le_bytes());
    wav.extend_from_slice(&data.sample_rate.to_le_bytes());
    wav.extend_from_slice(&(data.sample_rate * ch as u32 * 2).to_le_bytes());
    wav.extend_from_slice(&((ch * 2) as u16).to_le_bytes());
    wav.extend_from_slice(&16u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_len.to_le_bytes());
    for s in data.samples.iter() {
        let v = (s.clamp(-1.0, 1.0) * 32767.0) as i16;
        wav.extend_from_slice(&v.to_le_bytes());
    }
    std::fs::write(path, wav)
        .with_context(|| format!("写出 WAV 失败: {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 手工构造最小 PCM16 WAV(44 字节头),验证 symphonia 全链路。
    fn write_test_wav(path: &Path, frames: usize) {
        let rate: u32 = 8000;
        let data_len = (frames * 2) as u32;
        let mut wav: Vec<u8> = Vec::with_capacity(44 + data_len as usize);
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36 + data_len).to_le_bytes());
        wav.extend_from_slice(b"WAVE");
        wav.extend_from_slice(b"fmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
        wav.extend_from_slice(&1u16.to_le_bytes()); // mono
        wav.extend_from_slice(&rate.to_le_bytes());
        wav.extend_from_slice(&(rate * 2).to_le_bytes()); // byte rate
        wav.extend_from_slice(&2u16.to_le_bytes()); // block align
        wav.extend_from_slice(&16u16.to_le_bytes()); // bits
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&data_len.to_le_bytes());
        for i in 0..frames {
            let v = ((i % 100) as i16 - 50) * 100;
            wav.extend_from_slice(&v.to_le_bytes());
        }
        std::fs::write(path, wav).unwrap();
    }

    #[test]
    fn decodes_wav_end_to_end() {
        let path = std::env::temp_dir().join("aura_decode_test.wav");
        write_test_wav(&path, 300);

        let audio = load(&path).unwrap();
        assert_eq!(audio.sample_rate, 8000);
        assert_eq!(audio.channels, 1);
        assert_eq!(audio.total_frames(), 300);
        assert!((audio.duration_secs() - 300.0 / 8000.0).abs() < 1e-9);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn wav_write_read_roundtrip() {
        let path = std::env::temp_dir().join("aura_wav_roundtrip.wav");
        // 左右声道不同幅度,验证交错顺序
        let samples: Vec<f32> = (0..100).flat_map(|i| [i as f32 / 100.0, 0.5]).collect();
        let data = playback::AudioData::new(samples.clone(), 8000, 2);
        write_wav(&path, &data).unwrap();

        let back = load(&path).unwrap();
        assert_eq!(back.sample_rate, 8000);
        assert_eq!(back.channels, 2);
        assert_eq!(back.total_frames(), 100);
        for (a, b) in back.samples.iter().zip(samples.iter()) {
            assert!((a - b).abs() < 0.001, "{a} vs {b}");
        }
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn rejects_garbage() {
        let path = std::env::temp_dir().join("aura_decode_garbage.bin");
        std::fs::write(&path, b"definitely not audio").unwrap();
        assert!(load(&path).is_err());
        let _ = std::fs::remove_file(&path);
    }
}
