//! Server Redirection PDU.
//!
//! Defined in [\[MS-RDPBCGR\] 2.2.13.1] (the redirection packet) and
//! [\[MS-RDPBCGR\] 2.2.13.3.1] (its Enhanced Security wrapper, the only form a TLS
//! or CredSSP session receives).
//!
//! A server sends this when the session lives somewhere else: a broker handing the
//! client to a session host, or GNOME Remote Desktop's system daemon handing it to
//! the login screen and then to the user's own session. The client drops the
//! connection and reconnects, presenting the routing token from `load_balance_info`
//! and, when `LB_PASSWORD_IS_PK_ENCRYPTED` is set, authenticating with RDSTLS
//! instead of CredSSP.
//!
//! [\[MS-RDPBCGR\] 2.2.13.1]: https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpbcgr/0ff8c84d-c6f3-4d2b-b22c-3d0b4fae4f8f
//! [\[MS-RDPBCGR\] 2.2.13.3.1]: https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpbcgr/5b2dcf8a-d3a4-4bff-9a71-fc5d6f6a7ff9

use bitflags::bitflags;
use ironrdp_core::{
    Decode, DecodeResult, Encode, EncodeResult, ReadCursor, WriteCursor, cast_length, ensure_size, invalid_field_err,
    read_padding, write_padding,
};

use crate::utils::{from_utf16_bytes, to_utf16_bytes};

/// `SEC_REDIRECTION_PKT`, the fixed value of the packet's `flags` field.
const SEC_REDIRECTION_PKT: u16 = 0x0400;

/// `flags`, `length`, `sessionId` and `redirFlags`.
const PACKET_FIXED_PART_SIZE: usize = 2 + 2 + 4 + 4;

/// `pad2Octets` in front of the packet in the Enhanced Security wrapper.
const PAD2_OCTETS_SIZE: usize = 2;

bitflags! {
    /// `redirFlags` of the Server Redirection Packet.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    #[cfg_attr(feature = "arbitrary", derive(arbitrary::Arbitrary))]
    pub struct RedirectionFlags: u32 {
        const TARGET_NET_ADDRESS = 0x0000_0001;
        const LOAD_BALANCE_INFO = 0x0000_0002;
        const USERNAME = 0x0000_0004;
        const DOMAIN = 0x0000_0008;
        const PASSWORD = 0x0000_0010;
        const DONT_STORE_USERNAME = 0x0000_0020;
        const SMARTCARD_LOGON = 0x0000_0040;
        const NO_REDIRECT = 0x0000_0080;
        const TARGET_FQDN = 0x0000_0100;
        const TARGET_NETBIOS_NAME = 0x0000_0200;
        const TARGET_NET_ADDRESSES = 0x0000_0800;
        const CLIENT_TSV_URL = 0x0000_1000;
        const SERVER_TSV_CAPABLE = 0x0000_2000;
        /// The password is an opaque blob for the target to verify (RDSTLS), not
        /// a UTF-16 string the client can read.
        const PASSWORD_IS_PK_ENCRYPTED = 0x0000_4000;
        const REDIRECTION_GUID = 0x0000_8000;
        const TARGET_CERTIFICATE = 0x0001_0000;
    }
}

impl RedirectionFlags {
    /// Flags that announce a field in the packet, as opposed to flags that only
    /// qualify one. Encoding derives these from which fields are present.
    const FIELDS: Self = Self::TARGET_NET_ADDRESS
        .union(Self::LOAD_BALANCE_INFO)
        .union(Self::USERNAME)
        .union(Self::DOMAIN)
        .union(Self::PASSWORD)
        .union(Self::TARGET_FQDN)
        .union(Self::TARGET_NETBIOS_NAME)
        .union(Self::TARGET_NET_ADDRESSES)
        .union(Self::CLIENT_TSV_URL)
        .union(Self::REDIRECTION_GUID)
        .union(Self::TARGET_CERTIFICATE);
}

