//! Read-only COMpx packet primitives grounded in the extracted ATK frontend.
//!
//! Evidence references use UTF-16 code-unit ranges in
//! `evidence/validated/main/static/index-DcEiQt_8.js`, indexed by
//! `evidence/source_ranges.json`:
//! - `j$t`: `[11115906, 11117007)` defines the 16-byte packet, byte offsets,
//!   big-endian address access, and outbound checksum generation.
//! - `t0t`: `[11117064, 11118316)` assigns the GET command identifiers.
//! - `s2t`: `[11166087, 11167270)` keys pending replies by request byte 0 and
//!   matches the first pending transfer with the same input byte 0. No unique
//!   transaction identifier is evidenced in this source range.
//! - `B0t`: `[11134539, 11135027)` reads identity bytes at offsets 5 and 6.
//! - `b0t`: `[11135027, 11135259)` reads the mouse-version pair at offsets 5
//!   and 6.
//! - `Q0t`: `[11137700, 11137976)` reads the dongle-version pair at offsets 5
//!   and 6.
//! - `w0t`: `[11133846, 11134121)` reads battery fields at offsets 5 through 8.
//!
//! The source computes a checksum while serializing a packet for transport.
//! It does not establish a checksum-verification rule for received responses,
//! so [`Response::parse`] preserves response byte 15 without validating it.

use core::fmt;

pub const REPORT_ID: u8 = 8;
pub const PACKET_LEN: usize = 16;
pub const PAYLOAD_LEN: usize = 10;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
#[repr(u8)]
pub enum GetCommand {
    GetBattery = 0x04,
    GetIdentity = 0x10,
    GetMouseVersion = 0x12,
    GetDongleVersion = 0x1d,
}

impl GetCommand {
    pub const fn id(self) -> u8 {
        self as u8
    }

    pub const fn name(self) -> &'static str {
        match self {
            Self::GetBattery => "GetBatteryLevel",
            Self::GetIdentity => "GetMouseCIDMID",
            Self::GetMouseVersion => "GetMouseVersion",
            Self::GetDongleVersion => "GetDongleVersion",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Request {
    bytes: [u8; PACKET_LEN],
}

impl Request {
    /// Builds one of the four source-confirmed GET packets. All unspecified
    /// fields are zero, matching the frontend's `getData()` constructors.
    pub fn get(command: GetCommand) -> Self {
        Self::from_fields(command.id(), 0, 0, &[])
            .expect("the fixed GET fields fit the COMpx packet")
    }

    /// Builds a generic outbound packet without assigning semantics to its
    /// command ID. `data_valid_len` may be greater than `payload.len()` for
    /// read requests that reserve response capacity. Payload bytes not
    /// supplied by the caller are zero-filled. The transport ReportID remains
    /// separate from these 16 packet bytes.
    pub fn from_fields(
        command_id: u8,
        address_be: u16,
        data_valid_len: u8,
        payload: &[u8],
    ) -> Result<Self, BuildError> {
        if payload.len() > PAYLOAD_LEN {
            return Err(BuildError::PayloadTooLarge {
                actual: payload.len(),
                maximum: PAYLOAD_LEN,
            });
        }
        if usize::from(data_valid_len) > PAYLOAD_LEN {
            return Err(BuildError::DataLengthTooLarge {
                actual: data_valid_len,
                maximum: PAYLOAD_LEN,
            });
        }
        if usize::from(data_valid_len) < payload.len() {
            return Err(BuildError::DataLengthBelowPayload {
                data_valid_len,
                payload_len: payload.len(),
            });
        }

        let mut bytes = [0u8; PACKET_LEN];
        bytes[0] = command_id;
        // The evidenced packet constructor leaves outbound status at zero.
        bytes[1] = 0;
        bytes[2..4].copy_from_slice(&address_be.to_be_bytes());
        bytes[4] = data_valid_len;
        bytes[5..5 + payload.len()].copy_from_slice(payload);
        bytes[PACKET_LEN - 1] = outgoing_checksum(&bytes);
        Ok(Self { bytes })
    }

    pub const fn bytes(&self) -> &[u8; PACKET_LEN] {
        &self.bytes
    }

    /// The report ID is passed separately from the 16-byte packet by the
    /// source transport (`sendReport(reportId, payload)`).
    pub const fn report_id(&self) -> u8 {
        REPORT_ID
    }

    /// Recomputes the checksum rule evidenced in `j$t.toUint8Array()`.
    pub fn has_valid_outgoing_checksum(&self) -> bool {
        self.bytes[PACKET_LEN - 1] == outgoing_checksum(&self.bytes)
    }

    /// Returns exactly the bytes passed as the report payload, without the
    /// separately supplied HID report ID.
    pub const fn packet_bytes(&self) -> &[u8; PACKET_LEN] {
        &self.bytes
    }
}

fn outgoing_checksum(packet: &[u8; PACKET_LEN]) -> u8 {
    let low_sum = packet[..PACKET_LEN - 1]
        .iter()
        .fold(REPORT_ID, |sum, byte| sum.wrapping_add(*byte));
    0x55u8.wrapping_sub(low_sum)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BuildError {
    PayloadTooLarge { actual: usize, maximum: usize },
    DataLengthTooLarge { actual: u8, maximum: usize },
    DataLengthBelowPayload { data_valid_len: u8, payload_len: usize },
}

impl fmt::Display for BuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PayloadTooLarge { actual, maximum } => {
                write!(f, "payload length {actual} exceeds {maximum}")
            }
            Self::DataLengthTooLarge { actual, maximum } => {
                write!(f, "data length {actual} exceeds {maximum}")
            }
            Self::DataLengthBelowPayload { data_valid_len, payload_len } => write!(
                f,
                "data length {data_valid_len} is below payload length {payload_len}"
            ),
        }
    }
}

