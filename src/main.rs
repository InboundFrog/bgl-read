mod output;
mod protocol;

use anyhow::Result;
use clap::Parser;
use output::Format;
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
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    if cli.list {
        let api = hidapi::HidApi::new()?;
        protocol::list_devices(&api);
        return Ok(());
    }

    if let Some(path) = cli.from_records.as_deref() {
        let text = std::fs::read_to_string(path)?;
        let session = protocol::session_from_records_text(&text);
        if matches!(cli.format, Format::Bytes) {
            eprintln!(
                "warning: --format bytes has no data when reading from a records file; output will be empty"
            );
        }
        return output::write(&session, cli.format, cli.output.as_deref());
    }

    let api = hidapi::HidApi::new()?;
    let device = protocol::open_device(&api)?;
    let capture = matches!(cli.format, Format::Bytes);
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
                output::write(&partial, Format::Bytes, cli.output.as_deref())?;
            }
            return Err(e.error);
        }
    };

    output::write(&session, cli.format, cli.output.as_deref())
}
