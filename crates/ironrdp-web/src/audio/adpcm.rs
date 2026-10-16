//! IMA (DVI) and Microsoft ADPCM block decoding, 4 bits per sample.
//!
//! Both are block formats: every `nBlockAlign` bytes carry a per-channel header that
//! seeds the decoder, so a block decodes on its own and a lost block cannot corrupt
//! the next one. Layouts follow the [Multimedia Registration] ADPCM chapters, the
//! layout FFmpeg's `adpcm_ima_wav` and `adpcm_ms` codecs use.
//!
//! [Multimedia Registration]: https://www.mmsp.ece.mcgill.ca/Documents/AudioFormats/WAVE/Docs/RIFFNEW.pdf

use super::codec::Sink;

/// IMA ADPCM step sizes, indexed by the step index (0..=88).
const IMA_STEPS: [i32; 89] = [
    7, 8, 9, 10, 11, 12, 13, 14, 16, 17, 19, 21, 23, 25, 28, 31, 34, 37, 41, 45, 50, 55, 60, 66, 73, 80, 88, 97, 107,
    118, 130, 143, 157, 173, 190, 209, 230, 253, 279, 307, 337, 371, 408, 449, 494, 544, 598, 658, 724, 796, 876, 963,
    1060, 1166, 1282, 1411, 1552, 1707, 1878, 2066, 2272, 2499, 2749, 3024, 3327, 3660, 4026, 4428, 4871, 5358, 5894,
    6484, 7132, 7845, 8630, 9493, 10442, 11487, 12635, 13899, 15289, 16818, 18500, 20350, 22385, 24623, 27086, 29794,
    32767,
];

/// IMA ADPCM step index adjustment per nibble.
const IMA_INDEX_ADJUST: [i8; 16] = [-1, -1, -1, -1, 2, 4, 6, 8, -1, -1, -1, -1, 2, 4, 6, 8];

/// MS ADPCM delta scaling per nibble, in 1/256ths.
const MS_ADAPTATION: [i32; 16] = [
    230, 230, 230, 230, 307, 409, 512, 614, 768, 614, 512, 409, 307, 230, 230, 230,
];

/// The seven predictor coefficient pairs every MS ADPCM stream starts with, in 1/256ths.
pub(crate) const MS_STANDARD_COEFS: [(i16, i16); 7] = [
    (256, 0),
    (512, -256),
    (0, 0),
    (192, 64),
    (240, 0),
    (460, -208),
    (392, -232),
];

/// Bounds the MS ADPCM delta like FFmpeg does, so hostile data cannot overflow the
/// prediction arithmetic.
const MS_MAX_DELTA: i32 = i32::MAX / 768;

/// Bytes of per-channel header at the start of an IMA ADPCM block.
pub(crate) const IMA_HEADER_LEN: usize = 2 /* iSamp0 */ + 1 /* bStepTableIndex */ + 1 /* bReserved */;

/// Bytes of per-channel header at the start of an MS ADPCM block.
pub(crate) const MS_HEADER_LEN: usize = 1 /* bPredictor */ + 2 /* iDelta */ + 2 /* iSamp1 */ + 2 /* iSamp2 */;

/// Frames in one IMA ADPCM block: the header sample, then two per body byte.
pub(crate) fn ima_frames_per_block(n_channels: usize, block_align: usize) -> usize {
    (block_align / n_channels - IMA_HEADER_LEN) * 2 + 1
}

/// Frames in one MS ADPCM block: the two header samples, then two per body byte.
pub(crate) fn ms_frames_per_block(n_channels: usize, block_align: usize) -> usize {
    (block_align / n_channels - MS_HEADER_LEN) * 2 + 2
}

