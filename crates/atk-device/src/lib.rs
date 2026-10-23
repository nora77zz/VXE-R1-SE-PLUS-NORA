//! Read-only transport for the four source-confirmed ATK information GETs.
//!
//! The public JSON representation never exposes a HID path or serial number.
//! Identity-report bytes in protocol-packet offsets 11 through 14 are masked
//! before serialization; the checksum byte is retained but not validated.

use std::fmt;
use std::time::{Duration, Instant};
use std::sync::{Arc, atomic::{AtomicU64, Ordering}};

use atk_protocol::{
    BatteryRaw, GetCommand, ParseError, RawResponse, Request, Response, PACKET_LEN, REPORT_ID,
};
use serde::Serialize;
use sha2::{Digest, Sha256};

pub mod device_changes;

pub const TARGET_VENDOR_ID: u16 = 0x373b;
pub const TARGET_PRODUCT_ID: u16 = 0x1085;
// The active frontend lists VXE R1SE+ under this USB identity (bundle
// index-DcEiQt_8.js, UTF-16 offset 11220359). Descriptor inspection on the
// connected mouse confirms FF02/2 report 8 and its FF04/2 sibling.
pub const WIRED_VENDOR_ID: u16 = 0x3554;
pub const WIRED_PRODUCT_ID: u16 = 0xf58f;
pub fn is_supported_transport(vendor_id: u16, product_id: u16) -> bool {
    matches!((vendor_id, product_id),
        (TARGET_VENDOR_ID, TARGET_PRODUCT_ID) | (WIRED_VENDOR_ID, WIRED_PRODUCT_ID))
}
pub const TARGET_USAGE_PAGE: u16 = 0xff02;
pub const TARGET_USAGE: u16 = 0x0002;
pub const TRANSPORT_REPORT_ID: u8 = REPORT_ID;
pub const WIRE_REPORT_LEN: usize = PACKET_LEN + 1;
pub const DEFAULT_QUERY_TIMEOUT: Duration = Duration::from_millis(2_000);
/// Normal mouse commands observed in the active ATK configuration frontend.
/// Firmware, reset, and pairing operations are intentionally excluded.
pub const NATIVE_MOUSE_COMMAND_ALLOWLIST: [u8; 11] = [
    0x03, 0x04, 0x07, 0x08, 0x0e, 0x0f, 0x10, 0x12, 0x16, 0x17, 0x1d,
];
// Source-confirmed EEPROM 7/8, current-profile 14/15, and far-distance 22/23.
const CONFIG_COMMAND_IDS: [u8; 6] = [0x07, 0x08, 0x0e, 0x0f, 0x16, 0x17];
// GET-only set used by atk-config::read_requests() for a settings snapshot.
const SETTINGS_GET_COMMAND_IDS: [u8; 6] = [0x08, 0x0e, 0x10, 0x12, 0x17, 0x1d];

const IDENTITY_REDACTED_WIRE_RANGE: RedactionRange = RedactionRange { start: 12, end: 16 };
const WIRE_COORDINATE_SYSTEM: &str = "zero_based_half_open_wire_byte_offsets_including_report_id";

/// Public collection metadata. Device path, serial, product strings, and
/// manufacturer strings are intentionally not part of this type.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InterfaceMetadata {
    pub vendor_id: u16,
    pub product_id: u16,
    pub usage_page: u16,
    pub usage: u16,
}

impl InterfaceMetadata {
    pub const fn target() -> Self {
        Self {
            vendor_id: TARGET_VENDOR_ID,
            product_id: TARGET_PRODUCT_ID,
            usage_page: TARGET_USAGE_PAGE,
            usage: TARGET_USAGE,
        }
    }
}

/// One physically enumerated receiver configuration collection. The opaque
/// key is a stable digest of its native HID path and collection identity; the
/// raw path is never serialized. Serial numbers are returned only so the app
/// can match the user's first WebHID selection to this native collection.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TargetDevice {
    pub device_key: String,
    pub interface: InterfaceMetadata,
    pub product_name: Option<String>,
    pub serial_number: Option<String>,
    /// The two ATK collections needed by the original WebHID filter and
    /// configuration transport. Their report IDs are grounded in the local
    /// read-only descriptor capture; a candidate is returned only when both
    /// collections share the same Windows device-container identity.
    pub collections: Vec<CollectionMetadata>,
    pub descriptor_source: &'static str,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CollectionMetadata {
    pub usage_page: u16,
    pub usage: u16,
    pub input_reports: Vec<ReportMetadata>,
    pub output_reports: Vec<ReportMetadata>,
    pub feature_reports: Vec<ReportMetadata>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReportMetadata {
    pub report_id: u8,
}

/// Serializable results for the four GETs. The reports in this DTO are
/// explicitly sanitized representations, not byte-for-byte raw captures.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceInfo {
    pub schema_version: u8,
    pub interface: InterfaceMetadata,
    pub transport_report_id: u8,
    pub queries: Vec<QueryInfo>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QueryInfo {
    pub command: String,
    pub command_id: u8,
    pub command_status_raw: u8,
    pub address_be_raw: u16,
    pub data_valid_len: u8,
    pub report: SanitizedReport,
    pub known_values: KnownValues,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SanitizedReport {
    /// Full wire report (ReportID + 16-byte packet), with ranges below masked.
    pub wire_bytes_redacted: Vec<u8>,
    pub coordinate_system: &'static str,
    pub redacted_wire_byte_ranges: Vec<RedactionRange>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RedactionRange {
    /// Inclusive start offset in `coordinateSystem`.
    pub start: usize,
    /// Exclusive end offset in `coordinateSystem`.
    pub end: usize,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct KnownValues {
    pub cid_raw: Option<u8>,
    pub mid_raw: Option<u8>,
    pub version_bytes_raw: Option<[u8; 2]>,
    /// Present only when the response declares at least two version bytes.
    pub version_hex_pair: Option<String>,
    pub battery_raw: Option<BatteryFieldsRaw>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BatteryFieldsRaw {
    pub battery_level: u8,
    pub charging: u8,
    pub voltage_be_raw: u16,
    pub voltage_be_bytes_raw: [u8; 2],
}

impl From<BatteryRaw> for BatteryFieldsRaw {
    fn from(raw: BatteryRaw) -> Self {
        Self {
            battery_level: raw.battery_level,
            charging: raw.charging,
            voltage_be_raw: raw.voltage_be,
            voltage_be_bytes_raw: raw.voltage_be.to_be_bytes(),
        }
    }
}

#[derive(Debug)]
pub enum DeviceError {
    UnsupportedPlatform,
    HidApiInitialization,
    DeviceContainerEnumeration,
    NoMatchingCollection,
    MultipleMatchingCollections { count: usize },
    DeviceNotFound,
    OpenCollection,
    UnsupportedConfigCommand { command_id: u8 },
    UnsupportedReadOnlyCommand { command_id: u8 },
    WriteFailed,
    ShortWrite { expected: usize, actual: usize },
    ReadFailed,
    TransportChanged,
    DeadlineExpired { command_id: u8 },
    WrongReportLength { expected: usize, actual: usize },
    WrongReportId { expected: u8, actual: u8 },
    InvalidRequestReportLength { actual: usize },
    InvalidRequestChecksum,
    UnsupportedMouseCommand { command_id: u8 },
    Protocol(ParseError),
}

impl fmt::Display for DeviceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedPlatform => f.write_str("native ATK HID access is supported on Windows only"),
            Self::HidApiInitialization => f.write_str("could not initialize the Windows HID API"),
            Self::DeviceContainerEnumeration => {
                f.write_str("could not correlate HID interfaces with Windows device containers")
            }
            Self::NoMatchingCollection => f.write_str("no matching ATK HID collection was found"),
            Self::MultipleMatchingCollections { count } => {
                write!(f, "expected one matching ATK HID collection, found {count}")
            }
            Self::DeviceNotFound => f.write_str("the selected ATK HID collection is not connected"),
            Self::OpenCollection => f.write_str("could not open the matching ATK HID collection"),
            Self::UnsupportedConfigCommand { command_id } => write!(
                f,
                "configuration command 0x{command_id:02x} is outside the confirmed allowlist"
            ),
            Self::UnsupportedReadOnlyCommand { command_id } => write!(
                f,
                "read-only command 0x{command_id:02x} is outside the settings GET allowlist"
            ),
            Self::WriteFailed => f.write_str("HID report write failed"),
            Self::ShortWrite { expected, actual } => {
                write!(f, "HID report write returned {actual} bytes; expected {expected}")
            }
            Self::ReadFailed => f.write_str("HID report read failed"),
            Self::TransportChanged => f.write_str("A conexão USB mudou; aguardando o transporte atual do mouse."),
            Self::DeadlineExpired { command_id } => {
                write!(f, "deadline expired while awaiting GET command 0x{command_id:02x}")
            }
            Self::WrongReportLength { expected, actual } => {
                write!(f, "HID report length {actual} does not equal {expected}")
            }
            Self::WrongReportId { expected, actual } => {
                write!(f, "HID report ID 0x{actual:02x} does not equal 0x{expected:02x}")
            }
            Self::InvalidRequestReportLength { actual } => write!(
                f,
                "outbound HID report payload has {actual} bytes; expected {PACKET_LEN}"
            ),
            Self::InvalidRequestChecksum => {
                f.write_str("outbound HID packet does not match the source-confirmed checksum")
            }
            Self::UnsupportedMouseCommand { command_id } => write!(
                f,
                "mouse command 0x{command_id:02x} is outside the native normal-operation allowlist"
            ),
            Self::Protocol(error) => write!(f, "invalid ATK response: {error}"),
        }
    }
}

impl std::error::Error for DeviceError {}

impl From<ParseError> for DeviceError {
    fn from(value: ParseError) -> Self {
        Self::Protocol(value)
    }
}

/// Sanitized transport error. The native HID error message is deliberately
/// omitted because it can include machine-specific collection details.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransportError {
    WriteFailed,
    ReadFailed,
}

