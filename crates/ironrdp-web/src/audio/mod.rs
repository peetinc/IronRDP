//! RDPSND audio playback through Web Audio.
//!
//! [`WasmRdpsndBackend`] is the `RdpsndClientHandler` the session attaches when
//! the `enable_audio_playback` extension is on. Wave bytes go through
//! [`codec::decode`] to planar f32 and then to [`player::Player`]. A new codec is
//! added in `codec` alone (sync) or, for WebCodecs, with an async path that feeds
//! the player from the decoder's output callback — see the design doc
//! `docs/plans/2026-10-15-web-audio-playback-design.md`.

mod codec;
mod player;

use std::borrow::Cow;

use ironrdp::rdpsnd::client::RdpsndClientHandler;
use ironrdp::rdpsnd::pdu::{AudioFormat, AudioFormatFlags, PitchPdu, VolumePdu};
use tracing::{info, trace};

use self::codec::{DecodedAudio, Volume};
use self::player::Player;

#[derive(Debug)]
pub(crate) struct WasmRdpsndBackend {
    formats: Vec<AudioFormat>,
    player: Player,
    volume: Volume,
    /// Last format a block played in; logged on change so the negotiated codec is visible.
    active_format: Option<AudioFormat>,
    /// Decode target reused across blocks.
    decoded: DecodedAudio,
}

// SAFETY: `RdpsndClientHandler: Send` is a supertrait bound required by `Rdpsnd`'s
// `Box<dyn RdpsndClientHandler>` storage, not evidence the backend ever crosses an OS
// thread. Like `WasmDriveBackend`, this backend is created and driven on the single wasm
// event-loop thread; its `AudioContext` and `Rc` never leave it.
unsafe impl Send for WasmRdpsndBackend {}

impl WasmRdpsndBackend {
    pub(crate) fn new() -> anyhow::Result<Self> {
        Ok(Self {
            formats: codec::supported_formats(),
            player: Player::new()?,
            volume: Volume::FULL,
            active_format: None,
            decoded: DecodedAudio::default(),
        })
    }
}

impl RdpsndClientHandler for WasmRdpsndBackend {
    fn get_flags(&self) -> AudioFormatFlags {
        // Volume is applied while decoding. Pitch is not implemented, so not advertised.
        AudioFormatFlags::VOLUME
    }

    fn get_formats(&self) -> &[AudioFormat] {
        &self.formats
    }

    fn wave(&mut self, format: &AudioFormat, _ts: u32, data: Cow<'_, [u8]>) {
        if self.active_format.as_ref() != Some(format) {
            info!(?format, "Playing RDPSND format");
            self.active_format = Some(format.clone());
        }

        if codec::decode(format, &data, self.volume, &mut self.decoded) {
            self.player.enqueue(&self.decoded);
        } else {
            trace!(len = data.len(), "Dropping undecodable RDPSND block");
        }
    }

    fn set_volume(&mut self, volume: VolumePdu) {
        self.volume = volume_from_pdu(&volume);
    }

    fn set_pitch(&mut self, _pitch: PitchPdu) {}

    fn close(&mut self) {
        // Timing is left alone: the queue's end time is in context seconds, valid across
        // formats, and resetting it would overlap new audio with the still-queued tail.
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
}
