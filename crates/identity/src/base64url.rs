//! Decoding one segment of a compact JWS, and saying which one failed.
//!
//! Unpadded base64url is the only encoding a compact JWS may use, so a padded
//! segment is a refusal here rather than something the verifier has to notice.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;

use crate::vocabulary::Denied;

pub(crate) enum Segment {
    Header,
    Payload,
    Signature,
}

/// The other direction, for the one place that makes a token rather than
/// reads one ([`crate::issue`]). Unpadded, because a compact JWS may not
/// carry padding and a token that did would be refused by our own verifier.
pub(crate) fn b64url_encode(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

pub(crate) fn b64url(segment: &str, which: Segment) -> Result<Vec<u8>, Denied> {
    URL_SAFE_NO_PAD.decode(segment).map_err(|_| match which {
        Segment::Header => Denied::Malformed("header is not base64url"),
        Segment::Payload => Denied::Malformed("payload is not base64url"),
        Segment::Signature => Denied::Malformed("signature is not base64url"),
    })
}
