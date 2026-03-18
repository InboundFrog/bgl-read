# Bayer / Ascensia Contour Next — Protocol Notes

## Supported models

| Model | USB VID | USB PID | Notes |
|---|---|---|---|
| Contour Next One | `0x1A79` | `0x7800` | Tested |
| Contour Next USB | `0x1A79` | `0x7440` | Untested |
| Contour Next | `0x1A79` | `0x7350` | Untested |
| Ascensia Contour Next | `0x1A79` | `0x7900` | Untested |
| Contour Next Link | `0x1A79` | `0x6220` | Untested |
| Contour Next Link 2.4 | `0x1A79` | `0x6230` | Untested |

The device presents as a USB HID device (same class as keyboards/mice), not a
CDC serial port or bulk-transfer device.

---

## Transport: HID packets

Communication uses 64-byte HID reports.

### Received packets (meter → host)

```
byte  0–2   device header (3 bytes, ignored)
byte  3     SIZE: number of valid data bytes following
byte  4..   ASTM data (SIZE bytes)
remainder   zero padding
```

### Sent packets (host → meter, via hidapi `write()`)

```
byte  0     report ID = 0x00  (required by hidapi)
byte  1–3   header (3 zero bytes)
byte  4     length of payload
byte  5..   payload bytes
remainder   zero padding to 65 bytes total
```

A packet is complete (no more HID reads needed) when any of the following is true:

- `SIZE < 60` (partial / last packet of a multi-packet frame)
- First data byte is a single-byte control character (ENQ, EOT, ACK, NAK)
- `data[SIZE - 5]` is `ETX` or `ETB` (end of ASTM frame — see below)

---

## ASTM framing

The meter speaks **ASTM E1381 Minimum Protocol** — a simple framed
request/response protocol originally designed for laboratory instruments.

### Frame structure

```
STX  seq  record_text  CR  ETX|ETB  CS1  CS2  CR  LF
```

| Field | Size | Description |
|---|---|---|
| `STX` | 1 byte | `0x02` — marks start of frame |
| `seq` | 1 byte | ASCII digit `'0'`–`'7'`, frame sequence number |
| `record_text` | variable | Pipe-delimited ASTM record (see below) |
| `CR` | 1 byte | `0x0D` |
| `ETX` or `ETB` | 1 byte | `0x03` end of final frame / `0x17` end of intermediate frame |
| `CS1 CS2` | 2 bytes | Checksum as two uppercase ASCII hex digits |
| `CR LF` | 2 bytes | `0x0D 0x0A` |

`ETB` is used when a logical record spans multiple physical frames (uncommon in
practice for these meters). `ETX` marks the final frame of a record.

### Checksum

Sum of all bytes from `seq` through `ETX`/`ETB` (inclusive), modulo 256,
formatted as two uppercase hex digits:

```
checksum = format!("{:02X}", bytes[seq..=frame_end].iter().sum::<u32>() % 256)
```

### Single-byte control messages

Some messages are a single control byte with no framing:

| Byte | Hex | Meaning |
|---|---|---|
| `ENQ` | `0x05` | Meter signalling it is ready to send data |
| `EOT` | `0x04` | End of transmission |
| `ACK` | `0x06` | Acknowledge |
| `NAK` | `0x15` | Not acknowledge — request retransmit |

---

## Session flow

The host drives the session by sending `ACK` after each record. The meter
responds with the next record. Retransmit is requested by sending `NAK` instead.

```
host → meter   ACK               (wake / signal readiness)
meter → host   ENQ               (meter announcing it has data)
host → meter   ACK
meter → host   [frame] H record  (session header)
host → meter   ACK
meter → host   [frame] P record  (patient record, typically empty)
host → meter   ACK
meter → host   [frame] R record  (first glucose reading)
host → meter   ACK
  … repeats for each reading …
meter → host   [frame] L record  (terminator)
host → meter   ACK
meter → host   EOT
```

**Contour Next One quirk:** this model does not send an `ACK` in response to the
host's initial `ACK`, and does not expect an `EOT` command before the `NAK`
phase used for date/time setting. Other models differ — see the original
[Tidepool driver source][driver] for per-model branches.

[driver]: https://github.com/tidepool-org/uploader/blob/master/lib/drivers/bayer/bayerContourNext.js

---

## Record types

All records use pipe (`|`) as a field delimiter. The first field is always the
record type character.

### H — Header

Sent once at the start of a session. Contains device identification and
configuration.

```
H|\^&||<mid>|<device>|<config>|<nrecs>|||||P|1|<timestamp>
```

Fields of interest (0-indexed):

| Index | Example | Description |
|---|---|---|
| 4 | `Contour7800^FW^7830H5001733^` | `^`-delimited device info |
| 4[0] | `Contour7800` | Model code (maps to human name via lookup table) |
| 4[2] | `7830H5001733` | Raw serial — strip leading digits + one separator to get `5001733` |
| 5 | `A=1^U=0^V=20600^…` | `^`-delimited config key=value pairs |
| 5[U] | `0` | Units: `0` = mg/dL, `1` = mmol/L |
| 5[V] | `20600` | Thresholds: first 2 digits = low (20), next 3 = high (600), in display units |
| 6 | `150` | Number of records that follow |
| 13 | `202411031245` | Device clock at session start (`YYYYMMDDHHmm` or `YYYYMMDDHHmmss`) |

### P — Patient

Sent after the header. Typically empty; can be ignored.

### R — Result (glucose reading)

One per stored reading, in chronological order.

```
R|<nrec>|^^^Glucose|<value>|<units>^<method>||<markers>||<timestamp>
```

| Field | Index | Example | Description |
|---|---|---|---|
| Record seq | 1 | `3` | Position in this session (not a persistent ID) |
| Value | 3 | `5.4` or `93` | Glucose value (float for mmol/L, integer for mg/dL) |
| Units | 4 | `mmol/L^P` | Units string before `^`; method after (`P`=plasma, `B`=whole blood, `C`=capillary) |
| Markers | 6 | `A/M0/T1` | See below |
| Timestamp | 8 | `202411031245` | 12-digit `YYYYMMDDHHmm`, device local time |

#### Marker field

The marker field (index 6) is a slash-separated string. The leading character
encodes the meal mark:

| Char | Meaning |
|---|---|
| `B` | Pre-meal |
| `A` | Post-meal |
| `D` | Logbook |
| `>` | Above high threshold |
| `<` | Below low threshold |
| `C` | Control solution test (discard — not a patient reading) |

### L — Terminator

Sent after all R records. Signals that the data set is complete and valid. If
the session ends without an L record, the data should be considered incomplete.

---

## Device time

The meter has no concept of timezone. All timestamps are in the user's local
time as set on the device. The host must apply a timezone offset if UTC is
needed. `bgl-read` preserves timestamps as-is (`YYYY-MM-DDTHH:MM:SS` local).

---

## Model code lookup

The model string in the H record (e.g. `Contour7800`) maps to a human-readable
name:

| Code | Name |
|---|---|
| `Contour7800` | Contour Next One |
| `Contour7410` | Contour Next USB |
| `Contour7350` | Contour Next |
| `Contour7900` | Ascensia Contour Next |
| `Contour6200` | Contour Next Link |
| `Contour6210` | Contour Next Link 2.4 |
| `Contour7390` | Contour USB |

The prefix may be `Bayer` instead of `Contour` on older firmware.
