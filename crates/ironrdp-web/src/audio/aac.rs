//! AAC (`WAVE_FORMAT_AAC_MS`) decoding through the browser's WebCodecs `AudioDecoder`.
//!
//! The bitstream framing is detected from the first block: ADTS (each frame starts
//! with a sync header) is fed frame by frame with no decoder description; anything
//! else is treated as raw AAC-LC access units, one per wave block, described by an
//! `AudioSpecificConfig` built from the negotiated format ([ISO/IEC 14496-3] 1.6.2.1).
//! That is the WebCodecs AAC registration's split: a `description` means raw, none means ADTS.
//!
//! Decoding is asynchronous: frames come back through the output callback, which
//! hands them to [`Output`] for playback.
//!
//! [ISO/IEC 14496-3]: https://www.iso.org/standard/76383.html

use core::cell::{Cell, RefCell};
use std::rc::Rc;

use anyhow::anyhow;
use ironrdp::rdpsnd::pdu::AudioFormat;
use js_sys::{Object, Reflect, Uint8Array};
use tracing::{debug, warn};
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;

use super::Output;

/// WebCodecs codec string for AAC-LC. ADTS streams carry their own profile, so this
/// also covers HE-AAC sent with ADTS headers.
const CODEC: &str = "mp4a.40.2";

/// Samples per AAC-LC frame, used to advance chunk timestamps.
const FRAME_SAMPLES: f64 = 1024.0;

/// [ISO/IEC 14496-3] `samplingFrequencyIndex` table.
///
/// [ISO/IEC 14496-3]: https://www.iso.org/standard/76383.html
const SAMPLING_FREQUENCIES: [u32; 13] = [
    96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350,
];

/// Bytes in an ADTS header without CRC.
const ADTS_HEADER_LEN: usize = 7;

pub(crate) fn sampling_frequency_index(rate: u32) -> Option<u8> {
    SAMPLING_FREQUENCIES
        .iter()
        .position(|&candidate| candidate == rate)
        .and_then(|index| u8::try_from(index).ok())
}

/// Two-byte `AudioSpecificConfig` for AAC-LC at the format's rate and channel count:
/// `audioObjectType` (5 bits) = 2, `samplingFrequencyIndex` (4), `channelConfiguration` (4),
/// then three zero `GASpecificConfig` flags.
pub(crate) fn audio_specific_config(format: &AudioFormat) -> Option<[u8; 2]> {
    const AAC_LC: u16 = 2;

    let frequency_index = u16::from(sampling_frequency_index(format.n_samples_per_sec)?);
    let channels = format.n_channels;
    if !(1..=7).contains(&channels) {
        return None;
    }

    Some(((AAC_LC << 11) | (frequency_index << 7) | (channels << 3)).to_be_bytes())
}

/// Whether `data` starts with an ADTS sync word (12 set bits, MPEG layer 0).
pub(crate) fn is_adts(data: &[u8]) -> bool {
    matches!(data, [0xFF, second, ..] if second & 0xF6 == 0xF0) && ADTS_HEADER_LEN <= data.len()
}

/// Split concatenated ADTS frames, headers included. Stops at the first header that is
/// not ADTS or claims more bytes than remain, so a corrupt block yields its intact prefix.
pub(crate) fn adts_frames(mut data: &[u8]) -> impl Iterator<Item = &[u8]> {
    core::iter::from_fn(move || {
        if !is_adts(data) {
            return None;
        }
        // aac_frame_length: 13 bits spanning header bytes 3..=5, header included.
        let length = (usize::from(data[3] & 0x03) << 11) | (usize::from(data[4]) << 3) | (usize::from(data[5]) >> 5);
        if length < ADTS_HEADER_LEN || data.len() < length {
            return None;
        }
        let (frame, rest) = data.split_at(length);
        data = rest;
        Some(frame)
    })
}

/// Whether this browser can decode AAC-LC through WebCodecs, resolved in the background.
///
/// Reads `false` until the probe answers, which happens long before a server offers
/// formats: the probe starts with the backend, at connect time.
pub(crate) fn probe_support() -> Rc<Cell<bool>> {
    let supported = Rc::new(Cell::new(false));

    if !Reflect::has(&js_sys::global(), &JsValue::from_str("AudioDecoder")).unwrap_or(false) {
        debug!("WebCodecs AudioDecoder unavailable; AAC disabled");
        return supported;
    }

    let result = Rc::clone(&supported);
    wasm_bindgen_futures::spawn_local(async move {
        let config = config(44100, 2, None);
        let answer = match AudioDecoder::is_config_supported(&config) {
            Ok(promise) => JsFuture::from(promise).await,
            Err(error) => Err(error),
        };
        match answer.and_then(|support| Reflect::get(&support, &JsValue::from_str("supported"))) {
            Ok(value) => result.set(value.is_truthy()),
            Err(error) => debug!(?error, "AAC support probe failed"),
        }
        debug!(supported = result.get(), "Probed WebCodecs AAC support");
    });

    supported
}