/// Decode whole IMA ADPCM blocks; a trailing partial block is dropped.
///
/// INVARIANT: `1 <= n_channels <= 2`, and `block_align` is a multiple of `4 * n_channels`
/// holding more than the headers (checked by `codec::classify`).
pub(crate) fn decode_ima(n_channels: usize, block_align: usize, data: &[u8], sink: &mut Sink<'_>) {
    let header_len = IMA_HEADER_LEN * n_channels;

    for block in data.chunks_exact(block_align) {
        let (header, body) = block.split_at(header_len);
        let mut states = [ImaState::default(); 2];

        for (channel, (state, header)) in states.iter_mut().zip(header.chunks_exact(IMA_HEADER_LEN)).enumerate() {
            let predictor = i16::from_le_bytes([header[0], header[1]]);
            state.predictor = i32::from(predictor);
            state.index = usize::from(header[2]).min(IMA_STEPS.len() - 1);
            sink.push(channel, predictor);
        }

        // Body: per channel in turn, 4 bytes = 8 samples, low nibble first.
        for group in body.chunks_exact(4 * n_channels) {
            for (channel, (state, bytes)) in states.iter_mut().zip(group.chunks_exact(4)).enumerate() {
                for &byte in bytes {
                    sink.push(channel, state.expand(byte & 0x0F));
                    sink.push(channel, state.expand(byte >> 4));
                }
            }
        }
    }

    #[derive(Clone, Copy, Default)]
    struct ImaState {
        predictor: i32,
        index: usize,
    }

    impl ImaState {
        fn expand(&mut self, nibble: u8) -> i16 {
            let step = IMA_STEPS[self.index];
            let mut diff = step >> 3;
            if nibble & 4 != 0 {
                diff += step;
            }
            if nibble & 2 != 0 {
                diff += step >> 1;
            }
            if nibble & 1 != 0 {
                diff += step >> 2;
            }
            if nibble & 8 != 0 {
                self.predictor -= diff;
            } else {
                self.predictor += diff;
            }

            let sample = clamp_i16(i64::from(self.predictor));
            self.predictor = i32::from(sample);
            self.index = self
                .index
                .saturating_add_signed(isize::from(IMA_INDEX_ADJUST[usize::from(nibble)]))
                .min(IMA_STEPS.len() - 1);
            sample
        }
    }
}

/// Decode whole MS ADPCM blocks with the format's predictor coefficients; a trailing
/// partial block is dropped, and so is a block naming a coefficient pair that does not
/// exist.
///
/// INVARIANT: `1 <= n_channels <= 2`, `block_align` holds more than the headers, and
/// `predictors` is not empty (checked by `codec::classify`).
pub(crate) fn decode_ms(
    n_channels: usize,
    block_align: usize,
    predictors: &[(i16, i16)],
    data: &[u8],
    sink: &mut Sink<'_>,
) {
    let header_len = MS_HEADER_LEN * n_channels;

    'blocks: for block in data.chunks_exact(block_align) {
        let (header, body) = block.split_at(header_len);
        let mut states = [MsState::default(); 2];

        // Header fields are grouped by field, not by channel: all predictors, then all deltas, ...
        let field = |offset: usize, channel: usize| {
            let at = n_channels + (offset * n_channels + channel) * 2;
            i16::from_le_bytes([header[at], header[at + 1]])
        };
        for (channel, state) in states.iter_mut().take(n_channels).enumerate() {
            let Some(&(coef1, coef2)) = predictors.get(usize::from(header[channel])) else {
                continue 'blocks;
            };
            *state = MsState {
                coef1: i64::from(coef1),
                coef2: i64::from(coef2),
                delta: i32::from(field(0, channel)),
                sample1: i32::from(field(1, channel)),
                sample2: i32::from(field(2, channel)),
            };
        }

        // The header carries the first two samples, oldest (iSamp2) first.
        for (channel, state) in states.iter().take(n_channels).enumerate() {
            sink.push(channel, clamp_i16(i64::from(state.sample2)));
        }
        for (channel, state) in states.iter().take(n_channels).enumerate() {
            sink.push(channel, clamp_i16(i64::from(state.sample1)));
        }

        // Body: high nibble first; in stereo the high nibble is left, the low one right.
        for &byte in body {
            let (first, second) = if n_channels == 2 { (0, 1) } else { (0, 0) };
            sink.push(first, states[first].expand(byte >> 4));
            sink.push(second, states[second].expand(byte & 0x0F));
        }
    }

    #[derive(Clone, Copy, Default)]
    struct MsState {
        coef1: i64,
        coef2: i64,
        delta: i32,
        sample1: i32,
        sample2: i32,
    }

    impl MsState {
        fn expand(&mut self, nibble: u8) -> i16 {
            // Truncating division, as the reference decoder does.
            let prediction = (i64::from(self.sample1) * self.coef1 + i64::from(self.sample2) * self.coef2) / 256;
            // The nibble is a 4-bit two's complement multiplier.
            let signed = if nibble & 8 != 0 {
                i64::from(nibble) - 16
            } else {
                i64::from(nibble)
            };
            let sample = clamp_i16(prediction + signed * i64::from(self.delta));

            self.sample2 = self.sample1;
            self.sample1 = i32::from(sample);
            self.delta = ((MS_ADAPTATION[usize::from(nibble)] * self.delta) >> 8).clamp(16, MS_MAX_DELTA);
            sample
        }
    }
}

fn clamp_i16(value: i64) -> i16 {
    i16::try_from(value.clamp(i64::from(i16::MIN), i64::from(i16::MAX))).expect("clamped to the i16 range")
}
