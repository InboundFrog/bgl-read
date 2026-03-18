# bgl-read

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
```

### Output formats

| `--format` | Description |
|---|---|
| `json` | Device info and readings as pretty-printed JSON |
| `csv` | One reading per row, header included |
| `records` | Raw ASTM record text as received from the meter (useful for debugging) |
| `bytes` | Hex dump of every HID packet exchanged (TX and RX) |

Timestamps are in the meter's own local time (no timezone attached — the device
has no concept of timezone).
