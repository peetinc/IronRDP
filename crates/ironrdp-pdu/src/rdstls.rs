//! RDSTLS authentication PDUs.
//!
//! Defined in [\[MS-RDPBCGR\] 2.2.17]. RDSTLS replaces CredSSP on a connection the
//! client opened because of a Server Redirection PDU: once TLS is up, the server
//! announces its versions and the client proves it is the redirected client by
//! echoing the redirection GUID and password blob it was handed. No NTLM or
//! Kerberos is involved, so the redirected-to server never sees a real password.
//!
//! The PDUs travel bare over TLS, without TPKT, and the two the client receives
//! have a fixed size, which is what [`RdstlsHint`] relies on.
//!
//! [\[MS-RDPBCGR\] 2.2.17]: https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpbcgr/0f6ce0b6-2d9f-4a2b-8a48-1db1d36dfbd6

use ironrdp_core::{
    Decode, DecodeResult, Encode, EncodeResult, ReadCursor, WriteCursor, cast_length, ensure_fixed_part_size,
    ensure_size, invalid_field_err,
};

use crate::PduHint;
use crate::utils::to_utf16_bytes;

const RDSTLS_VERSION_1: u16 = 0x0001;

const PDU_TYPE_CAPABILITIES: u16 = 0x0001;
const PDU_TYPE_AUTHENTICATION_REQUEST: u16 = 0x0002;
const PDU_TYPE_AUTHENTICATION_RESPONSE: u16 = 0x0004;

const DATA_TYPE_CAPABILITIES: u16 = 0x0001;
const DATA_TYPE_PASSWORD_CREDS: u16 = 0x0001;
const DATA_TYPE_RESULT_CODE: u16 = 0x0001;

/// RDSTLS Capabilities PDU, sent by the server right after the TLS handshake.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RdstlsCapabilities {
    /// Bitmask of RDSTLS versions the server speaks (`0x0001` is version 1).
    pub supported_versions: u16,
}

impl RdstlsCapabilities {
    const NAME: &'static str = "RdstlsCapabilities";

    const FIXED_PART_SIZE: usize = 2 + 2 + 2 + 2;

    pub fn supports_version_1(&self) -> bool {
        self.supported_versions & RDSTLS_VERSION_1 != 0
    }
}

impl Encode for RdstlsCapabilities {
    fn encode(&self, dst: &mut WriteCursor<'_>) -> EncodeResult<()> {
        ensure_fixed_part_size!(in: dst);
        dst.write_u16(RDSTLS_VERSION_1);
        dst.write_u16(PDU_TYPE_CAPABILITIES);
        dst.write_u16(DATA_TYPE_CAPABILITIES);
        dst.write_u16(self.supported_versions);
        Ok(())
    }

    fn name(&self) -> &'static str {
        Self::NAME
    }

    fn size(&self) -> usize {
        Self::FIXED_PART_SIZE
    }
}

impl<'de> Decode<'de> for RdstlsCapabilities {
    fn decode(src: &mut ReadCursor<'de>) -> DecodeResult<Self> {
        ensure_fixed_part_size!(in: src);

        // The PDU's own version field is 1 even when the server also offers 2;
        // what it can speak is in `supportedVersions`.
        let _version = src.read_u16();
        if src.read_u16() != PDU_TYPE_CAPABILITIES {
            return Err(invalid_field_err!("pduType", "expected RDSTLS capabilities"));
        }
        if src.read_u16() != DATA_TYPE_CAPABILITIES {
            return Err(invalid_field_err!("dataType", "expected RDSTLS_DATA_CAPABILITIES"));
        }

        Ok(Self {
            supported_versions: src.read_u16(),
        })
    }
}

/// RDSTLS Authentication Request PDU with password credentials (version 1).
///
/// `redirection_guid` and `password` are the opaque blobs from the Server
/// Redirection PDU, sent back exactly as received.
#[derive(Clone, PartialEq, Eq)]
pub struct RdstlsAuthenticationRequest {
    pub redirection_guid: Vec<u8>,
    pub username: String,
    pub domain: Option<String>,
    pub password: Vec<u8>,
}

impl core::fmt::Debug for RdstlsAuthenticationRequest {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("RdstlsAuthenticationRequest")
            .field("redirection_guid_len", &self.redirection_guid.len())
            // A g-r-d handover username is a one-time credential, like the password.
            .field("username", &"<redacted>")
            .field("domain", &self.domain)
            .field("password", &"<redacted>")
            .finish()
    }
}