/// Enhanced Security Server Redirection PDU, as it appears after the 6-byte Share
/// Control Header (which carries no `shareId` for this PDU type).
///
/// Every field is kept byte-for-byte where the client has to hand it back
/// unchanged: the routing token, the password blob and the redirection GUID are
/// echoed to the target, never interpreted.
///
/// `Debug` prints only the presence or length of `username`, `password`,
/// `redirection_guid` and `target_certificate`: on GNOME Remote Desktop the
/// "PK-encrypted" password is plaintext UTF-16, and with the GUID and username it
/// is a working RDSTLS credential.
#[derive(Clone, PartialEq, Eq)]
#[cfg_attr(feature = "arbitrary", derive(arbitrary::Arbitrary))]
pub struct ServerRedirectionPdu {
    pub session_id: u32,
    /// As received. On encode, the field-announcing bits are recomputed from the
    /// fields present, so only the qualifier bits here are authoritative.
    pub flags: RedirectionFlags,
    pub target_net_address: Option<String>,
    /// Routing token for the reconnect's X.224 Connection Request, including the
    /// `Cookie: msts=` prefix and usually a trailing CRLF.
    pub load_balance_info: Option<Vec<u8>>,
    pub username: Option<String>,
    pub domain: Option<String>,
    pub password: Option<Vec<u8>>,
    pub target_fqdn: Option<String>,
    pub target_netbios_name: Option<String>,
    pub tsv_url: Option<Vec<u8>>,
    pub redirection_guid: Option<Vec<u8>>,
    pub target_certificate: Option<Vec<u8>>,
    pub target_net_addresses: Option<Vec<String>>,
}

impl core::fmt::Debug for ServerRedirectionPdu {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ServerRedirectionPdu")
            .field("session_id", &self.session_id)
            .field("flags", &self.flags)
            .field("target_net_address", &self.target_net_address)
            .field(
                "load_balance_info",
                &self.load_balance_info.as_deref().map(String::from_utf8_lossy),
            )
            .field("has_username", &self.username.is_some())
            .field("domain", &self.domain)
            .field("password_len", &self.password.as_ref().map(Vec::len))
            .field("target_fqdn", &self.target_fqdn)
            .field("target_netbios_name", &self.target_netbios_name)
            .field("tsv_url_len", &self.tsv_url.as_ref().map(Vec::len))
            .field("redirection_guid_len", &self.redirection_guid.as_ref().map(Vec::len))
            .field(
                "target_certificate_len",
                &self.target_certificate.as_ref().map(Vec::len),
            )
            .field("target_net_addresses", &self.target_net_addresses)
            .finish()
    }
}

impl ServerRedirectionPdu {
    const NAME: &'static str = "ServerRedirectionPdu";

    /// Flags as they go on the wire: qualifiers from `flags`, field bits from the
    /// fields actually present.
    fn wire_flags(&self) -> RedirectionFlags {
        let mut flags = self.flags.difference(RedirectionFlags::FIELDS);
        flags.set(RedirectionFlags::TARGET_NET_ADDRESS, self.target_net_address.is_some());
        flags.set(RedirectionFlags::LOAD_BALANCE_INFO, self.load_balance_info.is_some());
        flags.set(RedirectionFlags::USERNAME, self.username.is_some());
        flags.set(RedirectionFlags::DOMAIN, self.domain.is_some());
        flags.set(RedirectionFlags::PASSWORD, self.password.is_some());
        flags.set(RedirectionFlags::TARGET_FQDN, self.target_fqdn.is_some());
        flags.set(
            RedirectionFlags::TARGET_NETBIOS_NAME,
            self.target_netbios_name.is_some(),
        );
        flags.set(
            RedirectionFlags::TARGET_NET_ADDRESSES,
            self.target_net_addresses.is_some(),
        );
        flags.set(RedirectionFlags::CLIENT_TSV_URL, self.tsv_url.is_some());
        flags.set(RedirectionFlags::REDIRECTION_GUID, self.redirection_guid.is_some());
        flags.set(RedirectionFlags::TARGET_CERTIFICATE, self.target_certificate.is_some());
        flags
    }

    /// Size of the packet proper, from `flags` to the last field: the value of
    /// its `length` field.
    fn packet_size(&self) -> usize {
        let string = |value: &Option<String>| value.as_ref().map_or(0, |value| 4 + utf16z_len(value));
        let data = |value: &Option<Vec<u8>>| value.as_ref().map_or(0, |value| 4 + value.len());

        PACKET_FIXED_PART_SIZE
            + string(&self.target_net_address)
            + data(&self.load_balance_info)
            + string(&self.username)
            + string(&self.domain)
            + data(&self.password)
            + string(&self.target_fqdn)
            + string(&self.target_netbios_name)
            + data(&self.tsv_url)
            + data(&self.redirection_guid)
            + data(&self.target_certificate)
            + self.target_net_addresses.as_ref().map_or(0, |addresses| {
                4 + 4 + addresses.iter().map(|address| 4 + utf16z_len(address)).sum::<usize>()
            })
    }

