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
    /// Raw HID packet capture, replayable offline with `--from-bytes`
    Binary,
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
        Format::Binary => write_binary(session, &mut w),
    }?;
    // BufWriter ignores write errors when it is dropped. Flush here so
    // that a full disk or a closed pipe is reported.
    w.flush()?;
    Ok(())
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

/// Text from the meter or an input file can start with a character that a
/// spreadsheet runs as a formula. A leading quote makes it plain text.
fn csv_safe(cell: &str) -> String {
    if cell.starts_with(['=', '+', '-', '@', '\t', '\r']) {
        format!("'{cell}")
    } else {
        cell.to_string()
    }
}

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
            csv_safe(&r.timestamp),
            csv_safe(&r.analyte),
            r.value.to_string(),
            csv_safe(&r.units),
            r.high.to_string(),
            r.low.to_string(),
            csv_safe(r.meal_marker.as_deref().unwrap_or_default()),
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

// ── Binary packet capture ─────────────────────────────────────────────────────

fn write_binary<W: Write>(session: &Session, w: &mut W) -> Result<()> {
    w.write_all(&crate::protocol::encode_packets(&session.raw_packets))?;
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

    // Reused across rows: clear() keeps capacity, so the whole dump costs
    // two allocations regardless of length.
    let mut hex = String::with_capacity(49);
    let mut ascii = String::with_capacity(16);

    for (row, chunk) in data.chunks(16).enumerate() {
        hex.clear();
        ascii.clear();
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
    use super::{Format, hex_dump, write_to};
    use crate::protocol::{DeviceInfo, Session};
    use std::io::{self, BufWriter, Write};

    /// A writer that accepts no data and fails on flush.
    struct FailingWriter;

    impl Write for FailingWriter {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::Error::other("write failed"))
        }
        fn flush(&mut self) -> io::Result<()> {
            Err(io::Error::other("flush failed"))
        }
    }

    #[test]
    fn write_to_reports_flush_error() {
        let session = Session {
            device: DeviceInfo {
                model: String::new(),
                serial_number: String::new(),
                record_count: 0,
                device_time: String::new(),
                low_threshold: 0,
                high_threshold: 0,
            },
            readings: vec![],
            raw_records: vec!["H|test".to_string()],
            raw_packets: vec![],
        };
        // The record is smaller than the buffer, so only the flush can fail.
        let w = BufWriter::new(FailingWriter);
        assert!(write_to(&session, Format::Records, w).is_err());
    }

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

    #[test]
    fn csv_quotes_formula_cells() {
        use crate::protocol::{Reading, Session};
        let reading = |analyte: &str, units: &str, timestamp: &str, meal: Option<&str>| Reading {
            analyte: analyte.into(),
            units: units.into(),
            timestamp: timestamp.into(),
            record_number: 1,
            value: 5.5,
            high: false,
            low: false,
            meal_marker: meal.map(Into::into),
            is_control: false,
        };
        let render = |r: Reading| {
            let s = Session {
                readings: vec![r],
                ..Default::default()
            };
            let mut buf = Vec::new();
            super::write_to(&s, crate::Format::Csv, &mut buf).unwrap();
            String::from_utf8(buf).unwrap()
        };
        let bad = render(reading("@x", "=1+1", "=2020", Some("+m")));
        assert!(bad.contains(",'@x,"), "{bad}");
        assert!(bad.contains(",'=1+1,"), "{bad}");
        assert!(bad.contains(",'=2020,"), "{bad}");
        assert!(bad.contains(",'+m"), "{bad}");
        let ok = render(reading("Glucose", "mmol/L", "2020-01-01T09:00:00", None));
        assert!(ok.contains(",Glucose,"), "{ok}");
        assert!(ok.contains(",mmol/L,"), "{ok}");
        assert!(!ok.contains('\''), "{ok}");
        assert_eq!(super::csv_safe("-1"), "'-1");
        assert_eq!(super::csv_safe("\tx"), "'\tx");
        assert_eq!(super::csv_safe("\rx"), "'\rx");
        assert_eq!(super::csv_safe("normal"), "normal");
    }
}
