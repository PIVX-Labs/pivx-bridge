//! SIMD-accelerated hex decoding.
//!
//! Ported from Vector's `vector-core/src/hex.rs` (MIT).
//! Uses NEON (ARM64) or SSE2 (x86_64) with scalar LUT fallback.
//!
//! Performance (1835-byte Sapling tx):
//!   Hand-rolled from_str_radix: 3125 ns
//!   Scalar LUT:                 1354 ns  (2.3x)
//!   NEON/SSE2 SIMD:               91 ns  (34x)

#[cfg(target_arch = "aarch64")]
use std::arch::aarch64::*;

#[cfg(target_arch = "x86_64")]
use std::arch::x86_64::*;

/// Compile-time lookup table: ASCII byte → nibble value (0-15).
/// Invalid characters map to 0 (no validation — caller must ensure valid hex).
const HEX_DECODE_LUT: [u8; 256] = {
    let mut table = [0u8; 256];
    let mut i = 0;
    while i < 256 {
        table[i] = match i as u8 {
            b'0'..=b'9' => (i as u8) - b'0',
            b'a'..=b'f' => (i as u8) - b'a' + 10,
            b'A'..=b'F' => (i as u8) - b'A' + 10,
            _ => 0,
        };
        i += 1;
    }
    table
};

/// Decode a hex string to bytes using SIMD acceleration.
///
/// Returns `None` for odd-length input. Invalid hex chars decode as 0x00.
///
/// # Algorithm (NEON/SSE2)
/// `nibble = (char & 0x0F) + 9 * (char has bit 0x40 set)`
/// - '0'-'9': (0x30-0x39 & 0x0F) = 0-9, bit 0x40 clear → +0
/// - 'a'-'f': (0x61-0x66 & 0x0F) = 1-6, bit 0x40 set   → +9 = 10-15
/// - 'A'-'F': (0x41-0x46 & 0x0F) = 1-6, bit 0x40 set   → +9 = 10-15
pub fn hex_decode(hex: &str) -> Option<Vec<u8>> {
    let h = hex.as_bytes();
    if !h.len().is_multiple_of(2) {
        return None;
    }
    let out_len = h.len() / 2;
    Some(decode_inner(h, out_len))
}

#[cfg(target_arch = "aarch64")]
fn decode_inner(h: &[u8], out_len: usize) -> Vec<u8> {
    let mut result = vec![0u8; out_len];
    unsafe {
        let out_ptr: *mut u8 = result.as_mut_ptr();

        let mask_0f = vdupq_n_u8(0x0F);
        let mask_40 = vdupq_n_u8(0x40);
        let nine = vdupq_n_u8(9);

        // SIMD: 32 hex chars → 16 output bytes per iteration
        let chunks = out_len / 16;
        for chunk in 0..chunks {
            let in_off = chunk * 32;
            let out_off = chunk * 16;

            let hex_0 = vld1q_u8(h.as_ptr().add(in_off));
            let hex_1 = vld1q_u8(h.as_ptr().add(in_off + 16));

            let lo0 = vandq_u8(hex_0, mask_0f);
            let lo1 = vandq_u8(hex_1, mask_0f);

            let is_letter0 = vtstq_u8(hex_0, mask_40);
            let is_letter1 = vtstq_u8(hex_1, mask_40);

            let n0 = vaddq_u8(lo0, vandq_u8(is_letter0, nine));
            let n1 = vaddq_u8(lo1, vandq_u8(is_letter1, nine));

            // Pack nibbles: UZP separates even/odd, SLI combines in one instruction
            let evens = vuzp1q_u8(n0, n1);
            let odds = vuzp2q_u8(n0, n1);
            let bytes = vsliq_n_u8(odds, evens, 4);

            vst1q_u8(out_ptr.add(out_off), bytes);
        }

        // Scalar remainder
        let mut i = chunks * 32;
        let mut out_idx = chunks * 16;
        while i + 1 < h.len() {
            *out_ptr.add(out_idx) = (HEX_DECODE_LUT[h[i] as usize] << 4)
                                  | HEX_DECODE_LUT[h[i + 1] as usize];
            out_idx += 1;
            i += 2;
        }
    }
    result
}