    /// The password as a string, when the server sent it readable.
    ///
    /// `None` when absent or when `PASSWORD_IS_PK_ENCRYPTED` marks it opaque.
    pub fn plaintext_password(&self) -> Option<String> {
        if self.flags.contains(RedirectionFlags::PASSWORD_IS_PK_ENCRYPTED) {
            return None;
        }
        self.password.as_deref().map(utf16z_to_string)
    }

    /// The routing token to present on reconnect, without its terminating CRLF.
    pub fn routing_token(&self) -> Option<String> {
        let bytes = self.load_balance_info.as_deref()?;
        let bytes = bytes.strip_suffix(b"\r\n").unwrap_or(bytes);
        core::str::from_utf8(bytes).ok().map(str::to_owned)
    }
}

impl Encode for ServerRedirectionPdu {
    fn encode(&self, dst: &mut WriteCursor<'_>) -> EncodeResult<()> {
        ensure_size!(in: dst, size: self.size());

        write_padding!(dst, PAD2_OCTETS_SIZE);
        dst.write_u16(SEC_REDIRECTION_PKT);
        dst.write_u16(cast_length!("length", self.packet_size())?);
        dst.write_u32(self.session_id);
        dst.write_u32(self.wire_flags().bits());

        write_string(dst, self.target_net_address.as_deref())?;
        write_data(dst, self.load_balance_info.as_deref())?;
        write_string(dst, self.username.as_deref())?;
        write_string(dst, self.domain.as_deref())?;
        write_data(dst, self.password.as_deref())?;
        write_string(dst, self.target_fqdn.as_deref())?;
        write_string(dst, self.target_netbios_name.as_deref())?;
        write_data(dst, self.tsv_url.as_deref())?;
        write_data(dst, self.redirection_guid.as_deref())?;
        write_data(dst, self.target_certificate.as_deref())?;

        if let Some(addresses) = &self.target_net_addresses {
            let length = 4 + addresses.iter().map(|address| 4 + utf16z_len(address)).sum::<usize>();
            dst.write_u32(cast_length!("targetNetAddressesLength", length)?);
            dst.write_u32(cast_length!("addressCount", addresses.len())?);
            for address in addresses {
                write_string(dst, Some(address))?;
            }
        }

        Ok(())
    }

    fn name(&self) -> &'static str {
        Self::NAME
    }

    fn size(&self) -> usize {
        PAD2_OCTETS_SIZE + self.packet_size()
    }
}

