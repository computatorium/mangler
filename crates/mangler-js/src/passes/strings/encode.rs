//! Pure encoding pipeline for the string-obfuscation pass (ported from the legacy
//! `lang::js::passes::strings::encode`).
//!
//! ## Wire format (per entry, bytes)
//!
//! ```text
//! [ prefix_len: 1 byte ][ prefix_len random bytes ]
//! [ payload_len: varint                            ]
//! [ payload XOR'd with key_i (payload_len bytes)   ]
//! [ suffix_len: 1 byte ][ suffix_len random bytes ]
//! ```
//!
//! ## Cross-reference XOR — DAG scheme (Layer A)
//!
//! Each entry `i` references some earlier entry `refs[i] < i`; because the
//! reference graph is a DAG, encoding is a single forward pass. `refs[0] == 0` is
//! a sentinel meaning "no cross-ref".
//!
//! ```text
//! For entry i with reference m = refs[i]:
//!   * If i == 0: key_byte(0, j) = base_key[j % bk_len].   (no cross-ref)
//!   * Else:      key_byte(i, j) = base_key[j % bk_len] XOR raw[m][j % raw[m].len()].
//! ```
//!
//! Randomness comes from the per-pass [`Rng`] (replacing the legacy
//! `PassContext`), so the output is byte-identical for a given (seed, corpus).

use mangler_core::Rng;

/// Length of the runtime-key keystream derived by [`derive_runtime_key`].
pub const RUNTIME_KEY_LEN: usize = 17;

/// One encoded entry.
#[derive(Debug, Clone)]
pub struct EncodedEntry {
    /// Base64 of the raw bytes — what ships in the JS array literal.
    pub b64: String,
    /// The raw (pre-base64) bytes — what the cross-ref derivation reads.
    pub raw: Vec<u8>,
}

/// Bundle returned by [`encode_entries`]: the per-entry encoded bytes plus the DAG
/// reference table the decoder must mirror.
#[derive(Debug, Clone)]
pub struct EncodedBlob {
    pub entries: Vec<EncodedEntry>,
    /// `refs[i]` is the index of the entry that XOR-keys entry i. `refs[0] == 0` is
    /// a sentinel ("no cross-ref"); both encoder and decoder skip it.
    pub refs: Vec<u32>,
}

/// Parameters that fully determine encoding output given the plaintexts.
#[derive(Debug, Clone)]
pub struct EncodingParams {
    /// XOR base key. Length must be >= 1.
    pub base_key: Vec<u8>,
    /// 0..=255 — controls maximum junk bytes per side (`max_junk = junk_rate / 4`).
    pub junk_rate: u8,
    /// Build-time keystream from a dynamic key's `expected` value, XOR'd into every
    /// payload byte. Empty = no runtime binding (byte-identical to a non-dynamic build).
    pub runtime_key: Vec<u8>,
}

