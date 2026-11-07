//! The `Sec-MS-GEC` token Microsoft's read-aloud service wants with every
//! request (version 2, as worked out by the `edge-tts` project).
//!
//! The token is built from the Windows file time rounded down to five
//! minutes, followed by the client token. It is two digests of that text: a
//! SHA-256 of it behind a hidden 57-byte prefix (resumed from that prefix's
//! midstate, so the prefix itself never appears), and a 64-bit multiplicative
//! hash keyed by the first. Written `<16 hex>-<64 hex>`.

const MASK: u64 = 0xFFFF_FFFF;

/// SHA-256's standard starting state.
const INITIAL: [u32; 8] = [
    0x6A09E667, 0xBB67AE85, 0x3C6EF372, 0xA54FF53A, 0x510E527F, 0x9B05688C, 0x1F83D9AB, 0x5BE0CD19,
];
/// The working registers after the hidden prefix's first 14 rounds.
const PREFIX_STATE: [u32; 8] = [
    0xFD9FBEA2, 0x91243CE7, 0xBE499094, 0xCD2E1E0A, 0x6C006F49, 0x04BD35AB, 0xFEB0A7E4, 0x03EFFBD2,
];
/// The hidden prefix's contributions to message words 16–29.
const PREFIX_SCHEDULE: [u32; 14] = [
    0xEB24BF9E, 0x1558691F, 0xA0C09EE2, 0x0CFA32BA, 0xA622EE8C, 0xD1DD9491, 0xC0DD25A0, 0xF9E70AB6,
    0xF0C31573, 0x7058CE4E, 0x7188E606, 0x00E5AB33, 0xD258B060, 0x72766564,
];
const K: [u32; 64] = [
    0x428A2F98, 0x71374491, 0xB5C0FBCF, 0xE9B5DBA5, 0x3956C25B, 0x59F111F1, 0x923F82A4, 0xAB1C5ED5,
    0xD807AA98, 0x12835B01, 0x243185BE, 0x550C7DC3, 0x72BE5D74, 0x80DEB1FE, 0x9BDC06A7, 0xC19BF174,
    0xE49B69C1, 0xEFBE4786, 0x0FC19DC6, 0x240CA1CC, 0x2DE92C6F, 0x4A7484AA, 0x5CB0A9DC, 0x76F988DA,
    0x983E5152, 0xA831C66D, 0xB00327C8, 0xBF597FC7, 0xC6E00BF3, 0xD5A79147, 0x06CA6351, 0x14292967,
    0x27B70A85, 0x2E1B2138, 0x4D2C6DFC, 0x53380D13, 0x650A7354, 0x766A0ABB, 0x81C2C92E, 0x92722C85,
    0xA2BFE8A1, 0xA81A664B, 0xC24B8B70, 0xC76C51A3, 0xD192E819, 0xD6990624, 0xF40E3585, 0x106AA070,
    0x19A4C116, 0x1E376C08, 0x2748774C, 0x34B0BCB5, 0x391C0CB3, 0x4ED8AA4A, 0x5B9CCA4F, 0x682E6FF3,
    0x748F82EE, 0x78A5636F, 0x84C87814, 0x8CC70208, 0x90BEFFFA, 0xA4506CEB, 0xBEF9A3F7, 0xC67178F2,
];
/// Round 14's constant in the block holding the prefix's end, with the
/// prefix's word folded in.
const ADJUSTMENT: u32 = 0xC9DC06A7;
const SIGMA0_ADJUSTMENT: u32 = 0x059C0B80;
const SIGMA1_ADJUSTMENT: u32 = 0x000B92C0;