/// A single HID collection with one write/read interface. Implementations
/// must return a complete wire report including the report-ID byte.
pub trait ReportIo: Send {
    fn write_report(&mut self, report: &[u8]) -> Result<usize, TransportError>;
    fn read_report(
        &mut self,
        timeout: Duration,
    ) -> Result<Option<Vec<u8>>, TransportError>;
}

/// Performs exactly one write for a GET and reads until its matching command
/// reply arrives or the fixed deadline expires. Other command IDs are treated
/// as interleaved reports and discarded; they never trigger a resend.
pub fn read_info_with_transport<T: ReportIo>(
    mut transport: T,
    interface: InterfaceMetadata,
    timeout: Duration,
) -> Result<DeviceInfo, DeviceError> {
    let mut queries = Vec::with_capacity(4);
    for command in [
        GetCommand::GetIdentity,
        GetCommand::GetMouseVersion,
        GetCommand::GetDongleVersion,
        GetCommand::GetBattery,
    ] {
        let (response, exact_wire_report) = execute_get(&mut transport, command, timeout)?;
        queries.push(QueryInfo::from_response(command, &response, &exact_wire_report));
    }

    Ok(DeviceInfo {
        schema_version: 1,
        interface,
        transport_report_id: TRANSPORT_REPORT_ID,
        queries,
    })
}

/// One serialized session on the selected FF02/0002 HID collection.
///
/// It exposes only source-confirmed EEPROM, current-profile and far-distance
/// command IDs. The source transfer matcher correlates replies by command ID,
/// not by EEPROM address; the address remains available in [`RawResponse`]
/// but is not treated as a proven response-matching field.
pub struct ConfigSession {
    transport: Box<dyn ReportIo>,
    interface: InterfaceMetadata,
    timeout: Duration,
}

impl ConfigSession {
    fn new<T: ReportIo + 'static>(
        transport: T,
        interface: InterfaceMetadata,
        timeout: Duration,
    ) -> Self {
        Self {
            transport: Box::new(transport),
            interface,
            timeout,
        }
    }

    pub fn interface(&self) -> &InterfaceMetadata {
        &self.interface
    }

    /// Sends one typed request and waits for a response with the same command
    /// ID. Allowed IDs are GET/SET EEPROM 8/7, GET/SET current profile 14/15,
    /// and GET/SET far-distance 23/22. Every other command is rejected before
    /// I/O. These command IDs are confirmed by the original frontend classes
    /// `j0t`, `y0t`, and `D0t` plus the `t0t` command enum.
    /// The mutable receiver makes calls sequential; Tauri should keep the
    /// session in one managed mutex for app-wide serialization.
    pub fn transact_config(&mut self, request: &Request) -> Result<RawResponse, DeviceError> {
        let command_id = request.packet_bytes()[0];
        if !CONFIG_COMMAND_IDS.contains(&command_id) {
            return Err(DeviceError::UnsupportedConfigCommand { command_id });
        }
        let (response, _) = transact_request(&mut *self.transport, request, self.timeout)?;
        Ok(response)
    }

    /// Executes only the GET command IDs used by `atk-config::read_requests`
    /// (EEPROM 8, profile 14, identity 16, mouse version 18, far-distance 23,
    /// and receiver version 29). All SET, reset, pairing and upgrade IDs are
    /// rejected before the transport is touched.
    pub fn transact_settings_read(
        &mut self,
        request: &Request,
    ) -> Result<RawResponse, DeviceError> {
        let command_id = request.packet_bytes()[0];
        if !SETTINGS_GET_COMMAND_IDS.contains(&command_id) {
            return Err(DeviceError::UnsupportedReadOnlyCommand { command_id });
        }
        let (response, _) = transact_request(&mut *self.transport, request, self.timeout)?;
        Ok(response)
    }
}

/// Serialized native HID session used by the desktop WebHID compatibility
/// layer. It accepts only the source-observed normal mouse command IDs and
/// keeps the report transport available for idle input-event polling.
pub struct ReportSession {
    transport: Box<dyn ReportIo>,
    interface: InterfaceMetadata,
    device_key: String,
    timeout: Duration,
    topology: Option<(Arc<AtomicU64>, u64)>,
}

