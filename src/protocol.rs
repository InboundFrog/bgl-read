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
//! The checksum covers seq_digit through ETX/ETB (inclusive).
//! It is the sum of those bytes mod 256, in uppercase hex.

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

fn product_name(pid: u16) -> Option<&'static str> {
    SUPPORTED_DEVICES
        .iter()
        .find(|(p, _)| *p == pid)
        .map(|(_, n)| *n)
}

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
/// Most passes of the receive loop that may end without a record.
/// Receive errors, parse errors, unexpected bytes, ENQ and ACK all count.
/// A normal start needs one or two passes, so 6 leaves room.
const MAX_RETRIES: u32 = 6;
const RECEIVE_TIMEOUT: Duration = Duration::from_secs(10);
/// How long to wait for the meter's EOT reply when closing with a NAK.
const CLOSE_TIMEOUT: Duration = Duration::from_secs(2);
/// Largest message we accept. Real ASTM frames are much smaller than 1 KiB.
const MAX_MESSAGE_SIZE: usize = 64 * 1024;
/// Default low and high glucose thresholds in mg/dL. We use them when the
/// header config field is missing or cannot be parsed.
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
    /// The meter's own clock at the time of the session. ISO 8601 local time, no time zone.
    pub device_time: String,
    /// Low limit in mg/dL. Values from mmol/L meters are converted.
    pub low_threshold: u32,
    /// High limit in mg/dL. Values from mmol/L meters are converted.
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Packet {
    pub dir: Dir,
    pub data: [u8; HID_PACKET_SIZE],
}

/// Everything captured during a session.
#[derive(Debug, Default)]
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
        let name = product_name(dev.product_id()).unwrap_or("Unknown Contour device");
        println!(
            "{name}  VID={:#06x}  PID={:#06x}  S/N={}",
            dev.vendor_id(),
            dev.product_id(),
            dev.serial_number().unwrap_or("").escape_debug()
        );
    }
    if !found {
        println!("No Contour devices found.");
    }
}

pub fn open_device(api: &HidApi) -> Result<HidDevice> {
    let mut errors = Vec::new();
    for &(pid, name) in SUPPORTED_DEVICES {
        for info in api.device_list() {
            if info.vendor_id() != VENDOR_ID || info.product_id() != pid {
                continue;
            }
            match info.open_device(api) {
                Ok(dev) => {
                    eprintln!("Opened: {name}");
                    return Ok(dev);
                }
                Err(e) => errors.push(format!("  {name}: {e}")),
            }
        }
    }
    if !errors.is_empty() {
        return Err(anyhow!(
            "Found a supported device, but could not open it:\n{}\n\
             Check permissions (macOS: Input Monitoring; Linux: udev rules) \
             and that no other program is using the meter.",
            errors.join("\n")
        ));
    }
    Err(anyhow!(
        "No supported Contour device found — is it plugged in?\n\
         Run with --list to enumerate connected HID devices."
    ))
}

// ── Low-level HID I/O ─────────────────────────────────────────────────────────

/// Position of the size byte in a 64-byte packet.
const PKT_SIZE_OFFSET: usize = 3;
/// Position of the first data byte in a 64-byte packet.
const PKT_DATA_OFFSET: usize = 4;
/// A write buffer is one byte longer than a packet. The extra byte is the report ID.
const WRITE_PACKET_SIZE: usize = HID_PACKET_SIZE + 1;

/// Build a 65-byte write buffer (report-ID + 64 payload bytes).
fn build_write_packet(data: &[u8]) -> [u8; WRITE_PACKET_SIZE] {
    debug_assert!(
        data.len() <= MAX_PAYLOAD,
        "write payload {} exceeds MAX_PAYLOAD {}",
        data.len(),
        MAX_PAYLOAD
    );
    // Layout: [report_id=0, hdr0=0, hdr1=0, hdr2=0, length, data..., padding]
    let mut pkt = [0u8; WRITE_PACKET_SIZE];
    // The report ID shifts every offset by one.
    pkt[PKT_SIZE_OFFSET + 1] = data.len() as u8;
    pkt[PKT_DATA_OFFSET + 1..PKT_DATA_OFFSET + 1 + data.len()].copy_from_slice(data);
    pkt
}

/// Keeps HID packets for `--format bytes` and `--format binary`. When `capture` is false,
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

/// The two device calls the session loop needs. A test can replace the real
/// meter with a scripted one, because the loop only sees this trait.
pub(crate) trait Transport {
    fn write(&self, data: &[u8]) -> Result<()>;
    fn read_timeout(&self, buf: &mut [u8], timeout_ms: i32) -> Result<usize>;
}

impl Transport for HidDevice {
    fn write(&self, data: &[u8]) -> Result<()> {
        HidDevice::write(self, data)?;
        Ok(())
    }

    fn read_timeout(&self, buf: &mut [u8], timeout_ms: i32) -> Result<usize> {
        Ok(HidDevice::read_timeout(self, buf, timeout_ms)?)
    }
}

fn hid_write(device: &impl Transport, data: &[u8], log: &mut PacketLog) -> Result<()> {
    let pkt = build_write_packet(data);
    // Log without the report-ID byte
    let tx_data: [u8; HID_PACKET_SIZE] =
        pkt[1..].try_into().expect("pkt is WRITE_PACKET_SIZE bytes");
    log.push(Packet {
        dir: Dir::Tx,
        data: tx_data,
    });
    device.write(&pkt)?;
    Ok(())
}

