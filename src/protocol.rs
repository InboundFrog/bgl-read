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
/// Timeout for the meter's EOT reply during the NAK close handshake.
const CLOSE_TIMEOUT: Duration = Duration::from_secs(2);
/// Hard cap on reassembled message size — real ASTM frames are well under 1 KiB.
const MAX_MESSAGE_SIZE: usize = 64 * 1024;
/// Default low/high glucose thresholds in mg/dL, used as fallbacks when the
/// header config field is absent or unparseable.
const DEFAULT_LOW_THRESHOLD: u32 = 20;
const DEFAULT_HIGH_THRESHOLD: u32 = 600;
/// mmol/L → mg/dL conversion factor for glucose.
const MMOL_TO_MGDL: f64 = 18.01559;

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
            low_threshold: DEFAULT_LOW_THRESHOLD,
            high_threshold: DEFAULT_HIGH_THRESHOLD,
        }
    }
}

#[derive(Debug, Serialize, Clone)]
pub struct Reading {
    pub record_number: u32,
    pub analyte: String,
    pub value: f64,
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
#[derive(Debug, Clone, Copy)]
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
#[derive(Debug)]
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
    // Log without the report-ID byte
    let tx_data: [u8; HID_PACKET_SIZE] = pkt[1..].try_into().expect("pkt is 65 bytes");
    log.push(Packet {
        dir: Dir::Tx,
        data: tx_data,
    });
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
    let mut asm = MessageAssembler::new();

    loop {
        let pkt = hid_read(device, deadline, log)?;
        if let Some(msg) = asm.push(&pkt) {
            return msg;
        }
    }
}

/// Reassembles ASTM messages from a stream of 64-byte HID packets.
///
/// Held separate from the device so `--from-bytes` replays a capture through
/// exactly the framing rules it was read with — one decoder, not two.
struct MessageAssembler {
    buf: Vec<u8>,
}

impl MessageAssembler {
    fn new() -> Self {
        Self { buf: Vec::new() }
    }

    /// Feed one packet. Returns `Some` once the accumulated bytes form a
    /// complete message, resetting for the next one.
    fn push(&mut self, pkt: &[u8; HID_PACKET_SIZE]) -> Option<Result<Message>> {
        let size = pkt[3] as usize;
        let data_end = (4 + size).min(HID_PACKET_SIZE);
        self.buf.extend_from_slice(&pkt[4..data_end]);

        if self.buf.len() > MAX_MESSAGE_SIZE {
            self.buf.clear();
            return Some(Err(anyhow!(
                "ASTM message exceeds {MAX_MESSAGE_SIZE} bytes — aborting"
            )));
        }

        let first = self.buf.first().copied().unwrap_or(0);
        let is_complete = size < MAX_PAYLOAD
            || matches!(first, ENQ | EOT | ACK | NAK)
            || (self.buf.len() >= 5 && matches!(self.buf[self.buf.len() - 5], ETX | ETB));

        if !is_complete {
            return None;
        }

        let msg = decode_message(&self.buf);
        self.buf.clear();
        Some(msg)
    }
}

fn compute_checksum(data: &[u8]) -> String {
    let sum: u32 = data.iter().map(|&b| b as u32).sum();
    format!("{:02X}", sum % 256)
}