impl ReportSession {
    fn new<T: ReportIo + 'static>(
        transport: T,
        interface: InterfaceMetadata,
        device_key: String,
        timeout: Duration,
    ) -> Self {
        Self {
            transport: Box::new(transport),
            interface,
            device_key,
            timeout,
            topology: None,
        }
    }

    pub fn watch_topology_changes(&mut self, revision: Arc<AtomicU64>) {
        let baseline = revision.load(Ordering::Acquire);
        self.topology = Some((revision, baseline));
    }

    fn topology_changed(&self) -> bool {
        self.topology.as_ref().is_some_and(|(revision, baseline)| revision.load(Ordering::Acquire) != *baseline)
    }

    pub fn interface(&self) -> &InterfaceMetadata {
        &self.interface
    }

    pub fn device_key(&self) -> &str {
        &self.device_key
    }

    /// Sends one full source-format packet and returns its matching wire
    /// response. Interleaved command 0x0A reports are delivered to the caller
    /// while the session keeps waiting for the requested command ID.
    pub fn transact_hid_report<F>(
        &mut self,
        report_id: u8,
        packet: &[u8],
        mut on_unsolicited: F,
    ) -> Result<Vec<u8>, DeviceError>
    where
        F: FnMut(Vec<u8>),
    {
        if report_id != TRANSPORT_REPORT_ID {
            return Err(DeviceError::WrongReportId {
                expected: TRANSPORT_REPORT_ID,
                actual: report_id,
            });
        }
        if packet.len() != PACKET_LEN {
            return Err(DeviceError::InvalidRequestReportLength {
                actual: packet.len(),
            });
        }

        let command_id = packet[0];
        // The active frontend's H0t.get()/y2t.getMouseInfo() sends command
        // 0x01 as an eight-byte transport challenge: four random bytes followed
        // by four zero bytes, at address zero (bundle offset 11138551).
        // Permit that exact frame shape; other 0x01 payloads remain blocked.
        let transport_challenge = command_id == 0x01 && packet[2..4] == [0, 0]
            && packet[4] == 8 && packet[9..15].iter().all(|byte| *byte == 0);
        if !NATIVE_MOUSE_COMMAND_ALLOWLIST.contains(&command_id) && !transport_challenge {
            return Err(DeviceError::UnsupportedMouseCommand { command_id });
        }
        let data_len = usize::from(packet[4]);
        if data_len > atk_protocol::PAYLOAD_LEN {
            return Err(DeviceError::Protocol(ParseError::DataLengthTooLarge {
                actual: packet[4],
                maximum: atk_protocol::PAYLOAD_LEN,
            }));
        }

        // Rebuild the request with the exact source serializer. Equality
        // validates the outbound checksum and all reserved/unused bytes.
        let request = Request::from_fields(
            command_id,
            u16::from_be_bytes([packet[2], packet[3]]),
            packet[4],
            &packet[5..5 + data_len],
        )
        .map_err(|_| DeviceError::InvalidRequestChecksum)?;
        if request.packet_bytes().as_slice() != packet {
            return Err(DeviceError::InvalidRequestChecksum);
        }
        if self.topology_changed() { return Err(DeviceError::TransportChanged); }

        let mut wire_request = Vec::with_capacity(WIRE_REPORT_LEN);
        wire_request.push(report_id);
        wire_request.extend_from_slice(packet);
        let written = self
            .transport
            .write_report(&wire_request)
            .map_err(|_| DeviceError::WriteFailed)?;
        if written != WIRE_REPORT_LEN {
            return Err(DeviceError::ShortWrite {
                expected: WIRE_REPORT_LEN,
                actual: written,
            });
        }

        let deadline = Instant::now()
            .checked_add(self.timeout)
            .unwrap_or_else(Instant::now);
        loop {
            if self.topology_changed() { return Err(DeviceError::TransportChanged); }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(DeviceError::DeadlineExpired { command_id });
            }
            let read_timeout = if self.topology.is_some() { remaining.min(Duration::from_millis(25)) } else { remaining };
            let report = self
                .transport
                .read_report(read_timeout)
                .map_err(|_| DeviceError::ReadFailed)?;
            if self.topology_changed() { return Err(DeviceError::TransportChanged); }
            let Some(wire_report) = report else {
                if self.topology.is_some() && Instant::now() < deadline { continue; }
                return Err(DeviceError::DeadlineExpired { command_id });
            };
            if Instant::now() >= deadline {
                return Err(DeviceError::DeadlineExpired { command_id });
            }
            if wire_report.len() != WIRE_REPORT_LEN {
                return Err(DeviceError::WrongReportLength {
                    expected: WIRE_REPORT_LEN,
                    actual: wire_report.len(),
                });
            }
            if wire_report[0] != report_id {
                return Err(DeviceError::WrongReportId {
                    expected: report_id,
                    actual: wire_report[0],
                });
            }
            let response = RawResponse::parse(wire_report[1], &wire_report[1..])?;
            if response.command_id_raw() == 0x0a {
                on_unsolicited(wire_report);
                continue;
            }
            if response.command_id_raw() != command_id {
                continue;
            }
            return Ok(wire_report);
        }
    }

    /// Reads one report without sending a command, for the actor's idle event
    /// drain loop. The caller validates and routes only known unsolicited IDs.
    pub fn read_report(&mut self, timeout: Duration) -> Result<Option<Vec<u8>>, DeviceError> {
        self.transport
            .read_report(timeout)
            .map_err(|_| DeviceError::ReadFailed)
    }
}

/// Reads the four source-confirmed information GETs through an existing
/// serialized native session, allowing its actor to keep routing 0x0A reports.
pub fn read_info_with_report_session<F>(
    session: &mut ReportSession,
    mut on_unsolicited: F,
) -> Result<DeviceInfo, DeviceError>
where
    F: FnMut(Vec<u8>),
{
    let mut queries = Vec::with_capacity(4);
    for command in [
        GetCommand::GetIdentity,
        GetCommand::GetMouseVersion,
        GetCommand::GetDongleVersion,
        GetCommand::GetBattery,
    ] {
        let request = Request::get(command);
        let wire_report = session.transact_hid_report(REPORT_ID, request.packet_bytes(), |report| {
            on_unsolicited(report)
        })?;
        let response = Response::parse(command, &wire_report[1..])?;
        queries.push(QueryInfo::from_response(command, &response, &wire_report));
    }
    Ok(DeviceInfo {
        schema_version: 1,
        interface: session.interface.clone(),
        transport_report_id: TRANSPORT_REPORT_ID,
        queries,
    })
}

fn execute_get<T: ReportIo>(
    transport: &mut T,
    command: GetCommand,
    timeout: Duration,
) -> Result<(Response, Vec<u8>), DeviceError> {
    let request = Request::get(command);
    let (raw_response, exact_wire_report) = transact_request(transport, &request, timeout)?;
    let response = Response::parse(command, raw_response.raw())?;
    Ok((response, exact_wire_report))
}

fn transact_request<T: ReportIo + ?Sized>(
    transport: &mut T,
    request: &Request,
    timeout: Duration,
) -> Result<(RawResponse, Vec<u8>), DeviceError> {
    let command_id = request.packet_bytes()[0];
    let mut wire_request = Vec::with_capacity(WIRE_REPORT_LEN);
    wire_request.push(request.report_id());
    wire_request.extend_from_slice(request.packet_bytes());

    let written = transport
        .write_report(&wire_request)
        .map_err(|_| DeviceError::WriteFailed)?;
    if written != WIRE_REPORT_LEN {
        return Err(DeviceError::ShortWrite {
            expected: WIRE_REPORT_LEN,
            actual: written,
        });
    }

    let deadline = Instant::now()
        .checked_add(timeout)
        .unwrap_or_else(Instant::now);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(DeviceError::DeadlineExpired {
                command_id,
            });
        }

        let Some(wire_report) = transport
            .read_report(remaining)
            .map_err(|_| DeviceError::ReadFailed)?
        else {
            return Err(DeviceError::DeadlineExpired {
                command_id,
            });
        };
        if Instant::now() >= deadline {
            return Err(DeviceError::DeadlineExpired {
                command_id,
            });
        }

        if wire_report.len() != WIRE_REPORT_LEN {
            return Err(DeviceError::WrongReportLength {
                expected: WIRE_REPORT_LEN,
                actual: wire_report.len(),
            });
        }
        if wire_report[0] != TRANSPORT_REPORT_ID {
            return Err(DeviceError::WrongReportId {
                expected: TRANSPORT_REPORT_ID,
                actual: wire_report[0],
            });
        }

        if wire_report[1] != command_id {
            continue;
        }

        let response = RawResponse::parse(command_id, &wire_report[1..])?;
        return Ok((response, wire_report));
    }
}

impl QueryInfo {
    fn from_response(command: GetCommand, response: &Response, exact_wire_report: &[u8]) -> Self {
        let (wire_bytes_redacted, redacted_wire_byte_ranges) =
            sanitize_wire_report(command, exact_wire_report);
        let data_valid_len = response.data_valid_len();

        let mut known_values = KnownValues::default();
        match command {
            GetCommand::GetIdentity if data_valid_len >= 2 => {
                if let Some((cid, mid)) = response.cid_mid_raw() {
                    known_values.cid_raw = Some(cid);
                    known_values.mid_raw = Some(mid);
                }
            }
            GetCommand::GetMouseVersion | GetCommand::GetDongleVersion
                if data_valid_len >= 2 =>
            {
                known_values.version_bytes_raw = response
                    .version_bytes_raw()
                    .map(|(major, minor)| [major, minor]);
                known_values.version_hex_pair = response.version_hex_pair();
            }
            GetCommand::GetBattery => {
                known_values.battery_raw = response.battery_raw().map(Into::into);
            }
            _ => {}
        }

        Self {
            command: command.name().to_owned(),
            command_id: command.id(),
            command_status_raw: response.command_status_raw(),
            address_be_raw: response.address_be(),
            data_valid_len,
            report: SanitizedReport {
                wire_bytes_redacted,
                coordinate_system: WIRE_COORDINATE_SYSTEM,
                redacted_wire_byte_ranges,
            },
            known_values,
        }
    }
}