/// Read one 64-byte HID packet with a deadline.
fn hid_read(
    device: &impl Transport,
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
    /// The digit after STX, normally 0 to 7. None for control bytes.
    frame_number: Option<u8>,
}

/// Read one or more HID packets and join them into one complete ASTM message.
///
/// The message is complete when any of these is true:
///   - The packet is not full (SIZE < MAX_PAYLOAD).
///   - The first data byte is a single-byte control character (ENQ, EOT, ACK, NAK).
///   - The data ends with the ASTM tail:  … CR  ETX|ETB  CS1 CS2  CR  LF
///     In that case ETX/ETB is at data[SIZE-5] (= pkt[SIZE-1] from the start of the packet).
fn receive_message(
    device: &impl Transport,
    timeout: Duration,
    log: &mut PacketLog,
) -> Result<Message> {
    let deadline = Instant::now() + timeout;
    let mut asm = MessageAssembler::new();

    loop {
        let pkt = hid_read(device, deadline, log)?;
        if let Some(msg) = asm.push(&pkt) {
            return msg;
        }
    }
}

/// Joins a stream of 64-byte HID packets into ASTM messages.
///
/// This is separate from the device. So `--from-bytes` replays a capture with
/// the same framing rules that read it. There is one decoder, not two.
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
        let size = pkt[PKT_SIZE_OFFSET] as usize;
        let data_end = (PKT_DATA_OFFSET + size).min(HID_PACKET_SIZE);
        self.buf.extend_from_slice(&pkt[PKT_DATA_OFFSET..data_end]);

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
        // If a session starts right after a previous one, the meter sends
        // EOT+ENQ in one packet. It ends the old session and offers a new one.
        // We return the ENQ so the handshake goes on. Otherwise we would
        // treat it as the end of the transmission.
        let msg_type = if buf == [EOT, ENQ] { ENQ } else { msg_type };
        return Ok(Message {
            msg_type,
            frame: String::new(),
            frame_number: None,
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
            "Checksum mismatch (expected {expected:?}, computed {computed})"
        ));
    }

    // Frame content is buf[2..len-4]. It includes the trailing CR and ETX/ETB.
    // We remove them so callers see only the record text.
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
    let frame_number = buf[1].checked_sub(b'0');
    Ok(Message {
        msg_type,
        frame,
        frame_number,
    })
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
        Some(c) => Err(anyhow!(
            "Unknown record type '{}' in frame: {frame:?}",
            c.escape_debug()
        )),
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

/// Get the serial number from the raw field.
/// "7830H5001733" → "5001733",  "6301-1C2CF8C" → "1C2CF8C"
/// Same as the JS regex /^\d+[\w-]\s*(\w+)/: skip the leading digits and one separator character.
fn parse_serial(raw: &str) -> String {
    let after_digits = raw.trim_start_matches(|c: char| c.is_ascii_digit());
    // Skip exactly one separator character.
    match after_digits.chars().next() {
        Some(sep) => after_digits[sep.len_utf8()..].trim_start().to_string(),
        None => raw.to_string(),
    }
}