#[cfg(target_arch = "x86_64")]
fn decode_inner(h: &[u8], out_len: usize) -> Vec<u8> {
    let mut result = vec![0u8; out_len];
    unsafe {
        let out_ptr = result.as_mut_ptr();

        let mask_0f = _mm_set1_epi8(0x0F);
        let mask_40 = _mm_set1_epi8(0x40);
        let nine = _mm_set1_epi8(9);
        let hi_mask = _mm_set1_epi16(0x00F0u16 as i16);
        let lo_mask = _mm_set1_epi16(0x000Fu16 as i16);
        let zero = _mm_setzero_si128();

        // SIMD: 16 hex chars → 8 output bytes per iteration
        let chunks = out_len / 8;
        for chunk in 0..chunks {
            let in_off = chunk * 16;
            let out_off = chunk * 8;

            let hex_chars = _mm_loadu_si128(h.as_ptr().add(in_off) as *const __m128i);

            let lo = _mm_and_si128(hex_chars, mask_0f);
            let masked = _mm_and_si128(hex_chars, mask_40);
            let is_letter = _mm_cmpeq_epi8(masked, mask_40);
            let nine_if_letter = _mm_and_si128(is_letter, nine);
            let nibbles = _mm_add_epi8(lo, nine_if_letter);

            let hi_nibbles = _mm_slli_epi16(nibbles, 4);
            let hi = _mm_and_si128(hi_nibbles, hi_mask);
            let lo_shifted = _mm_and_si128(_mm_srli_epi16(nibbles, 8), lo_mask);
            let combined = _mm_or_si128(hi, lo_shifted);

            let packed = _mm_packus_epi16(combined, zero);
            _mm_storel_epi64(out_ptr.add(out_off) as *mut __m128i, packed);
        }

        // Scalar remainder
        let mut i = chunks * 16;
        let mut out_idx = chunks * 8;
        while i + 1 < h.len() {
            *out_ptr.add(out_idx) = (HEX_DECODE_LUT[h[i] as usize] << 4)
                                  | HEX_DECODE_LUT[h[i + 1] as usize];
            out_idx += 1;
            i += 2;
        }
    }
    result
}

#[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
fn decode_inner(h: &[u8], out_len: usize) -> Vec<u8> {
    let mut result = Vec::with_capacity(out_len);
    for chunk in h.chunks(2) {
        if chunk.len() == 2 {
            result.push(
                (HEX_DECODE_LUT[chunk[0] as usize] << 4) | HEX_DECODE_LUT[chunk[1] as usize]
            );
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_valid() {
        assert_eq!(hex_decode("deadbeef"), Some(vec![0xDE, 0xAD, 0xBE, 0xEF]));
        assert_eq!(hex_decode("0300"), Some(vec![0x03, 0x00]));
        assert_eq!(hex_decode(""), Some(vec![]));
    }

    #[test]
    fn decode_odd_length() {
        assert_eq!(hex_decode("abc"), None);
    }

    #[test]
    fn decode_uppercase() {
        assert_eq!(hex_decode("DEADBEEF"), hex_decode("deadbeef"));
    }

    #[test]
    fn decode_real_mainnet_tx() {
        let hex = include_str!("../tests/mainnet_sapling_tx.hex").trim();
        let bytes = hex_decode(hex).unwrap();
        assert_eq!(bytes.len(), 1835);
        assert_eq!(&bytes[..4], &[0x03, 0x00, 0x00, 0x00]); // PIVX Sapling header
    }

    #[test]
    fn decode_short_input() {
        // Shorter than one SIMD chunk — exercises scalar remainder
        assert_eq!(hex_decode("ff"), Some(vec![0xFF]));
        assert_eq!(hex_decode("0102"), Some(vec![0x01, 0x02]));
    }
}
