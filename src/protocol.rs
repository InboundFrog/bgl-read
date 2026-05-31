//! ASTM E1381/E1394 protocol over HID for Bayer/Ascensia Contour meters.
//!
//! Packet format (received, 64 bytes):
//!   [0..2]  device header (3 bytes, ignored)
//!   [3]     SIZE: number of valid data bytes following
//!   [4..4+SIZE-1]  ASTM data
//!   [4+SIZE..63]   padding (zeros)
//!
//! Packet format (sent, 65 bytes including report ID):
//!   [0]     report ID = 0x00
//!   [1..3]  header (3 zeros)
//!   [4]     length of payload
//!   [5..]   payload bytes
//!   remainder: zero-padded to 65 bytes
//!
//! ASTM frame structure inside the data bytes:
//!   STX  seq_digit  record_text  CR  ETX|ETB  CS1  CS2  CR  LF
//! Checksum covers: seq_digit through ETX/ETB (inclusive), sum of bytes mod 256, uppercase hex.

use anyhow::{Result, anyhow};
use hidapi::{HidApi, HidDevice};
use serde::Serialize;
use std::fmt;
use std::io::Write as _;
use std::time::{Duration, Instant};

// ── Device IDs ────────────────────────────────────────────────────────────────

pub const VENDOR_ID: u16 = 0x1A79;

pub const SUPPORTED_DEVICES: &[(u16, &str)] = &[
    (0x7800, "Contour Next One"),
    (0x7440, "Contour Next USB"),
    (0x7350, "Contour Next"),
    (0x7900, "Ascensia Contour Next"),
    (0x6220, "Contour Next Link"),
    (0x6230, "Contour Next Link 2.4"),
];

// ── ASTM control bytes ────────────────────────────────────────────────────────

const ACK: u8 = 0x06;
const NAK: u8 = 0x15;
const STX: u8 = 0x02;
const ETX: u8 = 0x03;
const ETB: u8 = 0x17;
const ENQ: u8 = 0x05;
const EOT: u8 = 0x04;
const CR: u8 = 0x0D;

// ── Constants ─────────────────────────────────────────────────────────────────

const HID_PACKET_SIZE: usize = 64;
// Maximum data bytes per HID packet = 64 total - 4 overhead (3 header + 1 length byte)
const MAX_PAYLOAD: usize = HID_PACKET_SIZE - 4;
/// Transient I/O retries (receive_message returned Err).
const IO_RETRIES: u32 = 6;
/// Protocol-violation retries (unexpected message type or parse failure).
const PROTO_RETRIES: u32 = 6;
const RECEIVE_TIMEOUT: Duration = Duration::from_secs(10);

// ── Public data types ─────────────────────────────────────────────────────────

#[derive(Debug, Serialize, Clone)]
pub struct DeviceInfo {
    pub model: String,
    pub serial_number: String,
    pub record_count: u32,
    /// Device's own clock at time of session, ISO 8601 local (no TZ)
    pub device_time: String,
    pub low_threshold: u32,
    pub high_threshold: u32,
}

impl Default for DeviceInfo {
    fn default() -> Self {
        Self {
            model: "Unknown".into(),
            serial_number: "Unknown".into(),
            record_count: 0,
            device_time: String::new(),
            low_threshold: 20,
            high_threshold: 600,
        }
    }
}

#[derive(Debug, Serialize, Clone)]
pub struct Reading {
    pub record_number: u32,
    pub glucose_value: f64,
    /// "mg/dL" or "mmol/L"
    pub units: String,
    /// Device local time, ISO 8601 (no TZ)
    pub timestamp: String,
    pub high: bool,
    pub low: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub meal_marker: Option<String>,
    #[serde(skip)]
    pub is_control: bool,
}

/// Direction of a captured HID packet.
#[derive(Debug, Serialize, Clone, PartialEq)]
pub enum Dir {
    Tx,
    Rx,
}

impl fmt::Display for Dir {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Dir::Tx => "TX",
            Dir::Rx => "RX",
        })
    }
}

/// One captured HID packet (raw 64 bytes + direction).
#[derive(Debug, Clone)]
pub struct Packet {
    pub dir: Dir,
    pub data: [u8; HID_PACKET_SIZE],
}

/// Everything captured during a session.
#[derive(Debug, Serialize)]
pub struct Session {
    pub device: DeviceInfo,
    pub readings: Vec<Reading>,
    /// Raw ASTM record frame strings, in order received (H, P, R…, L)
    pub raw_records: Vec<String>,
    /// Every HID packet exchanged, in order
    #[serde(skip)]
    pub raw_packets: Vec<Packet>,
}