fn sanitize_wire_report(command: GetCommand, exact_wire_report: &[u8]) -> (Vec<u8>, Vec<RedactionRange>) {
    let mut public_bytes = exact_wire_report.to_vec();
    if command == GetCommand::GetIdentity && public_bytes.len() == WIRE_REPORT_LEN {
        // Packet byte offsets 11..14 shift by one on the wire because byte 0
        // is the transport ReportID. Keep the final checksum byte at wire 16.
        public_bytes[IDENTITY_REDACTED_WIRE_RANGE.start..IDENTITY_REDACTED_WIRE_RANGE.end]
            .fill(0);
        (public_bytes, vec![IDENTITY_REDACTED_WIRE_RANGE])
    } else {
        (public_bytes, Vec::new())
    }
}

#[cfg(windows)]
mod native {
    use super::*;
    use hidapi::{HidApi, HidDevice};
    use std::collections::HashMap;
    use std::mem::size_of;
    use windows::core::PCWSTR;
    use windows::Win32::Devices::DeviceAndDriverInstallation::{
        SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInterfaces, SetupDiGetClassDevsW,
        SetupDiGetDeviceInterfaceDetailW, SetupDiGetDevicePropertyW,
        SP_DEVICE_INTERFACE_DATA, SP_DEVICE_INTERFACE_DETAIL_DATA_W, SP_DEVINFO_DATA,
        DIGCF_DEVICEINTERFACE, DIGCF_PRESENT, HDEVINFO,
    };
    use windows::Win32::Devices::HumanInterfaceDevice::HidD_GetHidGuid;
    use windows::Win32::Devices::Properties::{
        DEVPROP_TYPE_GUID, DEVPROPTYPE, DEVPKEY_Device_ContainerId,
    };
    use windows::Win32::Foundation::{HWND, ERROR_NO_MORE_ITEMS};

    const DESCRIPTOR_EVIDENCE: &str = "evidence/hid_descriptor_diagnostics.json";

    fn target_webhid_collections() -> Vec<CollectionMetadata> {
        vec![
            CollectionMetadata {
                usage_page: TARGET_USAGE_PAGE,
                usage: TARGET_USAGE,
                input_reports: vec![ReportMetadata { report_id: 8 }],
                output_reports: vec![ReportMetadata { report_id: 8 }],
                feature_reports: Vec::new(),
            },
            CollectionMetadata {
                usage_page: 0xff04,
                usage: TARGET_USAGE,
                input_reports: Vec::new(),
                output_reports: Vec::new(),
                feature_reports: vec![ReportMetadata { report_id: 6 }],
            },
        ]
    }

    fn interface_path_key(path: &str) -> String {
        path.to_ascii_lowercase()
    }

    fn same_physical_collection_group(
        left_path: &str,
        right_path: &str,
        container_ids: &HashMap<String, [u8; 16]>,
    ) -> bool {
        match (
            container_ids.get(&interface_path_key(left_path)),
            container_ids.get(&interface_path_key(right_path)),
        ) {
            (Some(left), Some(right)) => left == right,
            _ => false,
        }
    }

    struct DeviceInfoSet(HDEVINFO);

    impl Drop for DeviceInfoSet {
        fn drop(&mut self) {
            let _ = unsafe { SetupDiDestroyDeviceInfoList(self.0) };
        }
    }

    fn is_no_more_items(error: &windows::core::Error) -> bool {
        error.code().0 as u32 == (0x8007_0000 | ERROR_NO_MORE_ITEMS.0)
    }

    /// Maps exact HID interface paths to Windows' physical device-container ID.
    /// `DEVPKEY_Device_ContainerId` identifies devnodes belonging to one
    /// physical device container; SetupDiGetDevicePropertyW reads that key.
    /// References: https://learn.microsoft.com/en-us/windows-hardware/drivers/install/devpkey-device-containerid
    /// and https://learn.microsoft.com/en-us/windows/win32/api/setupapi/nf-setupapi-setupdigetdevicepropertyw.
    /// The path is only a lookup key; grouping itself uses the system property.
    fn enumerate_hid_container_ids() -> Result<HashMap<String, [u8; 16]>, DeviceError> {
        let hid_guid = unsafe { HidD_GetHidGuid() };
        let info_set = unsafe {
            SetupDiGetClassDevsW(
                Some(&hid_guid),
                PCWSTR::null(),
                HWND::default(),
                DIGCF_DEVICEINTERFACE | DIGCF_PRESENT,
            )
        }
        .map(DeviceInfoSet)
        .map_err(|_| DeviceError::DeviceContainerEnumeration)?;

        let mut container_ids = HashMap::new();
        let mut index = 0u32;
        loop {
            let mut interface_data = SP_DEVICE_INTERFACE_DATA::default();
            interface_data.cbSize = size_of::<SP_DEVICE_INTERFACE_DATA>() as u32;
            match unsafe {
                SetupDiEnumDeviceInterfaces(
                    info_set.0,
                    None,
                    &hid_guid,
                    index,
                    &mut interface_data,
                )
            } {
                Ok(()) => {}
                Err(error) if is_no_more_items(&error) => break,
                Err(_) => return Err(DeviceError::DeviceContainerEnumeration),
            }
            index = index.saturating_add(1);

            let mut required_size = 0u32;
            let _ = unsafe {
                SetupDiGetDeviceInterfaceDetailW(
                    info_set.0,
                    &interface_data,
                    None,
                    0,
                    Some(&mut required_size),
                    None,
                )
            };
            if required_size < size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32 {
                continue;
            }

            let word_count = (required_size as usize).div_ceil(size_of::<usize>());
            let mut detail_storage = vec![0usize; word_count];
            let detail = detail_storage
                .as_mut_ptr()
                .cast::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>();
            unsafe { (*detail).cbSize = size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32 };
            let mut device_data = SP_DEVINFO_DATA::default();
            device_data.cbSize = size_of::<SP_DEVINFO_DATA>() as u32;
            if unsafe {
                SetupDiGetDeviceInterfaceDetailW(
                    info_set.0,
                    &interface_data,
                    Some(detail),
                    required_size,
                    Some(&mut required_size),
                    Some(&mut device_data),
                )
            }
            .is_err()
            {
                continue;
            }

            let device_path = unsafe { PCWSTR((*detail).DevicePath.as_ptr()).to_string() }
                .map_err(|_| DeviceError::DeviceContainerEnumeration)?;
            let lowered_path = interface_path_key(&device_path);
            if !lowered_path.contains("vid_373b&pid_1085")
                && !lowered_path.contains("vid_3554&pid_f58f") {
                continue;
            }
            let mut property_type = DEVPROPTYPE(0);
            let mut property_size = 0u32;
            let mut raw_container_id = [0u8; 16];
            if unsafe {
                SetupDiGetDevicePropertyW(
                    info_set.0,
                    &device_data,
                    &DEVPKEY_Device_ContainerId,
                    &mut property_type,
                    Some(&mut raw_container_id),
                    Some(&mut property_size),
                    0,
                )
            }
            .is_err()
                || property_type != DEVPROP_TYPE_GUID
                || property_size as usize != raw_container_id.len()
            {
                continue;
            }

            container_ids.insert(lowered_path, raw_container_id);
        }
        Ok(container_ids)
    }

    fn opaque_device_key(path: &[u8], interface: &InterfaceMetadata) -> String {
        let mut hasher = Sha256::new();
        hasher.update(path);
        hasher.update(interface.vendor_id.to_le_bytes());
        hasher.update(interface.product_id.to_le_bytes());
        hasher.update(interface.usage_page.to_le_bytes());
        hasher.update(interface.usage.to_le_bytes());
        let digest = hasher.finalize();
        let mut key = String::with_capacity(digest.len() * 2);
        for byte in digest {
            use std::fmt::Write as _;
            let _ = write!(&mut key, "{byte:02x}");
        }
        key
    }

    fn matches_target(info: &hidapi::DeviceInfo) -> bool {
        is_supported_transport(info.vendor_id(), info.product_id())
            && info.usage_page() == TARGET_USAGE_PAGE
            && info.usage() == TARGET_USAGE
    }

    fn metadata(info: &hidapi::DeviceInfo) -> InterfaceMetadata {
        InterfaceMetadata {
            vendor_id: info.vendor_id(),
            product_id: info.product_id(),
            usage_page: info.usage_page(),
            usage: info.usage(),
        }
    }

