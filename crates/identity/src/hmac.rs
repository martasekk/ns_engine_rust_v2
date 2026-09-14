//! The two primitives every credential check in this crate is built from.
//!
//! Neither knows what a tenant, a token or a channel is: they take bytes and
//! return bytes, and are proven against the published vectors below.

use hmac::{Hmac, Mac};
use sha2::Sha256;

/// HMAC-SHA256 (RFC 2104), proven against the RFC 4231 vectors in the tests.
pub fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC takes a key of any length");
    mac.update(message);
    mac.finalize().into_bytes().into()
}

/// Equality that does not stop at the first differing byte. The lengths are
/// not secret; the bytes are.
pub(crate) fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    fn hmac_matches_the_rfc_4231_vectors() {
        // RFC 4231 section 4, cases 1-4, 6 and 7. Case 5 is a truncation case
        // and is checked on its first 128 bits.
        let cases: &[(Vec<u8>, Vec<u8>, &str)] = &[
            (
                vec![0x0b; 20],
                b"Hi There".to_vec(),
                "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7",
            ),
            (
                b"Jefe".to_vec(),
                b"what do ya want for nothing?".to_vec(),
                "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843",
            ),
            (
                vec![0xaa; 20],
                vec![0xdd; 50],
                "773ea91e36800e46854db8ebd09181a72959098b3ef8c122d9635514ced565fe",
            ),
            (
                (1u8..=25).collect(),
                vec![0xcd; 50],
                "82558a389a443c0ea4cc819899f2083a85f0faa3e578f8077a2e3ff46729665b",
            ),
            (
                vec![0xaa; 131],
                b"Test Using Larger Than Block-Size Key - Hash Key First".to_vec(),
                "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54",
            ),
            (
                vec![0xaa; 131],
                b"This is a test using a larger than block-size key and a larger than block-size data. The key needs to be hashed before being used by the HMAC algorithm.".to_vec(),
                "9b09ffa71b942fcb27635fbcd5b0e944bfdc63644f0713938a7f51535c3a35e2",
            ),
        ];
        for (i, (key, data, want)) in cases.iter().enumerate() {
            assert_eq!(
                hex(&hmac_sha256(key, data)),
                *want,
                "RFC 4231 case {}",
                i + 1
            );
        }
        // Case 5, truncated to 128 bits.
        assert_eq!(
            hex(&hmac_sha256(&[0x0c; 20], b"Test With Truncation")[..16]),
            "a3b6167473100ee06e0c796c2955552b"
        );
    }
}
