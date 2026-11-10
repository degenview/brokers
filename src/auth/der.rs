//! A tiny DER reader: enough to take a P-256 key out of its SEC1 wrapper. Not a general parser.

/// One tag-length-value, plus what follows it.
pub struct Tlv<'a> {
    pub tag: u8,
    pub content: &'a [u8],
    pub rest: &'a [u8],
}

/// Reads one element from the front of `input`. `None` on any malformed or truncated length.
pub fn read(input: &[u8]) -> Option<Tlv<'_>> {
    let (&tag, after_tag) = input.split_first()?;
    let (&first, after_first) = after_tag.split_first()?;
    let (length, body) = if first < 0x80 {
        (usize::from(first), after_first)
    } else {
        let count = usize::from(first & 0x7f);
        if count == 0 || count > 4 || after_first.len() < count {
            return None;
        }
        let length = after_first[..count]
            .iter()
            .fold(0_usize, |acc, byte| (acc << 8) | usize::from(*byte));
        (length, &after_first[count..])
    };
    if body.len() < length {
        return None;
    }
    Some(Tlv {
        tag,
        content: &body[..length],
        rest: &body[length..],
    })
}

pub const SEQUENCE: u8 = 0x30;
pub const INTEGER: u8 = 0x02;
pub const OCTET_STRING: u8 = 0x04;
pub const BIT_STRING: u8 = 0x03;
/// Context-specific, constructed, tag 0 and 1 (the `[0]` parameters and `[1]` public key of
/// RFC 5915).
pub const CONTEXT_0: u8 = 0xa0;
pub const CONTEXT_1: u8 = 0xa1;

/// DER of the named-curve OID prime256v1 (1.2.840.10045.3.1.7).
pub const OID_PRIME256V1: &[u8] = &[0x06, 0x08, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07];

/// The private scalar and uncompressed public point of an RFC 5915 `ECPrivateKey` on P-256.
pub struct EcKey<'a> {
    pub private: &'a [u8],
    pub public: &'a [u8],
}

/// Why a SEC1 key was refused, in words that name no key material.
#[derive(Debug, Eq, PartialEq)]
pub enum Sec1Error {
    Malformed,
    /// Only P-256 keys are accepted.
    WrongCurve,
    /// The key carries no public point; the signer needs it.
    NoPublicKey,
}

pub fn parse_sec1_p256(der: &[u8]) -> Result<EcKey<'_>, Sec1Error> {
    let outer = read(der)
        .filter(|t| t.tag == SEQUENCE)
        .ok_or(Sec1Error::Malformed)?;
    let version = read(outer.content)
        .filter(|t| t.tag == INTEGER && t.content == [1])
        .ok_or(Sec1Error::Malformed)?;
    let private = read(version.rest)
        .filter(|t| t.tag == OCTET_STRING)
        .ok_or(Sec1Error::Malformed)?;
    let mut rest = private.rest;
    let mut public = None;
    while let Some(element) = read(rest) {
        match element.tag {
            CONTEXT_0 => {
                // The parameters must name P-256 if they are present.
                if element.content != OID_PRIME256V1 {
                    return Err(Sec1Error::WrongCurve);
                }
            }
            CONTEXT_1 => {
                let bits = read(element.content)
                    .filter(|t| t.tag == BIT_STRING)
                    .ok_or(Sec1Error::Malformed)?;
                // First content byte is the count of unused bits, always 0 here.
                match bits.content.split_first() {
                    Some((0, point)) => public = Some(point),
                    _ => return Err(Sec1Error::Malformed),
                }
            }
            _ => {}
        }
        rest = element.rest;
    }
    let public = public.ok_or(Sec1Error::NoPublicKey)?;
    if private.content.len() != 32 || public.len() != 65 || public[0] != 0x04 {
        return Err(Sec1Error::WrongCurve);
    }
    Ok(EcKey {
        private: private.content,
        public,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_short_and_long_lengths_and_rejects_truncation() {
        let short = [0x04, 0x02, 0xaa, 0xbb, 0xcc];
        let tlv = read(&short).unwrap();
        assert_eq!(
            (tlv.tag, tlv.content, tlv.rest),
            (0x04, &[0xaa, 0xbb][..], &[0xcc][..])
        );
        let mut long = vec![0x04, 0x81, 0x80];
        long.extend(std::iter::repeat_n(7_u8, 0x80));
        assert_eq!(read(&long).unwrap().content.len(), 0x80);
        assert!(read(&[0x04, 0x05, 1, 2]).is_none());
        assert!(read(&[0x04, 0x80]).is_none());
        assert!(read(&[]).is_none());
    }

    #[test]
    fn garbage_is_not_a_sec1_key() {
        assert_eq!(
            parse_sec1_p256(&[1, 2, 3]).err(),
            Some(Sec1Error::Malformed)
        );
        assert_eq!(
            parse_sec1_p256(&[0x30, 0x00]).err(),
            Some(Sec1Error::Malformed)
        );
    }
}