// ── Device discovery ──────────────────────────────────────────────────────────

pub fn list_devices(api: &HidApi) {
    let mut found = false;
    for dev in api.device_list() {
        if dev.vendor_id() != VENDOR_ID {
            continue;
        }
        found = true;
        let name = SUPPORTED_DEVICES
            .iter()
            .find(|(pid, _)| *pid == dev.product_id())
            .map(|(_, n)| *n)
            .unwrap_or("Unknown Contour device");
        println!(
            "{name}  VID={:#06x}  PID={:#06x}  S/N={}",
            dev.vendor_id(),
            dev.product_id(),
            dev.serial_number().unwrap_or("")
        );
    }
    if !found {
        println!("No Contour devices found.");
    }
}

pub fn open_device(api: &HidApi) -> Result<HidDevice> {
    for (pid, name) in SUPPORTED_DEVICES {
        if let Ok(dev) = api.open(VENDOR_ID, *pid) {
            eprintln!("Opened: {name}");
            return Ok(dev);
        }
    }
    Err(anyhow!(
        "No supported Contour device found — is it plugged in?\n\
         Run with --list to enumerate connected HID devices."
    ))
}

// ── Low-level HID I/O ─────────────────────────────────────────────────────────

/// Build a 65-byte write buffer (report-ID + 64 payload bytes).
fn build_write_packet(data: &[u8]) -> [u8; 65] {
    debug_assert!(
        data.len() <= MAX_PAYLOAD,
        "write payload {} exceeds MAX_PAYLOAD {}",
        data.len(),
        MAX_PAYLOAD
    );
    // Layout: [report_id=0, hdr0=0, hdr1=0, hdr2=0, length, data..., padding]
    let mut pkt = [0u8; 65];
    pkt[4] = data.len() as u8;
    pkt[5..5 + data.len()].copy_from_slice(data);
    pkt
}

/// Captures HID packets for `--format bytes`. When `capture` is false,
/// `push` is a no-op and no allocations happen beyond the empty Vec itself.
struct PacketLog {
    packets: Vec<Packet>,
    capture: bool,
}

impl PacketLog {
    fn new(capture: bool) -> Self {
        Self {
            packets: Vec::new(),
            capture,
        }
    }

    fn push(&mut self, packet: Packet) {
        if self.capture {
            self.packets.push(packet);
        }
    }
}

fn hid_write(device: &HidDevice, data: &[u8], log: &mut PacketLog) -> Result<()> {
    let pkt = build_write_packet(data);
    let mut tx_data = [0u8; HID_PACKET_SIZE];
    tx_data.copy_from_slice(&pkt[1..]);
    log.push(Packet {
        dir: Dir::Tx,
        data: tx_data,
    }); // log without report-ID
    device.write(&pkt)?;
    Ok(())
}

/// Read one 64-byte HID packet with a deadline.
fn hid_read(
    device: &HidDevice,
    deadline: Instant,
    log: &mut PacketLog,
) -> Result<[u8; HID_PACKET_SIZE]> {
    let remaining = deadline
        .saturating_duration_since(Instant::now())
        .as_millis()
        .min(i32::MAX as u128) as i32;
    if remaining <= 0 {
        return Err(anyhow!("Timeout waiting for device"));
    }
    let mut pkt = [0u8; HID_PACKET_SIZE];
    let n = device.read_timeout(&mut pkt, remaining)?;
    if n == 0 {
        return Err(anyhow!("Device read timeout (no data)"));
    }
    log.push(Packet {
        dir: Dir::Rx,
        data: pkt,
    });
    Ok(pkt)
}

// ── ASTM framing ──────────────────────────────────────────────────────────────

struct Message {
    msg_type: u8,
    /// Frame content stripped of STX, seq digit, ETX/ETB, checksum, and trailing CR.
    /// Empty for single-byte control messages (ENQ, EOT, ACK, NAK).
    frame: String,
}