impl std::error::Error for BuildError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ParseError {
    WrongLength { expected: usize, actual: usize },
    CommandMismatch { expected: u8, actual: u8 },
    DataLengthTooLarge { actual: u8, maximum: usize },
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WrongLength { expected, actual } => {
                write!(f, "packet length {actual} does not equal {expected}")
            }
            Self::CommandMismatch { expected, actual } => write!(
                f,
                "response command 0x{actual:02x} does not match request 0x{expected:02x}"
            ),
            Self::DataLengthTooLarge { actual, maximum } => write!(
                f,
                "data length {actual} exceeds packet payload capacity {maximum}"
            ),
        }
    }
}

impl std::error::Error for ParseError {}

/// A generic response packet for commands that do not have a typed GET enum.
/// It validates packet shape and command correlation but does not validate the
/// final checksum byte, whose response rule is not established by the source.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RawResponse {
    raw: [u8; PACKET_LEN],
}

impl RawResponse {
    pub fn parse(expected_command_id: u8, input: &[u8]) -> Result<Self, ParseError> {
        if input.len() != PACKET_LEN {
            return Err(ParseError::WrongLength {
                expected: PACKET_LEN,
                actual: input.len(),
            });
        }
        if input[0] != expected_command_id {
            return Err(ParseError::CommandMismatch {
                expected: expected_command_id,
                actual: input[0],
            });
        }
        if usize::from(input[4]) > PAYLOAD_LEN {
            return Err(ParseError::DataLengthTooLarge {
                actual: input[4],
                maximum: PAYLOAD_LEN,
            });
        }
        let mut raw = [0u8; PACKET_LEN];
        raw.copy_from_slice(input);
        Ok(Self { raw })
    }

    pub const fn raw(&self) -> &[u8; PACKET_LEN] {
        &self.raw
    }

    pub const fn command_id_raw(&self) -> u8 {
        self.raw[0]
    }

    pub const fn command_status_raw(&self) -> u8 {
        self.raw[1]
    }

    pub const fn address_be(&self) -> u16 {
        u16::from_be_bytes([self.raw[2], self.raw[3]])
    }

    pub const fn data_valid_len(&self) -> u8 {
        self.raw[4]
    }

    pub fn payload_bytes(&self) -> [u8; PAYLOAD_LEN] {
        let mut payload = [0u8; PAYLOAD_LEN];
        payload.copy_from_slice(&self.raw[5..15]);
        payload
    }