    pub fn list_target_interfaces() -> Result<Vec<InterfaceMetadata>, DeviceError> {
        let api = HidApi::new().map_err(|_| DeviceError::HidApiInitialization)?;
        Ok(api
            .device_list()
            .filter(|info| matches_target(info))
            .map(metadata)
            .collect())
    }

    pub fn list_target_devices() -> Result<Vec<TargetDevice>, DeviceError> {
        let api = HidApi::new().map_err(|_| DeviceError::HidApiInitialization)?;
        let container_ids = enumerate_hid_container_ids()?;
        let infos: Vec<_> = api
            .device_list()
            .filter(|info| {
                is_supported_transport(info.vendor_id(), info.product_id())
            })
            .collect();
        Ok(infos
            .iter()
            .copied()
            .filter(|info| matches_target(info))
            .filter(|info| {
                // The WebHID permission/filter identity observed in the
                // frontend is FF04/usage 2. Do not present an FF02 native
                // endpoint as a WebHID device unless its real sibling
                // collection is also enumerated on the same HID instance.
                infos.iter().any(|sibling| {
                    sibling.usage_page() == 0xff04
                        && sibling.usage() == TARGET_USAGE
                        && same_physical_collection_group(
                            &info.path().to_string_lossy(),
                            &sibling.path().to_string_lossy(),
                            &container_ids,
                        )
                })
            })
            .map(|info| {
                let interface = metadata(info);
                TargetDevice {
                    device_key: opaque_device_key(info.path().to_bytes(), &interface),
                    interface,
                    product_name: info.product_string().map(str::to_owned),
                    serial_number: info.serial_number().map(str::to_owned),
                    collections: target_webhid_collections(),
                    descriptor_source: DESCRIPTOR_EVIDENCE,
                }
            })
            .collect())
    }

    pub fn open_report_session_for_key(
        key: &str,
        timeout: Duration,
    ) -> Result<ReportSession, DeviceError> {
        let api = HidApi::new().map_err(|_| DeviceError::HidApiInitialization)?;
        let matching: Vec<_> = api
            .device_list()
            .filter(|info| matches_target(info))
            .filter(|info| opaque_device_key(info.path().to_bytes(), &metadata(info)) == key)
            .collect();
        let selected = match matching.as_slice() {
            [] => return Err(DeviceError::DeviceNotFound),
            [one] => *one,
            many => return Err(DeviceError::MultipleMatchingCollections { count: many.len() }),
        };
        let interface = metadata(selected);
        let device = api
            .open_path(selected.path())
            .map_err(|_| DeviceError::OpenCollection)?;
        Ok(ReportSession::new(
            NativeTransport { device },
            interface,
            key.to_owned(),
            timeout,
        ))
    }

    pub fn open_unique_report_session(timeout: Duration) -> Result<ReportSession, DeviceError> {
        let devices = list_target_devices()?;
        let selected = match devices.as_slice() {
            [] => return Err(DeviceError::NoMatchingCollection),
            [one] => one,
            many => return Err(DeviceError::MultipleMatchingCollections { count: many.len() }),
        };
        open_report_session_for_key(&selected.device_key, timeout)
    }

    pub fn read_target_info(timeout: Duration) -> Result<DeviceInfo, DeviceError> {
        let api = HidApi::new().map_err(|_| DeviceError::HidApiInitialization)?;
        let matching: Vec<_> = api
            .device_list()
            .filter(|info| matches_target(info))
            .collect();
        let selected = match matching.as_slice() {
            [] => return Err(DeviceError::NoMatchingCollection),
            [one] => *one,
            many => return Err(DeviceError::MultipleMatchingCollections { count: many.len() }),
        };
        let interface = metadata(selected);
        let device = api
            .open_path(selected.path())
            .map_err(|_| DeviceError::OpenCollection)?;
        read_info_with_transport(NativeTransport { device }, interface, timeout)
    }

    pub fn open_config_session(timeout: Duration) -> Result<ConfigSession, DeviceError> {
        let api = HidApi::new().map_err(|_| DeviceError::HidApiInitialization)?;
        let matching: Vec<_> = api
            .device_list()
            .filter(|info| matches_target(info))
            .collect();
        let selected = match matching.as_slice() {
            [] => return Err(DeviceError::NoMatchingCollection),
            [one] => *one,
            many => return Err(DeviceError::MultipleMatchingCollections { count: many.len() }),
        };
        let interface = metadata(selected);
        let device = api
            .open_path(selected.path())
            .map_err(|_| DeviceError::OpenCollection)?;
        Ok(ConfigSession::new(
            NativeTransport { device },
            interface,
            timeout,
        ))
    }

    struct NativeTransport {
        device: HidDevice,
    }

    impl ReportIo for NativeTransport {
        fn write_report(&mut self, report: &[u8]) -> Result<usize, TransportError> {
            self.device
                .write(report)
                .map_err(|_| TransportError::WriteFailed)
        }

        fn read_report(
            &mut self,
            timeout: Duration,
        ) -> Result<Option<Vec<u8>>, TransportError> {
            let millis = timeout.as_millis().max(1).min(i32::MAX as u128) as i32;
            let mut bytes = vec![0u8; WIRE_REPORT_LEN];
            let count = self
                .device
                .read_timeout(&mut bytes, millis)
                .map_err(|_| TransportError::ReadFailed)?;
            if count == 0 {
                return Ok(None);
            }
            bytes.truncate(count);
            Ok(Some(bytes))
        }
    }

    #[cfg(test)]
    mod physical_group_tests {
        use super::*;

        #[test]
        fn sibling_collections_with_distinct_paths_share_the_container_identity() {
            let left_path = r"\\?\hid#vid_373b&pid_1085&mi_01&col05#7&example&0&0004#{hid}";
            let right_path = r"\\?\hid#vid_373b&pid_1085&mi_01&col06#7&example&0&0005#{hid}";
            let mut container_ids = HashMap::new();
            container_ids.insert(interface_path_key(left_path), [0x2a; 16]);
            container_ids.insert(interface_path_key(right_path), [0x2a; 16]);

            assert!(same_physical_collection_group(
                left_path,
                right_path,
                &container_ids
            ));
        }

        #[test]
        fn paths_from_different_device_containers_are_never_grouped() {
            let left_path = r"\\?\hid#vid_373b&pid_1085&mi_01&col05#7&first&0&0004#{hid}";
            let right_path = r"\\?\hid#vid_373b&pid_1085&mi_01&col06#7&second&0&0005#{hid}";
            let mut container_ids = HashMap::new();
            container_ids.insert(interface_path_key(left_path), [0x2a; 16]);
            container_ids.insert(interface_path_key(right_path), [0x2b; 16]);

            assert!(!same_physical_collection_group(
                left_path,
                right_path,
                &container_ids
            ));
            assert!(!same_physical_collection_group(
                left_path,
                right_path,
                &HashMap::new()
            ));
        }
    }
}

#[cfg(windows)]
pub use native::{
    list_target_devices, list_target_interfaces, open_config_session,
    open_report_session_for_key, open_unique_report_session, read_target_info,
};

#[cfg(not(windows))]
pub fn list_target_interfaces() -> Result<Vec<InterfaceMetadata>, DeviceError> {
    Err(DeviceError::UnsupportedPlatform)
}

#[cfg(not(windows))]
pub fn list_target_devices() -> Result<Vec<TargetDevice>, DeviceError> {
    Err(DeviceError::UnsupportedPlatform)
}

#[cfg(not(windows))]
pub fn read_target_info(_timeout: Duration) -> Result<DeviceInfo, DeviceError> {
    Err(DeviceError::UnsupportedPlatform)
}

#[cfg(not(windows))]
pub fn open_config_session(_timeout: Duration) -> Result<ConfigSession, DeviceError> {
    Err(DeviceError::UnsupportedPlatform)
}

#[cfg(not(windows))]
pub fn open_report_session_for_key(
    _key: &str,
    _timeout: Duration,
) -> Result<ReportSession, DeviceError> {
    Err(DeviceError::UnsupportedPlatform)
}