/// Receive and reassemble a complete ASTM message from one or more HID packets.
///
/// Completion is signalled by:
///   - Partial packet (SIZE < MAX_PAYLOAD)
///   - First data byte is a single-byte control character (ENQ, EOT, ACK, NAK)
///   - Data ends with the ASTM tail:  … CR  ETX|ETB  CS1 CS2  CR  LF
///     which puts ETX/ETB at data[SIZE-5] (= pkt[SIZE-1] from start of packet)
fn receive_message(device: &HidDevice, timeout: Duration, log: &mut PacketLog) -> Result<Message> {
    let deadline = Instant::now() + timeout;
    let mut buf: Vec<u8> = Vec::new();

    loop {
        let pkt = hid_read(device, deadline, log)?;

        let size = pkt[3] as usize;
        let data_end = (4 + size).min(HID_PACKET_SIZE);
        let data = &pkt[4..data_end];
        buf.extend_from_slice(data);

        let first = buf.first().copied().unwrap_or(0);
        let is_complete = size < MAX_PAYLOAD
            || matches!(first, ENQ | EOT | ACK | NAK)
            || (buf.len() >= 5 && matches!(buf[buf.len() - 5], ETX | ETB));

        if is_complete {
            break;
        }
    }

    decode_message(&buf)
}

fn compute_checksum(data: &[u8]) -> String {
    let sum: u32 = data.iter().map(|&b| b as u32).sum();
    format!("{:02X}", sum % 256)
}

/// Decode the accumulated raw bytes into a Message.
fn decode_message(buf: &[u8]) -> Result<Message> {
    let msg_type = *buf.first().ok_or_else(|| anyhow!("Empty message buffer"))?;

    if msg_type != STX {
        return Ok(Message {
            msg_type,
            frame: String::new(),
        });
    }

    // Minimum STX frame: STX seq ETX/ETB CS1 CS2 CR LF = 7 bytes
    if buf.len() < 7 {
        return Err(anyhow!("ASTM frame too short ({} bytes)", buf.len()));
    }

    // buf layout:
    //   [0]          STX
    //   [1]          sequence digit (ASCII '0'..'7')
    //   [2..len-5]   frame content (record text, ends with CR ETX|ETB)
    //   [len-4..len-3] checksum (2 ASCII hex chars)
    //   [len-2..len-1] CR LF
    let checksum_region = &buf[1..buf.len() - 4];
    let expected = std::str::from_utf8(&buf[buf.len() - 4..buf.len() - 2]).unwrap_or("??");
    let computed = compute_checksum(checksum_region);
    if computed != expected {
        return Err(anyhow!(
            "Checksum mismatch (expected {expected}, computed {computed})"
        ));
    }

    // Frame content is buf[2..len-4] — includes the trailing CR+ETX/ETB.
    // Strip them so callers just see the record text.
    let content = &buf[2..buf.len() - 4];
    let content = match content {
        [rest @ .., ETX | ETB] => rest,
        _ => content,
    };
    let content = match content {
        [rest @ .., CR] => rest,
        _ => content,
    };

    let frame = String::from_utf8_lossy(content).into_owned();
    Ok(Message { msg_type, frame })
}

// ── Record parsing ────────────────────────────────────────────────────────────

#[derive(Debug)]
enum Record {
    Header(DeviceInfo),
    Result(Reading),
    Skip,
    Terminator,
    EndOfTransmission,
}

fn parse_record(frame: &str) -> Result<Record> {
    match frame.chars().next() {
        Some('H') => Ok(Record::Header(parse_header(frame)?)),
        Some('R') => parse_result_record(frame),
        Some('L') => Ok(Record::Terminator),
        Some('P') => Ok(Record::Skip),
        Some(c) => Err(anyhow!("Unknown record type '{c}' in frame: {frame:?}")),
        None => Err(anyhow!("Empty frame")),
    }
}

/// H record example:
///   H|\^&||qvqOi8|Bayer7350^FW\App\Boot^7358-1611135^0000-|A=1^U=0^V=20600|4|||||P|1|202505291248
fn parse_header(frame: &str) -> Result<DeviceInfo> {
    let fields: Vec<&str> = frame.split('|').collect();
    if fields.len() < 14 {
        return Err(anyhow!(
            "Header record has only {} fields (need 14)",
            fields.len()
        ));
    }

    let device_parts: Vec<&str> = fields[4].split('^').collect();
    let model = device_parts
        .first()
        .copied()
        .unwrap_or("Unknown")
        .to_string();
    let serial_raw = device_parts.get(2).copied().unwrap_or("");
    let serial_number = parse_serial(serial_raw);

    let nrecs: u32 = fields[6].trim().parse().unwrap_or(0);

    let ts_raw = fields[13].trim();
    let device_time = parse_timestamp(ts_raw);

    let (low_threshold, high_threshold) = parse_thresholds(fields[5]);

    Ok(DeviceInfo {
        model,
        serial_number,
        record_count: nrecs,
        device_time,
        low_threshold,
        high_threshold,
    })
}

