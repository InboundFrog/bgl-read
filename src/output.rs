use crate::protocol::Session;
use anyhow::Result;
use clap::ValueEnum;
use std::io::{self, BufWriter, Write};
use std::path::Path;

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum Format {
    /// Structured readings + device info as JSON
    Json,
    /// One reading per row as CSV
    Csv,
    /// Raw ASTM record text as received (H/P/R/L frames)
    Records,
    /// Hex dump of every HID packet sent and received
    Bytes,
}

// ── Entry point ───────────────────────────────────────────────────────────────

pub fn write(session: &Session, format: Format, path: Option<&Path>) -> Result<()> {
    match path {
        Some(p) => {
            let f = std::fs::File::create(p)?;
            write_to(session, format, BufWriter::new(f))
        }
        None => {
            let stdout = io::stdout();
            write_to(session, format, BufWriter::new(stdout.lock()))
        }
    }
}

fn write_to<W: Write>(session: &Session, format: Format, mut w: W) -> Result<()> {
    match format {
        Format::Json => write_json(session, &mut w),
        Format::Csv => write_csv(session, &mut w),
        Format::Records => write_records(session, &mut w),
        Format::Bytes => write_bytes(session, &mut w),
    }
}

// ── JSON ──────────────────────────────────────────────────────────────────────

fn write_json<W: Write>(session: &Session, w: &mut W) -> Result<()> {
    // Emit only device + readings (not the raw debugging fields)
    let out = serde_json::json!({
        "device": session.device,
        "readings": session.readings,
    });
    writeln!(w, "{}", serde_json::to_string_pretty(&out)?)?;
    Ok(())
}

// ── CSV ───────────────────────────────────────────────────────────────────────

fn write_csv<W: Write>(session: &Session, w: &mut W) -> Result<()> {
    let mut wtr = csv::Writer::from_writer(w);
    wtr.write_record([
        "record_number",
        "timestamp",
        "analyte",
        "value",
        "units",
        "high",
        "low",
        "meal_marker",
    ])?;
    for r in &session.readings {
        wtr.write_record([
            r.record_number.to_string(),
            r.timestamp.clone(),
            r.analyte.clone(),
            r.value.to_string(),
            r.units.clone(),
            r.high.to_string(),
            r.low.to_string(),
            r.meal_marker.clone().unwrap_or_default(),
        ])?;
    }
    wtr.flush()?;
    Ok(())
}

// ── Raw ASTM records ──────────────────────────────────────────────────────────

fn write_records<W: Write>(session: &Session, w: &mut W) -> Result<()> {
    for record in &session.raw_records {
        writeln!(w, "{record}")?;
    }
    Ok(())
}

// ── Raw HID bytes ─────────────────────────────────────────────────────────────

fn write_bytes<W: Write>(session: &Session, w: &mut W) -> Result<()> {
    for (i, pkt) in session.raw_packets.iter().enumerate() {
        writeln!(w, "--- packet {i:04} {} ---", pkt.dir)?;
        hex_dump(&pkt.data, w)?;
    }
    Ok(())
}

/// Classic 16-bytes-per-row hex dump with ASCII sidebar.
fn hex_dump<W: Write>(data: &[u8], w: &mut W) -> Result<()> {
    use std::fmt::Write as _;

    for (row, chunk) in data.chunks(16).enumerate() {
        let mut hex = String::new();
        let mut ascii = String::new();
        for (i, &b) in chunk.iter().enumerate() {
            if i == 8 {
                hex.push(' ');
            }
            write!(hex, "{b:02x} ")?;
            ascii.push(if b.is_ascii_graphic() || b == b' ' {
                b as char
            } else {
                '.'
            });
        }
        writeln!(w, "{:04x}  {hex:<49} |{ascii}|", row * 16)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::hex_dump;

    fn dump(data: &[u8]) -> String {
        let mut buf = Vec::new();
        hex_dump(data, &mut buf).unwrap();
        String::from_utf8(buf).unwrap()
    }

    #[test]
    fn hex_dump_full_rows() {
        let data: Vec<u8> = (0..=63).collect();
        let expected = "\
0000  00 01 02 03 04 05 06 07  08 09 0a 0b 0c 0d 0e 0f  |................|
0010  10 11 12 13 14 15 16 17  18 19 1a 1b 1c 1d 1e 1f  |................|
0020  20 21 22 23 24 25 26 27  28 29 2a 2b 2c 2d 2e 2f  | !\"#$%&'()*+,-./|
0030  30 31 32 33 34 35 36 37  38 39 3a 3b 3c 3d 3e 3f  |0123456789:;<=>?|
";
        assert_eq!(dump(&data), expected);
    }

    #[test]
    fn hex_dump_short_final_row_pads_hex_column() {
        let data: Vec<u8> = (0..20).collect();
        let expected = "\
0000  00 01 02 03 04 05 06 07  08 09 0a 0b 0c 0d 0e 0f  |................|
0010  10 11 12 13                                       |....|
";
        assert_eq!(dump(&data), expected);
    }
}