impl<'de> Decode<'de> for ServerRedirectionPdu {
    fn decode(src: &mut ReadCursor<'de>) -> DecodeResult<Self> {
        ensure_size!(in: src, size: PAD2_OCTETS_SIZE + PACKET_FIXED_PART_SIZE);

        read_padding!(src, PAD2_OCTETS_SIZE);

        let packet_flags = src.read_u16();
        if packet_flags != SEC_REDIRECTION_PKT {
            return Err(invalid_field_err!("flags", "not SEC_REDIRECTION_PKT"));
        }
        let _length = src.read_u16();
        let session_id = src.read_u32();
        let flags = RedirectionFlags::from_bits_retain(src.read_u32());

        let string = |src: &mut ReadCursor<'de>, flag: RedirectionFlags| -> DecodeResult<Option<String>> {
            if flags.contains(flag) {
                Ok(Some(utf16z_to_string(read_data(src)?)))
            } else {
                Ok(None)
            }
        };
        let data = |src: &mut ReadCursor<'de>, flag: RedirectionFlags| -> DecodeResult<Option<Vec<u8>>> {
            if flags.contains(flag) {
                Ok(Some(read_data(src)?.to_vec()))
            } else {
                Ok(None)
            }
        };

        // Field order is fixed by MS-RDPBCGR 2.2.13.1 and independent of flag order.
        let target_net_address = string(src, RedirectionFlags::TARGET_NET_ADDRESS)?;
        let load_balance_info = data(src, RedirectionFlags::LOAD_BALANCE_INFO)?;
        let username = string(src, RedirectionFlags::USERNAME)?;
        let domain = string(src, RedirectionFlags::DOMAIN)?;
        let password = data(src, RedirectionFlags::PASSWORD)?;
        let target_fqdn = string(src, RedirectionFlags::TARGET_FQDN)?;
        let target_netbios_name = string(src, RedirectionFlags::TARGET_NETBIOS_NAME)?;
        let tsv_url = data(src, RedirectionFlags::CLIENT_TSV_URL)?;
        let redirection_guid = data(src, RedirectionFlags::REDIRECTION_GUID)?;
        let target_certificate = data(src, RedirectionFlags::TARGET_CERTIFICATE)?;

        let target_net_addresses = if flags.contains(RedirectionFlags::TARGET_NET_ADDRESSES) {
            let mut list = ReadCursor::new(read_data(src)?);
            ensure_size!(in: list, size: 4);
            let count = list.read_u32();
            let mut addresses = Vec::new();
            for _ in 0..count {
                addresses.push(utf16z_to_string(read_data(&mut list)?));
            }
            Some(addresses)
        } else {
            None
        };

        // The optional trailing `pad` (8 bytes, MS-RDPBCGR 2.2.13.3.1) is ignored,
        // as is anything else up to the end of the Share Control PDU.
        read_padding!(src, src.len());

        Ok(Self {
            session_id,
            flags,
            target_net_address,
            load_balance_info,
            username,
            domain,
            password,
            target_fqdn,
            target_netbios_name,
            tsv_url,
            redirection_guid,
            target_certificate,
            target_net_addresses,
        })
    }
}

/// Reads a `u32` length followed by that many bytes.
fn read_data<'de>(src: &mut ReadCursor<'de>) -> DecodeResult<&'de [u8]> {
    ensure_size!(in: src, size: 4);
    let length = cast_length!("length", src.read_u32())?;
    ensure_size!(in: src, size: length);
    Ok(src.read_slice(length))
}

fn write_data(dst: &mut WriteCursor<'_>, value: Option<&[u8]>) -> EncodeResult<()> {
    if let Some(value) = value {
        dst.write_u32(cast_length!("length", value.len())?);
        dst.write_slice(value);
    }
    Ok(())
}

fn write_string(dst: &mut WriteCursor<'_>, value: Option<&str>) -> EncodeResult<()> {
    if let Some(value) = value {
        let mut bytes = to_utf16_bytes(value);
        bytes.extend_from_slice(&[0, 0]);
        write_data(dst, Some(&bytes))?;
    }
    Ok(())
}

/// UTF-16LE length of `value` with its null terminator.
fn utf16z_len(value: &str) -> usize {
    (value.encode_utf16().count() + 1) * 2
}

/// Decodes UTF-16LE, stopping at the first null terminator if there is one.
fn utf16z_to_string(bytes: &[u8]) -> String {
    let end = bytes
        .chunks_exact(2)
        .position(|unit| unit == [0, 0])
        .map_or(bytes.len() & !1, |units| units * 2);
    from_utf16_bytes(&bytes[..end])
}

#[cfg(test)]
mod tests {
    use ironrdp_core::{decode, encode_vec};

    use super::*;

    fn utf16z(value: &str) -> Vec<u8> {
        let mut bytes = to_utf16_bytes(value);
        bytes.extend_from_slice(&[0, 0]);
        bytes
    }