/// One `AudioDecoder` configured for one negotiated AAC format.
#[derive(Debug)]
pub(crate) struct AacDecoder {
    decoder: AudioDecoder,
    adts: bool,
    /// Microseconds; WebCodecs wants increasing chunk timestamps.
    timestamp: f64,
    frame_duration: f64,
    /// Set by the error callback; the decoder is then closed and must be replaced.
    failed: Rc<Cell<bool>>,
    _on_output: Closure<dyn FnMut(AudioData)>,
    _on_error: Closure<dyn FnMut(JsValue)>,
}

impl AacDecoder {
    /// A decoder for `format`, framed as ADTS or raw according to `first_block`.
    pub(crate) fn new(format: &AudioFormat, first_block: &[u8], output: Rc<RefCell<Output>>) -> anyhow::Result<Self> {
        let adts = is_adts(first_block);
        let description = if adts {
            None
        } else {
            Some(audio_specific_config(format).ok_or_else(|| anyhow!("no AudioSpecificConfig for {format:?}"))?)
        };

        let on_output = Closure::<dyn FnMut(AudioData)>::new(move |data: AudioData| {
            match output.try_borrow_mut() {
                Ok(mut output) => output.play_audio_data(&data),
                Err(_) => debug!("Dropping AAC frame: output busy"),
            }
            data.close();
        });

        let failed = Rc::new(Cell::new(false));
        let on_error = {
            let failed = Rc::clone(&failed);
            Closure::<dyn FnMut(JsValue)>::new(move |error: JsValue| {
                warn!(?error, "AAC decoder error");
                failed.set(true);
            })
        };

        let init = Object::new();
        set(&init, "output", on_output.as_ref())?;
        set(&init, "error", on_error.as_ref())?;
        let decoder = AudioDecoder::new(&init).map_err(|error| anyhow!("create AudioDecoder: {error:?}"))?;
        decoder
            .configure(&config(
                format.n_samples_per_sec,
                format.n_channels,
                description.as_ref().map(<[u8; 2]>::as_slice),
            ))
            .map_err(|error| anyhow!("configure AudioDecoder: {error:?}"))?;

        debug!(adts, "Configured AAC decoder");

        Ok(Self {
            decoder,
            adts,
            timestamp: 0.0,
            frame_duration: FRAME_SAMPLES * 1_000_000.0 / f64::from(format.n_samples_per_sec),
            failed,
            _on_output: on_output,
            _on_error: on_error,
        })
    }

    /// Whether the decoder hit an error and must be replaced.
    pub(crate) fn failed(&self) -> bool {
        self.failed.get()
    }

    pub(crate) fn decode(&mut self, block: &[u8]) -> anyhow::Result<()> {
        if self.adts {
            for frame in adts_frames(block) {
                self.decode_frame(frame)?;
            }
            Ok(())
        } else {
            self.decode_frame(block)
        }
    }

    fn decode_frame(&mut self, frame: &[u8]) -> anyhow::Result<()> {
        let init = Object::new();
        set(&init, "type", &JsValue::from_str("key"))?;
        set(&init, "timestamp", &JsValue::from_f64(self.timestamp))?;
        set(&init, "data", &Uint8Array::from(frame))?;
        let chunk = EncodedAudioChunk::new(&init).map_err(|error| anyhow!("create EncodedAudioChunk: {error:?}"))?;

        self.decoder
            .decode(&chunk)
            .map_err(|error| anyhow!("decode AAC frame: {error:?}"))?;
        self.timestamp += self.frame_duration;
        Ok(())
    }
}

impl Drop for AacDecoder {
    fn drop(&mut self) {
        if self.decoder.state() != "closed" {
            if let Err(error) = self.decoder.close() {
                debug!(?error, "Failed to close AudioDecoder");
            }
        }
    }
}

fn config(sample_rate: u32, n_channels: u16, description: Option<&[u8]>) -> Object {
    let config = Object::new();
    // Setting a property on a fresh plain object cannot fail.
    let _ = Reflect::set(&config, &JsValue::from_str("codec"), &JsValue::from_str(CODEC));
    let _ = Reflect::set(&config, &JsValue::from_str("sampleRate"), &JsValue::from(sample_rate));
    let _ = Reflect::set(
        &config,
        &JsValue::from_str("numberOfChannels"),
        &JsValue::from(n_channels),
    );
    if let Some(description) = description {
        let _ = Reflect::set(
            &config,
            &JsValue::from_str("description"),
            &Uint8Array::from(description),
        );
    }
    config
}

fn set(target: &Object, key: &str, value: &JsValue) -> anyhow::Result<()> {
    Reflect::set(target, &JsValue::from_str(key), value)
        .map(drop)
        .map_err(|error| anyhow!("set {key}: {error:?}"))
}