/// Standard-alphabet base64 encode (no line wrapping), mirroring JS `btoa`/`atob`.
///
/// Hand-rolled because the `base64` crate is not a dependency of this crate and the
/// file-ownership rules forbid editing `Cargo.toml`. Deterministic and total.
pub fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[((n >> 18) & 0x3F) as usize] as char);
        out.push(ALPHABET[((n >> 12) & 0x3F) as usize] as char);
        if chunk.len() > 1 {
            out.push(ALPHABET[((n >> 6) & 0x3F) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[(n & 0x3F) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

/// Deterministic keystream ([`RUNTIME_KEY_LEN`] bytes) derived from `value`,
/// mirrored byte-for-byte by the JS the stub emits when a dynamic key is
/// configured: a per-lane FNV-style fold over the UTF-16 code units of `value`.
/// Rust `wrapping_mul` matches JS `Math.imul`, `>> 13` on `u32` matches `>>> 13`.
pub fn derive_runtime_key(value: &str) -> Vec<u8> {
    let units: Vec<u32> = value.encode_utf16().map(u32::from).collect();
    let mut out = vec![0u8; RUNTIME_KEY_LEN];
    for (lane, slot) in out.iter_mut().enumerate() {
        let mut h: u32 = 2166136261u32.wrapping_add((lane as u32).wrapping_mul(2654435761));
        for (p, &c) in units.iter().enumerate() {
            let mixed = c.wrapping_add(p as u32).wrapping_add(lane as u32) & 0xFFFF;
            h ^= mixed;
            h = h.wrapping_mul(16777619);
            h ^= h >> 13;
        }
        *slot = (h & 0xFF) as u8;
    }
    out
}

/// LEB128 / 7-bit-continuation varint encode.
pub(crate) fn varint_encode(mut n: u64, out: &mut Vec<u8>) {
    loop {
        let b = (n & 0x7F) as u8;
        n >>= 7;
        if n == 0 {
            out.push(b);
            return;
        }
        out.push(b | 0x80);
    }
}

/// LEB128 varint decode. Returns `(value, bytes_consumed)`. Test-only mirror.
#[cfg(test)]
pub(crate) fn varint_decode(bytes: &[u8]) -> Option<(u64, usize)> {
    let mut v: u64 = 0;
    let mut shift: u32 = 0;
    for (i, b) in bytes.iter().enumerate() {
        let chunk = (*b & 0x7F) as u64;
        if shift >= 64 {
            return None;
        }
        v |= chunk << shift;
        if *b & 0x80 == 0 {
            return Some((v, i + 1));
        }
        shift += 7;
    }
    None
}

/// Build one entry with payload XOR'd against `base_key XOR ref_full XOR runtime_key`.
/// For `i == 0`, `ref_full` is empty. An empty `runtime_key` contributes nothing.
fn build_entry(
    plaintext: &str,
    prefix: &[u8],
    suffix: &[u8],
    base_key: &[u8],
    ref_full: &[u8],
    runtime_key: &[u8],
) -> Vec<u8> {
    let pt = plaintext.as_bytes();
    let mut raw = Vec::with_capacity(1 + prefix.len() + 10 + pt.len() + 1 + suffix.len());
    raw.push(prefix.len() as u8);
    raw.extend_from_slice(prefix);
    varint_encode(pt.len() as u64, &mut raw);
    let bk_len = base_key.len();
    let rl = ref_full.len();
    let rk_len = runtime_key.len();
    for (j, b) in pt.iter().enumerate() {
        let bk_byte = base_key[j % bk_len];
        let ref_byte = if rl == 0 { 0 } else { ref_full[j % rl] };
        let rk_byte = if rk_len == 0 {
            0
        } else {
            runtime_key[j % rk_len]
        };
        raw.push(*b ^ bk_byte ^ ref_byte ^ rk_byte);
    }
    raw.push(suffix.len() as u8);
    raw.extend_from_slice(suffix);
    raw
}

/// Encode every plaintext into its wire-format entry in a single forward pass.
///
/// Deterministic given `rng`: the DAG `refs` table and every junk prefix/suffix are
/// drawn from the seeded RNG, so the same (seed, corpus) yields byte-identical
/// output.
pub fn encode_entries(
    plaintexts: &[String],
    params: &EncodingParams,
    rng: &mut Rng,
) -> EncodedBlob {
    assert!(!params.base_key.is_empty(), "base_key must be non-empty");
    let n = plaintexts.len();
    if n == 0 {
        return EncodedBlob {
            entries: Vec::new(),
            refs: Vec::new(),
        };
    }

    // junk_rate=0 -> max_junk=0 (no junk). junk_rate=255 -> max_junk=64.
    let max_junk = (params.junk_rate as usize).div_ceil(4);

    // 1) DAG refs: refs[0] = 0 (sentinel); refs[i>0] in [0, i).
    let mut refs: Vec<u32> = Vec::with_capacity(n);
    refs.push(0);
    for i in 1..n {
        let r = rng.pick(i);
        refs.push(r as u32);
    }

    // 2) Single forward pass: encode entry i using FINAL bytes of entry refs[i].
    let mut raws: Vec<Vec<u8>> = Vec::with_capacity(n);
    for (i, pt) in plaintexts.iter().enumerate() {
        let prefix_len = if max_junk == 0 {
            0
        } else {
            rng.pick(max_junk + 1)
        };
        let suffix_len = if max_junk == 0 {
            0
        } else {
            rng.pick(max_junk + 1)
        };
        let prefix = rng.random_bytes(prefix_len);
        let suffix = rng.random_bytes(suffix_len);

        let ref_full: &[u8] = if i == 0 { &[] } else { &raws[refs[i] as usize] };
        let raw = build_entry(
            pt,
            &prefix,
            &suffix,
            &params.base_key,
            ref_full,
            &params.runtime_key,
        );
        raws.push(raw);
    }

    let entries = raws
        .into_iter()
        .map(|raw| EncodedEntry {
            b64: base64_encode(&raw),
            raw,
        })
        .collect();
    EncodedBlob { entries, refs }
}

/// B2 — runtime-derived base-key mask. The decoder ships `base_key XOR mask` and
/// reconstructs `mask` at runtime by folding over the base_key-INDEPENDENT
/// `refs`/`lutP`/`lutS` tables, so the true key is never a flat literal.
///
/// Per output byte `k`, over `stream = refs ++ lutP ++ lutS`:
/// ```text
///   acc = (k * 0x9E + 1) & 0xFF
///   for p, x in enumerate(stream): acc = (acc + ((x + p + k) & 0xFF)) & 0xFF
///   mask[k] = acc
/// ```
pub fn derive_key_mask(len: usize, refs: &[u32], lut_p: &[u32], lut_s: &[u32]) -> Vec<u8> {
    let stream: Vec<u32> = refs
        .iter()
        .chain(lut_p.iter())
        .chain(lut_s.iter())
        .copied()
        .collect();
    let mut out = vec![0u8; len];
    for (k, slot) in out.iter_mut().enumerate() {
        let mut acc: u32 = ((k as u32).wrapping_mul(0x9E).wrapping_add(1)) & 0xFF;
        for (p, &x) in stream.iter().enumerate() {
            let term = (x.wrapping_add(p as u32).wrapping_add(k as u32)) & 0xFF;
            acc = acc.wrapping_add(term) & 0xFF;
        }
        *slot = acc as u8;
    }
    out
}

/// Apply [`derive_key_mask`] to obfuscate `base_key` for shipping: returns
/// `base_key[k] XOR mask[k]`. The decoder reverses this at runtime (XOR is its own
/// inverse).
pub fn mask_base_key(base_key: &[u8], refs: &[u32], lut_p: &[u32], lut_s: &[u32]) -> Vec<u8> {
    let mask = derive_key_mask(base_key.len(), refs, lut_p, lut_s);
    base_key
        .iter()
        .zip(mask.iter())
        .map(|(b, m)| b ^ m)
        .collect()
}

/// DJB2 hash (`h = (h*33 + x) >>> 0`, seeded `5381`) over the concatenated
/// decode-critical integer tables `refs ++ lutP ++ lutS` (anti-tamper). Byte-exact
/// mirror of the JS fold the stub emits.
pub fn djb2_nums(refs: &[u32], lut_p: &[u32], lut_s: &[u32]) -> u32 {
    let mut h: u32 = 5381;
    for &x in refs.iter().chain(lut_p.iter()).chain(lut_s.iter()) {
        h = h.wrapping_mul(33).wrapping_add(x);
    }
    h
}

/// Test-only Rust mirror of the JS decoder.
#[cfg(test)]
pub fn decode_entry_rust(blob: &EncodedBlob, i: usize, params: &EncodingParams) -> String {
    let raw = &blob.entries[i].raw;
    let prefix_len = raw[0] as usize;
    let cursor = 1 + prefix_len;
    let (payload_len, vbytes) = varint_decode(&raw[cursor..]).expect("varint");
    let payload_start = cursor + vbytes;
    let payload_len = payload_len as usize;

    let bk = &params.base_key;
    let bk_len = bk.len();
    let rk = &params.runtime_key;
    let rk_len = rk.len();
    let rk_byte = |j: usize| if rk_len == 0 { 0 } else { rk[j % rk_len] };

    let mut out = Vec::with_capacity(payload_len);
    if i == 0 {
        for j in 0..payload_len {
            out.push(raw[payload_start + j] ^ bk[j % bk_len] ^ rk_byte(j));
        }
    } else {
        let ref_raw = &blob.entries[blob.refs[i] as usize].raw;
        let rl = ref_raw.len();
        for j in 0..payload_len {
            let key_byte = bk[j % bk_len] ^ ref_raw[j % rl] ^ rk_byte(j);
            out.push(raw[payload_start + j] ^ key_byte);
        }
    }
    String::from_utf8(out).expect("payload was not valid UTF-8")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build_params(seed: u64, junk_rate: u8) -> (EncodingParams, Rng) {
        let mut rng = Rng::for_pass(seed, "strings");
        let base_key = rng.random_bytes(17);
        (
            EncodingParams {
                base_key,
                junk_rate,
                runtime_key: Vec::new(),
            },
            rng,
        )
    }

    #[test]
    fn base64_matches_known_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64_encode(&[0xFF, 0xFE, 0xFD]), "//79");
    }

    #[test]
    fn varint_round_trip() {
        for v in [0u64, 1, 127, 128, 16_383, u32::MAX as u64, u64::MAX] {
            let mut buf = Vec::new();
            varint_encode(v, &mut buf);
            let (decoded, used) = varint_decode(&buf).expect("decode");
            assert_eq!(decoded, v);
            assert_eq!(used, buf.len());
        }
    }

    #[test]
    fn derive_runtime_key_is_deterministic_and_value_sensitive() {
        let a = derive_runtime_key("example.com");
        assert_eq!(a.len(), RUNTIME_KEY_LEN);
        assert_eq!(a, derive_runtime_key("example.com"));
        assert_ne!(a, derive_runtime_key("example.org"));
        let empty = derive_runtime_key("");
        assert!(empty.iter().any(|&b| b != 0));
    }

    #[test]
    fn encode_then_decode_pure_rust_round_trip() {
        let corpus: Vec<String> = ["hello", "world", "Φωνή", "{\"a\":1}", "", "x"]
            .iter()
            .map(|s| (*s).to_string())
            .collect();
        for junk_rate in [0u8, 64, 200] {
            let (params, mut rng) = build_params(42, junk_rate);
            let blob = encode_entries(&corpus, &params, &mut rng);
            assert_eq!(blob.refs[0], 0);
            for (i, reference) in blob.refs.iter().enumerate().take(corpus.len()).skip(1) {
                assert!((*reference as usize) < i);
            }
            for (i, original) in corpus.iter().enumerate() {
                assert_eq!(&decode_entry_rust(&blob, i, &params), original, "entry {i}");
            }
        }
    }

    #[test]
    fn runtime_key_round_trips_with_matching_key() {
        let corpus: Vec<String> = ["getContext", "webgl2", "Φωνή", ""]
            .iter()
            .map(|s| (*s).to_string())
            .collect();
        let mut rng = Rng::for_pass(7, "strings");
        let base_key = rng.random_bytes(17);
        let params = EncodingParams {
            base_key,
            junk_rate: 0,
            runtime_key: derive_runtime_key("example.com"),
        };
        let blob = encode_entries(&corpus, &params, &mut rng);
        for (i, original) in corpus.iter().enumerate() {
            assert_eq!(&decode_entry_rust(&blob, i, &params), original, "entry {i}");
        }
    }

    #[test]
    fn same_seed_same_encoded_output() {
        let corpus: Vec<String> = vec!["alpha".into(), "beta".into(), "gamma".into()];
        let (pa, mut ra) = build_params(99, 100);
        let (pb, mut rb) = build_params(99, 100);
        assert_eq!(pa.base_key, pb.base_key);
        let a = encode_entries(&corpus, &pa, &mut ra);
        let b = encode_entries(&corpus, &pb, &mut rb);
        let ba: Vec<&str> = a.entries.iter().map(|e| e.b64.as_str()).collect();
        let bb: Vec<&str> = b.entries.iter().map(|e| e.b64.as_str()).collect();
        assert_eq!(ba, bb);
        assert_eq!(a.refs, b.refs);
    }

    #[test]
    fn masked_key_round_trips() {
        let bk: Vec<u8> = (0..17u8).collect();
        let refs = [0u32, 0, 1];
        let lut_p = [0u32, 1, 0];
        let lut_s = [0u32, 0, 1];
        let masked = mask_base_key(&bk, &refs, &lut_p, &lut_s);
        let mask = derive_key_mask(bk.len(), &refs, &lut_p, &lut_s);
        let recovered: Vec<u8> = masked.iter().zip(mask.iter()).map(|(m, k)| m ^ k).collect();
        assert_eq!(recovered, bk);
    }

    #[test]
    fn empty_input_yields_empty_output() {
        let (params, mut rng) = build_params(7, 64);
        let blob = encode_entries(&[], &params, &mut rng);
        assert!(blob.entries.is_empty());
        assert!(blob.refs.is_empty());
    }
}