impl RdstlsAuthenticationRequest {
    const NAME: &'static str = "RdstlsAuthenticationRequest";

    const FIXED_PART_SIZE: usize = 2 + 2 + 2;

    /// A string field: UTF-16LE with its null terminator, as FreeRDP's server
    /// reads it. An absent string is a lone terminator.
    fn string_bytes(value: Option<&str>) -> Vec<u8> {
        let mut bytes = value.map(to_utf16_bytes).unwrap_or_default();
        bytes.extend_from_slice(&[0, 0]);
        bytes
    }

    fn fields(&self) -> [Vec<u8>; 2] {
        [
            Self::string_bytes(Some(&self.username)),
            Self::string_bytes(self.domain.as_deref()),
        ]
    }
}

impl Encode for RdstlsAuthenticationRequest {
    fn encode(&self, dst: &mut WriteCursor<'_>) -> EncodeResult<()> {
        ensure_size!(in: dst, size: self.size());

        dst.write_u16(RDSTLS_VERSION_1);
        dst.write_u16(PDU_TYPE_AUTHENTICATION_REQUEST);
        dst.write_u16(DATA_TYPE_PASSWORD_CREDS);

        let [username, domain] = self.fields();
        for field in [&self.redirection_guid, &username, &domain, &self.password] {
            dst.write_u16(cast_length!("length", field.len())?);
            dst.write_slice(field);
        }

        Ok(())
    }

    fn name(&self) -> &'static str {
        Self::NAME
    }

    fn size(&self) -> usize {
        let [username, domain] = self.fields();
        Self::FIXED_PART_SIZE
            + [&self.redirection_guid, &username, &domain, &self.password]
                .iter()
                .map(|field| 2 + field.len())
                .sum::<usize>()
    }
}

/// RDSTLS Authentication Response PDU, the server's verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RdstlsAuthenticationResponse {
    pub result_code: RdstlsResultCode,
}

impl RdstlsAuthenticationResponse {
    const NAME: &'static str = "RdstlsAuthenticationResponse";

    const FIXED_PART_SIZE: usize = 2 + 2 + 2 + 4;
}

impl Encode for RdstlsAuthenticationResponse {
    fn encode(&self, dst: &mut WriteCursor<'_>) -> EncodeResult<()> {
        ensure_fixed_part_size!(in: dst);
        dst.write_u16(RDSTLS_VERSION_1);
        dst.write_u16(PDU_TYPE_AUTHENTICATION_RESPONSE);
        dst.write_u16(DATA_TYPE_RESULT_CODE);
        dst.write_u32(self.result_code.0);
        Ok(())
    }

    fn name(&self) -> &'static str {
        Self::NAME
    }

    fn size(&self) -> usize {
        Self::FIXED_PART_SIZE
    }
}

impl<'de> Decode<'de> for RdstlsAuthenticationResponse {
    fn decode(src: &mut ReadCursor<'de>) -> DecodeResult<Self> {
        ensure_fixed_part_size!(in: src);

        let _version = src.read_u16();
        if src.read_u16() != PDU_TYPE_AUTHENTICATION_RESPONSE {
            return Err(invalid_field_err!("pduType", "expected RDSTLS authentication response"));
        }
        if src.read_u16() != DATA_TYPE_RESULT_CODE {
            return Err(invalid_field_err!("dataType", "expected RDSTLS_DATA_RESULT_CODE"));
        }

        Ok(Self {
            result_code: RdstlsResultCode(src.read_u32()),
        })
    }
}

/// `resultCode` of the RDSTLS Authentication Response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RdstlsResultCode(pub u32);

impl RdstlsResultCode {
    pub const SUCCESS: Self = Self(0x0000_0000);
    pub const ACCESS_DENIED: Self = Self(0x0000_0005);
    pub const LOGON_FAILURE: Self = Self(0x0000_052E);
    pub const INVALID_LOGON_HOURS: Self = Self(0x0000_0530);
    pub const PASSWORD_EXPIRED: Self = Self(0x0000_0532);
    pub const ACCOUNT_DISABLED: Self = Self(0x0000_0533);
    pub const PASSWORD_MUST_CHANGE: Self = Self(0x0000_0773);
    pub const ACCOUNT_LOCKED_OUT: Self = Self(0x0000_0775);

