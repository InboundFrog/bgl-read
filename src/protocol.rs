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

use anyhow::{anyhow, Result};
use hidapi::{HidApi, HidDevice};
use serde::Serialize;
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
const MAX_RETRIES: u32 = 6;

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

/// One captured HID packet (raw 64 bytes + direction).
#[derive(Debug, Serialize, Clone)]
pub struct Packet {
    pub dir: Dir,
    pub data: Vec<u8>,
}

/// Everything captured during a session.
#[derive(Debug, Serialize)]
pub struct Session {
    pub device: DeviceInfo,
    pub readings: Vec<Reading>,
    /// Raw ASTM record frame strings, in order received (H, P, R…, L)
    pub raw_records: Vec<String>,
    /// Every HID packet exchanged, in order
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
    // Layout: [report_id=0, hdr0=0, hdr1=0, hdr2=0, length, data..., padding]
    let mut pkt = [0u8; 65];
    pkt[4] = data.len() as u8;
    pkt[5..5 + data.len()].copy_from_slice(data);
    pkt
}

fn hid_write(device: &HidDevice, data: &[u8], log: &mut Vec<Packet>) -> Result<()> {
    let pkt = build_write_packet(data);
    log.push(Packet { dir: Dir::Tx, data: pkt[1..].to_vec() }); // log without report-ID
    device.write(&pkt)?;
    Ok(())
}

/// Read one 64-byte HID packet with a deadline.
fn hid_read(device: &HidDevice, deadline: Instant, log: &mut Vec<Packet>) -> Result<[u8; HID_PACKET_SIZE]> {
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
    log.push(Packet { dir: Dir::Rx, data: pkt.to_vec() });
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
fn receive_message(device: &HidDevice, timeout: Duration, log: &mut Vec<Packet>) -> Result<Message> {
    let deadline = Instant::now() + timeout;
    let mut buf: Vec<u8> = Vec::new();

    loop {
        let pkt = hid_read(device, deadline, log)?;

        let size = pkt[3] as usize;
        let data_end = (4 + size).min(HID_PACKET_SIZE);
        let data = &pkt[4..data_end];
        buf.extend_from_slice(data);

        let first = data.first().copied().unwrap_or(0);
        let is_complete = size < MAX_PAYLOAD
            || matches!(first, ENQ | EOT | ACK | NAK)
            || (data.len() >= 5 && matches!(data[data.len() - 5], ETX | ETB));

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
        return Ok(Message { msg_type, frame: String::new() });
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
        eprintln!("Warning: checksum mismatch (expected {expected}, computed {computed})");
    }

    // Frame content is buf[2..len-4] — includes the trailing CR+ETX/ETB.
    // Strip them so callers just see the record text.
    let mut content = &buf[2..buf.len() - 4];
    if content.last() == Some(&ETX) || content.last() == Some(&ETB) {
        content = &content[..content.len() - 1];
    }
    if content.last() == Some(&CR) {
        content = &content[..content.len() - 1];
    }

    let frame = String::from_utf8_lossy(content).into_owned();
    Ok(Message { msg_type, frame })
}

// ── Record parsing ────────────────────────────────────────────────────────────

enum Record {
    Header(DeviceInfo),
    Result(Reading),
    Patient,
    Terminator,
    EndOfTransmission,
}

fn parse_record(frame: &str) -> Result<Record> {
    match frame.chars().next() {
        Some('H') => Ok(Record::Header(parse_header(frame)?)),
        Some('R') => parse_result_record(frame),
        Some('L') => Ok(Record::Terminator),
        Some('P') => Ok(Record::Patient),
        Some(c) => Err(anyhow!("Unknown record type '{c}' in frame: {frame:?}")),
        None => Err(anyhow!("Empty frame")),
    }
}

/// H record example:
///   H|\^&||qvqOi8|Bayer7350^FW\App\Boot^7358-1611135^0000-|A=1^U=0^V=20600|4|||||P|1|202505291248
fn parse_header(frame: &str) -> Result<DeviceInfo> {
    let fields: Vec<&str> = frame.split('|').collect();
    if fields.len() < 14 {
        return Err(anyhow!("Header record has only {} fields (need 14)", fields.len()));
    }

    let device_parts: Vec<&str> = fields[4].split('^').collect();
    let model = device_parts.first().copied().unwrap_or("Unknown").to_string();
    let serial_raw = device_parts.get(2).copied().unwrap_or("");
    let serial_number = parse_serial(serial_raw);

    let nrecs: u32 = fields[6].trim().parse().unwrap_or(0);

    let ts_raw = fields[13].trim();
    let device_time = parse_timestamp(ts_raw);

    let (low_threshold, high_threshold) = parse_thresholds(fields[5]);

    Ok(DeviceInfo { model, serial_number, record_count: nrecs, device_time, low_threshold, high_threshold })
}

/// Extract the serial number from the raw field.
/// "7830H5001733" → "5001733",  "6301-1C2CF8C" → "1C2CF8C"
/// Matches JS regex /^\d+[\w-]\s*(\w+)/ — skip leading digits + one separator char.
fn parse_serial(raw: &str) -> String {
    let after_digits = raw.trim_start_matches(|c: char| c.is_ascii_digit());
    if after_digits.is_empty() {
        return raw.to_string();
    }
    // Skip exactly one separator character
    let after_sep = &after_digits[after_digits.char_indices().next().map(|(_, c)| c.len_utf8()).unwrap_or(0)..];
    after_sep.trim_start().to_string()
}

/// Parse the config field (field[5]) for thresholds and units.
/// Config looks like:  A=1^C=00^I=0200^R=0^S=01^U=0^V=20600^X=...
fn parse_thresholds(config: &str) -> (u32, u32) {
    let mut low = 20u32;
    let mut high = 600u32;
    let mut units = 0u32;

    for part in config.split('^') {
        if let Some(val) = part.strip_prefix("V=") {
            // V=LLOHHH  (LL = 2-digit low, HHH = 3-digit high)
            if val.len() >= 5 {
                low = val[..2].parse().unwrap_or(20);
                high = val[2..5].parse().unwrap_or(600);
            }
        } else if let Some(val) = part.strip_prefix("U=") {
            units = val.trim().parse().unwrap_or(0);
        }
    }

    if units == 1 {
        // Values were in mmol/L × 10; convert to mg/dL
        low = ((low as f64 / 10.0) * 18.01559) as u32;
        high = ((high as f64 / 10.0) * 18.01559) as u32;
    }

    (low, high)
}

/// R record example:
///   R|3|^^^Glucose|93|mg/dL^P||A/M0/T1||201505261150
fn parse_result_record(frame: &str) -> Result<Record> {
    let fields: Vec<&str> = frame.split('|').collect();

    // Need at least 9 fields; field[2] must be "^^^Glucose"
    if fields.len() < 9 || !fields[2].starts_with("^^^Glucose") {
        return Ok(Record::Patient); // non-glucose result, skip
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
            &digits[0..4], &digits[4..6], &digits[6..8],
            &digits[8..10], &digits[10..12], &digits[12..14]
        ),
        n if n >= 12 => format!(
            "{}-{}-{}T{}:{}:00",
            &digits[0..4], &digits[4..6], &digits[6..8],
            &digits[8..10], &digits[10..12]
        ),
        _ => s.to_string(),
    }
}

