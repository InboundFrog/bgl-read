mod output;
mod protocol;

use anyhow::{Context, Result};
use clap::Parser;
use output::Format;
use std::io::{self, IsTerminal};
use std::path::{Path, PathBuf};

#[derive(Parser, Debug)]
#[command(
    name = "bgl-read",
    version,
    about = "Read blood glucose data from a Contour Next USB meter"
)]
struct Cli {
    #[arg(short, long, value_enum, default_value = "json")]
    format: Format,

    /// Write output to FILE instead of stdout
    #[arg(short, long, value_name = "FILE")]
    output: Option<PathBuf>,

    /// List connected Contour devices and exit
    #[arg(short, long, conflicts_with_all = ["format", "output"])]
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
        let packets = protocol::decode_packets(
            &std::fs::read(path).with_context(|| format!("reading {}", path.display()))?,
        )?;
        // A packet dump needs no session. Skipping the rebuild lets a person
        // dump an incomplete capture, which is when a dump helps most.
        let session = if matches!(cli.format, Format::Bytes | Format::Binary) {
            protocol::Session {
                raw_packets: packets,
                ..Default::default()
            }
        } else {
            protocol::session_from_packets(packets)?
        };
        return output::write(&session, cli.format, cli.output.as_deref());
    }

    if let Some(path) = cli.from_records.as_deref() {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
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
                    raw_packets: e.packets,
                    ..Default::default()
                };
                // Dump in whichever capture format was asked for, so a
                // `--format binary` failure still leaves a replayable file.
                let dump = match cli.format {
                    Format::Binary => Format::Binary,
                    _ => Format::Bytes,
                };
                // Do not let a dump failure hide the session error below.
                // A dump in a different format must not overwrite the file the
                // user asked for, so it goes beside it.
                let dump_path = match (&cli.format, cli.output.as_deref()) {
                    (Format::Binary, path) => path.map(Path::to_path_buf),
                    (_, path) => path.map(dump_path),
                };
                match output::write(&partial, dump, dump_path.as_deref()) {
                    Ok(()) => {
                        if let Some(p) = &dump_path {
                            eprintln!("Packet dump written to {}", p.display());
                        }
                    }
                    Err(dump_err) => {
                        eprintln!("warning: could not write the packet dump: {dump_err}");
                    }
                }
            }
            return Err(e.error);
        }
    };

    output::write(&session, cli.format, cli.output.as_deref())
}

/// `readings.csv` -> `readings.csv.dump`: append, never replace the extension.
fn dump_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".dump");
    name.into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dump_path_appends_to_the_extension() {
        assert_eq!(
            dump_path(Path::new("readings.csv")),
            PathBuf::from("readings.csv.dump")
        );
    }

    #[test]
    fn dump_path_without_an_extension() {
        assert_eq!(
            dump_path(Path::new("readings")),
            PathBuf::from("readings.dump")
        );
    }

    #[test]
    fn dump_path_keeps_the_directory() {
        assert_eq!(
            dump_path(Path::new("out/dir/readings.csv")),
            PathBuf::from("out/dir/readings.csv.dump")
        );
    }
}