    pub fn description(self) -> &'static str {
        match self {
            Self::SUCCESS => "success",
            Self::ACCESS_DENIED => "access denied (redirection GUID mismatch)",
            Self::LOGON_FAILURE => "logon failure (username, domain or password mismatch)",
            Self::INVALID_LOGON_HOURS => "invalid logon hours",
            Self::PASSWORD_EXPIRED => "password expired",
            Self::ACCOUNT_DISABLED => "account disabled",
            Self::PASSWORD_MUST_CHANGE => "password must change",
            Self::ACCOUNT_LOCKED_OUT => "account locked out",
            _ => "unknown result code",
        }
    }
}

/// Frames the fixed-size RDSTLS PDUs a client receives.
#[derive(Debug, Clone, Copy)]
pub struct RdstlsHint {
    size: usize,
}

/// Hint for the server's [`RdstlsCapabilities`].
pub const RDSTLS_CAPABILITIES_HINT: RdstlsHint = RdstlsHint {
    size: RdstlsCapabilities::FIXED_PART_SIZE,
};

/// Hint for the server's [`RdstlsAuthenticationResponse`].
pub const RDSTLS_AUTHENTICATION_RESPONSE_HINT: RdstlsHint = RdstlsHint {
    size: RdstlsAuthenticationResponse::FIXED_PART_SIZE,
};

impl PduHint for RdstlsHint {
    fn find_size(&self, bytes: &[u8]) -> DecodeResult<Option<(bool, usize)>> {
        Ok((bytes.len() >= self.size).then_some((true, self.size)))
    }
}

#[cfg(test)]
mod tests {
    use ironrdp_core::{decode, encode_vec};

    use super::*;

    #[test]
    fn decodes_capabilities() {
        let caps = decode::<RdstlsCapabilities>(&[0x01, 0x00, 0x01, 0x00, 0x01, 0x00, 0x03, 0x00]).unwrap();
        assert_eq!(caps.supported_versions, 0x0003);
        assert!(caps.supports_version_1());
    }

    #[test]
    fn rejects_capabilities_with_wrong_pdu_type() {
        decode::<RdstlsCapabilities>(&[0x01, 0x00, 0x04, 0x00, 0x01, 0x00, 0x01, 0x00]).unwrap_err();
    }

    /// Layout FreeRDP's `rdstls_write_authentication_request_with_password` produces
    /// and its server-side reader expects.
    #[test]
    fn encodes_password_authentication_request() {
        let request = RdstlsAuthenticationRequest {
            redirection_guid: vec![0xAA, 0xBB],
            username: "ab".to_owned(),
            domain: None,
            password: vec![0x61, 0x00, 0x00, 0x00],
        };

        let bytes = encode_vec(&request).unwrap();

        assert_eq!(
            bytes,
            [
                0x01, 0x00, // version
                0x02, 0x00, // pduType: authentication request
                0x01, 0x00, // dataType: password creds
                0x02, 0x00, 0xAA, 0xBB, // redirection GUID, verbatim
                0x06, 0x00, b'a', 0x00, b'b', 0x00, 0x00, 0x00, // username + terminator
                0x02, 0x00, 0x00, 0x00, // empty domain: a lone terminator
                0x04, 0x00, 0x61, 0x00, 0x00, 0x00, // password blob, verbatim
            ]
        );
        assert_eq!(bytes.len(), request.size());
    }

    /// The username in a g-r-d handover is a one-time credential, like the password.
    #[test]
    fn debug_redacts_the_credentials() {
        let request = RdstlsAuthenticationRequest {
            redirection_guid: vec![0xAA, 0xBB],
            username: "gdm-handover".to_owned(),
            domain: None,
            password: vec![0x61, 0x00, 0x00, 0x00],
        };
        let printed = format!("{request:?}");

        assert!(!printed.contains("gdm-handover"), "{printed}");
        assert!(!printed.contains("[97, 0, 0, 0]"), "{printed}");
    }

    #[test]
    fn decodes_authentication_response() {
        let response =
            decode::<RdstlsAuthenticationResponse>(&[0x01, 0x00, 0x04, 0x00, 0x01, 0x00, 0x2E, 0x05, 0x00, 0x00])
                .unwrap();
        assert_eq!(response.result_code, RdstlsResultCode::LOGON_FAILURE);
    }

    #[test]
    fn hint_waits_for_the_whole_pdu() {
        assert_eq!(RDSTLS_CAPABILITIES_HINT.find_size(&[0; 7]).unwrap(), None);
        assert_eq!(RDSTLS_CAPABILITIES_HINT.find_size(&[0; 8]).unwrap(), Some((true, 8)));
        assert_eq!(
            RDSTLS_AUTHENTICATION_RESPONSE_HINT.find_size(&[0; 12]).unwrap(),
            Some((true, 10))
        );
    }
}
