//! Base64 (RFC 4648): the standard alphabet with or without padding, and the URL-safe one without.
//! Decoding is strict: no whitespace, no misplaced padding, zero trailing bits.

use super::{Error, Result};

/// Standard alphabet without padding (Go `base64.RawStdEncoding`): the handshake salt.
pub(crate) const RAW_STD: Base64 = Base64 {
    url: false,
    pad: false,
};
/// URL-safe alphabet without padding (Go `base64.RawURLEncoding`): JWS segments and JWK numbers.
pub(crate) const RAW_URL: Base64 = Base64 {
    url: true,
    pad: false,
};
/// Standard alphabet with padding (Go `base64.StdEncoding`): keys and client data.
pub(crate) const STD: Base64 = Base64 {
    url: false,
    pad: true,
};

/// One base64 variant: an alphabet and whether the text ends in `=` padding.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Base64 {
    url: bool,
    pad: bool,
}

impl Base64 {
    /// The bytes `text` decodes to, in a buffer of exactly their length.
    /// Output past `max` is an error, found before anything is allocated.
    pub(crate) fn decode(self, text: &str, max: usize) -> Result<Vec<u8>> {
        let symbols = self.symbols(text.as_bytes())?;
        let len = output_len(symbols);
        if len > max {
            return Err(Error::TooLarge);
        }
        let mut out = Vec::with_capacity(len);
        self.walk(symbols, |b| out.push(b))?;
        Ok(out)
    }

    /// The decoded length of `text`, checking it without allocating.
    pub(crate) fn decoded_len(self, text: &str) -> Result<usize> {
        let symbols = self.symbols(text.as_bytes())?;
        self.walk(symbols, |_| {})?;
        Ok(output_len(symbols))
    }

    /// The encoding of `data`.
    pub(crate) fn encode(self, data: &[u8]) -> String {
        let mut out = String::new();
        self.encode_into(data, &mut out);
        out
    }

    /// Appends the encoding of `data` to `out`.
    pub(crate) fn encode_into(self, data: &[u8], out: &mut String) {
        out.reserve(data.len().div_ceil(3).saturating_mul(4));
        let (groups, rest) = data.as_chunks::<3>();
        for &group in groups {
            self.push_group(group, 4, out);
        }
        match *rest {
            [a] => self.push_group([a, 0, 0], 2, out),
            [a, b] => self.push_group([a, b, 0], 3, out),
            _ => {}
        }
    }

    fn push_group(self, [a, b, c]: [u8; 3], symbols: u32, out: &mut String) {
        let group = u32::from_be_bytes([0, a, b, c]);
        for i in 0..4 {
            if i < symbols {
                let [.., six] = (group >> (18 - 6 * i) & 0x3f).to_be_bytes();
                out.push(char::from(self.symbol(six)));
            } else if self.pad {
                out.push('=');
            }
        }
    }

    const fn symbol(self, value: u8) -> u8 {
        match value {
            0..=25 => b'A' + value,
            26..=51 => b'a' + (value - 26),
            52..=61 => b'0' + (value - 52),
            62 if self.url => b'-',
            62 => b'+',
            _ if self.url => b'_',
            _ => b'/',
        }
    }

    /// The symbols of `text` without its padding.
    /// A length no encoding has is an error; a misplaced `=` is left in and fails as a symbol.
    fn symbols(self, text: &[u8]) -> Result<&[u8]> {
        let symbols = if self.pad {
            if !text.len().is_multiple_of(4) {
                return Err(Error::Base64);
            }
            match text {
                [symbols @ .., b'=', b'='] | [symbols @ .., b'='] => symbols,
                _ => text,
            }
        } else {
            text
        };
        if symbols.len() % 4 == 1 {
            return Err(Error::Base64);
        }
        Ok(symbols)
    }

    /// Validates `symbols` and passes each decoded byte to `out`.
    fn walk(self, symbols: &[u8], mut out: impl FnMut(u8)) -> Result<()> {
        for chunk in symbols.chunks(4) {
            let mut group = 0_u32;
            for &symbol in chunk {
                let value = self.value(symbol).ok_or(Error::Base64)?;
                group = group << 6 | u32::from(value);
            }
            match chunk.len() {
                4 => {
                    let [_, a, b, c] = group.to_be_bytes();
                    out(a);
                    out(b);
                    out(c);
                }
                3 if group.trailing_zeros() >= 2 => {
                    let [_, _, a, b] = (group >> 2).to_be_bytes();
                    out(a);
                    out(b);
                }
                2 if group.trailing_zeros() >= 4 => {
                    let [.., a] = (group >> 4).to_be_bytes();
                    out(a);
                }
                _ => return Err(Error::Base64),
            }
        }
        Ok(())
    }

