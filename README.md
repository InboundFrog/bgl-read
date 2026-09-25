# bgl-read

## Note

This is based on the [Sensotrend/sensotrend-uploader](https://github.com/Sensotrend/sensotrend-uploader/).

I just needed to be able to read data from a Contour Next One (older USB model)
to grab the BGL data for someone else.

[Claude](https://claude.com/product/claude-code) did all the hard yards
figuring out how to convert the small portion I needed into Rust.

## Overview

Reads blood glucose readings from a Bayer/Ascensia Contour Next USB meter and
outputs them as JSON or CSV.

Tested against the **Contour Next One** (USB model). Other Contour Next variants
that use the same ASTM-over-HID protocol should also work — see
[`docs/devices/contour-next.md`](docs/devices/contour-next.md) for protocol
details.

---

## Building

```sh
cargo build --release
```

The binary ends up at `target/release/bgl-read`.

On macOS you may need to grant input-device access the first time you run it;
the OS will prompt you.

---

## Running

Plug the meter in via USB, then:

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

Every run that talks to the meter is a full transfer of up to 800 records, so
producing four formats used to mean four transfers. `--format binary` writes
the raw HID packet stream to a file, and `--from-bytes` replays that file
through the same framing and parsing code the live read uses — so a single
transfer can produce every format, the `bytes` hex dump included.

```sh
bgl-read --format binary --progress --output session.bin
bgl-read --from-bytes session.bin --format records --output session.txt
bgl-read --from-bytes session.bin --format csv     --output session.csv
bgl-read --from-bytes session.bin --format json    --output session.json
bgl-read --from-bytes session.bin --format bytes   --output session.hex
```

`bin/capture-bgl <prefix>` does exactly this and leaves the files in
`captures/<date>/`.

`--from-records FILE` is the narrower version: it re-parses a saved `records`
dump, which is enough for `csv` and `json` but cannot reproduce `bytes` or
`binary`, because the text dump does not carry the HID traffic. Both replay
flags are offline-only, so neither accepts `--progress`.

Damaged input is reported rather than quietly producing a short reading list:
`--from-bytes` rejects a truncated or foreign file outright, and
`--from-records` warns on stderr with a count of the lines it could not parse.

### Output formats

Formats are chosen with `--format`, and `--output FILE` writes to a file
instead of stdout. `binary` refuses to write to a terminal — give it `--output`
or pipe it somewhere.

| `--format` | Description                                                            |
|------------|------------------------------------------------------------------------|
| `json`     | Device info and readings as pretty-printed JSON                        |
| `csv`      | One reading per row, header included                                   |
| `records`  | Raw ASTM record text as received from the meter (useful for debugging) |
| `bytes`    | Hex dump of every HID packet exchanged (TX and RX)                     |
| `binary`   | Compact binary packet capture, replayable with `--from-bytes`          |

Timestamps are in the meter's own local time (no timezone attached — the device
has no concept of timezone).

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

The tool opens the first matching device it finds. Run `--list` to see what is
connected.

---

## Notes

- The meter stores the **800 most recent readings** in a circular buffer —
  older readings are silently overwritten. Read regularly if you need a
  complete history.
- **Control solution** readings (used to verify the meter) are detected via the
  annotation field and excluded from output.
- `--format records` output is the **raw meter transcript**: the header (`H`)
  line includes the meter's **password** and serial number, and the dump
  contains every reading. The `bytes` and `binary` captures hold that same
  transcript verbatim. Redact these before sharing dumps publicly (e.g. when
  filing issues).
