//! Drives the built binary. Nothing here touches USB: `--list` and live reads
//! are deliberately not run.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const FIXTURE: &str = "tests/fixtures/sample.txt";

fn run(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_bgl-read"))
        .args(args)
        .output()
        .unwrap()
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

/// A temp path unique to this test and process, so parallel tests cannot
/// collide. Any leftover file is removed before use; callers remove it after.
fn temp(name: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("bgl-read-cli-{name}-{}", std::process::id()));
    let _ = std::fs::remove_file(&p);
    p
}

fn s(p: &Path) -> &str {
    p.to_str().unwrap()
}

const CSV: &str = "\
record_number,timestamp,analyte,value,units,high,low,meal_marker
1,2020-01-01T09:00:00,Glucose,7.4,mmol/L,false,false,
2,2020-01-01T12:00:00,Glucose,6.3,mmol/L,false,false,
3,2020-01-01T15:00:00,Glucose,8.1,mmol/L,false,false,
4,2020-01-01T18:00:00,Glucose,7.9,mmol/L,false,false,post-meal
5,2020-01-01T21:00:00,Glucose,601,mg/dL,true,false,
6,2020-01-02T09:00:00,Glucose,19,mg/dL,false,true,
";

const DEVICE: &str = r#"  "device": {
    "device_time": "2020-01-01T09:00:00",
    "high_threshold": 33.3,
    "low_threshold": 0.6,
    "model": "ContourTest",
    "record_count": 6,
    "serial_number": "0000000",
    "threshold_units": "mmol/L"
  },"#;

// ── Packet capture builder ────────────────────────────────────────────────────
// Without a meter the binary cannot write a capture that holds packets, so the
// replay test frames the fixture as the meter would and writes it in the
// capture layout: 9-byte header, then a direction byte plus one 64-byte HID
// packet per entry (SIZE at offset 3, payload from offset 4).

const HID: usize = 64;
const MAX_PAYLOAD: usize = HID - 4;

fn header() -> Vec<u8> {
    let mut h = b"BGLCAP\0".to_vec();
    h.extend([1, HID as u8]); // version, packet size
    h
}

fn packet(dir: u8, data: &[u8]) -> Vec<u8> {
    assert!(data.len() <= MAX_PAYLOAD, "payload too big for one packet");
    let mut p = vec![0u8; HID];
    p[3] = data.len() as u8;
    p[4..4 + data.len()].copy_from_slice(data);
    let mut out = vec![dir];
    out.extend(p);
    out
}

fn capture_of_fixture() -> Vec<u8> {
    let mut out = header();
    out.extend(packet(0, &[0x06])); // TX ACK
    let text = std::fs::read_to_string(FIXTURE).unwrap();
    for (i, line) in text.lines().filter(|l| !l.trim().is_empty()).enumerate() {
        let mut checked = vec![b'0' + (i % 8) as u8];
        checked.extend(line.trim().bytes());
        checked.extend([0x0D, 0x03]); // CR ETX
        let sum: u32 = checked.iter().map(|&b| b as u32).sum();
        let mut msg = vec![0x02]; // STX
        msg.extend(&checked);
        msg.extend(format!("{:02X}", sum % 256).bytes());
        msg.extend([0x0D, 0x0A]); // CR LF
        for chunk in msg.chunks(MAX_PAYLOAD) {
            out.extend(packet(1, chunk));
        }
        out.extend(packet(0, &[0x06]));
    }
    out.extend(packet(1, &[0x04])); // EOT
    out
}

// ── --from-records ────────────────────────────────────────────────────────────

