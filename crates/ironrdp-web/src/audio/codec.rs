//! RDPSND format negotiation and synchronous wave decoding for the browser player.
//!
//! Pure Rust and free of `web-sys`, so it is unit-testable on the host. [`classify`]
//! is the single place that decides what the player can decode: [`negotiate`] offers
//! the server exactly those of its own formats, and [`decode`] handles the synchronous
//! codecs among them. AAC decodes asynchronously through WebCodecs (`super::aac`).

use ironrdp::rdpsnd::pdu::{AudioFormat, WaveFormat};

use super::{aac, adpcm};

/// Decoded audio, planar (one `Vec` per channel, equal lengths), the shape
/// `AudioBuffer::copy_to_channel` takes.
///
/// Reused across blocks (see [`decode`]) so the audio hot path does not allocate.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct DecodedAudio {
    pub(crate) sample_rate: u32,
    pub(crate) channels: Vec<Vec<f32>>,
}

impl DecodedAudio {
    pub(crate) fn frames(&self) -> usize {
        self.channels.first().map_or(0, Vec::len)
    }

    /// Clear for a block of `frames` frames, keeping the allocations.
    pub(crate) fn begin(&mut self, sample_rate: u32, n_channels: usize, frames: usize) {
        self.sample_rate = sample_rate;
        self.channels.resize_with(n_channels, Vec::new);
        for channel in &mut self.channels {
            channel.clear();
            channel.reserve(frames);
        }
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

    /// Linear gain per output channel. Same lane mapping as the native backend: left on
    /// channel 0, right on channel 1; mono takes the left level.
    pub(crate) fn gains(self) -> [f32; 2] {
        [self.left, self.right].map(|level| f32::from(level) / f32::from(u16::MAX))
    }
}

/// Writes 16-bit samples into a [`DecodedAudio`] as volume-scaled f32.
pub(crate) struct Sink<'a> {
    channels: &'a mut [Vec<f32>],
    gains: [f32; 2],
}

impl Sink<'_> {
    pub(crate) fn push(&mut self, channel: usize, sample: i16) {
        self.channels[channel].push(f32::from(sample) / 32768.0 * self.gains[channel]);
    }
}

/// How the player decodes a format, with the parameters it needs.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Codec {
    /// 16-bit little-endian interleaved PCM.
    Pcm16,
    /// IMA (DVI) ADPCM, `WAVE_FORMAT_DVI_ADPCM`.
    ImaAdpcm,
    /// Microsoft ADPCM, with the format's predictor coefficients.
    MsAdpcm(Vec<(i16, i16)>),
    /// AAC through WebCodecs, `WAVE_FORMAT_AAC_MS`.
    Aac,
}

impl Codec {
    /// Negotiation preference, lowest first: smallest on the wire first.
    fn rank(&self) -> u8 {
        match self {
            Self::Aac => 0,
            Self::ImaAdpcm => 1,
            Self::MsAdpcm(_) => 2,
            Self::Pcm16 => 3,
        }
    }
}

/// The codec decoding `format`, or `None` if the player cannot play it.
///
/// `aac` says whether this browser's WebCodecs can decode AAC.
pub(crate) fn classify(format: &AudioFormat, aac: bool) -> Option<Codec> {
    if !(1..=2).contains(&format.n_channels) || format.n_samples_per_sec == 0 {
        return None;
    }
    let n_channels = usize::from(format.n_channels);
    let block_align = usize::from(format.n_block_align);

    match (format.format, format.bits_per_sample) {
        (WaveFormat::PCM, 16) => Some(Codec::Pcm16),
        // Body bytes come in 4-byte groups per channel, after the per-channel headers.
        (WaveFormat::DVI_ADPCM, 4)
            if adpcm::IMA_HEADER_LEN * n_channels < block_align && block_align % (4 * n_channels) == 0 =>
        {
            Some(Codec::ImaAdpcm)
        }
        (WaveFormat::ADPCM, 4) if adpcm::MS_HEADER_LEN * n_channels < block_align => {
            ms_coefs(format.data.as_deref()).map(Codec::MsAdpcm)
        }
        (WaveFormat::AAC_MS, _) if aac && aac::sampling_frequency_index(format.n_samples_per_sec).is_some() => {
            Some(Codec::Aac)
        }
        _ => None,
    }
}

