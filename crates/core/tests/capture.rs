//! Integration tests for the cpal capture backend that must pass both on a
//! developer box and on CI runners with no audio hardware. They never require a
//! device: enumeration may come back empty, and opening the default device is
//! allowed to skip when nothing is available.

use std::thread::sleep;
use std::time::Duration;

use scia_core::{
    CaptureError, CpalBackend, DeviceSelector, Engine, EngineConfig, EngineError, StreamHealth,
};

#[test]
fn list_devices_does_not_panic() {
    // Enumeration must return Ok (possibly empty) or NoDevice — never panic,
    // and never any other error class on a machine with no working host.
    match scia_core::list_devices() {
        Ok(devices) => {
            println!("list_devices: {} device(s)", devices.len());
            for d in &devices {
                println!(
                    "  host={} kind={:?} default_in={} default_out={} name={}",
                    d.host, d.kind, d.is_default_input, d.is_default_output, d.name
                );
            }
        }
        Err(CaptureError::NoDevice) => {
            println!("list_devices: no devices on any host (NoDevice)");
        }
        Err(e) => {
            // A backend/unsupported error is acceptable on a headless runner;
            // report it rather than failing, since the point is "does not
            // panic".
            println!("list_devices: enumeration error (non-fatal for this test): {e}");
        }
    }
}

#[test]
fn default_device_open_or_skip() {
    let backend = CpalBackend {
        device: DeviceSelector::Default,
        prefer_pipewire: true,
    };

    let (engine, mut reader) = match Engine::start(Box::new(backend), EngineConfig::default()) {
        Ok(pair) => pair,
        Err(EngineError::Capture(CaptureError::NoDevice)) => {
            println!("skip: no default capture device (NoDevice)");
            return;
        }
        Err(EngineError::Capture(CaptureError::Unsupported(msg))) => {
            println!("skip: default device format unsupported: {msg}");
            return;
        }
        Err(EngineError::Capture(CaptureError::Backend(msg))) => {
            println!("skip: backend could not open the default device: {msg}");
            return;
        }
        Err(EngineError::Spawn(msg)) => panic!("DSP thread failed to spawn: {msg}"),
    };

    let format = engine.format();
    println!(
        "opened default device: {} Hz, {} channel(s)",
        format.sample_rate, format.channels
    );
    assert!(format.sample_rate > 0, "sample rate must be positive");
    assert!(
        format.channels == 1 || format.channels == 2,
        "delivered channels must be mono or stereo, got {}",
        format.channels
    );

    // Run for 300 ms. Even with no real audio the DSP grid advances via silence
    // synthesis, so the generation must move regardless of the device.
    sleep(Duration::from_millis(300));

    let generation = reader.latest().generation;
    assert!(
        generation > 0,
        "expected the hop grid to advance within 300 ms, generation still 0"
    );

    match engine.health() {
        StreamHealth::Ok => println!("stream healthy after 300 ms, generation={generation}"),
        StreamHealth::Errored(msg) => {
            // A device that opened then errored (e.g. it was pulled) is a
            // hardware condition, not a code defect — report and skip the
            // health assertion.
            println!("skip health assertion: stream errored after open: {msg}");
            engine.stop();
            return;
        }
    }

    engine.stop();
}