/// Extract the serial number from the raw field.
/// "7830H5001733" → "5001733",  "6301-1C2CF8C" → "1C2CF8C"
/// Matches JS regex /^\d+[\w-]\s*(\w+)/ — skip leading digits + one separator char.
fn parse_serial(raw: &str) -> String {
    let after_digits = raw.trim_start_matches(|c: char| c.is_ascii_digit());
    // Skip exactly one separator character
    match after_digits.chars().next() {
        Some(sep) => after_digits[sep.len_utf8()..].trim_start().to_string(),
        None => raw.to_string(),
    }
}

/// Parse the config field (field[5]) for thresholds and units.
/// Config looks like:  A=1^C=00^I=0200^R=0^S=01^U=0^V=20600^X=...
fn parse_thresholds(config: &str) -> (u32, u32) {
    let mut low = 20u32;
    let mut high = 600u32;
    let mut mmol = false;

    for part in config.split('^') {
        if let Some(val) = part.strip_prefix("V=") {
            // V=LLOHHH  (LL = 2-digit low, HHH = 3-digit high)
            if val.len() >= 5 {
                low = val[..2].parse().unwrap_or(20);
                high = val[2..5].parse().unwrap_or(600);
            }
        } else if let Some(val) = part.strip_prefix("U=") {
            mmol = val.trim() == "1";
        }
    }

    if mmol {
        // Values were in mmol/L × 10; convert to mg/dL
        low = ((low as f64 / 10.0) * 18.01559).round() as u32;
        high = ((high as f64 / 10.0) * 18.01559).round() as u32;
    }

    (low, high)
}

/// R record example:
///   R|3|^^^Glucose|93|mg/dL^P||A/M0/T1||201505261150
fn parse_result_record(frame: &str) -> Result<Record> {
    let fields: Vec<&str> = frame.split('|').collect();

    // Need at least 9 fields; field[2] must be "^^^Glucose"
    if fields.len() < 9 || !fields[2].starts_with("^^^Glucose") {
        return Ok(Record::Skip); // non-glucose result, skip
    }

    let record_number: u32 = fields[1].parse().unwrap_or(0);
    let glucose_value: f64 = fields[3].parse().unwrap_or(0.0);
    let units = fields[4].split('^').next().unwrap_or("mg/dL").to_string();

    // fields[6] is the annotation / marker field, e.g. "A/M0/T1", ">", "C"
    let annotation = fields[6];
    let is_control = annotation.contains('C');
    let high = annotation.contains('>');
    let low = annotation.contains('<');
    let meal_marker = parse_meal_marker(annotation);

    // fields[8] is the timestamp (12 or 14 digits)
    let timestamp = parse_timestamp(fields[8].trim());

    Ok(Record::Result(Reading {
        record_number,
        glucose_value,
        units,
        timestamp,
        high,
        low,
        meal_marker,
        is_control,
    }))
}

/// First char of the annotation encodes the meal mark:  B=pre-meal  A=post-meal  D=logbook
fn parse_meal_marker(annotation: &str) -> Option<String> {
    match annotation.chars().next() {
        Some('B') => Some("pre-meal".to_string()),
        Some('A') => Some("post-meal".to_string()),
        Some('D') => Some("logbook".to_string()),
        _ => None,
    }
}

/// Parse a 12-digit (YYYYMMDDHHmm) or 14-digit (YYYYMMDDHHmmss) timestamp
/// into an ISO 8601 local-time string.
fn parse_timestamp(s: &str) -> String {
    let digits: String = s.chars().filter(|c| c.is_ascii_digit()).take(14).collect();
    match digits.len() {
        n if n >= 14 => format!(
            "{}-{}-{}T{}:{}:{}",
            &digits[0..4],
            &digits[4..6],
            &digits[6..8],
            &digits[8..10],
            &digits[10..12],
            &digits[12..14]
        ),
        n if n >= 12 => format!(
            "{}-{}-{}T{}:{}:00",
            &digits[0..4],
            &digits[4..6],
            &digits[6..8],
            &digits[8..10],
            &digits[10..12]
        ),
        _ => s.to_string(),
    }
}

// ── Text-format round-trip ────────────────────────────────────────────────────

/// Parse a `--format records` text dump (one ASTM frame per line) back into
/// structured data.  Lines that cannot be parsed are silently skipped.
pub fn parse_records_from_text(text: &str) -> (DeviceInfo, Vec<Reading>) {
    let mut device_info = DeviceInfo::default();
    let mut readings = Vec::new();
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
        match parse_record(line) {
            Ok(Record::Header(info)) => device_info = info,
            Ok(Record::Result(r)) if !r.is_control => readings.push(r),
            _ => {}
        }
    }
    (device_info, readings)
}

