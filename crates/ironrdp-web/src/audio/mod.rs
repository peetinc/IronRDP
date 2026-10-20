//! RDPSND audio playback through Web Audio.
//!
//! [`WasmRdpsndBackend`] is the `RdpsndClientHandler` the session attaches when
//! the `enable_audio_playback` extension is on. It advertises back the server's own
//! entries for every format [`codec::classify`] accepts, best first (AAC, ADPCM, PCM).
//! PCM and ADPCM blocks decode synchronously through [`codec::decode`]; AAC decodes
//! asynchronously through WebCodecs ([`aac`]). Both end in [`Output`], which applies
//! volume and hands planar f32 to [`player::Player`]. See the design doc
//! `docs/plans/2026-10-15-web-audio-playback-design.md`.

mod aac;
mod adpcm;
mod codec;
mod player;

use core::cell::{Cell, RefCell};
use core::fmt;
use std::borrow::Cow;
use std::rc::Rc;

use ironrdp::rdpsnd::client::RdpsndClientHandler;
use ironrdp::rdpsnd::pdu::{AudioFormat, AudioFormatFlags, PitchPdu, VolumePdu, WaveFormat};
use js_sys::{Object, Reflect};
use tracing::{debug, info, trace, warn};
use wasm_bindgen::JsValue;

use self::aac::{AacDecoder, AudioData};
use self::codec::{DecodedAudio, Volume};
use self::player::Player;

/// AAC decoder failures after which the backend gives up on AAC for the rest of the
/// session, rather than rebuilding a decoder for every block.
const MAX_AAC_FAILURES: u32 = 3;

/// Everything both decode paths share: the AAC output callback runs outside any
/// `RdpsndClientHandler` call, so this sits behind an `Rc<RefCell<_>>`.
#[derive(Debug)]
pub(crate) struct Output {
    player: Player,
    volume: Volume,
    /// Set by the page (the `audio_muted` extension): plays silence, leaving `volume` alone.
    muted: Rc<Cell<bool>>,
    /// Decode target reused across blocks.
    decoded: DecodedAudio,
}

impl Output {
    /// Queue a frame the AAC decoder produced, scaled by the current volume.
    fn play_audio_data(&mut self, data: &AudioData) {
        let Self {
            player,
            volume,
            muted,
            decoded,
        } = self;

        let Ok(frames) = usize::try_from(data.number_of_frames()) else {
            return;
        };
        // Parametric stereo can turn a mono format into stereo output; more than two
        // channels never comes from a format `classify` accepts.
        let n_channels = usize::try_from(data.number_of_channels()).unwrap_or(0).min(2);
        if frames == 0 || n_channels == 0 {
            return;
        }

        #[expect(
            clippy::as_conversions,
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "AudioData sample rates are positive integers far below u32::MAX"
        )]
        let sample_rate = data.sample_rate() as u32;
        decoded.begin(sample_rate, n_channels, frames);

        let gains = volume.heard(muted.get()).gains();
        for (index, (channel, gain)) in decoded.channels.iter_mut().zip(gains).enumerate() {
            channel.resize(frames, 0.0);
            let options = Object::new();
            // Setting a property on a fresh plain object cannot fail.
            let _ = Reflect::set(&options, &JsValue::from_str("planeIndex"), &JsValue::from(index));
            let _ = Reflect::set(&options, &JsValue::from_str("format"), &JsValue::from_str("f32-planar"));
            if let Err(error) = data.copy_to(channel, &options) {
                debug!(?error, "Dropping AAC frame: copyTo failed");
                return;
            }
            for sample in channel.iter_mut() {
                *sample *= gain;
            }
        }

        player.enqueue(decoded);
    }
}

pub(crate) struct WasmRdpsndBackend {
    output: Rc<RefCell<Output>>,
    /// Whether WebCodecs can decode AAC; resolved shortly after construction.
    aac_supported: Rc<Cell<bool>>,
    aac: Option<(AudioFormat, AacDecoder)>,
    aac_failures: u32,
    /// Last format a block played in; logged on change so the negotiated codec is visible.
    active_format: Option<AudioFormat>,
}

impl fmt::Debug for WasmRdpsndBackend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WasmRdpsndBackend")
            .field("aac_supported", &self.aac_supported.get())
            .field("active_format", &self.active_format)
            .finish_non_exhaustive()
    }
}

// SAFETY: `RdpsndClientHandler: Send` is a supertrait bound required by `Rdpsnd`'s
// `Box<dyn RdpsndClientHandler>` storage, not evidence the backend ever crosses an OS
// thread. Like `WasmDriveBackend`, this backend is created and driven on the single wasm
// event-loop thread; its `AudioContext`, `AudioDecoder` and `Rc`s never leave it.
unsafe impl Send for WasmRdpsndBackend {}

impl WasmRdpsndBackend {
    /// `worklet_url` serves the playout worklet module from the page's origin; `None` loads
    /// it from a Blob URL (see [`Player::new`]). While `muted` is set, blocks play as silence.
    pub(crate) fn new(worklet_url: Option<String>, muted: Rc<Cell<bool>>) -> anyhow::Result<Self> {
        Ok(Self {
            output: Rc::new(RefCell::new(Output {
                player: Player::new(worklet_url)?,
                volume: Volume::FULL,
                muted,
                decoded: DecodedAudio::default(),
            })),
            aac_supported: aac::probe_support(),
            aac: None,
            aac_failures: 0,
            active_format: None,
        })
    }

