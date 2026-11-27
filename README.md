# bgl-read

## Note

This is based on the [Sensotrend/sensotrend-uploader](https://github.com/Sensotrend/sensotrend-uploader/).

I needed to read data from a Contour Next One (older USB model). I wanted the
BGL data for someone else.

[Claude](https://claude.com/product/claude-code) did most of the hard work. It
converted the small part I needed into Rust.

## Overview

This tool reads blood glucose readings from a Bayer/Ascensia Contour Next USB
meter. It outputs them in one of five formats. The default is JSON.

It is tested with the **Contour Next One** (USB model). Other Contour Next
variants use the same ASTM-over-HID protocol, so they should also work. See
[`docs/devices/contour-next.md`](docs/devices/contour-next.md) for protocol
details.

---

## Building

```sh
cargo build --release
```

The binary is at `target/release/bgl-read`.

On macOS, you may need to allow input-device access the first time you run it.
The OS asks you.

---

## Running

Plug the meter into a USB port. Then run:

```sh
# JSON (default) — device info + all readings
bgl-read

# CSV
bgl-read --format csv

# Show a live progress line while reading
bgl-read --progress

# Write to a file instead of stdout
bgl-read --format csv --output readings.csv

# List connected Contour devices
bgl-read --list

# Capture the raw packet stream, then convert it without the meter
bgl-read --format binary --progress --output session.bin
bgl-read --from-bytes session.bin --format csv --output readings.csv
```

### Reading the meter only once

Each run that talks to the meter transfers up to 800 records. Before, four
formats needed four transfers. Now `--format binary` writes the raw HID packet
stream to a file. `--from-bytes` reads that file with the same framing and
parsing code as a live read. So one transfer can make every format, including
the `bytes` hex dump.

```sh
bgl-read --format binary --progress --output session.bin
bgl-read --from-bytes session.bin --format records --output session.txt
bgl-read --from-bytes session.bin --format csv     --output session.csv
bgl-read --from-bytes session.bin --format json    --output session.json
bgl-read --from-bytes session.bin --format bytes   --output session.hex
```

`bin/capture-bgl <prefix>` does this for you. It saves the files in
`captures/<date>/`.

`--from-records FILE` does less. It parses a saved `records` dump again. This
is enough for `csv` and `json`. It cannot make `bytes` or `binary`, because the
text dump has no HID traffic. Both replay flags work offline only, so neither
accepts `--progress`.

The tool reports damaged input. It does not quietly give a short list of
readings. `--from-bytes` rejects a cut-off or foreign file. `--from-records`
warns on stderr and gives a count of the lines it could not parse.

### Output formats

Use `--format` to choose a format. Use `--output FILE` to write to a file
instead of stdout. `binary` refuses to write to a terminal. Give it `--output`
or pipe it to another program. On Unix, only your user can read or write
a new output file (mode 0600, or stricter if your umask is stricter). An
existing file keeps its mode.

| `--format` | Description                                                            |
|------------|------------------------------------------------------------------------|
| `json`     | Device info and readings as pretty-printed JSON                        |
| `csv`      | One reading per row, header included                                   |
| `records`  | Raw ASTM record text as received from the meter (useful for debugging) |
| `bytes`    | Hex dump of every HID packet exchanged (TX and RX)                     |
| `binary`   | Compact binary packet capture, replayable with `--from-bytes`          |

Timestamps use the meter's own local time. They have no timezone, because the
device does not know about timezones.

---

## Supported devices

All Contour Next devices share USB vendor ID `0x1A79` (Ascensia / Bayer).

| Product ID | Device                              |
|------------|-------------------------------------|
| `0x7800`   | Contour Next One (USB-only variant) |
| `0x7440`   | Contour Next USB                    |
| `0x7350`   | Contour Next                        |
| `0x7900`   | Ascensia Contour Next               |
| `0x6220`   | Contour Next Link                   |
| `0x6230`   | Contour Next Link 2.4               |

The tool opens the first matching device it finds. Run `--list` to see the
connected devices.

---

## Notes

- The meter stores the **800 most recent readings** in a circular buffer.
  New readings overwrite older ones without a warning. Read the meter often if
  you need the full history.
- **Control solution** readings (used to check the meter) are found in the
  annotation field. The tool leaves them out of the output and prints how
  many it left out.
- `--format records` output is the **raw meter transcript**. The header (`H`)
  line has the meter's **password** and serial number. The dump has every
  reading. The `bytes` and `binary` captures hold the same transcript without
  changes. Remove these details before you share a dump in public, for example
  in an issue.
