//! Integration coverage for the feature stream (US-UX-2): decoding a stream
//! delivered over a real local socket and injecting the frames onto the feature
//! bus — the exact seam `--input` drives — plus the schema-rejection guard on
//! the ingest path.

use std::io::{BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::thread;

use scia_core::stream::{
    Encoding, FeatureFrame, FrameStreamReader, StreamError, to_json_line, write_binary_frame,
    write_binary_header,
};
use scia_core::{Activity, FeatureSnapshot, STREAM_SCHEMA_VERSION, feature_bus};

/// A frame with a recognisable generation and spectrum, for asserting identity
/// after a socket round-trip.
fn frame(generation: u64) -> FeatureFrame {
    let mut snap = FeatureSnapshot {
        generation,
        sample_rate: 48_000,
        channels: 2,
        rms: 0.3,
        peak: 0.6,
        activity: Activity::Active,
        spectrum_len: 4,
        bands: [1.0, 0.9, 0.8],
        beat_confidence: 0.7,
        tempo_bpm: 128.0,
        ..FeatureSnapshot::default()
    };
    snap.spectrum[..4].copy_from_slice(&[0.1, 0.2, 0.3, 0.4]);
    FeatureFrame::from_snapshot(&snap)
}

/// Serve `frames` to the first client that connects, in `encoding`, then close.
fn serve(listener: TcpListener, encoding: Encoding, frames: Vec<FeatureFrame>) {
    thread::spawn(move || {
        let (mut sock, _) = listener.accept().expect("accept");
        match encoding {
            Encoding::Json => {
                for f in &frames {
                    let line = to_json_line(f).expect("encode");
                    writeln!(sock, "{line}").expect("write line");
                }
            }
            Encoding::Binary => {
                write_binary_header(&mut sock).expect("header");
                for f in &frames {
                    write_binary_frame(&mut sock, f).expect("write frame");
                }
            }
        }
        sock.flush().expect("flush");
        // Dropping `sock` closes the stream: the reader sees a clean EOF.
    });
}

/// Feed a recorded stream over a local socket and assert the decoded snapshots
/// arrive on the feature bus — the `--input` injection seam, end to end.
fn bus_injection_roundtrip(encoding: Encoding) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let sent: Vec<FeatureFrame> = (1..=8).map(frame).collect();
    serve(listener, encoding, sent.clone());

    let stream = TcpStream::connect(addr).expect("connect");
    let mut reader = FrameStreamReader::new(BufReader::new(stream)).expect("handshake");
    assert_eq!(reader.encoding(), encoding);

    // Inject decoded frames onto the bus exactly where the synthetic generator
    // would publish, then read them back off the reader half.
    let (mut writer, mut bus_reader) = feature_bus();
    let mut received = Vec::new();
    while let Some(f) = reader.next_frame().expect("decode") {
        let snap = f.to_snapshot();
        writer.publish(snap);
        // The bus is a triple buffer (latest wins); capture each generation as
        // it is published so nothing is coalesced away in this single-threaded
        // drain.
        received.push(*bus_reader.latest());
    }

    assert_eq!(received.len(), sent.len(), "every frame reached the bus");
    for (got, want) in received.iter().zip(&sent) {
        assert_eq!(got.generation, want.generation);
        assert_eq!(got.schema_version, STREAM_SCHEMA_VERSION);
        assert_eq!(got.spectrum_len, 4);
        assert_eq!(&got.spectrum[..4], &[0.1, 0.2, 0.3, 0.4]);
        assert_eq!(got.tempo_bpm, 128.0);
    }
}

#[test]
fn json_stream_injects_onto_the_bus() {
    bus_injection_roundtrip(Encoding::Json);
}

#[test]
fn binary_stream_injects_onto_the_bus() {
    bus_injection_roundtrip(Encoding::Binary);
}

/// A binary stream opening with a future schema version is rejected at the
/// handshake with a clear error, not a panic.
#[test]
fn future_binary_schema_is_rejected_at_handshake() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    thread::spawn(move || {
        let (mut sock, _) = listener.accept().expect("accept");
        // A well-formed header advertising an unsupported schema.
        let mut header = [0u8; 8];
        header[..4].copy_from_slice(b"SCIA");
        header[4..6].copy_from_slice(&(STREAM_SCHEMA_VERSION as u16 + 1).to_le_bytes());
        sock.write_all(&header).expect("write header");
        sock.flush().ok();
    });

    let stream = TcpStream::connect(addr).expect("connect");
    match FrameStreamReader::new(BufReader::new(stream)) {
        Err(StreamError::UnsupportedSchema { found, expected }) => {
            assert_eq!(found, STREAM_SCHEMA_VERSION + 1);
            assert_eq!(expected, STREAM_SCHEMA_VERSION);
        }
        Err(other) => panic!("expected UnsupportedSchema, got {other:?}"),
        Ok(_) => panic!("expected UnsupportedSchema, handshake unexpectedly succeeded"),
    }
}

/// A JSON stream carrying a future schema on a line is rejected when that line
/// is decoded.
#[test]
fn future_json_schema_is_rejected_on_decode() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    thread::spawn(move || {
        let (mut sock, _) = listener.accept().expect("accept");
        let mut f = frame(1);
        f.schema = STREAM_SCHEMA_VERSION + 7;
        let line = serde_json::to_string(&f).expect("encode");
        writeln!(sock, "{line}").expect("write");
        sock.flush().ok();
    });

    let stream = TcpStream::connect(addr).expect("connect");
    let mut reader = FrameStreamReader::new(BufReader::new(stream)).expect("handshake");
    match reader.next_frame() {
        Err(StreamError::UnsupportedSchema { found, .. }) => {
            assert_eq!(found, STREAM_SCHEMA_VERSION + 7);
        }
        other => panic!("expected UnsupportedSchema, got {other:?}"),
    }
}