#[cfg(not(windows))]
pub fn open_unique_report_session(_timeout: Duration) -> Result<ReportSession, DeviceError> {
    Err(DeviceError::UnsupportedPlatform)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use std::collections::VecDeque;
    use std::thread;

    #[derive(Default)]
    struct MockTransport {
        writes: Vec<Vec<u8>>,
        reads: VecDeque<MockRead>,
        observed_timeouts: Vec<Duration>,
        write_len_override: Option<usize>,
        shared_stats: Option<Arc<Mutex<IoStats>>>,
    }

    #[derive(Default)]
    struct IoStats {
        writes: usize,
        reads: usize,
        reports: Vec<Vec<u8>>,
    }

    enum MockRead {
        Report(Vec<u8>),
        Timeout,
        DelayTimeout,
        DelayedReport(Vec<u8>),
    }

    impl ReportIo for MockTransport {
        fn write_report(&mut self, report: &[u8]) -> Result<usize, TransportError> {
            self.writes.push(report.to_vec());
            if let Some(stats) = &self.shared_stats {
                let mut stats = stats.lock().unwrap();
                stats.writes += 1;
                stats.reports.push(report.to_vec());
            }
            Ok(self.write_len_override.unwrap_or(report.len()))
        }

        fn read_report(
            &mut self,
            timeout: Duration,
        ) -> Result<Option<Vec<u8>>, TransportError> {
            self.observed_timeouts.push(timeout);
            if let Some(stats) = &self.shared_stats {
                stats.lock().unwrap().reads += 1;
            }
            match self.reads.pop_front().unwrap_or(MockRead::Timeout) {
                MockRead::Report(report) => Ok(Some(report)),
                MockRead::Timeout => Ok(None),
                MockRead::DelayTimeout => {
                    thread::sleep(timeout);
                    Ok(None)
                }
                MockRead::DelayedReport(report) => {
                    thread::sleep(timeout + Duration::from_millis(2));
                    Ok(Some(report))
                }
            }
        }
    }

    impl MockTransport {
        fn tracked() -> (Self, Arc<Mutex<IoStats>>) {
            let stats = Arc::new(Mutex::new(IoStats::default()));
            let transport = Self {
                shared_stats: Some(Arc::clone(&stats)),
                ..Self::default()
            };
            (transport, stats)
        }
    }

    fn wire_report(packet: [u8; PACKET_LEN]) -> Vec<u8> {
        let mut report = Vec::with_capacity(WIRE_REPORT_LEN);
        report.push(TRANSPORT_REPORT_ID);
        report.extend_from_slice(&packet);
        report
    }

    fn packet(command: GetCommand, status: u8, data_len: u8, payload: &[u8]) -> [u8; PACKET_LEN] {
        let mut packet = [0; PACKET_LEN];
        packet[0] = command.id();
        packet[1] = status;
        packet[4] = data_len;
        let end = 5 + payload.len().min(10);
        packet[5..end].copy_from_slice(&payload[..end - 5]);
        packet
    }

    fn raw_packet(command_id: u8, address_be: u16, status: u8, data_len: u8, payload: &[u8]) -> [u8; PACKET_LEN] {
        let mut packet = [0; PACKET_LEN];
        packet[0] = command_id;
        packet[1] = status;
        packet[2..4].copy_from_slice(&address_be.to_be_bytes());
        packet[4] = data_len;
        let end = 5 + payload.len().min(10);
        packet[5..end].copy_from_slice(&payload[..end - 5]);
        packet
    }

    fn raw_wire_report(
        command_id: u8,
        status: u8,
        data_len: u8,
        payload: &[u8],
        checksum: u8,
    ) -> Vec<u8> {
        let mut packet = raw_packet(command_id, 0, status, data_len, payload);
        packet[PACKET_LEN - 1] = checksum;
        wire_report(packet)
    }

    fn complete_fixture() -> MockTransport {
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../tests/fixtures/round3_get_reports.json"))
                .unwrap();
        let reports = fixture["wireReports"].as_array().unwrap();
        let mut transport = MockTransport::default();
        for report in reports {
            let bytes: Vec<u8> = report
                .as_array()
                .unwrap()
                .iter()
                .map(|byte| byte.as_u64().unwrap() as u8)
                .collect();
            transport.reads.push_back(MockRead::Report(bytes));
        }
        transport
    }

    #[test]
    fn request_framing_uses_report_id_8_plus_one_16_byte_get_packet() {
        let mut transport = MockTransport::default();
        transport.reads.push_back(MockRead::Report(wire_report(packet(
            GetCommand::GetIdentity,
            0,
            2,
            &[2, 32],
        ))));
        let result = execute_get(
            &mut transport,
            GetCommand::GetIdentity,
            Duration::from_millis(100),
        )
        .unwrap();

        assert_eq!(transport.writes.len(), 1);
        assert_eq!(transport.writes[0].len(), WIRE_REPORT_LEN);
        assert_eq!(transport.writes[0][0], 8);
        assert_eq!(transport.writes[0][1], GetCommand::GetIdentity.id());
        assert_eq!(result.1.len(), WIRE_REPORT_LEN);
    }

    #[test]
    fn interleaved_command_is_skipped_without_resending_get() {
        let mut transport = MockTransport::default();
        transport.reads.push_back(MockRead::Report(wire_report(packet(
            GetCommand::GetMouseVersion,
            0,
            2,
            &[3, 21],
        ))));
        transport.reads.push_back(MockRead::Report(wire_report(packet(
            GetCommand::GetIdentity,
            0,
            2,
            &[2, 32],
        ))));

        let (response, _) = execute_get(
            &mut transport,
            GetCommand::GetIdentity,
            Duration::from_millis(100),
        )
        .unwrap();
        assert_eq!(response.command(), GetCommand::GetIdentity);
        assert_eq!(transport.writes.len(), 1);
        assert_eq!(transport.observed_timeouts.len(), 2);
    }

    #[test]
    fn deadline_is_finite_and_does_not_retry_the_write() {
        let mut transport = MockTransport::default();
        transport.reads.push_back(MockRead::DelayTimeout);
        let timeout = Duration::from_millis(25);
        let started = Instant::now();
        let result = execute_get(&mut transport, GetCommand::GetBattery, timeout);

        assert!(matches!(result, Err(DeviceError::DeadlineExpired { .. })));
        assert_eq!(transport.writes.len(), 1);
        assert_eq!(transport.observed_timeouts.len(), 1);
        assert!(transport.observed_timeouts[0] <= timeout);
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn matching_reply_returned_after_deadline_is_rejected() {
        let mut transport = MockTransport::default();
        transport.reads.push_back(MockRead::DelayedReport(wire_report(packet(
            GetCommand::GetBattery,
            0,
            2,
            &[30, 0, 14, 94],
        ))));
        let result = execute_get(
            &mut transport,
            GetCommand::GetBattery,
            Duration::from_millis(20),
        );

        assert!(matches!(result, Err(DeviceError::DeadlineExpired { .. })));
        assert_eq!(transport.writes.len(), 1);
    }

    #[test]
    fn config_session_denies_flash_reset_pairing_and_non_allowlisted_ids_before_io() {
        let (transport, stats) = MockTransport::tracked();
        let mut session = ConfigSession::new(
            transport,
            InterfaceMetadata::target(),
            Duration::from_millis(50),
        );
        for command_id in [0x01, 0x05, 0x09, 0x0d, 0x20] {
            let request = Request::from_fields(command_id, 0, 0, &[]).unwrap();
            assert!(matches!(
                session.transact_config(&request),
                Err(DeviceError::UnsupportedConfigCommand { command_id: actual })
                    if actual == command_id
            ));
        }
        let stats = stats.lock().unwrap();
        assert_eq!(stats.writes, 0);
        assert_eq!(stats.reads, 0);
    }

    #[test]
    fn report_session_allows_only_the_source_confirmed_transport_challenge_shape() {
        let (mut transport, stats) = MockTransport::tracked();
        transport.reads.push_back(MockRead::Report(raw_wire_report(1, 0, 8, &[1, 2, 3, 4, 2, 32, 1, 0], 0)));
        let mut session = ReportSession::new(transport, InterfaceMetadata::target(), "usb".into(), Duration::from_millis(50));
        for request in [Request::from_fields(1, 0, 0, &[]).unwrap(),
            Request::from_fields(1, 1, 8, &[1, 2, 3, 4, 0, 0, 0, 0]).unwrap(),
            Request::from_fields(1, 0, 8, &[1, 2, 3, 4, 9, 0, 0, 0]).unwrap()] {
            assert!(matches!(session.transact_hid_report(8, request.packet_bytes(), |_| {}), Err(DeviceError::UnsupportedMouseCommand { command_id: 1 })));
        }
        assert_eq!(stats.lock().unwrap().writes, 0);
        let challenge = Request::from_fields(1, 0, 8, &[1, 2, 3, 4, 0, 0, 0, 0]).unwrap();
        assert!(session.transact_hid_report(8, challenge.packet_bytes(), |_| {}).is_ok());
        assert_eq!(stats.lock().unwrap().writes, 1);
    }

    #[test]
    fn report_session_cancels_topology_change_before_write_and_during_wait() {
        struct ChangingTransport { revision: Arc<AtomicU64>, writes: Arc<AtomicU64> }
        impl ReportIo for ChangingTransport {
            fn write_report(&mut self, report: &[u8]) -> Result<usize, TransportError> {
                self.writes.fetch_add(1, Ordering::Relaxed);
                Ok(report.len())
            }
            fn read_report(&mut self, timeout: Duration) -> Result<Option<Vec<u8>>, TransportError> {
                assert!(timeout <= Duration::from_millis(25));
                self.revision.fetch_add(1, Ordering::AcqRel);
                Ok(None)
            }
        }
        for before_write in [true, false] {
            let revision = Arc::new(AtomicU64::new(0));
            let writes = Arc::new(AtomicU64::new(0));
            let mut session = ReportSession::new(
                ChangingTransport { revision: Arc::clone(&revision), writes: Arc::clone(&writes) },
                InterfaceMetadata::target(), "mouse".into(), Duration::from_secs(2));
            session.watch_topology_changes(Arc::clone(&revision));
            if before_write { revision.fetch_add(1, Ordering::AcqRel); }
            let request = Request::get(GetCommand::GetBattery);
            assert!(matches!(session.transact_hid_report(8, request.packet_bytes(), |_| {}), Err(DeviceError::TransportChanged)));
            assert_eq!(writes.load(Ordering::Relaxed), if before_write { 0 } else { 1 });
        }
    }

    #[test]
    fn report_session_validates_outbound_frames_before_io() {
        let (transport, stats) = MockTransport::tracked();
        let mut session = ReportSession::new(
            transport,
            InterfaceMetadata::target(),
            "test-device-key".to_owned(),
            Duration::from_millis(50),
        );
        let request = Request::get(GetCommand::GetBattery);

        assert!(matches!(
            session.transact_hid_report(7, request.packet_bytes(), |_| {}),
            Err(DeviceError::WrongReportId { .. })
        ));
        assert!(matches!(
            session.transact_hid_report(8, &request.packet_bytes()[..15], |_| {}),
            Err(DeviceError::InvalidRequestReportLength { .. })
        ));
        let unsupported = Request::from_fields(0x20, 0, 0, &[]).unwrap();
        assert!(matches!(
            session.transact_hid_report(8, unsupported.packet_bytes(), |_| {}),
            Err(DeviceError::UnsupportedMouseCommand { command_id: 0x20 })
        ));
        let mut bad_checksum = *request.packet_bytes();
        bad_checksum[15] ^= 1;
        assert!(matches!(
            session.transact_hid_report(8, &bad_checksum, |_| {}),
            Err(DeviceError::InvalidRequestChecksum)
        ));

        let stats = stats.lock().unwrap();
        assert_eq!(stats.writes, 0);
        assert_eq!(stats.reads, 0);
    }

    #[test]
    fn report_session_routes_interleaved_events_and_keeps_unknown_response_checksum() {
        let (mut transport, stats) = MockTransport::tracked();
        let unsolicited = raw_wire_report(0x0a, 0, 2, &[1, 2], 0x77);
        let unrelated = raw_wire_report(0x12, 0, 2, &[3, 21], 0x22);
        let response = raw_wire_report(0x04, 0, 2, &[25, 0], 0xa5);
        transport.reads.push_back(MockRead::Report(unsolicited.clone()));
        transport.reads.push_back(MockRead::Report(unrelated));
        transport.reads.push_back(MockRead::Report(response.clone()));
        let mut session = ReportSession::new(
            transport,
            InterfaceMetadata::target(),
            "test-device-key".to_owned(),
            Duration::from_millis(100),
        );
        let request = Request::get(GetCommand::GetBattery);
        let mut delivered = Vec::new();
        let actual = session
            .transact_hid_report(8, request.packet_bytes(), |event| delivered.push(event))
            .unwrap();

        assert_eq!(delivered, vec![unsolicited]);
        assert_eq!(actual, response);
        assert_eq!(actual[PACKET_LEN], 0xa5);
        let stats = stats.lock().unwrap();
        assert_eq!(stats.writes, 1);
        assert_eq!(stats.reads, 3);
    }

    #[test]
    fn report_session_returns_real_error_status_response_without_fabricating_a_reply() {
        let mut transport = MockTransport::default();
        let response = raw_wire_report(0x04, 1, 0, &[], 0x99);
        transport.reads.push_back(MockRead::Report(response.clone()));
        let mut session = ReportSession::new(
            transport,
            InterfaceMetadata::target(),
            "test-device-key".to_owned(),
            Duration::from_millis(100),
        );
        let request = Request::get(GetCommand::GetBattery);
        let actual = session
            .transact_hid_report(8, request.packet_bytes(), |_| {})
            .unwrap();
        assert_eq!(actual, response);
        let parsed = RawResponse::parse(0x04, &actual[1..]).unwrap();
        assert_eq!(parsed.command_status_raw(), 1);
        assert_eq!(parsed.data_valid_len(), 0);
    }

    #[test]
    fn report_session_times_out_without_retransmitting() {
        let (mut transport, stats) = MockTransport::tracked();
        transport.reads.push_back(MockRead::DelayTimeout);
        let mut session = ReportSession::new(
            transport,
            InterfaceMetadata::target(),
            "test-device-key".to_owned(),
            Duration::from_millis(25),
        );
        let request = Request::get(GetCommand::GetBattery);
        assert!(matches!(
            session.transact_hid_report(8, request.packet_bytes(), |_| {}),
            Err(DeviceError::DeadlineExpired { command_id: 0x04 })
        ));
        let stats = stats.lock().unwrap();
        assert_eq!(stats.writes, 1);
        assert_eq!(stats.reads, 1);
    }

    #[test]
    fn settings_read_method_rejects_every_set_and_boot_command_before_io() {
        let (transport, stats) = MockTransport::tracked();
        let mut session = ConfigSession::new(
            transport,
            InterfaceMetadata::target(),
            Duration::from_millis(50),
        );
        for command_id in [0x01, 0x05, 0x07, 0x09, 0x0d, 0x0f, 0x16] {
            let request = Request::from_fields(command_id, 0, 0, &[]).unwrap();
            assert!(matches!(
                session.transact_settings_read(&request),
                Err(DeviceError::UnsupportedReadOnlyCommand { command_id: actual })
                    if actual == command_id
            ));
        }
        let stats = stats.lock().unwrap();
        assert_eq!(stats.writes, 0);
        assert_eq!(stats.reads, 0);
    }

    #[test]
    fn partial_config_write_is_reported_without_read_or_retry() {
        let (mut transport, stats) = MockTransport::tracked();
        transport.write_len_override = Some(9);
        let mut session = ConfigSession::new(
            transport,
            InterfaceMetadata::target(),
            Duration::from_millis(50),
        );
        let request = Request::from_fields(0x08, 0x1234, 8, &[]).unwrap();

        assert!(matches!(
            session.transact_config(&request),
            Err(DeviceError::ShortWrite {
                expected: WIRE_REPORT_LEN,
                actual: 9
            })
        ));
        let stats = stats.lock().unwrap();
        assert_eq!(stats.writes, 1);
        assert_eq!(stats.reads, 0);
    }

    #[test]
    fn eeprom_get_keeps_reserved_length_and_correlates_interleaved_reports_by_command() {
        let mut transport = MockTransport::default();
        transport.reads.push_back(MockRead::Report(wire_report(packet(
            GetCommand::GetMouseVersion,
            0,
            2,
            &[3, 21],
        ))));
        transport.reads.push_back(MockRead::Report(wire_report(raw_packet(
            0x08,
            0x1234,
            0,
            8,
            &[0x10, 0x20, 0x30, 0x40, 0x50, 0x60, 0x70, 0x80],
        ))));
        let (transport, stats) = {
            let stats = Arc::new(Mutex::new(IoStats::default()));
            transport.shared_stats = Some(Arc::clone(&stats));
            (transport, stats)
        };
        let mut session = ConfigSession::new(
            transport,
            InterfaceMetadata::target(),
            Duration::from_millis(100),
        );
        let request = Request::from_fields(0x08, 0x1234, 8, &[]).unwrap();
        let response = session.transact_config(&request).unwrap();

        assert_eq!(request.packet_bytes()[4], 8);
        assert_eq!(response.command_id_raw(), 0x08);
        assert_eq!(response.address_be(), 0x1234);
        assert_eq!(response.data_valid_len(), 8);
        assert_eq!(
            response.declared_payload(),
            &[0x10, 0x20, 0x30, 0x40, 0x50, 0x60, 0x70, 0x80]
        );
        let stats = stats.lock().unwrap();
        assert_eq!(stats.writes, 1);
        assert_eq!(stats.reads, 2);
        assert_eq!(stats.reports[0][0], TRANSPORT_REPORT_ID);
        assert_eq!(stats.reports[0].len(), WIRE_REPORT_LEN);
        assert_eq!(stats.reports[0][1], 0x08);
        assert_eq!(stats.reports[0][2], 0);
        assert_eq!(&stats.reports[0][3..5], &[0x12, 0x34]);
        assert_eq!(stats.reports[0][5], 8);
    }

    #[test]
    fn duplicate_config_command_calls_are_serialized_on_one_session() {
        let mut transport = MockTransport::default();
        transport.reads.push_back(MockRead::Report(wire_report(raw_packet(
            0x08,
            0x1010,
            0,
            1,
            &[0xa1],
        ))));
        transport.reads.push_back(MockRead::Report(wire_report(raw_packet(
            0x08,
            0x2020,
            0,
            1,
            &[0xb2],
        ))));
        let (transport, stats) = {
            let stats = Arc::new(Mutex::new(IoStats::default()));
            transport.shared_stats = Some(Arc::clone(&stats));
            (transport, stats)
        };
        let mut session = ConfigSession::new(
            transport,
            InterfaceMetadata::target(),
            Duration::from_millis(100),
        );
        let first = Request::from_fields(0x08, 0x1010, 1, &[]).unwrap();
        let second = Request::from_fields(0x08, 0x2020, 1, &[]).unwrap();

        assert_eq!(session.transact_config(&first).unwrap().declared_payload(), &[0xa1]);
        assert_eq!(session.transact_config(&second).unwrap().declared_payload(), &[0xb2]);
        let stats = stats.lock().unwrap();
        assert_eq!(stats.writes, 2);
        assert_eq!(stats.reads, 2);
        assert_eq!(stats.reports[0][1], 0x08);
        assert_eq!(stats.reports[1][1], 0x08);
        assert_eq!(&stats.reports[0][3..5], &[0x10, 0x10]);
        assert_eq!(&stats.reports[1][3..5], &[0x20, 0x20]);
    }

    #[test]
    fn config_read_times_out_after_one_get_write_without_retry() {
        let (mut transport, stats) = MockTransport::tracked();
        transport.reads.push_back(MockRead::Timeout);
        let mut session = ConfigSession::new(
            transport,
            InterfaceMetadata::target(),
            Duration::from_millis(20),
        );
        let request = Request::from_fields(0x08, 0x3000, 8, &[]).unwrap();

        assert!(matches!(
            session.transact_config(&request),
            Err(DeviceError::DeadlineExpired { command_id: 0x08 })
        ));
        let stats = stats.lock().unwrap();
        assert_eq!(stats.writes, 1);
        assert_eq!(stats.reads, 1);
    }

    #[test]
    fn wrong_report_id_and_wrong_wire_length_are_rejected() {
        let mut wrong_id = MockTransport::default();
        let mut report = wire_report(packet(GetCommand::GetBattery, 0, 0, &[]));
        report[0] = 7;
        wrong_id.reads.push_back(MockRead::Report(report));
        assert!(matches!(
            execute_get(
                &mut wrong_id,
                GetCommand::GetBattery,
                Duration::from_millis(100)
            ),
            Err(DeviceError::WrongReportId { actual: 7, .. })
        ));

        let mut wrong_length = MockTransport::default();
        wrong_length
            .reads
            .push_back(MockRead::Report(vec![TRANSPORT_REPORT_ID; WIRE_REPORT_LEN - 1]));
        assert!(matches!(
            execute_get(
                &mut wrong_length,
                GetCommand::GetBattery,
                Duration::from_millis(100)
            ),
            Err(DeviceError::WrongReportLength { actual: 16, .. })
        ));
    }

    #[test]
    fn captured_get_responses_keep_raw_values_and_empty_receiver_has_no_version() {
        let info = read_info_with_transport(
            complete_fixture(),
            InterfaceMetadata::target(),
            Duration::from_millis(100),
        )
        .unwrap();

        assert_eq!(info.queries.len(), 4);
        assert_eq!(info.queries[0].known_values.cid_raw, Some(2));
        assert_eq!(info.queries[0].known_values.mid_raw, Some(32));
        assert_eq!(info.queries[1].known_values.version_hex_pair.as_deref(), Some("3.15"));
        let receiver = &info.queries[2];
        assert_eq!(receiver.command_status_raw, 1);
        assert_eq!(receiver.data_valid_len, 0);
        assert_eq!(receiver.known_values.version_bytes_raw, None);
        assert_eq!(receiver.known_values.version_hex_pair, None);
        assert_eq!(
            info.queries[3].known_values.battery_raw,
            Some(BatteryFieldsRaw {
                battery_level: 30,
                charging: 0,
                voltage_be_raw: 3678,
                voltage_be_bytes_raw: [14, 94],
            })
        );
    }

    #[test]
    fn serialized_identity_report_masks_only_the_confirmed_range_and_marks_it() {
        let mut transport = MockTransport::default();
        let mut identity = wire_report(packet(
            GetCommand::GetIdentity,
            0,
            3,
            &[2, 32, 51],
        ));
        identity[12..16].copy_from_slice(&[0xaa, 0xbb, 0xcc, 0xdd]);
        identity[16] = 0xe5;
        transport.reads.push_back(MockRead::Report(identity));
        for command in [
            GetCommand::GetMouseVersion,
            GetCommand::GetDongleVersion,
            GetCommand::GetBattery,
        ] {
            let (status, len, payload) = match command {
                GetCommand::GetMouseVersion => (0, 2, &[3, 21][..]),
                GetCommand::GetDongleVersion => (1, 0, &[][..]),
                GetCommand::GetBattery => (0, 2, &[30, 0, 14, 94][..]),
                GetCommand::GetIdentity => unreachable!(),
            };
            transport
                .reads
                .push_back(MockRead::Report(wire_report(packet(command, status, len, payload))));
        }
        let info = read_info_with_transport(
            transport,
            InterfaceMetadata::target(),
            Duration::from_millis(100),
        )
        .unwrap();
        let value = serde_json::to_value(info).unwrap();
        let report = &value["queries"][0]["report"];
        let bytes = report["wireBytesRedacted"].as_array().unwrap();
        assert_eq!(bytes[8].as_u64(), Some(51));
        assert_eq!(bytes[11].as_u64(), Some(0));
        assert_eq!(&bytes[12..16], &[0, 0, 0, 0]);
        assert_eq!(bytes[16].as_u64(), Some(0xe5));
        assert_eq!(
            report["coordinateSystem"],
            WIRE_COORDINATE_SYSTEM
        );
        assert_eq!(report["redactedWireByteRanges"][0]["start"], 12);
        assert_eq!(report["redactedWireByteRanges"][0]["end"], 16);
    }
}
