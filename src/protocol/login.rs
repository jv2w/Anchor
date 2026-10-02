//! The envelope of Login's connection request: two little-endian int32 length-prefixed strings
//! (gophertunnel `login.parseLoginRequest` and `encodeRequest`). The JWTs inside are verified in
//! the session (PLAN.md 3), not here.

use super::io::{Codec, Error, Reader, Result, Writer, length};

/// The envelope of the [`Login`](super::packet::Login) packet's `connection_request` bytes.
///
/// The JWTs inside are not verified here; the session verifies them before trusting any claim.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ConnectionRequest {
    /// JSON text with the certificate chain, the authentication type and the token.
    pub chain: String,
    /// The client data JWT.
    pub raw_token: String,
}

impl ConnectionRequest {
    /// Reads a whole connection request, such as `Login::connection_request`. An empty chain or
    /// bytes after the raw token are an error, like in gophertunnel. Allocation is bounded by the
    /// input length.
    pub fn parse(data: &[u8]) -> Result<Self> {
        let mut r = Reader::new(data, data.len(), 0);
        let request = Self::decode(&mut r)?;
        if !r.remaining().is_empty() {
            return Err(Error::InvalidValue {
                field: "connection request",
                reason: "bytes after the raw token",
            });
        }
        Ok(request)
    }
}

impl Codec for ConnectionRequest {
    fn decode(r: &mut Reader<'_>) -> Result<Self> {
        let chain_len = r.int32()?;
        if chain_len == 0 {
            return Err(Error::InvalidValue {
                field: "connection request chain",
                reason: "empty",
            });
        }
        let chain = r.utf8(u64::try_from(chain_len).map_err(|_| Error::Size)?)?;
        let token_len = u64::try_from(r.int32()?).map_err(|_| Error::Size)?;
        let raw_token = r.utf8(token_len)?;
        Ok(Self { chain, raw_token })
    }

    fn encode(&self, w: &mut Writer) -> Result<()> {
        if self.chain.is_empty() {
            return Err(Error::InvalidValue {
                field: "connection request chain",
                reason: "empty",
            });
        }
        for part in [&self.chain, &self.raw_token] {
            w.int32(length(part.len())?);
            w.bytes(part.as_bytes());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::fuzz;

    fn sample() -> ConnectionRequest {
        ConnectionRequest {
            chain: "{}".to_owned(),
            raw_token: "t".to_owned(),
        }
    }

    #[test]
    fn requests_match_gophertunnel_bytes() {
        let mut w = Writer::new(Vec::new(), 0);
        sample().encode(&mut w).unwrap();
        let bytes = w.into_inner();
        assert_eq!(bytes, [2, 0, 0, 0, b'{', b'}', 1, 0, 0, 0, b't']);
        assert_eq!(ConnectionRequest::parse(&bytes), Ok(sample()));
        let no_token = [2, 0, 0, 0, b'{', b'}', 0, 0, 0, 0];
        assert!(ConnectionRequest::parse(&no_token).is_ok());
    }

    #[test]
    fn malformed_envelopes_are_rejected() {
        let trailing = [2, 0, 0, 0, b'{', b'}', 0, 0, 0, 0, 9];
        assert!(ConnectionRequest::parse(&trailing).is_err());
        assert!(ConnectionRequest::parse(&[0, 0, 0, 0, 0, 0, 0, 0]).is_err());
        let negative = [0xff, 0xff, 0xff, 0xff];
        assert_eq!(ConnectionRequest::parse(&negative), Err(Error::Size));
        let too_long = [9, 0, 0, 0, b'{', b'}'];
        assert_eq!(ConnectionRequest::parse(&too_long), Err(Error::Size));
        let negative_token = [1, 0, 0, 0, b'x', 0xff, 0xff, 0xff, 0xff];
        assert_eq!(ConnectionRequest::parse(&negative_token), Err(Error::Size));
        let bad_utf8 = [1, 0, 0, 0, 0xff, 0, 0, 0, 0];
        assert_eq!(ConnectionRequest::parse(&bad_utf8), Err(Error::InvalidUtf8));
        let empty = ConnectionRequest::default();
        assert!(empty.encode(&mut Writer::new(Vec::new(), 0)).is_err());
    }

    #[test]
    fn mutated_input_never_panics() {
        let mut w = Writer::new(Vec::new(), 0);
        ConnectionRequest {
            chain: r#"{"Certificate":"{\"chain\":[]}","AuthenticationType":0}"#.to_owned(),
            raw_token: "a.b.c".to_owned(),
        }
        .encode(&mut w)
        .unwrap();
        let decoded = fuzz(&w.into_inner(), 4000, |data| {
            let request = ConnectionRequest::parse(data)?;
            let mut w = Writer::new(Vec::new(), 0);
            request.encode(&mut w)?;
            assert_eq!(w.as_bytes(), data);
            Ok(())
        });
        assert!(decoded > 0);
    }
}