/// MS ADPCM predictor coefficients from `ADPCMWAVEFORMAT` extra data (`wSamplesPerBlock`,
/// `wNumCoef`, then `wNumCoef` pairs), or the standard seven when there is none.
fn ms_coefs(data: Option<&[u8]>) -> Option<Vec<(i16, i16)>> {
    let Some(data) = data.filter(|data| !data.is_empty()) else {
        return Some(adpcm::MS_STANDARD_COEFS.to_vec());
    };

    let count = usize::from(u16::from_le_bytes([*data.get(2)?, *data.get(3)?]));
    let pairs = data.get(4..)?.get(..count * 4)?;
    let coefs: Vec<(i16, i16)> = pairs
        .chunks_exact(4)
        .map(|pair| {
            (
                i16::from_le_bytes([pair[0], pair[1]]),
                i16::from_le_bytes([pair[2], pair[3]]),
            )
        })
        .collect();

    (!coefs.is_empty()).then_some(coefs)
}

/// The server formats to advertise back, best first: those [`classify`] accepts, by
/// codec rank, then higher sample rate, then more channels.
///
/// The entries are the server's own, so compressed formats keep the exact bitrate,
/// block layout and extra data the server will encode with.
pub(crate) fn negotiate(server_formats: &[AudioFormat], aac: bool) -> Vec<AudioFormat> {
    let mut accepted: Vec<(u8, &AudioFormat)> = server_formats
        .iter()
        .filter_map(|format| classify(format, aac).map(|codec| (codec.rank(), format)))
        .collect();

    accepted.sort_by(|(rank_a, a), (rank_b, b)| {
        rank_a
            .cmp(rank_b)
            .then(b.n_samples_per_sec.cmp(&a.n_samples_per_sec))
            .then(b.n_channels.cmp(&a.n_channels))
    });

    let mut formats: Vec<AudioFormat> = Vec::with_capacity(accepted.len());
    for (_, format) in accepted {
        // A server listing the same format twice would only waste a wFormatNo.
        if !formats.contains(format) {
            formats.push(format.clone());
        }
    }
    formats
}

