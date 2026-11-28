# Bayer / Ascensia Contour Next — Protocol Notes

## Supported models

| Model                 | USB VID  | USB PID  | Notes    |
|-----------------------|----------|----------|----------|
| Contour Next One      | `0x1A79` | `0x7800` | Tested   |
| Contour Next USB      | `0x1A79` | `0x7440` | Untested |
| Contour Next          | `0x1A79` | `0x7350` | Untested |
| Ascensia Contour Next | `0x1A79` | `0x7900` | Untested |
| Contour Next Link     | `0x1A79` | `0x6220` | Untested |
| Contour Next Link 2.4 | `0x1A79` | `0x6230` | Untested |

The device shows up as a USB HID device. This is the same class as keyboards
and mice. It is not a CDC serial port or a bulk-transfer device.

---

## Transport: HID packets

The meter and host exchange 64-byte HID reports.

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

A packet is complete when any of the following is true. The host then needs no
more HID reads.

- `SIZE < 60` (the last packet of a multi-packet frame)
- First data byte is a single-byte control character (ENQ, EOT, ACK, NAK)
- `data[SIZE - 5]` is `ETX` or `ETB` (the end of an ASTM frame; see below)

---

## ASTM framing

The meter speaks **ASTM E1381 Minimum Protocol**. This is a simple framed
request/response protocol. It was first made for laboratory instruments.

### Frame structure

```
STX  seq  record_text  CR  ETX|ETB  CS1  CS2  CR  LF
```

| Field          | Size     | Description                                                    |
|----------------|----------|----------------------------------------------------------------|
| `STX`          | 1 byte   | `0x02`, marks the start of a frame                             |
| `seq`          | 1 byte   | ASCII digit `'0'`–`'7'`, frame sequence number                 |
| `record_text`  | variable | Pipe-delimited ASTM record (see below)                         |
| `CR`           | 1 byte   | `0x0D`                                                         |
| `ETX` or `ETB` | 1 byte   | `0x03` ends the final frame; `0x17` ends an intermediate frame |
| `CS1 CS2`      | 2 bytes  | Checksum as two uppercase ASCII hex digits                     |
| `CR LF`        | 2 bytes  | `0x0D 0x0A`                                                    |

The meter uses `ETB` when one record needs several physical frames. This is
rare for these meters. `ETX` marks the final frame of a record.

The frame number goes up by one for each new frame and wraps from 7 to 0. A
frame that is sent again keeps its number. bgl-read drops a frame that repeats
the last one, and warns when the number jumps.

### Checksum

Add all bytes from `seq` through `ETX`/`ETB` (inclusive). Take the sum modulo
256. Write it as two uppercase hex digits:

```
checksum = format!("{:02X}", bytes[seq..=frame_end].iter().sum::<u32>() % 256)
```

### Single-byte control messages

Some messages are one control byte with no framing:

| Byte  | Hex    | Meaning                                       |
|-------|--------|-----------------------------------------------|
| `ENQ` | `0x05` | The meter is ready to send data               |
| `EOT` | `0x04` | End of transmission                           |
| `ACK` | `0x06` | Acknowledge                                   |
| `NAK` | `0x15` | Not acknowledge; asks the meter to send again |

---

## Session flow

The host sends `ACK` after each record. The meter then sends the next record.
To ask for a retransmit, the host sends `NAK` instead.

```
host → meter   ACK               (wakes the meter and shows the host is ready)
meter → host   ENQ               (the meter says it has data)
host → meter   ACK
meter → host   [frame] H record  (session header)
host → meter   ACK
meter → host   [frame] P record  (patient record, typically empty)
host → meter   ACK
meter → host   [frame] R record  (first glucose reading)
host → meter   ACK
  … repeats for each reading …
meter → host   [frame] L record  (terminator)
host → meter   ACK               (the meter does not reply)
host → meter   NAK
meter → host   EOT
```

A meter can send `EOT` and `ENQ` together in one packet when a new session
starts right after an old one. `bgl-read` treats that packet as `ENQ`.

**Ending the session:** after the L record, the host sends `ACK`. The
Contour Next One does not reply to this `ACK`, so `bgl-read` does not wait
for a reply. It then sends `NAK`, and the meter replies with `EOT`. `bgl-read`
waits a short time for that `EOT` and ignores a failure, because it has
already read all the readings.

This follows the reference [Tidepool driver][driver]. For most models, that
driver sends `EOT` between this `ACK` and the `NAK`. It does not do this for
the Contour Next One, Contour Plus One, Contour Plus Blue and Contour7900,
which do not expect `EOT` here. In that driver, this exchange is the first
part of the step that sets the meter clock, and it runs only when the clock is
wrong. `bgl-read` never sends `EOT` and never sets the clock. It uses the
`ACK` and `NAK` only to end the session. This is untested on the models that
expect `EOT`.

[driver]: https://github.com/tidepool-org/uploader/blob/master/lib/drivers/bayer/bayerContourNext.js

---

## Record types

All records use the pipe (`|`) as the field delimiter. The first field is
always the record type character.

### H — Header

The meter sends this once at the start of a session. It has the device
identification and configuration.

```
H|\^&||<mid>|<device>|<config>|<nrecs>|||||P|1|<timestamp>
```

These fields matter most (0-indexed):