/// Build a Session from a saved `--format records` text dump.
/// `raw_packets` is left empty — file-driven input has no HID traffic.
pub fn session_from_records_text(text: &str) -> Session {
    let (device, readings) = parse_records_from_text(text);
    let raw_records = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(String::from)
        .collect();
    Session {
        device,
        readings,
        raw_records,
        raw_packets: Vec::new(),
    }
}

// ── Main session loop ─────────────────────────────────────────────────────────

/// Send one command, receive one complete message, retry with NAK on transient errors.
fn get_one_record(device: &HidDevice, log: &mut PacketLog) -> Result<(Record, String)> {
    let mut retries = 0u32;
    let mut cmd = ACK;

    loop {
        hid_write(device, &[cmd], log)?;
        cmd = ACK;

        let msg = match receive_message(device, RECEIVE_TIMEOUT, log) {
            Ok(m) => m,
            Err(e) => {
                retries += 1;
                if retries >= IO_RETRIES {
                    return Err(e);
                }
                eprintln!("Receive error ({retries}/{IO_RETRIES}): {e}");
                cmd = NAK;
                continue;
            }
        };

        match msg.msg_type {
            // Device signalling it's ready to send — ACK and loop to get actual data
            ENQ => continue,
            EOT => return Ok((Record::EndOfTransmission, String::new())),
            ACK => continue,
            STX => match parse_record(&msg.frame) {
                Ok(record) => return Ok((record, msg.frame)),
                Err(e) => {
                    retries += 1;
                    if retries >= PROTO_RETRIES {
                        return Err(e);
                    }
                    eprintln!("Parse error ({retries}/{PROTO_RETRIES}): {e}");
                    cmd = NAK;
                }
            },
            other => {
                retries += 1;
                if retries >= PROTO_RETRIES {
                    return Err(anyhow!(
                        "Unexpected message byte {other:#04x} after {PROTO_RETRIES} retries"
                    ));
                }
                cmd = NAK;
            }
        }
    }
}