    const fn value(self, symbol: u8) -> Option<u8> {
        match symbol {
            b'A'..=b'Z' => Some(symbol - b'A'),
            b'a'..=b'z' => Some(symbol - b'a' + 26),
            b'0'..=b'9' => Some(symbol - b'0' + 52),
            b'-' if self.url => Some(62),
            b'_' if self.url => Some(63),
            b'+' if !self.url => Some(62),
            b'/' if !self.url => Some(63),
            _ => None,
        }
    }
}

/// The bytes `symbols` decode to.
/// That is three per four symbols, and one fewer than the symbols of a short last group.
fn output_len(symbols: &[u8]) -> usize {
    symbols.len() / 4 * 3 + (symbols.len() % 4).saturating_sub(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodings_match_rfc_4648_vectors() {
        let vectors = [
            ("", "", ""),
            ("f", "Zg==", "Zg"),
            ("fo", "Zm8=", "Zm8"),
            ("foo", "Zm9v", "Zm9v"),
            ("foob", "Zm9vYg==", "Zm9vYg"),
            ("fooba", "Zm9vYmE=", "Zm9vYmE"),
            ("foobar", "Zm9vYmFy", "Zm9vYmFy"),
        ];
        for (plain, padded, raw) in vectors {
            assert_eq!(STD.encode(plain.as_bytes()), padded);
            assert_eq!(RAW_STD.encode(plain.as_bytes()), raw);
            assert_eq!(RAW_URL.encode(plain.as_bytes()), raw);
            assert_eq!(STD.decode(padded, 64).unwrap(), plain.as_bytes());
            assert_eq!(RAW_STD.decode(raw, 64).unwrap(), plain.as_bytes());
            assert_eq!(STD.decoded_len(padded).unwrap(), plain.len());
        }
        assert_eq!(STD.encode(&[0xfb, 0xff]), "+/8=");
        assert_eq!(RAW_URL.encode(&[0xfb, 0xff]), "-_8");
        assert_eq!(RAW_URL.decode("-_8", 8).unwrap(), [0xfb, 0xff]);
    }

    #[test]
    fn malformed_text_is_rejected() {
        for bad in [
            "Zg", "Zg=", "Zg===", "Z===", "Zh==", "Zm9", "Zm8=Zm8=", "Zm 9v", "Zm9v\n", "-_8=",
        ] {
            assert!(STD.decode(bad, 64).is_err(), "{bad}");
        }
        for bad in ["Zg==", "Z", "Zh", "Zm9vY", "+/8", "Zm9=v"] {
            assert!(RAW_URL.decode(bad, 64).is_err(), "{bad}");
        }
        assert!(RAW_STD.decode("-_8", 8).is_err());
    }

    #[test]
    fn output_beyond_the_limit_is_rejected_before_allocating() {
        assert!(matches!(STD.decode("Zm9vYmFy", 5), Err(Error::TooLarge)));
        assert_eq!(STD.decode("Zm9vYmFy", 6).unwrap(), b"foobar");
        let huge = "A".repeat(4000);
        assert!(matches!(RAW_URL.decode(&huge, 100), Err(Error::TooLarge)));
    }

    #[test]
    fn decoding_allocates_the_output_and_not_the_limit() {
        assert_eq!(STD.decode("Zm9vYmFy", 100).unwrap().capacity(), 6);
        assert_eq!(STD.decode("Zm9vYg==", 100).unwrap().capacity(), 4);
        assert_eq!(RAW_URL.decode("Zm9vYg", 100).unwrap().capacity(), 4);
    }

    #[test]
    fn round_trips_every_length() {
        let data: Vec<u8> = (0..=255).collect();
        for len in 0..data.len() {
            for b64 in [STD, RAW_STD, RAW_URL] {
                let text = b64.encode(&data[..len]);
                assert_eq!(b64.decode(&text, len).unwrap(), &data[..len]);
                assert_eq!(b64.decoded_len(&text).unwrap(), len);
            }
        }
    }
}