| Index | Example                        | Description                                                                                                        |
|-------|--------------------------------|--------------------------------------------------------------------------------------------------------------------|
| 4     | `Contour7800^FW^7830H5001733^` | Device info, delimited by `^`                                                                                      |
| 4[0]  | `Contour7800`                  | Model code (`bgl-read` reports it as received; see the table below)                                                |
| 4[2]  | `7830H5001733`                 | Raw serial. Remove the leading digits and one separator to get `5001733`                                           |
| 5     | `A=1^U=0^V=20600^…`            | Config key=value pairs, delimited by `^`                                                                           |
| 5[U]  | `0`                            | Units: `0` = mg/dL, `1` = mmol/L                                                                                   |
| 5[V]  | `20600`                        | Thresholds. The first 2 digits are the low value (20). The next 3 are the high value (600). Both use display units |
| 6     | `150`                          | Number of records that follow                                                                                      |
| 13    | `202411031245`                 | Device clock at session start (`YYYYMMDDHHmm` or `YYYYMMDDHHmmss`)                                                 |

### P — Patient

The meter sends this after the header. It is usually empty. You can ignore it.

### R — Result (glucose reading)

The meter sends one for each stored reading, in time order.

```
R|<nrec>|^^^Glucose|<value>|<units>^<method>||<markers>||<timestamp>
```

| Field      | Index | Example        | Description                                                                           |
|------------|-------|----------------|---------------------------------------------------------------------------------------|
| Record seq | 1     | `3`            | Position in this session (not a lasting ID)                                           |
| Value      | 3     | `5.4` or `93`  | Glucose value (decimal for mmol/L, whole number for mg/dL)                            |
| Units      | 4     | `mmol/L^P`     | Units string before `^`; method after it (`P`=plasma, `B`=whole blood, `C`=capillary) |
| Markers    | 6     | `A/M0/T1`      | See below                                                                             |
| Timestamp  | 8     | `202411031245` | 12 digits `YYYYMMDDHHmm` or 14 digits `YYYYMMDDHHmmss`, device local time             |

#### Marker field

The marker field (index 6) is a string split by slashes. The first character
is the meal mark:

| Char | Meaning   |
|------|-----------|
| `B`  | Pre-meal  |
| `A`  | Post-meal |
| `D`  | Logbook.  |

`bgl-read` looks for these flags anywhere in the field:

| Char | Meaning                                                 |
|------|---------------------------------------------------------|
| `>`  | Above high threshold                                    |
| `<`  | Below low threshold                                     |
| `C`  | Control solution test (discard — not a patient reading) |

### L — Terminator

The meter sends this after all R records. It shows that the data set is
complete and valid. If the session ends without an L record, treat the data as
incomplete.

---

## Device time

The meter does not know about timezones. All timestamps use the local time set
on the device. If you need UTC, the host must apply a timezone offset.
`bgl-read` keeps timestamps as they are (`YYYY-MM-DDTHH:MM:SS` local).

---

## Model code lookup

The model string in the H record (e.g. `Contour7800`) identifies the model.
`bgl-read` has no lookup for it and reports the code as received. This table is
for reference only:

| Code          | Name                  |
|---------------|-----------------------|
| `Contour7800` | Contour Next One      |
| `Contour7410` | Contour Next USB      |
| `Contour7350` | Contour Next          |
| `Contour7900` | Ascensia Contour Next |
| `Contour6200` | Contour Next Link     |
| `Contour6210` | Contour Next Link 2.4 |
| `Contour7390` | Contour USB           |

Older firmware may use the prefix `Bayer` instead of `Contour`.

---

## V= threshold field — units detail

The `V` value holds thresholds in the device's *own display units*:

- **mg/dL meters (`U=0`):** the values are whole numbers.
  `V=20600` → low = 20 mg/dL, high = 600 mg/dL.
- **mmol/L meters (`U=1`):** the values are tenths of mmol/L.
  `V=06333` → low = 0.6 mmol/L, high = 33.3 mmol/L.

`bgl-read` reports `low_threshold` and `high_threshold` in the meter's own
units, the same as the readings, and does not convert them. The
`threshold_units` field says which: `"mg/dL"` or `"mmol/L"`. Both are always
decimal numbers in JSON (`20.0`, not `20`).

If `V=` is missing or cannot be read, `bgl-read` falls back to the meter's
published range in the units `U=` gives: `20.0`/`600.0` with `"mg/dL"`, or
`0.6`/`33.3` with `"mmol/L"`. Only one field of a `V=` pair parsing counts as
unreadable, so a real low is never paired with a guessed high. With no `U=`
either, the mg/dL fallback applies.

Despite their names, these values are not alert thresholds. They are the full
measurable range of the device. The user cannot change them. The meter flags
readings outside the range with `>` or `<` in the marker field.

---

## Observed behaviour — Contour Next One (PID `0x7800`, firmware `02.03`)

This comes from a USB capture (2026-03-18):

- Units: **mmol/L** (`U=1`)
- Threshold config: `V=06333` (0.6–33.3 mmol/L, which is almost the full range)
- All frames terminated with **ETB** (`0x17`), never ETX
- The **H record spans 3 HID packets** (about 143 bytes in total). Each P and R
  record fits in one packet
- Serial string format: `7802H7001396` → strip `7802` + `H` → serial `7001396`
- The marker field is almost always `T0/M0` (no context). `A/T0/M0` (post-meal)
  appears now and then
- The capture has no `>` or `<` flags in 800 readings. This fits the wide
  threshold range
- The H record timestamp has **14 digits** (`YYYYMMDDHHmmss`). R record
  timestamps also have 14 digits for this firmware
- The H record has a **trailing empty field** (15 fields, not 14). The last `|`
  makes an empty 15th element. You can ignore it