/// Open a session and read all records until the L terminator or EOT.
pub fn fetch_all(device: &HidDevice, progress: bool, capture_packets: bool) -> Result<Session> {
    let mut packets = PacketLog::new(capture_packets);
    let mut raw_records: Vec<String> = Vec::new();
    let mut device_info = DeviceInfo::default();
    let mut readings: Vec<Reading> = Vec::new();

    loop {
        let (record, raw) = get_one_record(device, &mut packets)?;

        if !raw.is_empty() {
            raw_records.push(raw);
        }

        match record {
            Record::Header(info) => device_info = info,
            Record::Result(r) => {
                if r.is_control {
                    if progress {
                        eprint!("\r{:80}\r", ""); // clear line
                        eprintln!("(skipping control reading #{})", r.record_number);
                    }
                } else {
                    if progress {
                        let total = device_info.record_count;
                        let n = readings.len() + 1;
                        let pct = (100 * n as u32).checked_div(total).unwrap_or(0);
                        eprint!(
                            "\r[{n:>3}/{total}] {pct:>3}%  {}  {:.1} {}  {}{}",
                            r.timestamp,
                            r.glucose_value,
                            r.units,
                            if r.high {
                                "HIGH "
                            } else if r.low {
                                "LOW  "
                            } else {
                                "     "
                            },
                            r.meal_marker.as_deref().unwrap_or(""),
                        );
                        let _ = std::io::stderr().flush();
                    }
                    readings.push(r);
                }
            }
            Record::Skip => {}
            Record::Terminator => {
                // Best-effort: send final ACK and wait for EOT.
                // The L record means the device is done and all readings
                // are already captured, so any error during this trailing
                // handshake is not a session failure.
                let _ = get_one_record(device, &mut packets);
                break;
            }
            Record::EndOfTransmission => break,
        }
    }

    if progress {
        // Move to a fresh line after the progress output
        eprintln!();
    }
    eprintln!(
        "Done: {} readings from {} (S/N {})",
        readings.len(),
        device_info.model,
        device_info.serial_number
    );

    Ok(Session {
        device: device_info,
        readings,
        raw_records,
        raw_packets: packets.packets,
    })
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Checksum ──────────────────────────────────────────────────────────────

    /// Verified against the example in the original Tidepool JS driver comments:
    ///   <STX>5R|3|^^^Glucose|93|mg/dL^P||A/M0/T1||201505261150<CR><ETB>74<CR><LF>
    /// Checksum covers seq digit '5' through ETB (inclusive) → sum mod 256 = 0x74.
    #[test]
    fn checksum_known_r_record() {
        let input = b"5R|3|^^^Glucose|93|mg/dL^P||A/M0/T1||201505261150\r\x17";
        assert_eq!(compute_checksum(input), "74");
    }

    #[test]
    fn checksum_single_byte() {
        // sum of one byte is that byte; 0x06 % 256 = 6 → "06"
        assert_eq!(compute_checksum(&[0x06]), "06");
    }

    #[test]
    fn checksum_overflow_wraps() {
        // 255 + 2 = 257 % 256 = 1 → "01"
        assert_eq!(compute_checksum(&[0xFF, 0x02]), "01");
    }

    // ── decode_message ────────────────────────────────────────────────────────

    fn r_record_frame() -> Vec<u8> {
        // Full raw buffer for: <STX>5R|3|^^^Glucose|93|mg/dL^P||A/M0/T1||201505261150<CR><ETB>74<CR><LF>
        let mut v = vec![STX, b'5'];
        v.extend_from_slice(b"R|3|^^^Glucose|93|mg/dL^P||A/M0/T1||201505261150");
        v.extend_from_slice(&[CR, ETB, b'7', b'4', CR, b'\n']);
        v
    }

    #[test]
    fn decode_stx_frame_extracts_content() {
        let msg = decode_message(&r_record_frame()).unwrap();
        assert_eq!(msg.msg_type, STX);
        assert_eq!(
            msg.frame,
            "R|3|^^^Glucose|93|mg/dL^P||A/M0/T1||201505261150"
        );
    }

    #[test]
    fn decode_single_enq() {
        let msg = decode_message(&[ENQ]).unwrap();
        assert_eq!(msg.msg_type, ENQ);
        assert!(msg.frame.is_empty());
    }

    #[test]
    fn decode_single_eot() {
        let msg = decode_message(&[EOT]).unwrap();
        assert_eq!(msg.msg_type, EOT);
    }

    #[test]
    fn decode_empty_returns_error() {
        assert!(decode_message(&[]).is_err());
    }

    // ── parse_timestamp ───────────────────────────────────────────────────────

    #[test]
    fn timestamp_12_digit() {
        assert_eq!(parse_timestamp("201505261150"), "2015-05-26T11:50:00");
    }

    #[test]
    fn timestamp_14_digit() {
        assert_eq!(parse_timestamp("20150529124800"), "2015-05-29T12:48:00");
    }

    #[test]
    fn timestamp_ignores_trailing_garbage() {
        // ETB or other trailing bytes sometimes leak through; only digits are taken
        assert_eq!(parse_timestamp("201505261150\r\x17"), "2015-05-26T11:50:00");
    }

    // ── parse_serial ──────────────────────────────────────────────────────────

    #[test]
    fn serial_letter_separator() {
        // "7830H5001733" → skip "7830", skip "H", get "5001733"
        assert_eq!(parse_serial("7830H5001733"), "5001733");
    }

    #[test]
    fn serial_dash_separator() {
        // "7358-1611135" → skip "7358", skip "-", get "1611135"
        assert_eq!(parse_serial("7358-1611135"), "1611135");
    }

    // ── parse_thresholds ──────────────────────────────────────────────────────

    #[test]
    fn thresholds_mg_dl() {
        let (lo, hi) = parse_thresholds("A=1^U=0^V=20600");
        assert_eq!(lo, 20);
        assert_eq!(hi, 600);
    }

    #[test]
    fn thresholds_mmol_converts_to_mg_dl() {
        // U=1 means mmol/L; V=02033 → lo=2 mmol/L×10=2, hi=033 mmol/L×10=3.3
        // 2/10 * 18.01559 ≈ 4,  3.3/10 * 18.01559 ≈ 59
        let (lo, hi) = parse_thresholds("U=1^V=02033");
        assert_eq!(lo, 4);
        assert_eq!(hi, 59);
    }

    // ── parse_meal_marker ─────────────────────────────────────────────────────

    #[test]
    fn meal_marker_pre() {
        assert_eq!(parse_meal_marker("B/M0/T1"), Some("pre-meal".to_string()));
    }

    #[test]
    fn meal_marker_post() {
        assert_eq!(parse_meal_marker("A/M0/T1"), Some("post-meal".to_string()));
    }

    #[test]
    fn meal_marker_logbook() {
        assert_eq!(parse_meal_marker("D/M0/T1"), Some("logbook".to_string()));
    }

    #[test]
    fn meal_marker_none_for_out_of_range() {
        assert_eq!(parse_meal_marker(">"), None);
        assert_eq!(parse_meal_marker("<"), None);
    }

    #[test]
    fn meal_marker_none_for_empty() {
        assert_eq!(parse_meal_marker(""), None);
    }

    // ── parse_result_record ───────────────────────────────────────────────────

    fn parse_r(frame: &str) -> Reading {
        match parse_result_record(frame).unwrap() {
            Record::Result(r) => r,
            other => panic!("expected Record::Result, got {other:?}"),
        }
    }

    #[test]
    fn result_normal_mg_dl() {
        let r = parse_r("R|3|^^^Glucose|93|mg/dL^P||A/M0/T1||201505261150");
        assert_eq!(r.record_number, 3);
        assert_eq!(r.glucose_value, 93.0);
        assert_eq!(r.units, "mg/dL");
        assert_eq!(r.timestamp, "2015-05-26T11:50:00");
        assert!(!r.high);
        assert!(!r.low);
        assert!(!r.is_control);
        assert_eq!(r.meal_marker, Some("post-meal".to_string()));
    }

    #[test]
    fn result_mmol_l() {
        let r = parse_r("R|1|^^^Glucose|5.4|mmol/L^P||B/M0/T1||202411030845");
        assert_eq!(r.glucose_value, 5.4);
        assert_eq!(r.units, "mmol/L");
        assert_eq!(r.meal_marker, Some("pre-meal".to_string()));
    }

    #[test]
    fn result_high() {
        let r = parse_r("R|5|^^^Glucose|601|mg/dL^P||>||201505261200");
        assert!(r.high);
        assert!(!r.low);
        assert!(!r.is_control);
    }

    #[test]
    fn result_low() {
        let r = parse_r("R|6|^^^Glucose|19|mg/dL^P||<||201505261210");
        assert!(!r.high);
        assert!(r.low);
    }

    #[test]
    fn result_control_flagged() {
        let r = parse_r("R|7|^^^Glucose|100|mg/dL^P||C||201505261220");
        assert!(r.is_control);
    }

    // ── parse_header ──────────────────────────────────────────────────────────

    #[test]
    fn header_parses_correctly() {
        // Minimal but valid H record with 14 pipe-delimited fields
        let frame = "H|\\^&|||Bayer7350^fw^7358-1611135|A=1^U=0^V=20600|4|||||P|1|201505291248";
        let info = parse_header(frame).unwrap();
        assert_eq!(info.model, "Bayer7350");
        assert_eq!(info.serial_number, "1611135");
        assert_eq!(info.record_count, 4);
        assert_eq!(info.device_time, "2015-05-29T12:48:00");
        assert_eq!(info.low_threshold, 20);
        assert_eq!(info.high_threshold, 600);
    }

    #[test]
    fn header_too_few_fields_is_error() {
        assert!(parse_header("H|\\^&||short").is_err());
    }

    // ── build_write_packet ────────────────────────────────────────────────────

    #[test]
    fn write_packet_structure() {
        let pkt = build_write_packet(&[ACK]);
        assert_eq!(pkt.len(), 65);
        assert_eq!(pkt[0], 0x00); // report ID
        assert_eq!(pkt[1], 0x00); // header byte 0
        assert_eq!(pkt[2], 0x00); // header byte 1
        assert_eq!(pkt[3], 0x00); // header byte 2
        assert_eq!(pkt[4], 0x01); // payload length
        assert_eq!(pkt[5], ACK); // payload
        assert!(pkt[6..].iter().all(|&b| b == 0)); // zero-padded
    }

    #[test]
    fn write_packet_multi_byte_payload() {
        let pkt = build_write_packet(&[0x01, 0x02, 0x03]);
        assert_eq!(pkt[4], 3);
        assert_eq!(&pkt[5..8], &[0x01, 0x02, 0x03]);
    }

    // ── Tests grounded in the real Contour Next One capture ───────────────────
    //
    // All personally-identifying values (serial, timestamps, glucose readings)
    // have been replaced with synthetic equivalents. The structural patterns —
    // serial format, config string layout, annotation variants — are taken
    // directly from the capture.

    /// Real device serial format: leading model digits + uppercase letter separator.
    /// "7802H7001396" → strip "7802" + "H" → "7001396"
    #[test]
    fn serial_real_device_format() {
        assert_eq!(parse_serial("7802H7001396"), "7001396");
    }

    /// Real device config string: U=1 (mmol/L), V=06333.
    /// V field: low = "06" / 10 = 0.6 mmol/L, high = "333" / 10 = 33.3 mmol/L.
    /// Converted to mg/dL: 0.6 × 18.01559 ≈ 11, 33.3 × 18.01559 ≈ 600.
    #[test]
    fn thresholds_real_device_config() {
        let config = "A=1^C=6^R=0^S=1^U=1^V=06333^X=039039100072^a=1^J=0";
        let (lo, hi) = parse_thresholds(config);
        assert_eq!(lo, 11);
        assert_eq!(hi, 600);
    }

    /// The most common annotation in the real data — no meal or time context.
    #[test]
    fn result_t0m0_no_meal_marker() {
        let r = parse_r("R|1|^^^Glucose|7.4|mmol/L^P||T0/M0||20200101090000");
        assert_eq!(r.meal_marker, None);
        assert!(!r.high);
        assert!(!r.low);
        assert!(!r.is_control);
    }

    /// The "A/T0/M0" annotation observed in the real data (post-meal flagged
    /// on the meter). The leading 'A' encodes post-meal; the rest is discarded.
    #[test]
    fn result_a_t0m0_is_postmeal() {
        let r = parse_r("R|354|^^^Glucose|7.9|mmol/L^P||A/T0/M0||20200101180000");
        assert_eq!(r.meal_marker, Some("post-meal".to_string()));
        assert!(!r.high);
        assert!(!r.low);
    }

    /// H record with the real device's structure: password in field[3],
    /// 14-digit timestamp, trailing empty field (15 total), U=1, V=06333.
    #[test]
    fn header_real_device_structure() {
        // Sanitised: fake password, fake serial, fake timestamp, synthetic values.
        let frame = "H|\\^&||AAAAAA|ContourTest^01.00\\01.00\\01.00^0000X0000000|\
                     A=1^C=6^R=0^S=1^U=1^V=06333^X=039039100072^a=1^J=0|\
                     800|||||P|1|20200101090000|";
        let info = parse_header(frame).unwrap();
        assert_eq!(info.model, "ContourTest");
        assert_eq!(info.serial_number, "0000000");
        assert_eq!(info.record_count, 800);
        assert_eq!(info.device_time, "2020-01-01T09:00:00");
        // U=1, V=06333 → low ≈ 11 mg/dL, high ≈ 600 mg/dL
        assert_eq!(info.low_threshold, 11);
        assert_eq!(info.high_threshold, 600);
    }

    // ── parse_records_from_text / fixture round-trip ──────────────────────────

    const FIXTURE: &str = include_str!("../tests/fixtures/sample.txt");

    #[test]
    fn fixture_device_info_parses() {
        let (device, _) = parse_records_from_text(FIXTURE);
        assert_eq!(device.model, "ContourTest");
        assert_eq!(device.serial_number, "0000000");
        assert_eq!(device.record_count, 6);
        assert_eq!(device.device_time, "2020-01-01T09:00:00");
    }

    #[test]
    fn fixture_reading_count_excludes_control() {
        // Fixture has 6 R records; R|6 is a control (<) — wait, '<' is low, not control.
        // R|5 has '>' (high), R|6 has '<' (low). Neither is control ('C').
        // All 6 should appear in readings.
        let (_, readings) = parse_records_from_text(FIXTURE);
        assert_eq!(readings.len(), 6);
    }

    #[test]
    fn fixture_first_reading() {
        let (_, readings) = parse_records_from_text(FIXTURE);
        let r = &readings[0];
        assert_eq!(r.glucose_value, 7.4);
        assert_eq!(r.units, "mmol/L");
        assert_eq!(r.timestamp, "2020-01-01T09:00:00");
        assert_eq!(r.meal_marker, None);
        assert!(!r.high);
        assert!(!r.low);
    }

    #[test]
    fn fixture_postmeal_annotation() {
        let (_, readings) = parse_records_from_text(FIXTURE);
        // R|4 has A/T0/M0
        let r = &readings[3];
        assert_eq!(r.meal_marker, Some("post-meal".to_string()));
    }

    #[test]
    fn fixture_high_and_low_flags() {
        let (_, readings) = parse_records_from_text(FIXTURE);
        // R|5 → high, R|6 → low
        assert!(readings[4].high);
        assert!(!readings[4].low);
        assert!(readings[5].low);
        assert!(!readings[5].high);
    }
}
