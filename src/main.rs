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
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    let api = hidapi::HidApi::new()?;

    if cli.list {
        protocol::list_devices(&api);
        return Ok(());
    }

    let device = protocol::open_device(&api)?;
    let session = protocol::fetch_all(&device, cli.progress)?;

    output::write(&session, &cli.format, cli.output.as_deref())
}