#[test]
fn from_records_json() {
    // No --format, so this pins the json default as well.
    let o = run(&["--from-records", FIXTURE]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    let out = stdout(&o);
    // Compared whole: a substring like "\"record_count\": 6" also matches 60,
    // so a regression reading the wrong H field would pass. Thresholds stay in
    // the meter's own units (mmol/L here) and are never converted. The reading
    // values are pinned exactly by from_records_csv.
    assert!(out.contains(DEVICE), "{out}");
    assert_eq!(out.matches("\"record_number\"").count(), 6);
}

#[test]
fn from_records_csv() {
    let o = run(&["--from-records", FIXTURE, "--format", "csv"]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    assert_eq!(stdout(&o), CSV);
}

#[test]
fn from_records_records_echoes_the_input() {
    let o = run(&["--from-records", FIXTURE, "--format", "records"]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    assert_eq!(stdout(&o), std::fs::read_to_string(FIXTURE).unwrap());
}

#[test]
fn from_records_warns_that_bytes_is_empty() {
    let o = run(&["--from-records", FIXTURE, "--format", "bytes"]);
    assert_eq!(o.status.code(), Some(0));
    assert!(
        stderr(&o).contains("--format bytes has no data"),
        "{}",
        stderr(&o)
    );
    assert!(o.stdout.is_empty());
}

#[test]
fn from_records_warns_that_binary_is_empty() {
    let out = temp("rec-binary");
    let o = run(&[
        "--from-records",
        FIXTURE,
        "--format",
        "binary",
        "--output",
        s(&out),
    ]);
    let written = std::fs::read(&out);
    let _ = std::fs::remove_file(&out);
    assert_eq!(o.status.code(), Some(0));
    assert!(
        stderr(&o).contains("--format binary has no data"),
        "{}",
        stderr(&o)
    );
    // Header only: no packets.
    assert_eq!(written.unwrap(), header());
}

#[test]
fn binary_to_piped_stdout_is_allowed() {
    // The terminal guard needs a TTY, which a test harness cannot provide, so
    // that branch is not covered; a pipe must pass through.
    let o = run(&["--from-records", FIXTURE, "--format", "binary"]);
    assert_eq!(o.status.code(), Some(0));
    assert_eq!(o.stdout, header());
}

// ── --from-bytes ──────────────────────────────────────────────────────────────

#[test]
fn from_bytes_json_matches_from_records() {
    let cap = temp("json");
    std::fs::write(&cap, capture_of_fixture()).unwrap();
    let a = run(&["--from-bytes", s(&cap), "--format", "json"]);
    let _ = std::fs::remove_file(&cap);
    let b = run(&["--from-records", FIXTURE, "--format", "json"]);
    assert_eq!(a.status.code(), Some(0), "{}", stderr(&a));
    assert_eq!(stdout(&a), stdout(&b));
    // Nothing else reads the frames the builder writes. Without this, a
    // capture with every sequence number set to '0' still passes, warning
    // "frame number jumped" to a stderr no test looks at.
    assert_eq!(
        stderr(&a),
        "Done: 6 readings from ContourTest (S/N 0000000)\n"
    );
}

#[test]
fn from_bytes_rejects_a_foreign_file() {
    // The fixture is a records file, so its first bytes are not the magic.
    let o = run(&["--from-bytes", FIXTURE, "--format", "csv"]);
    assert_eq!(o.status.code(), Some(1));
    assert!(stderr(&o).contains("bad magic"), "{}", stderr(&o));
}

#[test]
fn from_bytes_packet_dump_needs_no_complete_session() {
    // One packet, no records: csv fails on the empty session, but a bytes dump
    // skips the session rebuild and still dumps the packet.
    let cap = temp("dump");
    let mut capture = header();
    capture.extend(packet(0, &[0x06]));
    std::fs::write(&cap, capture).unwrap();
    let csv = run(&["--from-bytes", s(&cap), "--format", "csv"]);
    let dump = run(&["--from-bytes", s(&cap), "--format", "bytes"]);
    let _ = std::fs::remove_file(&cap);
    assert_eq!(csv.status.code(), Some(1));
    assert!(
        stderr(&csv).contains("Capture contains no records"),
        "{}",
        stderr(&csv)
    );
    assert_eq!(dump.status.code(), Some(0), "{}", stderr(&dump));
    assert!(
        stdout(&dump).contains("--- packet 0000 TX ---"),
        "{}",
        stdout(&dump)
    );
}

#[test]
fn missing_input_file_fails() {
    for flag in ["--from-bytes", "--from-records"] {
        let o = run(&[flag, "tests/fixtures/does-not-exist"]);
        assert_eq!(o.status.code(), Some(1), "{flag}");
        assert!(
            stderr(&o).contains("reading tests/fixtures/does-not-exist"),
            "{flag}: {}",
            stderr(&o)
        );
        assert!(stderr(&o).contains("No such file or directory"), "{flag}");
    }
}

#[test]
fn unwritable_output_names_the_path() {
    let out = temp("no-such-dir").join("x.csv");
    let o = run(&["--from-records", FIXTURE, "--output", s(&out)]);
    assert_eq!(o.status.code(), Some(1));
    assert!(
        stderr(&o).contains(&format!("writing {}", out.display())),
        "{}",
        stderr(&o)
    );
}

// ── Flag handling ─────────────────────────────────────────────────────────────

#[test]
fn clap_rejects_conflicting_flags() {
    let cases: &[&[&str]] = &[
        &["--from-records", FIXTURE, "--progress"],
        &["--from-bytes", FIXTURE, "--progress"],
        &["--from-bytes", FIXTURE, "--from-records", FIXTURE],
        &["--from-records", FIXTURE, "--list"],
        &["--from-bytes", FIXTURE, "--list"],
        &["--list", "--format", "csv"],
        &["--list", "--output", "x"],
    ];
    for args in cases {
        let o = run(args);
        assert_eq!(o.status.code(), Some(2), "{args:?}");
        assert!(
            stderr(&o).contains("cannot be used with"),
            "{args:?}: {}",
            stderr(&o)
        );
        assert!(o.stdout.is_empty(), "{args:?}");
    }
}

#[test]
fn output_writes_the_file_not_stdout() {
    let out = temp("output");
    let o = run(&[
        "--from-records",
        FIXTURE,
        "--format",
        "csv",
        "--output",
        s(&out),
    ]);
    let written = std::fs::read_to_string(&out);
    let _ = std::fs::remove_file(&out);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    assert!(o.stdout.is_empty());
    assert_eq!(written.unwrap(), CSV);
}

#[test]
fn version_prints_and_exits_zero() {
    let o = run(&["--version"]);
    assert_eq!(o.status.code(), Some(0));
    assert_eq!(
        stdout(&o),
        format!("bgl-read {}\n", env!("CARGO_PKG_VERSION"))
    );
}