const CS64_COEFFICIENTS: [(u32, u32); 14] = [
    (0xE82E91B1, 0xB32700D0),
    (0x44DE24C6, 0x88DFF51D),
    (0x3BEB65BC, 0x29514038),
    (0xC89C5933, 0xB328CBDF),
    (0x8D1EC211, 0xD405B4D5),
    (0x314EF5D5, 0x26E55F3C),
    (0xE92491B1, 0x607801C6),
    (0xF0A5B9D5, 0x82B2C1FB),
    (0x06064CF3, 0xFA51A3DA),
    (0xDA79FA60, 0x207CC464),
    (0xBED20560, 0x3C3C74F3),
    (0x89F9C9F8, 0xD2D14B61),
    (0x35FA38C6, 0xBB04A2C1),
    (0xCB49E3C6, 0xC5626167),
];
const CS64_FACTOR: u32 = 0x82893933;
const CS64_ADJUSTMENT0: u32 = 0xF2F90B1A;
const CS64_ADJUSTMENT1: u32 = 0x0B1A0000;

fn sigma0(x: u32) -> u32 {
    x.rotate_right(7) ^ x.rotate_right(18) ^ (x >> 3)
}

fn sigma1(x: u32) -> u32 {
    x.rotate_right(17) ^ x.rotate_right(19) ^ (x >> 10)
}

/// Fills message words `from..64` from the ones before.
fn expand(words: &mut Vec<u32>, from: usize) {
    for i in from..64 {
        let next = sigma1(words[i - 2])
            .wrapping_add(words[i - 7])
            .wrapping_add(sigma0(words[i - 15]))
            .wrapping_add(words[i - 16]);
        words.push(next);
    }
}

/// SHA-256's compression rounds `first..64` over `state`.
fn rounds(state: [u32; 8], words: &[u32], first: usize, constants: &[u32; 64]) -> [u32; 8] {
    let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = state;
    for i in first..64 {
        let t1 = h
            .wrapping_add(e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25))
            .wrapping_add(g ^ (e & (f ^ g)))
            .wrapping_add(constants[i])
            .wrapping_add(words[i]);
        let t2 = (a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22))
            .wrapping_add((a & b) | (c & (a | b)));
        (h, g, f, e, d, c, b, a) = (g, f, e, d.wrapping_add(t1), c, b, a, t1.wrapping_add(t2));
    }
    [a, b, c, d, e, f, g, h]
}

fn add(x: [u32; 8], y: [u32; 8]) -> [u32; 8] {
    std::array::from_fn(|i| x[i].wrapping_add(y[i]))
}

/// The state after the first block: the hidden prefix and `head`, the
/// text's first seven bytes.
fn first_block(head: &[u8]) -> [u32; 8] {
    let mut words = vec![0u32; 14];
    words.push(u32::from_be_bytes([0, head[0], head[1], head[2]]));
    words.push(u32::from_be_bytes([head[3], head[4], head[5], head[6]]));
    for i in 16..30 {
        let mut value = PREFIX_SCHEDULE[i - 16];
        let s1 = sigma1(words[i - 2]);
        value = value.wrapping_add(if i == 16 { s1 ^ SIGMA1_ADJUSTMENT } else { s1 });
        if i >= 21 {
            value = value.wrapping_add(words[i - 7]);
        }
        if i == 29 {
            value = value.wrapping_add(sigma0(words[i - 15]) ^ SIGMA0_ADJUSTMENT);
        }
        words.push(value);
    }
    words.push(
        sigma1(words[28])
            .wrapping_add(words[23])
            .wrapping_add(sigma0(words[15]))
            .wrapping_add(words[14])
            .wrapping_add(ADJUSTMENT)
            .wrapping_sub(K[14]),
    );
    expand(&mut words, 31);
    let mut constants = K;
    constants[14] = ADJUSTMENT;
    add(INITIAL, rounds(PREFIX_STATE, &words, 14, &constants))
}

/// SHA-256 of the hidden prefix followed by `text` (at least seven bytes).
fn digest(text: &[u8]) -> [u8; 32] {
    let length = 57 + text.len();
    let zeros = (55 - length as i64).rem_euclid(64) as usize;
    let mut padded = text[7..].to_vec();
    padded.push(0x80);
    padded.resize(padded.len() + zeros, 0);
    padded.extend_from_slice(&((length as u64) * 8).to_be_bytes());
    let mut state = first_block(&text[..7]);
    for block in padded.chunks(64) {
        let mut words: Vec<u32> = block
            .chunks(4)
            .map(|w| u32::from_be_bytes([w[0], w[1], w[2], w[3]]))
            .collect();
        expand(&mut words, 16);
        state = add(state, rounds(state, &words, 0, &K));
    }
    let mut out = [0u8; 32];
    for (chunk, word) in out.chunks_mut(4).zip(state) {
        chunk.copy_from_slice(&word.to_be_bytes());
    }
    out
}