// Hand-written WebCodecs bindings: `web-sys` only exposes these behind
// `--cfg=web_sys_unstable_apis`.
#[wasm_bindgen]
extern "C" {
    #[derive(Debug)]
    type AudioDecoder;

    #[wasm_bindgen(constructor, catch)]
    fn new(init: &Object) -> Result<AudioDecoder, JsValue>;

    #[wasm_bindgen(static_method_of = AudioDecoder, js_name = isConfigSupported, catch)]
    fn is_config_supported(config: &Object) -> Result<js_sys::Promise, JsValue>;

    #[wasm_bindgen(method, catch)]
    fn configure(this: &AudioDecoder, config: &Object) -> Result<(), JsValue>;

    #[wasm_bindgen(method, catch)]
    fn decode(this: &AudioDecoder, chunk: &EncodedAudioChunk) -> Result<(), JsValue>;

    #[wasm_bindgen(method, catch)]
    fn close(this: &AudioDecoder) -> Result<(), JsValue>;

    #[wasm_bindgen(method, getter)]
    fn state(this: &AudioDecoder) -> String;

    type EncodedAudioChunk;

    #[wasm_bindgen(constructor, catch)]
    fn new(init: &Object) -> Result<EncodedAudioChunk, JsValue>;

    pub(crate) type AudioData;

    #[wasm_bindgen(method, getter, js_name = numberOfFrames)]
    pub(crate) fn number_of_frames(this: &AudioData) -> u32;

    #[wasm_bindgen(method, getter, js_name = numberOfChannels)]
    pub(crate) fn number_of_channels(this: &AudioData) -> u32;

    #[wasm_bindgen(method, getter, js_name = sampleRate)]
    pub(crate) fn sample_rate(this: &AudioData) -> f32;

    /// `options` is an `AudioDataCopyToOptions`: `{ planeIndex, format }`.
    #[wasm_bindgen(method, catch, js_name = copyTo)]
    pub(crate) fn copy_to(this: &AudioData, destination: &mut [f32], options: &Object) -> Result<(), JsValue>;

    #[wasm_bindgen(method)]
    fn close(this: &AudioData);
}

#[cfg(test)]
mod tests {
    use ironrdp::rdpsnd::pdu::WaveFormat;

    use super::*;

    fn aac(n_channels: u16, n_samples_per_sec: u32) -> AudioFormat {
        AudioFormat {
            format: WaveFormat::AAC_MS,
            n_channels,
            n_samples_per_sec,
            n_avg_bytes_per_sec: 24000,
            n_block_align: 4,
            bits_per_sample: 16,
            data: None,
        }
    }

    /// ADTS header for an AAC-LC frame of `length` bytes total, followed by filler payload.
    fn adts_frame(length: usize) -> Vec<u8> {
        let length_bits = u32::try_from(length).expect("small");
        let mut frame = vec![
            0xFF,
            0xF1, // MPEG-4, layer 0, no CRC
            0x50, // AAC-LC, 44.1 kHz
            0x80 | u8::try_from(length_bits >> 11).expect("2 bits"),
            u8::try_from((length_bits >> 3) & 0xFF).expect("8 bits"),
            u8::try_from((length_bits & 0x07) << 5).expect("3 bits") | 0x1F,
            0xFC,
        ];
        frame.resize(length, 0xA5);
        frame
    }

    #[test]
    fn audio_specific_config_for_lc_stereo_44k() {
        // 00010 0100 0010 000: the canonical AAC-LC 44.1 kHz stereo config.
        assert_eq!(audio_specific_config(&aac(2, 44100)), Some([0x12, 0x10]));
        assert_eq!(audio_specific_config(&aac(1, 48000)), Some([0x11, 0x88]));
    }

    #[test]
    fn audio_specific_config_rejects_unlisted_rates() {
        assert_eq!(audio_specific_config(&aac(2, 44000)), None);
    }

    #[test]
    fn adts_is_recognized_by_its_sync_word() {
        assert!(is_adts(&adts_frame(20)));
        // Raw AAC access units start with a syntax element, never 12 set bits.
        assert!(!is_adts(&[0x21, 0x1B, 0x80, 0x00, 0x00, 0x00, 0x00]));
        assert!(!is_adts(&[0xFF, 0xF1]));
        assert!(!is_adts(&[]));
    }

    #[test]
    fn concatenated_adts_frames_are_split() {
        let first = adts_frame(20);
        let second = adts_frame(300);
        let block = [first.clone(), second.clone()].concat();

        let frames: Vec<&[u8]> = adts_frames(&block).collect();
        assert_eq!(frames, vec![first.as_slice(), second.as_slice()]);
    }

    #[test]
    fn truncated_adts_frame_stops_the_split() {
        let first = adts_frame(20);
        let mut block = [first.clone(), adts_frame(300)].concat();
        block.truncate(200);

        let frames: Vec<&[u8]> = adts_frames(&block).collect();
        assert_eq!(frames, vec![first.as_slice()]);
    }

    #[test]
    fn adts_header_claiming_less_than_itself_stops_the_split() {
        let mut frame = adts_frame(20);
        frame[3] &= 0xFC;
        frame[4] = 0;
        frame[5] &= 0x1F;
        assert_eq!(adts_frames(&frame).count(), 0);
    }
}