/// Decode the accumulated raw bytes into a Message.
fn decode_message(buf: &[u8]) -> Result<Message> {
    let msg_type = *buf.first().ok_or_else(|| anyhow!("Empty message buffer"))?;

    if msg_type != STX {
        // When a session starts on the heels of a previous one, the meter
        // packs EOT+ENQ into a single packet: it terminates the stale
        // session and immediately offers a new one. Surface the ENQ so the
        // handshake continues instead of treating it as end of transmission.
        let msg_type = if buf == [EOT, ENQ] { ENQ } else { msg_type };
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
        // Recognised but ignored ASTM E1394 record types:
        //   P = Patient
        //   C = Comment
        //   O = Order / test request
        //   M = Manufacturer-specific
        //   Q = Query / inquiry
        //   S = Scientific
        Some('P' | 'C' | 'O' | 'M' | 'Q' | 'S') => Ok(Record::Skip),
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
    let mut low = DEFAULT_LOW_THRESHOLD;
    let mut high = DEFAULT_HIGH_THRESHOLD;
    let mut mmol = false;

    for part in config.split('^') {
        if let Some(val) = part.strip_prefix("V=") {
            // V=LLOHHH  (LL = 2-digit low, HHH = 3-digit high)
            low = val
                .get(..2)
                .and_then(|s| s.parse().ok())
                .unwrap_or(DEFAULT_LOW_THRESHOLD);
            high = val
                .get(2..5)
                .and_then(|s| s.parse().ok())
                .unwrap_or(DEFAULT_HIGH_THRESHOLD);
        } else if let Some(val) = part.strip_prefix("U=") {
            mmol = val.trim() == "1";
        }
    }

    if mmol {
        // Values were in mmol/L × 10; convert to mg/dL
        low = ((low as f64 / 10.0) * MMOL_TO_MGDL).round() as u32;
        high = ((high as f64 / 10.0) * MMOL_TO_MGDL).round() as u32;
    }

    (low, high)
}

/// R record example:
///   R|3|^^^Glucose|93|mg/dL^P||A/M0/T1||201505261150
fn parse_result_record(frame: &str) -> Result<Record> {
    let fields: Vec<&str> = frame.split('|').collect();

    if fields.len() < 9 {
        return Ok(Record::Skip);
    }

    // Field 2 (1-based: field 3) is "^^^Analyte[^subcomponents...]"
    let analyte = match fields[2].strip_prefix("^^^") {
        Some(rest) => rest.split('^').next().unwrap_or("").trim().to_string(),
        None => return Ok(Record::Skip),
    };
    if analyte.is_empty() {
        return Ok(Record::Skip);
    }

    let record_number: u32 = fields[1].parse().unwrap_or(0);
    let value: f64 = fields[3].parse().unwrap_or(0.0);
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
        analyte,
        value,
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
        14 => format!(
            "{}-{}-{}T{}:{}:{}",
            &digits[0..4],
            &digits[4..6],
            &digits[6..8],
            &digits[8..10],
            &digits[10..12],
            &digits[12..14]
        ),
        12 | 13 => format!(
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

// ── Binary packet capture ─────────────────────────────────────────────────────

/// File magic for `--format binary` captures.
const CAPTURE_MAGIC: &[u8; 7] = b"BGLCAP\0";
/// Bumped whenever the on-disk layout changes incompatibly.
const CAPTURE_VERSION: u8 = 1;
/// magic + version + packet size.
const CAPTURE_HEADER_LEN: usize = CAPTURE_MAGIC.len() + 2;

/// Serialise a packet log: a 9-byte header, then one direction byte plus the
/// raw packet per entry. The stride is fixed and recorded in the header, so a
/// truncated file is detectable rather than silently short.
pub fn encode_packets(packets: &[Packet]) -> Vec<u8> {
    let stride = 1 + HID_PACKET_SIZE;
    let mut out = Vec::with_capacity(CAPTURE_HEADER_LEN + packets.len() * stride);
    out.extend_from_slice(CAPTURE_MAGIC);
    out.push(CAPTURE_VERSION);
    out.push(HID_PACKET_SIZE as u8);
    for p in packets {
        out.push(match p.dir {
            Dir::Tx => 0,
            Dir::Rx => 1,
        });
        out.extend_from_slice(&p.data);
    }
    out
}

/// Inverse of [`encode_packets`]. Rejects anything it cannot read exactly —
/// a half-decoded capture would produce a plausible-looking short reading list.
pub fn decode_packets(bytes: &[u8]) -> Result<Vec<Packet>> {
    let header = bytes
        .get(..CAPTURE_HEADER_LEN)
        .ok_or_else(|| anyhow!("Not a bgl-read capture: file is shorter than its header"))?;

    if &header[..CAPTURE_MAGIC.len()] != CAPTURE_MAGIC {
        return Err(anyhow!(
            "Not a bgl-read capture: bad magic (is this a --format bytes hex dump?)"
        ));
    }

    let version = header[CAPTURE_MAGIC.len()];
    if version != CAPTURE_VERSION {
        return Err(anyhow!(
            "Capture is version {version}; this build reads version {CAPTURE_VERSION}"
        ));
    }

    let packet_size = header[CAPTURE_MAGIC.len() + 1] as usize;
    if packet_size != HID_PACKET_SIZE {
        return Err(anyhow!(
            "Capture holds {packet_size}-byte packets; this build expects {HID_PACKET_SIZE}"
        ));
    }

    let body = &bytes[CAPTURE_HEADER_LEN..];
    let stride = 1 + HID_PACKET_SIZE;
    let trailing = body.len() % stride;
    if trailing != 0 {
        return Err(anyhow!(
            "Capture is truncated: {trailing} trailing byte(s) after the last whole packet"
        ));
    }

    body.chunks_exact(stride)
        .map(|chunk| {
            let dir = match chunk[0] {
                0 => Dir::Tx,
                1 => Dir::Rx,
                other => return Err(anyhow!("Bad direction byte {other:#04x} in capture")),
            };
            Ok(Packet {
                dir,
                data: chunk[1..].try_into().expect("chunk is one stride long"),
            })
        })
        .collect()
}

/// Rebuild a Session by replaying captured HID packets through the same
/// framing and record parsing the live read uses.
///
/// Only RX packets carry meter data — TX entries are our own ACK/NAK traffic.
/// Messages that fail to decode are skipped: in the live session those were
/// NAK'd and retried, so the retry's good copy appears later in the stream.
pub fn session_from_packets(packets: Vec<Packet>) -> Result<Session> {
    let mut asm = MessageAssembler::new();
    let mut builder = SessionBuilder::new(false);

    for pkt in packets.iter().filter(|p| matches!(p.dir, Dir::Rx)) {
        let Some(Ok(msg)) = asm.push(&pkt.data) else {
            continue;
        };

        let (record, raw) = match msg.msg_type {
            STX => match parse_record(&msg.frame) {
                Ok(record) => (record, msg.frame),
                // Superseded by the retry the meter sent after our NAK.
                Err(_) => continue,
            },
            EOT => (Record::EndOfTransmission, String::new()),
            // ENQ / ACK / NAK carry no record payload.
            _ => continue,
        };

        if !matches!(builder.push(record, raw), Flow::Continue) {
            break;
        }
    }

    let (device, readings, raw_records) = builder.finish(
        "Capture contains no records — it may be truncated, or from a session that failed \
         before the meter sent anything.",
    )?;

    Ok(Session {
        device,
        readings,
        raw_records,
        raw_packets: packets,
    })
}

// ── Text-format round-trip ────────────────────────────────────────────────────

/// Parse a `--format records` text dump (one ASTM frame per line) back into
/// structured data.  Lines that cannot be parsed are silently skipped.
/// Thin wrapper over [`session_from_records_text`]; currently exercised only
/// by tests, hence the dead-code allowance for non-test builds.
#[cfg_attr(not(test), allow(dead_code))]
pub fn parse_records_from_text(text: &str) -> (DeviceInfo, Vec<Reading>) {
    let session = session_from_records_text(text);
    (session.device, session.readings)
}

/// Build a Session from a saved `--format records` text dump in a single pass:
/// every trimmed non-empty line is kept verbatim in `raw_records`, and lines
/// that parse contribute to `device`/`readings`.
///
/// Lines that fail to parse are skipped but counted, and the count is warned
/// about on stderr — a truncated or mangled dump would otherwise yield a
/// quietly short CSV that looks perfectly valid.
/// `raw_packets` is left empty — file-driven input has no HID traffic.
pub fn session_from_records_text(text: &str) -> Session {
    let mut device = DeviceInfo::default();
    let mut readings = Vec::new();
    let mut raw_records = Vec::new();
    let mut unparsed = 0usize;
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
        raw_records.push(line.to_string());
        match parse_record(line) {
            Ok(Record::Header(info)) => device = info,
            Ok(Record::Result(r)) if !r.is_control => readings.push(r),
            // Control readings and the P/C/O/M/Q/S/L record types are
            // recognised and deliberately carry nothing into the output.
            Ok(_) => {}
            Err(e) => {
                unparsed += 1;
                eprintln!("warning: skipping unparseable record line: {e}");
            }
        }
    }
    if unparsed > 0 {
        eprintln!(
            "warning: {unparsed} of {} line(s) could not be parsed; output may be incomplete",
            raw_records.len()
        );
    }
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

/// A failed session, still carrying every HID packet captured before the
/// error so callers can dump them for debugging (`--format bytes`).
#[derive(Debug)]
pub struct FetchError {
    pub error: anyhow::Error,
    pub packets: Vec<Packet>,
}

/// Terminate the session after the L record: ACK it (the meter does not
/// reply), then send NAK and await the meter's EOT.
///
/// The meter does not volunteer EOT once the L record is acknowledged — the
/// host requests it with NAK. This matches the reference JS driver, which
/// also notes the Contour Next One must NOT be sent EOT. Without this the
/// meter sits silent until our receive timeout, adding ~10s to every
/// session. Best-effort: all readings are already captured at this point.
fn close_session(device: &HidDevice, log: &mut PacketLog) {
    if hid_write(device, &[ACK], log).is_err() {
        return;
    }
    if hid_write(device, &[NAK], log).is_err() {
        return;
    }
    let _ = receive_message(device, CLOSE_TIMEOUT, log);
}

/// Open a session and read all records until the L terminator or EOT.
///
/// On failure the captured packet log survives inside [`FetchError`] —
/// failure modes are exactly when a `--format bytes` dump is most useful.
pub fn fetch_all(
    device: &HidDevice,
    progress: bool,
    capture_packets: bool,
) -> Result<Session, FetchError> {
    let mut log = PacketLog::new(capture_packets);
    match fetch_all_inner(device, progress, &mut log) {
        Ok((device_info, readings, raw_records)) => Ok(Session {
            device: device_info,
            readings,
            raw_records,
            raw_packets: log.packets,
        }),
        Err(error) => Err(FetchError {
            error,
            packets: log.packets,
        }),
    }
}

fn fetch_all_inner(
    device: &HidDevice,
    progress: bool,
    packets: &mut PacketLog,
) -> Result<(DeviceInfo, Vec<Reading>, Vec<String>)> {
    let mut builder = SessionBuilder::new(progress);

    loop {
        let (record, raw) = get_one_record(device, packets)?;

        match builder.push(record, raw) {
            Flow::Continue => {}
            Flow::Close => {
                // The L record means the device is done and all readings are
                // already captured. Close the session; any error during this
                // trailing handshake is not a session failure.
                close_session(device, packets);
                break;
            }
            Flow::Stop => break,
        }
    }

    // An empty session — not even an H record — means the meter ended the
    // transmission without sending anything. Observed when a new session
    // starts too soon after the previous one. A genuinely empty (fresh)
    // meter still sends its H/P/L records, so it does not trip this.
    builder.finish(
        "Meter sent no records. It usually needs a short rest between \
         sessions — wait ~10 seconds and retry.",
    )
}

/// What the caller should do after feeding a record to [`SessionBuilder`].
enum Flow {
    Continue,
    /// Terminator (L) seen — a live session still needs closing down.
    Close,
    /// End of transmission — nothing further to do.
    Stop,
}

/// Accumulates decoded records into device info + readings + raw frames.
///
/// Shared by the live read loop and by `--from-bytes` replay, so a capture
/// replayed offline yields byte-identical output to the original session.
struct SessionBuilder {
    device: DeviceInfo,
    readings: Vec<Reading>,
    raw_records: Vec<String>,
    progress: bool,
}

impl SessionBuilder {
    fn new(progress: bool) -> Self {
        Self {
            device: DeviceInfo::default(),
            readings: Vec::new(),
            raw_records: Vec::new(),
            progress,
        }
    }

    /// Feed one decoded record and the raw frame text it came from.
    fn push(&mut self, record: Record, raw: String) -> Flow {
        if !raw.is_empty() {
            self.raw_records.push(raw);
        }

        match record {
            Record::Header(info) => self.device = info,
            Record::Result(r) => self.push_reading(r),
            Record::Skip => {}
            Record::Terminator => return Flow::Close,
            Record::EndOfTransmission => return Flow::Stop,
        }
        Flow::Continue
    }

    fn push_reading(&mut self, r: Reading) {
        if r.is_control {
            if self.progress {
                eprint!("\r{:80}\r", ""); // clear line
                eprintln!("(skipping control reading #{})", r.record_number);
            }
            return;
        }

        if self.progress {
            let total = self.device.record_count;
            let n = self.readings.len() + 1;
            let pct = (100 * n as u32).checked_div(total).unwrap_or(0);
            eprint!(
                "\r[{n:>3}/{total}] {pct:>3}%  {}  {:.1} {}  {}{}",
                r.timestamp,
                r.value,
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
        self.readings.push(r);
    }

    /// `empty_err` is the message for a session that decoded no records at
    /// all — the advice differs between a live meter and a capture file.
    fn finish(self, empty_err: &str) -> Result<(DeviceInfo, Vec<Reading>, Vec<String>)> {
        if self.progress {
            // Move to a fresh line after the progress output
            eprintln!();
        }

        if self.raw_records.is_empty() {
            return Err(anyhow!("{empty_err}"));
        }

        eprintln!(
            "Done: {} readings from {} (S/N {})",
            self.readings.len(),
            self.device.model,
            self.device.serial_number
        );

        Ok((self.device, self.readings, self.raw_records))
    }
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

    /// Observed on a real Contour Next One: a session started right after a
    /// previous one gets EOT+ENQ in one packet — stale session terminated,
    /// new one offered. The ENQ must win or the session ends empty.
    #[test]
    fn decode_eot_enq_pair_surfaces_enq() {
        let msg = decode_message(&[EOT, ENQ]).unwrap();
        assert_eq!(msg.msg_type, ENQ);
        assert!(msg.frame.is_empty());
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

    #[test]
    fn thresholds_multibyte_utf8_does_not_panic() {
        // Crafted device/file input with a multibyte char in the V field used to
        // panic on a non-char-boundary byte slice.
        let (lo, hi) = parse_thresholds("A=1^U=0^V=aé34");
        assert_eq!(lo, 20);
        assert_eq!(hi, 600);
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

    #[test]
    fn parse_record_accepts_comment() {
        assert!(matches!(
            parse_record("C|1|I|free text|G").unwrap(),
            Record::Skip
        ));
    }

    #[test]
    fn parse_record_accepts_order() {
        assert!(matches!(
            parse_record("O|1|sample|^^^Glucose|R||||").unwrap(),
            Record::Skip
        ));
    }

    #[test]
    fn parse_record_accepts_manufacturer() {
        assert!(matches!(
            parse_record("M|1|^^^vendor-specific").unwrap(),
            Record::Skip
        ));
    }

    #[test]
    fn parse_record_accepts_query() {
        assert!(matches!(
            parse_record("Q|1|^^^patientID").unwrap(),
            Record::Skip
        ));
    }

    #[test]
    fn parse_record_accepts_scientific() {
        assert!(matches!(
            parse_record("S|1|^^^analysis").unwrap(),
            Record::Skip
        ));
    }

    #[test]
    fn parse_record_rejects_unknown_letter() {
        assert!(parse_record("Z|1|junk").is_err());
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
        assert_eq!(r.analyte, "Glucose");
        assert_eq!(r.value, 93.0);
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
        assert_eq!(r.analyte, "Glucose");
        assert_eq!(r.value, 5.4);
        assert_eq!(r.units, "mmol/L");
        assert_eq!(r.meal_marker, Some("pre-meal".to_string()));
    }

    #[test]
    fn result_non_glucose_analyte_kept() {
        let r = parse_r("R|10|^^^Ketone|0.4|mmol/L^P||N||201505261150");
        assert_eq!(r.analyte, "Ketone");
        assert_eq!(r.value, 0.4);
        assert_eq!(r.units, "mmol/L");
        assert!(!r.is_control);
    }

    #[test]
    fn result_missing_analyte_prefix_skipped() {
        // Field 2 not starting with "^^^" — malformed; skip rather than panic
        let frame = "R|1|Glucose|93|mg/dL^P||A/M0/T1||201505261150";
        assert!(matches!(parse_result_record(frame).unwrap(), Record::Skip));
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
        assert_eq!(r.value, 7.4);
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

    // ── Binary capture + replay ───────────────────────────────────────────────

    const LF: u8 = b'\n';

    fn packet(dir: Dir, data: &[u8]) -> Packet {
        assert!(data.len() <= MAX_PAYLOAD, "payload too big for one packet");
        let mut buf = [0u8; HID_PACKET_SIZE];
        buf[3] = data.len() as u8;
        buf[4..4 + data.len()].copy_from_slice(data);
        Packet { dir, data: buf }
    }

    /// Frame one record line as the meter would (STX, seq, content, CR, ETX,
    /// checksum, CR, LF) and split it across HID packets.
    fn framed(seq: u8, record: &str) -> Vec<Packet> {
        let mut checked = vec![b'0' + seq];
        checked.extend_from_slice(record.as_bytes());
        checked.extend_from_slice(&[CR, ETX]);

        let mut msg = vec![STX];
        msg.extend_from_slice(&checked);
        msg.extend_from_slice(compute_checksum(&checked).as_bytes());
        msg.extend_from_slice(&[CR, LF]);

        msg.chunks(MAX_PAYLOAD)
            .map(|c| packet(Dir::Rx, c))
            .collect()
    }

    fn fixture_lines() -> Vec<&'static str> {
        FIXTURE
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .collect()
    }

    /// A capture of the fixture session, interleaved with the host ACKs that
    /// a real log contains — replay must ignore its own TX traffic.
    fn fixture_capture() -> Vec<Packet> {
        let mut packets = vec![packet(Dir::Tx, &[ACK])];
        for (i, line) in fixture_lines().iter().enumerate() {
            packets.extend(framed((i % 8) as u8, line));
            packets.push(packet(Dir::Tx, &[ACK]));
        }
        packets.push(packet(Dir::Rx, &[EOT]));
        packets
    }

    fn summarise(readings: &[Reading]) -> Vec<(u32, f64, String)> {
        readings
            .iter()
            .map(|r| (r.record_number, r.value, r.timestamp.clone()))
            .collect()
    }

    #[test]
    fn encode_decode_packets_round_trip() {
        let packets = fixture_capture();
        let decoded = decode_packets(&encode_packets(&packets)).unwrap();

        assert_eq!(decoded.len(), packets.len());
        for (a, b) in decoded.iter().zip(&packets) {
            assert_eq!(a.data, b.data);
            assert_eq!(a.dir.to_string(), b.dir.to_string());
        }
    }

    #[test]
    fn decode_packets_rejects_foreign_and_damaged_files() {
        let good = encode_packets(&fixture_capture());

        assert!(decode_packets(b"").is_err(), "empty file");
        assert!(
            decode_packets(b"0000  06 ").is_err(),
            "hex dump, not a capture"
        );

        let mut bad_version = good.clone();
        bad_version[CAPTURE_MAGIC.len()] = CAPTURE_VERSION + 1;
        assert!(decode_packets(&bad_version).is_err(), "future version");

        let mut bad_dir = good.clone();
        bad_dir[CAPTURE_HEADER_LEN] = 9;
        assert!(decode_packets(&bad_dir).is_err(), "bad direction byte");

        // Losing the tail must fail loudly, not yield a short reading list.
        assert!(
            decode_packets(&good[..good.len() - 10]).is_err(),
            "truncated capture"
        );
    }

    /// The point of the whole exercise: a replayed capture must produce the
    /// same session as parsing the records text the live read would have
    /// written, so one device read really can feed every output format.
    #[test]
    fn replay_matches_text_parsing() {
        let session = session_from_packets(fixture_capture()).unwrap();
        let (device, readings) = parse_records_from_text(FIXTURE);

        assert_eq!(session.raw_records, fixture_lines());
        assert_eq!(summarise(&session.readings), summarise(&readings));
        assert_eq!(session.device.serial_number, device.serial_number);
        assert_eq!(session.device.model, device.model);
        assert_eq!(session.device.record_count, device.record_count);
    }

    /// The H record is longer than one HID packet, so this also covers
    /// multi-packet reassembly surviving a trip through the binary file.
    #[test]
    fn replay_survives_a_file_round_trip() {
        let bytes = encode_packets(&fixture_capture());
        let session = session_from_packets(decode_packets(&bytes).unwrap()).unwrap();

        assert!(
            framed(0, fixture_lines()[0]).len() > 1,
            "fixture H record should span several packets"
        );
        assert_eq!(session.raw_records, fixture_lines());
        // Re-encoding what we decoded must be byte-identical.
        assert_eq!(encode_packets(&session.raw_packets), bytes);
    }

    #[test]
    fn replay_skips_frames_the_meter_retried() {
        let lines = fixture_lines();
        let mut packets = vec![packet(Dir::Tx, &[ACK])];
        for (i, line) in lines.iter().enumerate() {
            // A corrupt frame, NAK'd in the live session, then resent.
            if i == 2 {
                let mut broken = framed(1, "R|9|^^^Glucose|5.0|mmol/L^P||T0/M0||20200101090000");
                let last = broken.last_mut().unwrap();
                last.data[5] = b'Z'; // break the checksum
                packets.extend(broken);
            }
            packets.extend(framed((i % 8) as u8, line));
        }
        packets.push(packet(Dir::Rx, &[EOT]));

        let session = session_from_packets(packets).unwrap();
        assert_eq!(session.raw_records, lines, "retried frame must not appear");
    }

    #[test]
    fn empty_capture_is_an_error_not_an_empty_session() {
        let err = session_from_packets(vec![packet(Dir::Rx, &[EOT])]).unwrap_err();
        assert!(err.to_string().contains("no records"), "got: {err}");
    }
}