/// Read the thresholds and units from the config field (field[5]).
/// The config looks like this:  A=1^C=00^I=0200^R=0^S=01^U=0^V=20600^X=...
fn parse_thresholds(config: &str) -> (u32, u32) {
    let mut low = DEFAULT_LOW_THRESHOLD;
    let mut high = DEFAULT_HIGH_THRESHOLD;
    let mut mmol = false;

    for part in config.split('^') {
        if let Some(val) = part.strip_prefix("V=") {
            // V=LLHHH: LL is the 2-digit low limit, HHH is the 3-digit high limit.
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
        // The values are mmol/L × 10. Convert them to mg/dL.
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

    // A bad number must be an error. Using a default would invent a reading.
    let record_number: u32 = fields[1]
        .trim()
        .parse()
        .map_err(|_| anyhow!("bad record number in R record: {frame:?}"))?;
    let value: f64 = match fields[3].trim().parse::<f64>() {
        Ok(v) if v.is_finite() && v >= 0.0 => v,
        _ => return Err(anyhow!("unreadable value in R record: {frame:?}")),
    };
    let units = fields[4].split('^').next().unwrap_or("mg/dL").to_string();

    // fields[6] is the annotation / marker field, e.g. "A/M0/T1", ">", "C"
    let annotation = fields[6];
    // A substring match is kept on purpose: the reference driver uses the same
    // rule (markers.indexOf('C')), and all known markers agree with it.
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

/// Write a packet log. It starts with a 9-byte header. Then each entry is one
/// direction byte plus the raw packet. The entry size is fixed and stored in
/// the header. So we can detect a truncated file instead of reading it as a short one.
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

/// The reverse of [`encode_packets`]. It rejects anything it cannot read
/// exactly. A half-decoded capture would give a short reading list that looks valid.
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
/// Only RX packets carry meter data. TX entries are our own ACK/NAK traffic.
/// A TX packet also marks the start of a new receive: the live read always
/// writes before it reads, and it drops any half-read message. Replay does the
/// same, so a partial message cannot corrupt the meter's resend.
/// Messages that fail to decode are skipped: in the live session those were
/// NAK'd and retried, so the retry's good copy appears later in the stream.
pub fn session_from_packets(mut packets: Vec<Packet>) -> Result<Session> {
    let mut asm = MessageAssembler::new();
    let mut builder = SessionBuilder::new(false);

    for pkt in &packets {
        if matches!(pkt.dir, Dir::Tx) {
            asm = MessageAssembler::new();
            continue;
        }
        let Some(Ok(msg)) = asm.push(&pkt.data) else {
            continue;
        };

        let (record, raw, number) = match msg.msg_type {
            STX => match parse_record(&msg.frame) {
                Ok(record) => (record, msg.frame, msg.frame_number),
                // Superseded by the retry the meter sent after our NAK.
                Err(_) => continue,
            },
            EOT => (Record::EndOfTransmission, String::new(), None),
            // ENQ / ACK / NAK carry no record payload.
            _ => continue,
        };

        if !matches!(builder.push(record, raw, number), Flow::Continue) {
            break;
        }
    }

    builder.finish(
        "Capture contains no records — it may be truncated, or from a session that failed \
         before the meter sent anything.",
        &mut packets,
    )
}

// ── Text-format round-trip ────────────────────────────────────────────────────

/// Build a Session from a saved `--format records` text dump in a single pass:
/// every trimmed non-empty line is kept verbatim in `raw_records`, and lines
/// that parse contribute to `device`/`readings`.
///
/// Lines that fail to parse are skipped but counted, and the count is warned
/// about on stderr — a truncated or mangled dump would otherwise yield a
/// quietly short CSV that looks perfectly valid.
/// `raw_packets` is left empty — file-driven input has no HID traffic.
pub fn session_from_records_text(text: &str) -> Session {
    let (session, controls) = session_and_controls_from_text(text);
    if controls > 0 {
        eprintln!("{}", control_skip_message(controls));
    }
    session
}

/// Does the work of `session_from_records_text`. Also returns how many
/// control readings were left out, so a test can check the count.
fn session_and_controls_from_text(text: &str) -> (Session, usize) {
    let mut device = DeviceInfo::default();
    let mut readings = Vec::new();
    let mut raw_records = Vec::new();
    let mut unparsed = 0usize;
    let mut controls = 0usize;
    let mut saw_terminator = false;
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
        raw_records.push(line.to_string());
        match parse_record(line) {
            Ok(Record::Header(info)) => device = info,
            Ok(Record::Result(r)) if !r.is_control => readings.push(r),
            Ok(Record::Result(_)) => controls += 1,
            Ok(Record::Terminator) => saw_terminator = true,
            // The P/C/O/M/Q/S/L record types are recognised and
            // deliberately carry nothing into the output.
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
    if !raw_records.is_empty() && !saw_terminator {
        eprintln!("warning: no L (terminator) record found; the dump may be incomplete");
    }
    let session = Session {
        device,
        readings,
        raw_records,
        raw_packets: Vec::new(),
    };
    (session, controls)
}

/// One function builds the note, so the live read, the replay and
/// `--from-records` print exactly the same text.
fn control_skip_message(n: usize) -> String {
    format!("note: left out {n} control solution reading(s); they are still in the records output")
}

// ── Main session loop ─────────────────────────────────────────────────────────

/// Send one command, receive one complete message, retry with NAK on transient errors.
fn get_one_record(
    device: &impl Transport,
    log: &mut PacketLog,
) -> Result<(Record, String, Option<u8>)> {
    // Counts every pass that does not return a record.
    let mut retries = 0u32;
    let mut cmd = ACK;

    loop {
        hid_write(device, &[cmd], log)?;
        cmd = ACK;

        let msg = match receive_message(device, RECEIVE_TIMEOUT, log) {
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
            EOT => return Ok((Record::EndOfTransmission, String::new(), None)),
            STX => match parse_record(&msg.frame) {
                Ok(record) => return Ok((record, msg.frame, msg.frame_number)),
                Err(e) => {
                    retries += 1;
                    if retries >= MAX_RETRIES {
                        return Err(e);
                    }
                    eprintln!("Parse error ({retries}/{MAX_RETRIES}): {e}");
                    cmd = NAK;
                }
            },
            // ENQ or ACK: the meter is not ready to send yet. ACK again, but
            // count the pass so a device that never sends data cannot loop forever.
            ENQ | ACK => {
                retries += 1;
                if retries >= MAX_RETRIES {
                    return Err(anyhow!(
                        "No data after {MAX_RETRIES} tries; the last reply was {:#04x}, not data",
                        msg.msg_type
                    ));
                }
            }
            other => {
                retries += 1;
                if retries >= MAX_RETRIES {
                    return Err(anyhow!(
                        "Unexpected message byte {other:#04x} after {MAX_RETRIES} retries"
                    ));
                }
                eprintln!("Unexpected message byte {other:#04x} ({retries}/{MAX_RETRIES})");
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
fn close_session(device: &impl Transport, log: &mut PacketLog) {
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
    device: &impl Transport,
    progress: bool,
    capture_packets: bool,
) -> Result<Session, FetchError> {
    let mut log = PacketLog::new(capture_packets);
    fetch_all_inner(device, progress, &mut log).map_err(|error| FetchError {
        error,
        packets: log.packets,
    })
}

fn fetch_all_inner(
    device: &impl Transport,
    progress: bool,
    packets: &mut PacketLog,
) -> Result<Session> {
    let mut builder = SessionBuilder::new(progress);

    loop {
        let (record, raw, number) = get_one_record(device, packets)?;

        match builder.push(record, raw, number) {
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
        &mut packets.packets,
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
    controls: usize,
    raw_records: Vec<String>,
    progress: bool,
    saw_terminator: bool,
    /// Number of the last frame we accepted.
    last_frame: Option<u8>,
    /// How many times the frame number jumped by more than one.
    frame_jumps: u32,
}

impl SessionBuilder {
    fn new(progress: bool) -> Self {
        Self {
            device: DeviceInfo::default(),
            readings: Vec::new(),
            controls: 0,
            raw_records: Vec::new(),
            progress,
            saw_terminator: false,
            last_frame: None,
            frame_jumps: 0,
        }
    }

    /// Feed one decoded record and the raw frame text it came from.
    fn push(&mut self, record: Record, raw: String, number: Option<u8>) -> Flow {
        if let Some(n) = number {
            // Same number and same text: the meter sent this frame again.
            if self.last_frame == Some(n) && self.raw_records.last() == Some(&raw) {
                return Flow::Continue;
            }
            if self.last_frame.is_some_and(|last| n != (last + 1) % 8) {
                self.frame_jumps += 1;
            }
            self.last_frame = Some(n);
        }
        if !raw.is_empty() {
            self.raw_records.push(raw);
        }

        match record {
            Record::Header(info) => self.device = info,
            Record::Result(r) => self.push_reading(r),
            Record::Skip => {}
            Record::Terminator => {
                self.saw_terminator = true;
                return Flow::Close;
            }
            Record::EndOfTransmission => return Flow::Stop,
        }
        Flow::Continue
    }

    fn push_reading(&mut self, r: Reading) {
        if r.is_control {
            self.controls += 1;
            if self.progress {
                // Write over the open progress line, if any. Padding to the same
                // width hides its old text. "\r" does nothing at line start, so
                // no empty line is printed.
                let width = Self::progress_line(self.readings.len(), self.device.record_count, &r)
                    .chars()
                    .count();
                let note = format!("(skipping control reading #{})", r.record_number);
                eprintln!("\r{note:<width$}");
            }
            return;
        }

        if self.progress {
            let total = self.device.record_count;
            let n = self.readings.len() + 1;
            eprint!("\r{}", Self::progress_line(n, total, &r));
            let _ = std::io::stderr().flush();
        }
        self.readings.push(r);
    }

    /// Each part has a fixed width, so all lines are the same length and a
    /// new line fully covers the last one.
    ///
    /// The line is about 65 columns wide. It is at most 67 columns for a
    /// meter with 1000 or more readings. On a narrower terminal the line
    /// wraps, and each reading then prints on its own row. This is by
    /// design: the tool does not check the terminal size.
    fn progress_line(n: usize, total: u32, r: &Reading) -> String {
        let pct = (100 * n as u32).checked_div(total).unwrap_or(0);
        // Format to Strings first: escape_debug() ignores width.
        let ts = r.timestamp.escape_debug().to_string();
        let units = r.units.escape_debug().to_string();
        let flag = if r.high {
            "HIGH "
        } else if r.low {
            "LOW  "
        } else {
            "     "
        };
        let meal = r.meal_marker.as_deref().unwrap_or("");
        format!(
            "[{n:>3}/{total}] {pct:>3}%  {ts}  {:>5.1} {units:<6}  {flag}{meal:<9}",
            r.value
        )
    }

    /// `empty_err` is the message for a session that decoded no records at
    /// all — the advice differs between a live meter and a capture file.
    ///
    /// On success the packets move into the Session. On error they stay in
    /// `packets`, so the caller can still dump them.
    fn finish(self, empty_err: &str, packets: &mut Vec<Packet>) -> Result<Session> {
        if self.progress {
            // Move to a fresh line after the progress output
            eprintln!();
        }

        // This is a warning, not an error, on purpose. We could not check the
        // numbering on every meter model, and a wrong guess must not make
        // every read fail.
        if self.frame_jumps > 0 {
            eprintln!(
                "Warning: frame number jumped {} time(s). Some records may be missing.",
                self.frame_jumps
            );
        }

        if self.raw_records.is_empty() {
            return Err(anyhow!("{empty_err}"));
        }

        // Without the L record the list may be short. Do not call it done.
        if !self.saw_terminator {
            return Err(anyhow!(
                "Session ended before the L (terminator) record, so the data is incomplete."
            ));
        }

        eprintln!(
            "Done: {} readings from {} (S/N {})",
            self.readings.len(),
            self.device.model.escape_debug(),
            self.device.serial_number.escape_debug()
        );
        if self.controls > 0 {
            eprintln!("{}", control_skip_message(self.controls));
        }

        Ok(Session {
            device: self.device,
            readings: self.readings,
            raw_records: self.raw_records,
            raw_packets: std::mem::take(packets),
        })
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
        // The sum of one byte is that byte. 0x06 % 256 = 6, so the result is "06"
        assert_eq!(compute_checksum(&[0x06]), "06");
    }

    #[test]
    fn checksum_overflow_wraps() {
        // 255 + 2 = 257, and 257 % 256 = 1, so the result is "01"
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

    /// Seen on a real Contour Next One. If a session starts right after a
    /// previous one, the meter sends EOT+ENQ in one packet. This ends the old
    /// session and offers a new one. We must return the ENQ, or the session ends empty.
    #[test]
    fn decode_eot_enq_pair_surfaces_enq() {
        let msg = decode_message(&[EOT, ENQ]).unwrap();
        assert_eq!(msg.msg_type, ENQ);
        assert!(msg.frame.is_empty());
    }

    /// The checksum bytes come from the device. The error is printed on the
    /// terminal, so it must not hold a raw ESC byte.
    #[test]
    fn decode_checksum_error_escapes_device_bytes() {
        let frame = [STX, b'1', b'x', CR, ETX, 0x1b, b'c', CR, LF];
        let err = decode_message(&frame).err().unwrap().to_string();
        assert!(err.contains("Checksum mismatch"));
        assert!(!err.contains('\x1b'));
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
        // ETB or other trailing bytes sometimes get through. We take only the digits.
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
        // U=1 means mmol/L. V=02033 means low 0.2 mmol/L and high 3.3 mmol/L.
        // After the factor 18.01559 these are about 4 and 59 mg/dL.
        let (lo, hi) = parse_thresholds("U=1^V=02033");
        assert_eq!(lo, 4);
        assert_eq!(hi, 59);
    }

    #[test]
    fn thresholds_multibyte_utf8_does_not_panic() {
        // A multibyte character in the V field used to cause a panic.
        // The code sliced the string in the middle of a character.
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

    #[test]
    fn result_unreadable_value_is_error() {
        for v in ["HI", "", "NaN", "inf", "-1"] {
            let frame = format!("R|1|^^^Glucose|{v}|mg/dL^P||A/M0/T1||201505261150");
            assert!(parse_result_record(&frame).is_err(), "value {v:?}");
        }
    }

    #[test]
    fn result_bad_record_number_is_error() {
        let frame = "R|x|^^^Glucose|93|mg/dL^P||A/M0/T1||201505261150";
        assert!(parse_result_record(frame).is_err());
    }

    // ── parse_header ──────────────────────────────────────────────────────────

    #[test]
    fn header_parses_correctly() {
        // A short but valid H record with 14 fields, split by `|`.
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
    // All personal values (serial, timestamps, glucose readings) are replaced
    // with made-up ones. The structure comes straight from the capture: the
    // serial format, the config string layout and the annotation variants.

    /// Real device serial format: leading model digits, then one uppercase letter as a separator.
    /// "7802H7001396" → strip "7802" + "H" → "7001396"
    #[test]
    fn serial_real_device_format() {
        assert_eq!(parse_serial("7802H7001396"), "7001396");
    }

    /// Real device config string: U=1 (mmol/L), V=06333.
    /// In the V field, low = "06" / 10 = 0.6 mmol/L and high = "333" / 10 = 33.3 mmol/L.
    /// In mg/dL: 0.6 × 18.01559 ≈ 11 and 33.3 × 18.01559 ≈ 600.
    #[test]
    fn thresholds_real_device_config() {
        let config = "A=1^C=6^R=0^S=1^U=1^V=06333^X=039039100072^a=1^J=0";
        let (lo, hi) = parse_thresholds(config);
        assert_eq!(lo, 11);
        assert_eq!(hi, 600);
    }

    /// The most common annotation in the real data. It has no meal or time context.
    #[test]
    fn result_t0m0_no_meal_marker() {
        let r = parse_r("R|1|^^^Glucose|7.4|mmol/L^P||T0/M0||20200101090000");
        assert_eq!(r.meal_marker, None);
        assert!(!r.high);
        assert!(!r.low);
        assert!(!r.is_control);
    }

    /// The "A/T0/M0" annotation seen in the real data (post-meal set on the
    /// meter). The leading 'A' means post-meal. The rest is ignored.
    #[test]
    fn result_a_t0m0_is_postmeal() {
        let r = parse_r("R|354|^^^Glucose|7.9|mmol/L^P||A/T0/M0||20200101180000");
        assert_eq!(r.meal_marker, Some("post-meal".to_string()));
        assert!(!r.high);
        assert!(!r.low);
    }

    /// H record with the structure of the real device: password in field[3],
    /// 14-digit timestamp, an empty last field (15 fields in total), U=1, V=06333.
    #[test]
    fn header_real_device_structure() {
        // Cleaned: fake password, fake serial, fake timestamp, made-up values.
        let frame = "H|\\^&||AAAAAA|ContourTest^01.00\\01.00\\01.00^0000X0000000|\
                     A=1^C=6^R=0^S=1^U=1^V=06333^X=039039100072^a=1^J=0|\
                     800|||||P|1|20200101090000|";
        let info = parse_header(frame).unwrap();
        assert_eq!(info.model, "ContourTest");
        assert_eq!(info.serial_number, "0000000");
        assert_eq!(info.record_count, 800);
        assert_eq!(info.device_time, "2020-01-01T09:00:00");
        // U=1, V=06333 gives low ≈ 11 mg/dL and high ≈ 600 mg/dL.
        assert_eq!(info.low_threshold, 11);
        assert_eq!(info.high_threshold, 600);
    }

    // ── parse_records_from_text / fixture round-trip ──────────────────────────

    const FIXTURE: &str = include_str!("../tests/fixtures/sample.txt");

    /// Parse a records text dump and return only the device and readings.
    fn parse_records_from_text(text: &str) -> (DeviceInfo, Vec<Reading>) {
        let session = session_from_records_text(text);
        (session.device, session.readings)
    }

    #[test]
    fn fixture_device_info_parses() {
        let (device, _) = parse_records_from_text(FIXTURE);
        assert_eq!(device.model, "ContourTest");
        assert_eq!(device.serial_number, "0000000");
        assert_eq!(device.record_count, 6);
        assert_eq!(device.device_time, "2020-01-01T09:00:00");
    }

    #[test]
    fn fixture_keeps_all_six_readings() {
        // The fixture has no control reading, so all 6 R records are kept.
        let (_, readings) = parse_records_from_text(FIXTURE);
        assert_eq!(readings.len(), 6);
    }

    #[test]
    fn records_text_counts_control_readings() {
        let text = FIXTURE.replace(
            "L|1||N",
            "R|7|^^^Glucose|5.5|mmol/L^P||C||20200102100000\nL|1||N",
        );
        let (session, controls) = session_and_controls_from_text(&text);
        assert_eq!(controls, 1);
        assert!(session.readings.iter().all(|r| !r.is_control));
        assert_eq!(session.readings.len(), 6);
    }

    #[test]
    fn control_reading_is_excluded_but_kept_raw() {
        let text = FIXTURE.replace(
            "L|1||N",
            "R|7|^^^Glucose|5.5|mmol/L^P||C||20200102100000\nL|1||N",
        );
        let session = session_from_records_text(&text);
        assert_eq!(session.readings.len(), 6);
        assert!(session.raw_records.iter().any(|l| l.starts_with("R|7|")));
    }

    #[test]
    fn push_reading_counts_control_readings() {
        let mut b = SessionBuilder::new(false);
        let mk = |ann: &str| {
            let frame = format!("R|1|^^^Glucose|5.5|mmol/L^P||{ann}||20200102100000");
            match parse_record(&frame).unwrap() {
                Record::Result(r) => r,
                _ => panic!("not a result"),
            }
        };
        b.push_reading(mk("C"));
        b.push_reading(mk("B"));
        assert_eq!(b.controls, 1);
        assert_eq!(b.readings.len(), 1);
    }

    #[test]
    fn progress_line_has_fixed_width() {
        let mk = |value: f64, meal: Option<&str>| {
            let mut r =
                match parse_record("R|1|^^^Glucose|5.5|mmol/L^P||B||20200102100000").unwrap() {
                    Record::Result(r) => r,
                    _ => panic!("not a result"),
                };
            r.value = value;
            r.meal_marker = meal.map(String::from);
            r
        };
        let base = SessionBuilder::progress_line(1, 800, &mk(5.4, None))
            .chars()
            .count();
        for v in [5.4, 19.0, 601.0] {
            for meal in [None, Some("post-meal")] {
                let line = SessionBuilder::progress_line(1, 800, &mk(v, meal));
                assert_eq!(line.chars().count(), base, "{line:?}");
            }
        }
        // The doc comment on progress_line states these two widths.
        assert_eq!(base, 65);
        let big = SessionBuilder::progress_line(1000, 2000, &mk(601.0, Some("post-meal")));
        assert_eq!(big.chars().count(), 67, "{big:?}");
        // 70 is the width the README states.
        assert!(big.chars().count() <= 70, "{big:?}");
        // The flag texts are padded by hand, so check them too.
        let mut high = mk(5.4, None);
        high.high = true;
        let mut low = mk(5.4, None);
        low.low = true;
        for r in [high, low] {
            let line = SessionBuilder::progress_line(1, 800, &r);
            assert_eq!(line.chars().count(), base, "{line:?}");
        }
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
        // R|4 has A/T0/M0.
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
        buf[PKT_SIZE_OFFSET] = data.len() as u8;
        buf[PKT_DATA_OFFSET..PKT_DATA_OFFSET + data.len()].copy_from_slice(data);
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

        assert_eq!(decoded, packets);
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
    fn replay_drops_partial_message_before_a_resend() {
        let lines = fixture_lines();
        let mut packets = vec![packet(Dir::Tx, &[ACK])];
        for (i, line) in lines.iter().enumerate() {
            let frame = framed((i % 8) as u8, line);
            if i == 0 {
                // The read timed out after the first packet. We sent NAK and
                // the meter sent the whole message again.
                assert!(frame.len() > 1, "H record must span several packets");
                packets.push(frame[0].clone());
                packets.push(packet(Dir::Tx, &[NAK]));
            }
            packets.extend(frame);
        }
        packets.push(packet(Dir::Rx, &[EOT]));

        let session = session_from_packets(packets).unwrap();
        assert_eq!(session.raw_records, lines);
        assert_eq!(session.device.model, "ContourTest");
    }

    /// Index of the first R record in the fixture.
    fn first_result_index() -> usize {
        fixture_lines()
            .iter()
            .position(|l| l.starts_with('R'))
            .unwrap()
    }

    #[test]
    fn replay_drops_a_repeated_frame() {
        let lines = fixture_lines();
        let dup = first_result_index();
        let mut packets = Vec::new();
        for (i, line) in lines.iter().enumerate() {
            packets.extend(framed((i % 8) as u8, line));
            if i == dup {
                packets.extend(framed((i % 8) as u8, line));
            }
        }
        let session = session_from_packets(packets).unwrap();
        assert_eq!(session.raw_records, lines);
    }

    #[test]
    fn builder_counts_frame_number_jumps() {
        let rec = |t: &str| (parse_record(t).unwrap(), t.to_string());
        let lines = fixture_lines();
        let r = lines[first_result_index()];
        let h = lines[0];

        let mut b = SessionBuilder::new(false);
        let (rc, raw) = rec(h);
        b.push(rc, raw, Some(7));
        let (rc, raw) = rec(r);
        b.push(rc, raw, Some(0));
        assert_eq!(b.frame_jumps, 0);

        let (rc, raw) = rec(h);
        b.push(rc, raw, Some(2));
        assert_eq!(b.frame_jumps, 1);

        // Same number but different text is a new frame, so it is kept.
        let kept = b.raw_records.len();
        let (rc, raw) = rec(r);
        b.push(rc, raw, Some(2));
        assert_eq!(b.raw_records.len(), kept + 1);
        // A repeated number is not last + 1, so it also counts as a jump.
        assert_eq!(b.frame_jumps, 2);
    }

    #[test]
    fn live_session_drops_a_frame_the_meter_sends_twice() {
        let lines = fixture_lines();
        let dup = first_result_index();
        let mut script = script_of(vec![packet(Dir::Rx, &[ENQ])]);
        for (i, line) in lines.iter().enumerate() {
            script.extend(script_of(framed((i % 8) as u8, line)));
            if i == dup {
                // A timeout makes the host send NAK. The meter then repeats the frame.
                script.push(None);
                script.extend(script_of(framed((i % 8) as u8, line)));
            }
        }
        script.extend(script_of(vec![packet(Dir::Rx, &[EOT])]));

        let meter = ScriptedMeter::new(script);
        let session = fetch_all(&meter, false, false).unwrap();
        assert_eq!(session.raw_records, lines);
    }

    #[test]
    fn empty_capture_is_an_error_not_an_empty_session() {
        let err = session_from_packets(vec![packet(Dir::Rx, &[EOT])]).unwrap_err();
        assert!(err.to_string().contains("no records"), "got: {err}");
    }

    // ── Incomplete sessions and escaped errors ────────────────────────────────

    /// Packets for the fixture records without the final L record.
    fn capture_without_terminator() -> Vec<Packet> {
        let lines = fixture_lines();
        assert!(lines.last().unwrap().starts_with('L'));
        let mut packets = Vec::new();
        for (i, line) in lines[..lines.len() - 1].iter().enumerate() {
            packets.extend(framed((i % 8) as u8, line));
        }
        packets
    }

    #[test]
    fn capture_ending_with_eot_but_no_l_record_is_an_error() {
        let mut packets = capture_without_terminator();
        packets.push(packet(Dir::Rx, &[EOT]));
        let err = session_from_packets(packets).unwrap_err();
        assert!(err.to_string().contains("incomplete"), "got: {err}");
    }

    #[test]
    fn capture_that_just_stops_is_an_error() {
        let err = session_from_packets(capture_without_terminator()).unwrap_err();
        assert!(err.to_string().contains("incomplete"), "got: {err}");
    }

    #[test]
    fn unknown_record_error_does_not_contain_raw_escape_byte() {
        let err = parse_record("\x1b[2J|junk").unwrap_err().to_string();
        assert!(!err.contains('\x1b'), "got: {err:?}");
    }

    #[test]
    fn records_text_without_l_line_still_returns_readings() {
        let lines = fixture_lines();
        let text = lines[..lines.len() - 1].join("\n");
        let session = session_from_records_text(&text);
        assert!(!session.readings.is_empty());
    }

    // ── Live read with a scripted meter ───────────────────────────────────────

    type Script = Vec<Option<[u8; HID_PACKET_SIZE]>>;

    /// A fake meter. It answers each read with the next scripted packet.
    /// `None`, or an empty script, is a read timeout. It never sleeps.
    struct ScriptedMeter {
        replies: std::cell::RefCell<std::collections::VecDeque<Option<[u8; HID_PACKET_SIZE]>>>,
        sent: std::cell::RefCell<Vec<u8>>,
    }

    impl ScriptedMeter {
        fn new(script: Script) -> Self {
            Self {
                replies: std::cell::RefCell::new(script.into()),
                sent: std::cell::RefCell::new(Vec::new()),
            }
        }

        fn sent(&self) -> Vec<u8> {
            self.sent.borrow().clone()
        }
    }

    impl Transport for ScriptedMeter {
        fn write(&self, data: &[u8]) -> Result<()> {
            let size = data[PKT_SIZE_OFFSET + 1] as usize;
            let start = PKT_DATA_OFFSET + 1;
            self.sent
                .borrow_mut()
                .extend_from_slice(&data[start..start + size]);
            Ok(())
        }

        fn read_timeout(&self, buf: &mut [u8], _timeout_ms: i32) -> Result<usize> {
            match self.replies.borrow_mut().pop_front().flatten() {
                Some(pkt) => {
                    buf[..HID_PACKET_SIZE].copy_from_slice(&pkt);
                    Ok(HID_PACKET_SIZE)
                }
                None => Ok(0),
            }
        }
    }

    fn script_of(packets: Vec<Packet>) -> Script {
        packets.into_iter().map(|p| Some(p.data)).collect()
    }

    /// ENQ, then the given records, then EOT for the close handshake.
    fn full_script() -> Script {
        let mut script = script_of(vec![packet(Dir::Rx, &[ENQ])]);
        for (i, line) in fixture_lines().iter().enumerate() {
            script.extend(script_of(framed((i % 8) as u8, line)));
        }
        script.extend(script_of(vec![packet(Dir::Rx, &[EOT])]));
        script
    }

    #[test]
    fn live_session_reads_every_record_and_closes_with_nak() {
        let meter = ScriptedMeter::new(full_script());
        let session = fetch_all(&meter, false, false).unwrap();
        let (_, readings) = parse_records_from_text(FIXTURE);

        assert_eq!(session.raw_records, fixture_lines());
        assert_eq!(summarise(&session.readings), summarise(&readings));
        let sent = meter.sent();
        assert_eq!(sent.last(), Some(&NAK));
        assert!(sent[..sent.len() - 1].iter().all(|&b| b == ACK));
    }

    #[test]
    fn live_and_replay_agree() {
        let meter = ScriptedMeter::new(full_script());
        let live = fetch_all(&meter, false, true).unwrap();
        let replay = session_from_packets(live.raw_packets.clone()).unwrap();

        assert_eq!(live.raw_records, replay.raw_records);
    }

    #[test]
    fn live_session_naks_a_bad_checksum_and_uses_the_resent_frame() {
        let lines = fixture_lines();
        let mut script = script_of(vec![packet(Dir::Rx, &[ENQ])]);
        for (i, line) in lines.iter().enumerate() {
            let frame = framed((i % 8) as u8, line);
            if i == 2 {
                let mut broken = frame.clone();
                // Break the last checksum character, just before CR LF.
                let last = broken.last_mut().unwrap();
                let size = last.data[PKT_SIZE_OFFSET] as usize;
                last.data[PKT_DATA_OFFSET + size - 3] = b'?';
                script.extend(script_of(broken));
            }
            script.extend(script_of(frame));
        }
        script.extend(script_of(vec![packet(Dir::Rx, &[EOT])]));

        let meter = ScriptedMeter::new(script);
        let session = fetch_all(&meter, false, false).unwrap();

        assert_eq!(session.raw_records, lines);
        assert!(meter.sent()[..meter.sent().len() - 1].contains(&NAK));
    }

    #[test]
    fn live_session_naks_a_read_timeout_and_carries_on() {
        let mut script = full_script();
        // Time out just before the third record: after ENQ and two whole frames.
        let before_third: usize = 1
            + (0..2)
                .map(|i| framed(i as u8, fixture_lines()[i]).len())
                .sum::<usize>();
        script.insert(before_third, None);

        let meter = ScriptedMeter::new(script);
        let session = fetch_all(&meter, false, false).unwrap();

        assert_eq!(session.raw_records, fixture_lines());
        assert!(meter.sent()[..meter.sent().len() - 1].contains(&NAK));
    }

    #[test]
    fn live_session_gives_up_when_the_meter_only_sends_enq() {
        let enq = packet(Dir::Rx, &[ENQ]).data;
        let meter = ScriptedMeter::new(vec![Some(enq); 3 * MAX_RETRIES as usize]);
        let err = fetch_all(&meter, false, true).unwrap_err();

        assert_eq!(meter.sent().len(), MAX_RETRIES as usize);
        assert!(!err.packets.is_empty(), "captured packets must survive");
    }

    #[test]
    fn live_session_with_eot_before_the_l_record_is_incomplete() {
        let mut script = script_of(vec![packet(Dir::Rx, &[ENQ])]);
        script.extend(script_of(framed(0, fixture_lines()[0])));
        script.extend(script_of(vec![packet(Dir::Rx, &[EOT])]));

        let err = fetch_all(&ScriptedMeter::new(script), false, false).unwrap_err();
        assert!(
            err.error.to_string().contains("incomplete"),
            "got: {}",
            err.error
        );
    }

    #[test]
    fn live_session_error_names_the_last_reply_not_a_false_count() {
        let enq = packet(Dir::Rx, &[ENQ]).data;
        let mut script = vec![None];
        script.extend(vec![Some(enq); 3 * MAX_RETRIES as usize]);
        let meter = ScriptedMeter::new(script);
        let err = fetch_all(&meter, false, false).unwrap_err();

        let text = err.error.to_string();
        assert_eq!(meter.sent().len(), MAX_RETRIES as usize);
        assert!(text.contains("0x05"), "got: {text}");
        assert!(text.contains("last reply"), "got: {text}");
        assert!(
            !text.contains(&format!("sent 0x05 {MAX_RETRIES} times")),
            "got: {text}"
        );
    }
}
