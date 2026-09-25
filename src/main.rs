mod output;
mod protocol;

use anyhow::Result;
use clap::Parser;
use output::Format;
use std::io::{self, IsTerminal};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(
    name = "bgl-read",
    about = "Read blood glucose data from a Contour Next USB meter"
)]
struct Cli {
    #[arg(short, long, value_enum, default_value = "json")]
    format: Format,

    /// Write output to FILE instead of stdout
    #[arg(short, long, value_name = "FILE")]
    output: Option<PathBuf>,

    /// List connected Contour devices and exit
    #[arg(short, long)]
    list: bool,

    /// Show a live progress line on stderr while reading
    #[arg(short, long)]
    progress: bool,

    /// Parse a saved `--format records` file instead of reading from a meter
    #[arg(long, value_name = "FILE", conflicts_with_all = ["list", "progress"])]
    from_records: Option<PathBuf>,

    /// Replay a saved `--format binary` capture instead of reading from a meter.
    /// Unlike --from-records this reproduces every format, bytes included.
    #[arg(long, value_name = "FILE", conflicts_with_all = ["list", "progress", "from_records"])]
    from_bytes: Option<PathBuf>,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    if cli.list {
        let api = hidapi::HidApi::new()?;
        protocol::list_devices(&api);
        return Ok(());
    }

    // Binary captures are unreadable noise on a terminal, but piping them
    // onward (e.g. into xxd) is legitimate — only block the former.
    if matches!(cli.format, Format::Binary) && cli.output.is_none() && io::stdout().is_terminal() {
        anyhow::bail!("--format binary writes raw bytes; use --output FILE or pipe it somewhere");
    }

    if let Some(path) = cli.from_bytes.as_deref() {
        let packets = protocol::decode_packets(&std::fs::read(path)?)?;
        let session = protocol::session_from_packets(packets)?;
        return output::write(&session, cli.format, cli.output.as_deref());
    }

    if let Some(path) = cli.from_records.as_deref() {
        let text = std::fs::read_to_string(path)?;
        let session = protocol::session_from_records_text(&text);
        if matches!(cli.format, Format::Bytes | Format::Binary) {
            eprintln!(
                "warning: --format {} has no data when reading from a records file; output will be empty",
                if matches!(cli.format, Format::Bytes) {
                    "bytes"
                } else {
                    "binary"
                }
            );
        }
        return output::write(&session, cli.format, cli.output.as_deref());
    }

    let api = hidapi::HidApi::new()?;
    let device = protocol::open_device(&api)?;
    let capture = matches!(cli.format, Format::Bytes | Format::Binary);
    let session = match protocol::fetch_all(&device, cli.progress, capture) {
        Ok(session) => session,
        Err(e) => {
            // A failed session is exactly when a packet dump is most useful —
            // still write whatever was captured before bailing out.
            if !e.packets.is_empty() {
                eprintln!(
                    "Session failed; dumping the {} packets captured before the error",
                    e.packets.len()
                );
                let partial = protocol::Session {
                    device: protocol::DeviceInfo::default(),
                    readings: Vec::new(),
                    raw_records: Vec::new(),
                    raw_packets: e.packets,
                };
                // Dump in whichever capture format was asked for, so a
                // `--format binary` failure still leaves a replayable file.
                let dump = match cli.format {
                    Format::Binary => Format::Binary,
                    _ => Format::Bytes,
                };
                output::write(&partial, dump, cli.output.as_deref())?;
            }
            return Err(e.error);
        }
    };

    output::write(&session, cli.format, cli.output.as_deref())
}
