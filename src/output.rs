use crate::protocol::{Dir, Session};
use crate::Format;
use anyhow::Result;
use std::io::{self, BufWriter, Write};

// ── Entry point ───────────────────────────────────────────────────────────────

pub fn write(session: &Session, format: &Format, path: Option<&str>) -> Result<()> {
    match path {
        Some(p) => {
            let f = std::fs::File::create(p)?;
            write_to(session, format, BufWriter::new(f))
        }
        None => write_to(session, format, BufWriter::new(io::stdout())),
    }
}

fn write_to<W: Write>(session: &Session, format: &Format, mut w: W) -> Result<()> {
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
        "glucose_value",
        "units",
        "high",
        "low",
        "meal_marker",
    ])?;
    for r in &session.readings {
        wtr.write_record([
            r.record_number.to_string(),
            r.timestamp.clone(),
            r.glucose_value.to_string(),
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
        let dir = match pkt.dir {
            Dir::Tx => "TX",
            Dir::Rx => "RX",
        };
        writeln!(w, "--- packet {i:04} {dir} ---")?;
        hex_dump(&pkt.data, w)?;
    }
    Ok(())
}

/// Classic 16-bytes-per-row hex dump with ASCII sidebar.
fn hex_dump<W: Write>(data: &[u8], w: &mut W) -> Result<()> {
    for (row, chunk) in data.chunks(16).enumerate() {
        // Offset
        write!(w, "{:04x}  ", row * 16)?;
        // Hex bytes
        for (i, b) in chunk.iter().enumerate() {
            if i == 8 { write!(w, " ")?; }
            write!(w, "{b:02x} ")?;
        }
        // Padding if last row is short
        let pad = 16 - chunk.len();
        for i in 0..pad {
            if chunk.len() + i == 8 { write!(w, " ")?; }
            write!(w, "   ")?;
        }
        // ASCII sidebar
        write!(w, " |")?;
        for &b in chunk {
            let c = if b.is_ascii_graphic() || b == b' ' { b as char } else { '.' };
            write!(w, "{c}")?;
        }
        writeln!(w, "|")?;
    }
    Ok(())
}