// ── Main session loop ─────────────────────────────────────────────────────────

/// Send one command, receive one complete message, retry with NAK on transient errors.
fn get_one_record(device: &HidDevice, log: &mut Vec<Packet>) -> Result<(Record, String)> {
    let mut retries = 0u32;
    let mut cmd = ACK;

    loop {
        hid_write(device, &[cmd], log)?;
        cmd = ACK;

        let msg = match receive_message(device, Duration::from_secs(10), log) {
            Ok(m) => m,
            Err(e) => {
                retries += 1;
                if retries >= MAX_RETRIES {
                    return Err(e);
                }
                eprintln!("Receive error ({retries}/{MAX_RETRIES}): {e}");
                cmd = NAK;
                continue;
            }
        };

        match msg.msg_type {
            // Device signalling it's ready to send — ACK and loop to get actual data
            ENQ => continue,
            EOT => return Ok((Record::EndOfTransmission, String::new())),
            ACK => continue,
            STX => {
                let raw = msg.frame.clone();
                let record = parse_record(&msg.frame)?;
                return Ok((record, raw));
            }
            other => {
                retries += 1;
                if retries >= MAX_RETRIES {
                    return Err(anyhow!("Unexpected message byte {other:#04x} after {MAX_RETRIES} retries"));
                }
                cmd = NAK;
            }
        }
    }
}

/// Open a session and read all records until the L terminator or EOT.
pub fn fetch_all(device: &HidDevice, progress: bool) -> Result<Session> {
    let mut packets: Vec<Packet> = Vec::new();
    let mut raw_records: Vec<String> = Vec::new();
    let mut device_info = DeviceInfo::default();
    let mut readings: Vec<Reading> = Vec::new();

    loop {
        let (record, raw) = get_one_record(device, &mut packets)?;

        match record {
            Record::Header(info) => {
                if !raw.is_empty() { raw_records.push(raw); }
                device_info = info;
            }
            Record::Result(r) => {
                if !raw.is_empty() { raw_records.push(raw.clone()); }
                if r.is_control {
                    if progress {
                        eprint!("\r{:80}\r", ""); // clear line
                        eprintln!("(skipping control reading #{})", r.record_number);
                    }
                } else {
                    if progress {
                        let total = device_info.record_count;
                        let n = readings.len() + 1;
                        let pct = if total > 0 { 100 * n as u32 / total } else { 0 };
                        eprint!(
                            "\r[{n:>3}/{total}] {pct:>3}%  {}  {:.1} {}  {}{}",
                            r.timestamp,
                            r.glucose_value,
                            r.units,
                            if r.high { "HIGH " } else if r.low { "LOW  " } else { "     " },
                            r.meal_marker.as_deref().unwrap_or(""),
                        );
                        // Flush stderr — it's often line-buffered
                        use std::io::Write;
                        let _ = std::io::stderr().flush();
                    }
                    readings.push(r);
                }
            }
            Record::Patient => {
                if !raw.is_empty() { raw_records.push(raw); }
            }
            Record::Terminator => {
                if !raw.is_empty() { raw_records.push(raw); }
                // Send final ACK and wait for EOT
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

    Ok(Session { device: device_info, readings, raw_records, raw_packets: packets })
}