    fn play_aac(&mut self, format: &AudioFormat, data: &[u8]) {
        if MAX_AAC_FAILURES <= self.aac_failures {
            return;
        }

        if self.aac.as_ref().is_some_and(|(_, decoder)| decoder.failed()) {
            self.aac = None;
            self.count_aac_failure();
        }
        if self.aac.as_ref().is_some_and(|(active, _)| active != format) {
            self.aac = None;
        }

        if self.aac.is_none() {
            match AacDecoder::new(format, data, Rc::clone(&self.output)) {
                Ok(decoder) => self.aac = Some((format.clone(), decoder)),
                Err(error) => {
                    warn!(%error, "Failed to start AAC decoder");
                    self.count_aac_failure();
                    return;
                }
            }
        }

        if let Some((_, decoder)) = &mut self.aac {
            if let Err(error) = decoder.decode(data) {
                debug!(%error, "AAC decode failed");
                self.aac = None;
                self.count_aac_failure();
            }
        }
    }

    fn count_aac_failure(&mut self) {
        self.aac_failures += 1;
        if MAX_AAC_FAILURES <= self.aac_failures {
            warn!("AAC decoding keeps failing; audio stays silent for this session");
        }
    }
}

impl RdpsndClientHandler for WasmRdpsndBackend {
    fn get_flags(&self) -> AudioFormatFlags {
        // Volume is applied while decoding. Pitch is not implemented, so not advertised.
        AudioFormatFlags::VOLUME
    }

    fn get_formats(&self) -> &[AudioFormat] {
        // Formats come from the server's list; see `negotiate_formats`.
        &[]
    }

    fn negotiate_formats(&mut self, server_formats: &[AudioFormat]) -> Vec<AudioFormat> {
        let formats = codec::negotiate(server_formats, self.aac_supported.get());
        info!(
            offered = %FormatList(server_formats),
            advertised = %FormatList(&formats),
            "Negotiating RDPSND formats"
        );
        formats
    }

    fn wave(&mut self, format: &AudioFormat, ts: u32, data: Cow<'_, [u8]>) {
        self.output.borrow_mut().player.note_arrival(ts);
        if self.active_format.as_ref() != Some(format) {
            info!(format = %FormatName(format), "Playing RDPSND format");
            self.active_format = Some(format.clone());
        }

        if format.format == WaveFormat::AAC_MS {
            self.play_aac(format, &data);
            return;
        }

        let mut output = self.output.borrow_mut();
        let Output {
            player,
            volume,
            muted,
            decoded,
        } = &mut *output;
        if codec::decode(format, &data, volume.heard(muted.get()), decoded) {
            player.enqueue(decoded);
        } else {
            trace!(len = data.len(), "Dropping undecodable RDPSND block");
        }
    }

    fn set_volume(&mut self, volume: VolumePdu) {
        self.output.borrow_mut().volume = volume_from_pdu(&volume);
    }

    fn set_pitch(&mut self, _pitch: PitchPdu) {}

    fn close(&mut self) {
        // Timing is left alone: the queue's end time is in context seconds, valid across
        // formats, and resetting it would overlap new audio with the still-queued tail.
        // The AAC decoder stays too: the next stream is usually the same format.
        self.active_format = None;
    }
}

/// A named function rather than an inline block so the lane mapping is unit-testable
/// without an `AudioContext`.
fn volume_from_pdu(volume: &VolumePdu) -> Volume {
    Volume {
        left: volume.volume_left,
        right: volume.volume_right,
    }
}

/// Compact one-line format name for logs, e.g. `41222 AAC_MS 2ch 44100Hz 24000B/s`.
struct FormatName<'a>(&'a AudioFormat);

impl fmt::Display for FormatName<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let format = self.0;
        write!(
            f,
            "{} {}ch {}Hz {}B/s",
            format.format, format.n_channels, format.n_samples_per_sec, format.n_avg_bytes_per_sec
        )
    }
}

struct FormatList<'a>(&'a [AudioFormat]);

impl fmt::Display for FormatList<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[")?;
        for (index, format) in self.0.iter().enumerate() {
            if 0 < index {
                f.write_str(", ")?;
            }
            write!(f, "{}", FormatName(format))?;
        }
        f.write_str("]")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn volume_pdu_maps_lanes() {
        let volume = volume_from_pdu(&VolumePdu {
            volume_left: 0x1234,
            volume_right: 0xABCD,
        });
        assert_eq!(
            volume,
            Volume {
                left: 0x1234,
                right: 0xABCD
            }
        );
    }

    #[test]
    fn formats_log_compactly() {
        let formats = [
            AudioFormat {
                format: WaveFormat::AAC_MS,
                n_channels: 2,
                n_samples_per_sec: 44100,
                n_avg_bytes_per_sec: 24000,
                n_block_align: 4,
                bits_per_sample: 16,
                data: None,
            },
            AudioFormat {
                format: WaveFormat::MPEGLAYER3,
                n_channels: 1,
                n_samples_per_sec: 8000,
                n_avg_bytes_per_sec: 1000,
                n_block_align: 1,
                bits_per_sample: 4,
                data: None,
            },
        ];
        assert_eq!(
            FormatList(&formats).to_string(),
            "[41222 AAC_MS 2ch 44100Hz 24000B/s, 85 MPEGLAYER3 1ch 8000Hz 1000B/s]"
        );
    }
}