/// Decode one wave block of a synchronous codec into `out`, applying `volume`, reusing
/// `out`'s allocations.
///
/// Returns `false` for formats this decodes no frames of (AAC, anything [`classify`]
/// rejects, blocks shorter than one frame or one ADPCM block); `out` is then unspecified.
/// A trailing partial frame or block is dropped.
pub(crate) fn decode(format: &AudioFormat, data: &[u8], volume: Volume, out: &mut DecodedAudio) -> bool {
    let Some(codec) = classify(format, false) else {
        return false;
    };
    let n_channels = usize::from(format.n_channels);
    let block_align = usize::from(format.n_block_align);
    let pcm_frame_len = n_channels * 2 /* bytes per 16-bit sample */;

    let frames = match codec {
        Codec::Pcm16 => data.len() / pcm_frame_len,
        Codec::ImaAdpcm => data.len() / block_align * adpcm::ima_frames_per_block(n_channels, block_align),
        Codec::MsAdpcm(_) => data.len() / block_align * adpcm::ms_frames_per_block(n_channels, block_align),
        Codec::Aac => 0,
    };
    if frames == 0 {
        return false;
    }

    out.begin(format.n_samples_per_sec, n_channels, frames);
    let mut sink = Sink {
        channels: &mut out.channels,
        gains: volume.gains(),
    };

    match codec {
        Codec::Pcm16 => {
            for frame in data.chunks_exact(pcm_frame_len) {
                for (channel, sample) in frame.chunks_exact(2).enumerate() {
                    sink.push(channel, i16::from_le_bytes([sample[0], sample[1]]));
                }
            }
        }
        Codec::ImaAdpcm => adpcm::decode_ima(n_channels, block_align, data, &mut sink),
        Codec::MsAdpcm(coefs) => adpcm::decode_ms(n_channels, block_align, &coefs, data, &mut sink),
        Codec::Aac => return false,
    }

    // An MS ADPCM block with an invalid predictor is skipped, so the count can come up short.
    0 < out.frames()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pcm(n_channels: u16, n_samples_per_sec: u32) -> AudioFormat {
        let n_block_align = n_channels * 2;
        AudioFormat {
            format: WaveFormat::PCM,
            n_channels,
            n_samples_per_sec,
            n_avg_bytes_per_sec: n_samples_per_sec * u32::from(n_block_align),
            n_block_align,
            bits_per_sample: 16,
            data: None,
        }
    }

    /// Fixture formats, as FFmpeg wrote them into the source WAV files (22050 Hz, 1024-byte
    /// blocks). The fixtures are a 440 Hz / 660 Hz sine pair encoded by `ffmpeg -c:a
    /// adpcm_ima_wav` / `adpcm_ms`; each `.pcm` is FFmpeg's own decode of that `.adpcm`.
    fn ima(n_channels: u16, samples_per_block: u16) -> AudioFormat {
        AudioFormat {
            format: WaveFormat::DVI_ADPCM,
            n_channels,
            n_samples_per_sec: 22050,
            n_avg_bytes_per_sec: 11062,
            n_block_align: 1024,
            bits_per_sample: 4,
            data: Some(samples_per_block.to_le_bytes().to_vec()),
        }
    }

    fn ms(n_channels: u16, samples_per_block: u16) -> AudioFormat {
        let mut data = samples_per_block.to_le_bytes().to_vec();
        data.extend_from_slice(&7u16.to_le_bytes());
        for (coef1, coef2) in adpcm::MS_STANDARD_COEFS {
            data.extend_from_slice(&coef1.to_le_bytes());
            data.extend_from_slice(&coef2.to_le_bytes());
        }
        AudioFormat {
            format: WaveFormat::ADPCM,
            n_channels,
            n_samples_per_sec: 22050,
            n_avg_bytes_per_sec: 11089,
            n_block_align: 1024,
            bits_per_sample: 4,
            data: Some(data),
        }
    }

    fn aac(n_samples_per_sec: u32) -> AudioFormat {
        AudioFormat {
            format: WaveFormat::AAC_MS,
            n_channels: 2,
            n_samples_per_sec,
            n_avg_bytes_per_sec: 24000,
            n_block_align: 4,
            bits_per_sample: 16,
            data: None,
        }
    }

    fn decode_fresh(format: &AudioFormat, data: &[u8], volume: Volume) -> Option<DecodedAudio> {
        let mut out = DecodedAudio::default();
        decode(format, data, volume, &mut out).then_some(out)
    }

    fn le(samples: &[i16]) -> Vec<u8> {
        samples.iter().flat_map(|sample| sample.to_le_bytes()).collect()
    }

    /// Asserts `audio` holds exactly the interleaved 16-bit `expected` samples.
    fn assert_samples(audio: &DecodedAudio, expected: &[u8]) {
        let n_channels = audio.channels.len();
        let expected: Vec<i16> = expected
            .chunks_exact(2)
            .map(|sample| i16::from_le_bytes([sample[0], sample[1]]))
            .collect();
        assert_eq!(audio.frames() * n_channels, expected.len());

        for (frame_index, frame) in expected.chunks_exact(n_channels).enumerate() {
            for (channel, &sample) in frame.iter().enumerate() {
                let got = audio.channels[channel][frame_index];
                assert_eq!(
                    got.to_bits(),
                    (f32::from(sample) / 32768.0).to_bits(),
                    "frame {frame_index} channel {channel}"
                );
            }
        }
    }

    #[test]
    fn stereo_is_split_into_planar_channels() {
        let audio = decode_fresh(&pcm(2, 48000), &le(&[0, i16::MAX, i16::MIN, 16384]), Volume::FULL).expect("decodes");
        assert_eq!(audio.sample_rate, 48000);
        assert_eq!(audio.channels.len(), 2);
        assert_eq!(audio.channels[0], vec![0.0, -1.0]);
        assert_eq!(audio.channels[1], vec![f32::from(i16::MAX) / 32768.0, 0.5]);
        assert_eq!(audio.frames(), 2);
    }

    #[test]
    fn mono_uses_left_volume() {
        let volume = Volume { left: 0, right: 0xFFFF };
        let audio = decode_fresh(&pcm(1, 44100), &le(&[16384, -16384]), volume).expect("decodes");
        assert_eq!(audio.channels, vec![vec![0.0, 0.0]]);
    }

    #[test]
    fn volume_scales_each_lane() {
        let volume = Volume { left: 0xFFFF, right: 0 };
        let audio = decode_fresh(&pcm(2, 44100), &le(&[16384, 16384]), volume).expect("decodes");
        assert_eq!(audio.channels[0], vec![0.5]);
        assert_eq!(audio.channels[1], vec![0.0]);
    }

    #[test]
    fn trailing_partial_frame_is_dropped() {
        let mut data = le(&[100, 200, 300]);
        data.push(0x7F);
        let audio = decode_fresh(&pcm(2, 44100), &data, Volume::FULL).expect("decodes");
        assert_eq!(audio.frames(), 1);
    }

    #[test]
    fn short_or_empty_block_yields_nothing() {
        assert_eq!(decode_fresh(&pcm(2, 44100), &[], Volume::FULL), None);
        assert_eq!(decode_fresh(&pcm(2, 44100), &[1, 2, 3], Volume::FULL), None);
        assert_eq!(decode_fresh(&ima(2, 1017), &[0; 1023], Volume::FULL), None);
    }

    #[test]
    fn unsupported_formats_yield_nothing() {
        let data = le(&[1, 2, 3, 4]);

        let mut eight_bit = pcm(2, 44100);
        eight_bit.bits_per_sample = 8;
        assert_eq!(decode_fresh(&eight_bit, &data, Volume::FULL), None);

        let mut six_channel = pcm(2, 44100);
        six_channel.n_channels = 6;
        assert_eq!(decode_fresh(&six_channel, &data, Volume::FULL), None);

        // Decoded asynchronously, never here.
        assert_eq!(decode_fresh(&aac(44100), &data, Volume::FULL), None);
    }

    #[test]
    fn decoding_reuses_the_output_buffers() {
        let mut out = DecodedAudio::default();
        assert!(decode(&pcm(2, 48000), &le(&[1; 2 * 480]), Volume::FULL, &mut out));
        let capacity = out.channels[0].capacity();

        // A smaller mono block after a larger stereo one: no stale samples or channels,
        // and the first channel keeps its allocation.
        assert!(decode(&pcm(1, 44100), &le(&[16384]), Volume::FULL, &mut out));
        assert_eq!(out.channels, vec![vec![0.5]]);
        assert_eq!(out.sample_rate, 44100);
        assert!(capacity <= out.channels[0].capacity());
    }

    #[test]
    fn ima_adpcm_mono_matches_ffmpeg() {
        let audio =
            decode_fresh(&ima(1, 2041), include_bytes!("testdata/ima_mono.adpcm"), Volume::FULL).expect("decodes");
        assert_eq!(audio.sample_rate, 22050);
        assert_samples(&audio, include_bytes!("testdata/ima_mono.pcm"));
    }

    #[test]
    fn ima_adpcm_stereo_matches_ffmpeg() {
        let audio =
            decode_fresh(&ima(2, 1017), include_bytes!("testdata/ima_stereo.adpcm"), Volume::FULL).expect("decodes");
        assert_samples(&audio, include_bytes!("testdata/ima_stereo.pcm"));
    }

    #[test]
    fn ms_adpcm_mono_matches_ffmpeg() {
        let audio =
            decode_fresh(&ms(1, 2036), include_bytes!("testdata/ms_mono.adpcm"), Volume::FULL).expect("decodes");
        assert_samples(&audio, include_bytes!("testdata/ms_mono.pcm"));
    }

    #[test]
    fn ms_adpcm_stereo_matches_ffmpeg() {
        let audio =
            decode_fresh(&ms(2, 1012), include_bytes!("testdata/ms_stereo.adpcm"), Volume::FULL).expect("decodes");
        assert_samples(&audio, include_bytes!("testdata/ms_stereo.pcm"));
    }

    #[test]
    fn ms_adpcm_without_extra_data_uses_standard_coefficients() {
        let mut format = ms(2, 1012);
        format.data = None;
        let audio = decode_fresh(&format, include_bytes!("testdata/ms_stereo.adpcm"), Volume::FULL).expect("decodes");
        assert_samples(&audio, include_bytes!("testdata/ms_stereo.pcm"));
    }

    #[test]
    fn ms_adpcm_block_with_unknown_predictor_is_skipped() {
        let mut data = include_bytes!("testdata/ms_stereo.adpcm").to_vec();
        data[0] = 7; // Left predictor of block 0: only indices 0..7 exist.
        let audio = decode_fresh(&ms(2, 1012), &data, Volume::FULL).expect("later blocks decode");
        assert_eq!(audio.frames(), 2 * 1012);
        assert_eq!(audio.channels[0].len(), audio.channels[1].len());
    }

    #[test]
    fn hostile_adpcm_layouts_are_rejected() {
        // Block too small for its headers, or not a whole number of IMA body groups.
        let mut tiny = ima(2, 1);
        tiny.n_block_align = 8;
        assert_eq!(classify(&tiny, false), None);
        let mut ragged = ima(2, 1017);
        ragged.n_block_align = 1023;
        assert_eq!(classify(&ragged, false), None);
        let mut short_ms = ms(2, 1012);
        short_ms.n_block_align = 14;
        assert_eq!(classify(&short_ms, false), None);

        // Coefficient table shorter than its count claims, or empty.
        let mut truncated = ms(2, 1012);
        truncated.data.as_mut().expect("has data").truncate(10);
        assert_eq!(classify(&truncated, false), None);
        let mut empty = ms(2, 1012);
        empty.data = Some(vec![0xF4, 0x03, 0, 0]);
        assert_eq!(classify(&empty, false), None);
    }

    #[test]
    fn hostile_ms_adpcm_deltas_saturate_instead_of_overflowing() {
        // Every header field at its extreme, every nibble the largest multiplier.
        let mut block = vec![0u8; 1024];
        block[2..14].copy_from_slice(&[0xFF, 0x7F].repeat(6));
        block[14..].fill(0x88);
        let audio = decode_fresh(&ms(2, 1012), &block, Volume::FULL).expect("decodes");
        assert_eq!(audio.frames(), 1012);
    }

    #[test]
    fn negotiation_echoes_decodable_server_formats_best_first() {
        let server = [
            pcm(2, 44100),
            AudioFormat {
                format: WaveFormat::MPEGLAYER3,
                ..pcm(2, 44100)
            },
            ms(2, 1012),
            ima(2, 1017),
            pcm(2, 22050),
            pcm(1, 44100),
            aac(44100),
            aac(48000),
        ];

        assert_eq!(
            negotiate(&server, true),
            vec![
                aac(48000),
                aac(44100),
                ima(2, 1017),
                ms(2, 1012),
                pcm(2, 44100),
                pcm(1, 44100),
                pcm(2, 22050),
            ]
        );
    }

    #[test]
    fn negotiation_leaves_out_aac_when_the_browser_cannot_decode_it() {
        let server = [aac(44100), ima(2, 1017), pcm(2, 44100)];
        assert_eq!(negotiate(&server, false), vec![ima(2, 1017), pcm(2, 44100)]);
    }

    #[test]
    fn negotiation_drops_duplicates_and_unplayable_rates() {
        let server = [pcm(2, 44100), pcm(2, 44100), aac(44000)];
        assert_eq!(negotiate(&server, true), vec![pcm(2, 44100)]);
    }
}