    pub fn declared_payload(&self) -> &[u8] {
        &self.raw[5..5 + usize::from(self.data_valid_len())]
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Response {
    command: GetCommand,
    raw: [u8; PACKET_LEN],
}

impl Response {
    /// Parses a 16-byte protocol packet (without a transport-level report-ID
    /// byte), checking packet length, expected command, and the declared data
    /// length against the evidenced 10-byte payload area. The expected-command
    /// check validates the same byte the frontend uses to match a pending
    /// transfer. Since that matching has no unique transaction ID, callers
    /// that need unambiguous replies should serialize duplicate GET commands.
    /// The response checksum byte is retained in `raw`; its verification rule
    /// is unknown.
    pub fn parse(expected: GetCommand, input: &[u8]) -> Result<Self, ParseError> {
        if input.len() != PACKET_LEN {
            return Err(ParseError::WrongLength {
                expected: PACKET_LEN,
                actual: input.len(),
            });
        }
        if input[0] != expected.id() {
            return Err(ParseError::CommandMismatch {
                expected: expected.id(),
                actual: input[0],
            });
        }
        if usize::from(input[4]) > PAYLOAD_LEN {
            return Err(ParseError::DataLengthTooLarge {
                actual: input[4],
                maximum: PAYLOAD_LEN,
            });
        }
        let mut raw = [0u8; PACKET_LEN];
        raw.copy_from_slice(input);
        Ok(Self {
            command: expected,
            raw,
        })
    }

    pub const fn command(&self) -> GetCommand {
        self.command
    }

    pub const fn raw(&self) -> &[u8; PACKET_LEN] {
        &self.raw
    }

    /// Device status is preserved as an uninterpreted byte; no status enum is
    /// established by the current source evidence.
    pub const fn command_status_raw(&self) -> u8 {
        self.raw[1]
    }

    /// `DataView.getUint16(2)` in the source omits the little-endian flag,
    /// which means big-endian interpretation.
    pub const fn address_be(&self) -> u16 {
        u16::from_be_bytes([self.raw[2], self.raw[3]])
    }

    pub const fn data_valid_len(&self) -> u8 {
        self.raw[4]
    }

    pub fn payload_bytes(&self) -> [u8; PAYLOAD_LEN] {
        let mut payload = [0u8; PAYLOAD_LEN];
        payload.copy_from_slice(&self.raw[5..15]);
        payload
    }

    pub fn declared_payload(&self) -> &[u8] {
        &self.raw[5..5 + usize::from(self.data_valid_len())]
    }

    pub fn cid_mid_raw(&self) -> Option<(u8, u8)> {
        (self.command == GetCommand::GetIdentity).then_some((self.raw[5], self.raw[6]))
    }

    pub fn version_bytes_raw(&self) -> Option<(u8, u8)> {
        matches!(self.command, GetCommand::GetMouseVersion | GetCommand::GetDongleVersion)
            .then_some((self.raw[5], self.raw[6]))
    }

    /// Formats only the two raw bytes using the rendering observed in the
    /// frontend (`first.toString(16) + "." + second.toString(16).padStart(2)`).
    pub fn version_hex_pair(&self) -> Option<String> {
        self.version_bytes_raw()
            .map(|(major, minor)| format!("{major:x}.{minor:02x}"))
    }

    /// Exposes the battery response fields without assigning units/scales.
    pub fn battery_raw(&self) -> Option<BatteryRaw> {
        (self.command == GetCommand::GetBattery).then_some(BatteryRaw {
            battery_level: self.raw[5],
            charging: self.raw[6],
            voltage_be: u16::from_be_bytes([self.raw[7], self.raw[8]]),
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BatteryRaw {
    pub battery_level: u8,
    pub charging: u8,
    pub voltage_be: u16,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_confirmed_get_packets_have_report_id_and_checksum() {
        let expected = [
            (GetCommand::GetIdentity, 0x3d),
            (GetCommand::GetMouseVersion, 0x3b),
            (GetCommand::GetDongleVersion, 0x30),
            (GetCommand::GetBattery, 0x49),
        ];
        for (command, checksum) in expected {
            let request = Request::get(command);
            assert_eq!(request.report_id(), REPORT_ID);
            assert_eq!(request.bytes()[0], command.id());
            assert!(request.bytes()[1..15].iter().all(|byte| *byte == 0));
            assert_eq!(request.bytes()[15], checksum);
            assert!(request.has_valid_outgoing_checksum());
        }
    }

    #[test]
    fn response_parser_checks_length_correlation_and_payload_capacity() {
        let good = [GetCommand::GetIdentity.id(); PACKET_LEN];
        assert!(matches!(
            Response::parse(GetCommand::GetIdentity, &good[..PACKET_LEN - 1]),
            Err(ParseError::WrongLength { .. })
        ));
        assert!(matches!(
            Response::parse(GetCommand::GetMouseVersion, &good),
            Err(ParseError::CommandMismatch { .. })
        ));
        let mut oversized = good;
        oversized[4] = (PAYLOAD_LEN + 1) as u8;
        assert!(matches!(
            Response::parse(GetCommand::GetIdentity, &oversized),
            Err(ParseError::DataLengthTooLarge { .. })
        ));
    }

    #[test]
    fn response_keeps_raw_status_payload_and_unverified_final_byte() {
        let mut raw = [0u8; PACKET_LEN];
        raw[0] = GetCommand::GetIdentity.id();
        raw[1] = 0xe7;
        raw[2] = 0x12;
        raw[3] = 0x34;
        raw[4] = 2;
        raw[5] = 0x56;
        raw[6] = 0x78;
        raw[15] = 0xff;
        let response = Response::parse(GetCommand::GetIdentity, &raw).unwrap();
        assert_eq!(response.raw(), &raw);
        assert_eq!(response.command_status_raw(), 0xe7);
        assert_eq!(response.address_be(), 0x1234);
        assert_eq!(response.cid_mid_raw(), Some((0x56, 0x78)));
        assert_eq!(response.declared_payload(), &[0x56, 0x78]);
        assert_eq!(response.raw()[15], 0xff);
    }

    #[test]
    fn battery_and_version_values_remain_raw() {
        let mut version = [0u8; PACKET_LEN];
        version[0] = GetCommand::GetDongleVersion.id();
        version[5] = 0x0b;
        version[6] = 0x03;
        let parsed = Response::parse(GetCommand::GetDongleVersion, &version).unwrap();
        assert_eq!(parsed.version_bytes_raw(), Some((0x0b, 0x03)));
        assert_eq!(parsed.version_hex_pair().as_deref(), Some("b.03"));

        let mut battery = [0u8; PACKET_LEN];
        battery[0] = GetCommand::GetBattery.id();
        battery[5] = 77;
        battery[6] = 2;
        battery[7] = 0x12;
        battery[8] = 0x34;
        let parsed = Response::parse(GetCommand::GetBattery, &battery).unwrap();
        assert_eq!(
            parsed.battery_raw(),
            Some(BatteryRaw {
                battery_level: 77,
                charging: 2,
                voltage_be: 0x1234,
            })
        );
    }

    #[test]
    fn generic_builder_allows_reserved_read_length_and_validates_packet_bounds() {
        let reserved = Request::from_fields(0x70, 0x1234, 10, &[]).unwrap();
        assert_eq!(reserved.bytes()[0], 0x70);
        assert_eq!(reserved.bytes()[1], 0);
        assert_eq!(&reserved.bytes()[2..4], &[0x12, 0x34]);
        assert_eq!(reserved.bytes()[4], 10);
        assert_eq!(&reserved.bytes()[5..15], &[0; 10]);

        assert!(matches!(
            Request::from_fields(0x70, 0, 0, &[0; 11]),
            Err(BuildError::PayloadTooLarge { actual: 11, .. })
        ));
        assert!(matches!(
            Request::from_fields(0x70, 0, 11, &[]),
            Err(BuildError::DataLengthTooLarge { actual: 11, .. })
        ));
        assert!(matches!(
            Request::from_fields(0x70, 0, 1, &[1, 2]),
            Err(BuildError::DataLengthBelowPayload {
                data_valid_len: 1,
                payload_len: 2
            })
        ));
    }

    #[test]
    fn generic_builder_computes_checksum_with_byte_wrapping() {
        let request = Request::from_fields(0xfa, 0xfff0, 10, &[0xff; 10]).unwrap();
        assert_eq!(request.report_id(), REPORT_ID);
        assert_eq!(request.bytes()[1], 0);
        assert_eq!(request.bytes()[15], 0x64);
        assert!(request.has_valid_outgoing_checksum());
    }

    #[test]
    fn generic_raw_response_preserves_unknown_command_fields_without_rx_checksum_validation() {
        let packet = [
            0x66, 0x9a, 0x12, 0x34, 3, 0xaa, 0xbb, 0xcc, 0, 0, 0, 0, 0, 0, 0, 0xfe,
        ];
        let response = RawResponse::parse(0x66, &packet).unwrap();
        assert_eq!(response.raw(), &packet);
        assert_eq!(response.command_id_raw(), 0x66);
        assert_eq!(response.command_status_raw(), 0x9a);
        assert_eq!(response.address_be(), 0x1234);
        assert_eq!(response.data_valid_len(), 3);
        assert_eq!(response.declared_payload(), &[0xaa, 0xbb, 0xcc]);
        assert_eq!(response.raw()[15], 0xfe);

        assert!(matches!(
            RawResponse::parse(0x66, &packet[..15]),
            Err(ParseError::WrongLength { .. })
        ));
        assert!(matches!(
            RawResponse::parse(0x67, &packet),
            Err(ParseError::CommandMismatch { .. })
        ));
        let mut oversized = packet;
        oversized[4] = 11;
        assert!(matches!(
            RawResponse::parse(0x66, &oversized),
            Err(ParseError::DataLengthTooLarge { .. })
        ));
    }
}