fn rol16(x: u64) -> u64 {
    let x = x & MASK;
    ((x << 16) | (x >> 16)) & MASK
}

/// The 64-bit hash of `text`, keyed by its digest.
fn cs64(text: &[u8], digest: &[u8; 32]) -> (u32, u32) {
    let key_a = u32::from_le_bytes([digest[0], digest[1], digest[2], digest[3]]) as u64 | 1;
    let key_b = u32::from_le_bytes([digest[4], digest[5], digest[6], digest[7]]) as u64 | 1;
    let factors = [
        (key_a * CS64_FACTOR as u64) & MASK,
        (key_b * CS64_FACTOR as u64) & MASK,
    ];
    let mut terms: Vec<u64> = CS64_COEFFICIENTS
        .iter()
        .enumerate()
        .map(|(i, &(coefficient, _))| (coefficient as u64 * factors[i % 2]) & MASK)
        .collect();
    let mut offsets: Vec<u64> = CS64_COEFFICIENTS.iter().map(|&(_, o)| o as u64).collect();

    // Little-endian words of a zero byte and the text, padded to an even count.
    let mut bytes = vec![0u8];
    bytes.extend_from_slice(text);
    bytes.resize(bytes.len().div_ceil(4) * 4, 0);
    let mut words: Vec<u64> = bytes
        .chunks(4)
        .map(|w| u32::from_le_bytes([w[0], w[1], w[2], w[3]]) as u64)
        .collect();
    if words.len() % 2 == 1 {
        words.push(0);
    }
    for (i, word) in words.into_iter().enumerate() {
        let even = i % 2 == 0;
        let mut term = word * if even { key_a } else { key_b };
        let mut offset = rol16(word) * if even { 0xEF0569FB } else { 0xC31713DB };
        if i == 0 {
            term += CS64_ADJUSTMENT0 as u64 * factors[0];
            offset += CS64_ADJUSTMENT1 as u64;
        }
        terms.push(term & MASK);
        offsets.push(offset & MASK);
    }

    let (mut swap_acc, mut swap_last, mut state, mut acc) = (0u64, 0u64, 0u64, 0u64);
    for i in (0..terms.len()).step_by(2) {
        let (x, y) = (terms[i], terms[i + 1]);
        let t = rol16((offsets[i] + x) & MASK);
        let q = (t * 0x76F50F34) & MASK;
        swap_acc += (rol16(q) * 0xC3EFEA97 + q) & MASK;
        let t = rol16((offsets[i + 1] + y) & MASK);
        let q = (t * 0x1881CE3C) & MASK;
        swap_last = (rol16(q) * 0x35BD1EC9 + q) & MASK;
        swap_acc = (swap_acc + swap_last) & MASK;
        state = (state * key_a + x) & MASK;
        for factor in [0xCF98B111, 0x87085B9F, 0x12CEB96D, 0x257E1D83] {
            state = (rol16(state) * factor) & MASK;
        }
        acc += state;
        let mut t = (state * key_b + y) & MASK;
        t = (rol16(t) * 0xA27416F5) & MASK;
        t = (rol16(t) * 0xD38396FF) & MASK;
        state = (rol16((rol16(t) * 0x7C932B89) & MASK) * 0xBFA49F69) & MASK;
        acc = (acc + state) & MASK;
    }
    ((swap_last ^ state) as u32, (swap_acc ^ acc) as u32)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02X}")).collect()
}

/// The token for `text`, the time and client token joined.
pub fn token(text: &str) -> String {
    let text = text.as_bytes();
    let digest = digest(text);
    let (a, b) = cs64(text, &digest);
    let mut keyed = a.to_le_bytes().to_vec();
    keyed.extend_from_slice(&b.to_le_bytes());
    format!("{}-{}", hex(&keyed), hex(&digest))
}
