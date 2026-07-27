//! G.711 μ-law encoding — the codec that lets us skip libopus entirely.
//!
//! The realtime WebRTC endpoint answers offers that advertise PCMU (payload
//! type 0), and μ-law companding is a few arithmetic steps with no native
//! dependencies. 8 kHz mono is telephony grade, which speech-to-text handles
//! well; the linear-resampled PCM is low-passed upstream in `audio.rs` before
//! it reaches this encoder.

const MULAW_MAX: i32 = 32_635;
const MULAW_BIAS: i32 = 0x84;

/// Encode one signed 16-bit sample as a μ-law byte (ITU-T G.711).
pub fn mulaw_encode(sample: i16) -> u8 {
    let mut s = i32::from(sample).clamp(-MULAW_MAX, MULAW_MAX);
    let sign: u8 = if s < 0 {
        s = -s;
        0x80
    } else {
        0
    };
    let s = s + MULAW_BIAS;
    let mut exp: u8 = 7;
    let mut mask = 0x4000;
    while exp > 0 && (s & mask) == 0 {
        exp -= 1;
        mask >>= 1;
    }
    let mant = ((s >> (exp + 3)) & 0x0f) as u8;
    !(sign | (exp << 4) | mant)
}

/// Encode a block of little-endian PCM16 into μ-law bytes.
pub fn encode_pcm16le(pcm: &[u8], out: &mut Vec<u8>) {
    out.clear();
    out.reserve(pcm.len() / 2);
    for chunk in pcm.chunks_exact(2) {
        out.push(mulaw_encode(i16::from_le_bytes([chunk[0], chunk[1]])));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reference decoder for round-trip checks (not used in production).
    fn mulaw_decode(byte: u8) -> i16 {
        let b = !byte;
        let sign = b & 0x80;
        let exp = (b >> 4) & 0x07;
        let mant = b & 0x0f;
        let mut s = ((i32::from(mant) << 1) | 0x21) << (exp + 2);
        s -= MULAW_BIAS;
        if sign != 0 {
            -s as i16
        } else {
            s as i16
        }
    }

    #[test]
    fn silence_encodes_to_0xff() {
        assert_eq!(mulaw_encode(0), 0xff);
    }

    #[test]
    fn full_scale_clamps_instead_of_overflowing() {
        // Beyond MULAW_MAX the encoder must clamp, not wrap the segment math.
        let a = mulaw_encode(32_767);
        let b = mulaw_encode(MULAW_MAX as i16);
        assert_eq!(a, b);
        let c = mulaw_encode(-32_768);
        let d = mulaw_encode(-(MULAW_MAX as i16));
        assert_eq!(c, d);
    }

    #[test]
    fn roundtrip_stays_within_quantization_error() {
        // μ-law is lossy by design; the error grows with amplitude. Across the
        // speech band (a few thousand counts) it must stay inaudibly small.
        for &s in &[0i16, 100, -100, 500, -500, 2_000, -2_000, 8_000, -8_000, 20_000] {
            let decoded = mulaw_decode(mulaw_encode(s));
            let err = (i32::from(decoded) - i32::from(s)).abs();
            let tolerance = (i32::from(s).abs() / 8) + 100;
            assert!(
                err <= tolerance,
                "sample {s} decoded to {decoded}, error {err} > {tolerance}"
            );
        }
    }

    #[test]
    fn block_encoder_matches_byte_count() {
        let pcm: Vec<u8> = (0..320i16).flat_map(|s| (s - 160).to_le_bytes()).collect();
        let mut out = Vec::new();
        encode_pcm16le(&pcm, &mut out);
        // 320 i16 samples = 640 PCM bytes = 320 μ-law bytes (2:1 compression).
        assert_eq!(out.len(), 320);
    }
}
