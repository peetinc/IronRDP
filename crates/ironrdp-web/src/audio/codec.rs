//! RDPSND wave decoding for the browser player.
//!
//! Pure Rust and free of `web-sys`, so it is unit-testable on the host. Every
//! format [`supported_formats`] advertises must be decodable by [`decode`]:
//! `Rdpsnd` negotiates only from that list (filtered to what the server offers),
//! so it is the single place a new codec gets enabled.

use ironrdp::rdpsnd::pdu::{AudioFormat, WaveFormat};

/// Decoded audio, planar (one `Vec` per channel, equal lengths), the shape
/// `AudioBuffer::copy_to_channel` takes.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct DecodedAudio {
    pub(crate) sample_rate: u32,
    pub(crate) channels: Vec<Vec<f32>>,
}

impl DecodedAudio {
    pub(crate) fn frames(&self) -> usize {
        self.channels.first().map_or(0, Vec::len)
    }
}

/// Server-set playback volume (from the MS-RDPEA Volume PDU, [`VolumePdu`]); `0xFFFF` is full scale.
///
/// [`VolumePdu`]: ironrdp::rdpsnd::pdu::VolumePdu
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Volume {
    pub(crate) left: u16,
    pub(crate) right: u16,
}

impl Volume {
    pub(crate) const FULL: Self = Self {
        left: 0xFFFF,
        right: 0xFFFF,
    };
}

/// Formats advertised to the server, best first.
///
/// Phase 1 is uncompressed 16-bit PCM only (the same rates the native backend offers).
/// Web Audio resamples any `AudioBuffer` rate to the device, so no device filtering is needed.
pub(crate) fn supported_formats() -> Vec<AudioFormat> {
    const PCM16: &[(u16, u32)] = &[
        (2, 48000),
        (2, 44100),
        (2, 22050),
        (2, 16000),
        (1, 44100),
        (1, 22050),
        (1, 16000),
        (1, 8000),
    ];

    PCM16
        .iter()
        .map(|&(n_channels, n_samples_per_sec)| {
            let n_block_align = n_channels * 2 /* bytes per 16-bit sample */;
            AudioFormat {
                format: WaveFormat::PCM,
                n_channels,
                n_samples_per_sec,
                n_avg_bytes_per_sec: n_samples_per_sec * u32::from(n_block_align),
                n_block_align,
                bits_per_sample: 16,
                data: None,
            }
        })
        .collect()
}

/// Decode one wave block, applying `volume`.
///
/// Returns `None` for formats this player does not handle, and for blocks too
/// short to hold a single frame. A trailing partial frame is dropped.
pub(crate) fn decode(format: &AudioFormat, data: &[u8], volume: Volume) -> Option<DecodedAudio> {
    match (format.format, format.bits_per_sample) {
        (WaveFormat::PCM, 16) if (1..=2).contains(&format.n_channels) => {
            let n_channels = usize::from(format.n_channels);
            let frame_len = n_channels * 2 /* bytes per 16-bit sample */;
            let frames = data.len() / frame_len;
            if frames == 0 {
                return None;
            }

            // Same lane mapping as the native backend: left on even lanes, right on odd;
            // mono takes the left level. Linear amplitude scale.
            let gains = [volume.left, volume.right].map(|level| f32::from(level) / f32::from(u16::MAX));

            let mut channels: Vec<Vec<f32>> = core::iter::repeat_with(|| Vec::with_capacity(frames))
                .take(n_channels)
                .collect();
            for frame in data.chunks_exact(frame_len) {
                for ((sample, out), gain) in frame.chunks_exact(2).zip(channels.iter_mut()).zip(gains) {
                    let sample = i16::from_le_bytes([sample[0], sample[1]]);
                    out.push(f32::from(sample) / 32768.0 * gain);
                }
            }

            Some(DecodedAudio {
                sample_rate: format.n_samples_per_sec,
                channels,
            })
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pcm(n_channels: u16) -> AudioFormat {
        supported_formats()
            .into_iter()
            .find(|format| format.n_channels == n_channels)
            .expect("table has this channel count")
    }

    fn le(samples: &[i16]) -> Vec<u8> {
        samples.iter().flat_map(|sample| sample.to_le_bytes()).collect()
    }

    #[test]
    fn stereo_is_split_into_planar_channels() {
        let audio = decode(&pcm(2), &le(&[0, i16::MAX, i16::MIN, 16384]), Volume::FULL).expect("decodes");
        assert_eq!(audio.sample_rate, 48000);
        assert_eq!(audio.channels.len(), 2);
        assert_eq!(audio.channels[0], vec![0.0, -1.0]);
        assert_eq!(audio.channels[1], vec![f32::from(i16::MAX) / 32768.0, 0.5]);
        assert_eq!(audio.frames(), 2);
    }

    #[test]
    fn mono_uses_left_volume() {
        let volume = Volume { left: 0, right: 0xFFFF };
        let audio = decode(&pcm(1), &le(&[16384, -16384]), volume).expect("decodes");
        assert_eq!(audio.channels, vec![vec![0.0, 0.0]]);
    }

    #[test]
    fn volume_scales_each_lane() {
        let volume = Volume { left: 0xFFFF, right: 0 };
        let audio = decode(&pcm(2), &le(&[16384, 16384]), volume).expect("decodes");
        assert_eq!(audio.channels[0], vec![0.5]);
        assert_eq!(audio.channels[1], vec![0.0]);
    }

    #[test]
    fn trailing_partial_frame_is_dropped() {
        let mut data = le(&[100, 200, 300]);
        data.push(0x7F);
        let audio = decode(&pcm(2), &data, Volume::FULL).expect("decodes");
        assert_eq!(audio.frames(), 1);
    }

    #[test]
    fn short_or_empty_block_yields_nothing() {
        assert_eq!(decode(&pcm(2), &[], Volume::FULL), None);
        assert_eq!(decode(&pcm(2), &[1, 2, 3], Volume::FULL), None);
    }

    #[test]
    fn unsupported_formats_yield_nothing() {
        let data = le(&[1, 2, 3, 4]);

        let mut eight_bit = pcm(2);
        eight_bit.bits_per_sample = 8;
        assert_eq!(decode(&eight_bit, &data, Volume::FULL), None);

        let mut six_channel = pcm(2);
        six_channel.n_channels = 6;
        assert_eq!(decode(&six_channel, &data, Volume::FULL), None);

        let mut adpcm = pcm(2);
        adpcm.format = WaveFormat::ADPCM;
        assert_eq!(decode(&adpcm, &data, Volume::FULL), None);
    }

    #[test]
    fn advertised_formats_are_consistent_pcm16() {
        let formats = supported_formats();
        assert!(!formats.is_empty());
        for format in formats {
            assert_eq!(format.format, WaveFormat::PCM);
            assert_eq!(format.bits_per_sample, 16);
            assert!((1..=2).contains(&format.n_channels));
            assert_eq!(format.n_block_align, format.n_channels * 2);
            assert_eq!(
                format.n_avg_bytes_per_sec,
                format.n_samples_per_sec * u32::from(format.n_block_align)
            );
            assert_eq!(format.data, None);
        }
    }
}