    /// The packet GNOME Remote Desktop's system daemon sends when handing a client
    /// to the login screen: built the way `grd_session_rdp_send_server_redirection`
    /// and FreeRDP's packet writer lay it out.
    fn gnome_handover_bytes() -> Vec<u8> {
        let token = b"Cookie: msts=484355192\r\n";
        let password = utf16z("hunter2");
        let guid = utf16z("q83vEjRWeJCrze8SNFZ4kA==");
        let certificate = b"cert-container".to_vec();

        let mut fields = Vec::new();
        for data in [token.to_vec(), utf16z("gdm-handover"), password, guid, certificate] {
            fields.extend_from_slice(&u32::try_from(data.len()).unwrap().to_le_bytes());
            fields.extend_from_slice(&data);
        }

        let flags = RedirectionFlags::LOAD_BALANCE_INFO
            | RedirectionFlags::USERNAME
            | RedirectionFlags::PASSWORD
            | RedirectionFlags::PASSWORD_IS_PK_ENCRYPTED
            | RedirectionFlags::REDIRECTION_GUID
            | RedirectionFlags::TARGET_CERTIFICATE;

        let mut bytes = vec![0, 0]; // pad2Octets
        bytes.extend_from_slice(&SEC_REDIRECTION_PKT.to_le_bytes());
        bytes.extend_from_slice(
            &u16::try_from(PACKET_FIXED_PART_SIZE + fields.len())
                .unwrap()
                .to_le_bytes(),
        );
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&flags.bits().to_le_bytes());
        bytes.extend_from_slice(&fields);
        bytes.extend_from_slice(&[0; 8]); // optional pad
        bytes
    }

    #[test]
    fn decodes_gnome_remote_desktop_handover() {
        let pdu = decode::<ServerRedirectionPdu>(&gnome_handover_bytes()).unwrap();

        assert_eq!(pdu.routing_token().as_deref(), Some("Cookie: msts=484355192"));
        assert_eq!(pdu.username.as_deref(), Some("gdm-handover"));
        assert_eq!(pdu.domain, None);
        assert_eq!(pdu.password.as_deref(), Some(utf16z("hunter2").as_slice()));
        assert_eq!(pdu.plaintext_password(), None, "PK-encrypted passwords are opaque");
        assert_eq!(
            pdu.redirection_guid.as_deref(),
            Some(utf16z("q83vEjRWeJCrze8SNFZ4kA==").as_slice())
        );
        assert_eq!(pdu.target_certificate.as_deref(), Some(b"cert-container".as_slice()));
        assert!(!pdu.flags.contains(RedirectionFlags::NO_REDIRECT));
    }

    /// g-r-d's "PK-encrypted" password is plaintext UTF-16, and together with the
    /// GUID it is the whole RDSTLS secret, so neither may reach a log.
    #[test]
    fn debug_redacts_the_handover_credentials() {
        let pdu = decode::<ServerRedirectionPdu>(&gnome_handover_bytes()).unwrap();
        let printed = format!("{pdu:?}");

        assert!(!printed.contains("gdm-handover"), "{printed}");
        for secret in [utf16z("hunter2"), utf16z("q83vEjRWeJCrze8SNFZ4kA==")] {
            assert!(!printed.contains(&format!("{secret:?}")), "{printed}");
        }
        assert!(printed.contains("Cookie: msts=484355192"), "{printed}");
    }

    #[test]
    fn round_trips() {
        let pdu = ServerRedirectionPdu {
            session_id: 7,
            flags: RedirectionFlags::PASSWORD_IS_PK_ENCRYPTED | RedirectionFlags::DOMAIN,
            target_net_address: Some("10.0.0.1".to_owned()),
            load_balance_info: Some(b"Cookie: msts=1\r\n".to_vec()),
            username: Some("user".to_owned()),
            domain: None,
            password: Some(vec![1, 2, 3, 4]),
            target_fqdn: None,
            target_netbios_name: None,
            tsv_url: None,
            redirection_guid: Some(vec![9; 16]),
            target_certificate: None,
            target_net_addresses: Some(vec!["10.0.0.1".to_owned(), "10.0.0.2".to_owned()]),
        };

        let decoded = decode::<ServerRedirectionPdu>(&encode_vec(&pdu).unwrap()).unwrap();

        // The DOMAIN bit had no field behind it, so encoding drops it.
        assert_eq!(decoded.flags, pdu.wire_flags());
        assert!(!decoded.flags.contains(RedirectionFlags::DOMAIN));
        assert_eq!(
            ServerRedirectionPdu {
                flags: pdu.flags,
                ..decoded
            },
            pdu
        );
    }

    #[test]
    fn rejects_wrong_packet_flags() {
        let mut bytes = gnome_handover_bytes();
        bytes[2] = 0;
        bytes[3] = 0;
        decode::<ServerRedirectionPdu>(&bytes).unwrap_err();
    }

    #[test]
    fn rejects_truncated_field() {
        let mut bytes = gnome_handover_bytes();
        bytes.truncate(30);
        decode::<ServerRedirectionPdu>(&bytes).unwrap_err();
    }
}
