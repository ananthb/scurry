//! scurry dongle: USB CDC in from the controller, BLE HID out to the targets.
//!
//! # What this is testing
//!
//! Whether one ESP32-C3 can hold **concurrent bonded HID connections** to
//! several hosts at once. If it can, switching machines is just choosing which
//! connection handle to write a report on, and handoff is effectively instant.
//! If it cannot, every screen-edge crossing costs a BLE reconnect — 200ms to
//! 2s — which is too slow to be worth building on.
//!
//! The second question is latency. We request the tightest connection interval
//! the spec allows and log what the host actually grants; macOS is expected to
//! clamp to 15ms or worse.

use std::io::{BufRead, BufReader};
use std::sync::{Arc, Mutex};

use esp32_nimble::{
    enums::{AuthReq, SecurityIOCap},
    hid::*,
    BLEDevice, BLEHIDDevice,
};
use esp_idf_svc::hal::delay::FreeRtos;
use scurry_proto::{Frame, Payload, SeqGate};

/// Standard 5-button mouse with 16-bit relative axes, a wheel, and AC Pan.
///
/// 16-bit axes rather than the boot-protocol 8-bit ones: at 8 bits a fast flick
/// saturates at 127 units per report and the pointer visibly lags the hand.
const REPORT_MAP: &[u8] = &[
    0x05, 0x01, // Usage Page (Generic Desktop)
    0x09, 0x02, // Usage (Mouse)
    0xA1, 0x01, // Collection (Application)
    0x85, 0x01, //   Report ID (1)
    0x09, 0x01, //   Usage (Pointer)
    0xA1, 0x00, //   Collection (Physical)
    0x05, 0x09, //     Usage Page (Button)
    0x19, 0x01, //     Usage Minimum (Button 1)
    0x29, 0x05, //     Usage Maximum (Button 5)
    0x15, 0x00, //     Logical Minimum (0)
    0x25, 0x01, //     Logical Maximum (1)
    0x95, 0x05, //     Report Count (5)
    0x75, 0x01, //     Report Size (1)
    0x81, 0x02, //     Input (Data, Variable, Absolute)
    0x95, 0x01, //     Report Count (1)
    0x75, 0x03, //     Report Size (3)
    0x81, 0x03, //     Input (Constant) - pad to a byte
    0x05, 0x01, //     Usage Page (Generic Desktop)
    0x09, 0x30, //     Usage (X)
    0x09, 0x31, //     Usage (Y)
    0x16, 0x01, 0x80, //     Logical Minimum (-32767)
    0x26, 0xFF, 0x7F, //     Logical Maximum (32767)
    0x75, 0x10, //     Report Size (16)
    0x95, 0x02, //     Report Count (2)
    0x81, 0x06, //     Input (Data, Variable, Relative)
    0x09, 0x38, //     Usage (Wheel)
    0x15, 0x81, //     Logical Minimum (-127)
    0x25, 0x7F, //     Logical Maximum (127)
    0x75, 0x08, //     Report Size (8)
    0x95, 0x01, //     Report Count (1)
    0x81, 0x06, //     Input (Data, Variable, Relative)
    0x05, 0x0C, //     Usage Page (Consumer)
    0x0A, 0x38, 0x02, //     Usage (AC Pan)
    0x95, 0x01, //     Report Count (1)
    0x81, 0x06, //     Input (Data, Variable, Relative)
    0xC0, //   End Collection
    0xC0, // End Collection
];

const REPORT_ID: u8 = 1;

/// The 7 bytes the report map above describes.
fn encode_report(buttons: u8, dx: i16, dy: i16, wheel: i8, pan: i8) -> [u8; 7] {
    let mut r = [0u8; 7];
    r[0] = buttons & 0x1F;
    r[1..3].copy_from_slice(&dx.to_le_bytes());
    r[3..5].copy_from_slice(&dy.to_le_bytes());
    r[5] = wheel as u8;
    r[6] = pan as u8;
    r
}

fn main() -> anyhow::Result<()> {
    esp_idf_svc::sys::link_patches();
    esp_idf_svc::log::EspLogger::initialize_default();

    let device = BLEDevice::take();

    // Just-works pairing with bonding. A mouse has no display and no keypad,
    // so there is no channel to confirm a passkey over.
    device
        .security()
        .set_auth(AuthReq::all())
        .set_io_cap(SecurityIOCap::NoInputNoOutput);

    let server = device.get_server();

    // Keep advertising after a host connects. Without this the dongle would
    // stop being discoverable at one connection and could never reach a second
    // target -- which would answer the spike's question in the negative for
    // entirely uninteresting reasons.
    server.on_connect(|srv, desc| {
        log::info!("connected: {:?}", desc);
        log::info!("connection count now {}", srv.connected_count());
        if srv.connected_count() < CONFIG_MAX_HOSTS {
            log::info!("still room, resuming advertising");
            let _ = BLEDevice::take().get_advertising().lock().start();
        }
        // Ask for the fastest interval the spec permits: 7.5ms (6 * 1.25ms).
        // The host may refuse. Whatever it grants is logged on update.
        let _ = srv.update_conn_params(desc.conn_handle(), 6, 12, 0, 200);
    });

    server.on_disconnect(|desc, reason| {
        log::warn!("disconnected {:?}: {:?}", desc.address(), reason);
        let _ = BLEDevice::take().get_advertising().lock().start();
    });

    let mut hid = BLEHIDDevice::new(server);
    hid.manufacturer("scurry");
    hid.pnp(0x02, 0x1209, 0x5C71, 0x0100); // USB-IF vendor 0x1209 = pid.codes
    hid.hid_info(0x00, 0x01);
    hid.report_map(REPORT_MAP);
    hid.set_battery_level(100);

    let input = hid.input_report(REPORT_ID);

    let advertising = device.get_advertising();
    advertising
        .lock()
        .scan_response(false)
        .name("scurry")
        .appearance(0x03C2) // Generic HID -> Mouse
        .add_service_uuid(hid.hid_service().lock().uuid())
        .start()?;

    log::info!("advertising as a BLE HID mouse; waiting for hosts to bond");

    let gate = Arc::new(Mutex::new(SeqGate::new()));

    // The controller speaks scurry frames at us over USB Serial/JTAG, which
    // esp-idf wires to stdin.
    let stdin = BufReader::new(std::io::stdin());
    let mut buf = Vec::new();

    for line in stdin.split(b'\n') {
        let chunk = match line {
            Ok(c) => c,
            Err(e) => {
                log::error!("stdin: {e}");
                FreeRtos::delay_ms(10);
                continue;
            }
        };
        buf.extend_from_slice(&chunk);

        while buf.len() >= scurry_proto::FRAME_LEN {
            let frame: Vec<u8> = buf.drain(..scurry_proto::FRAME_LEN).collect();
            match Frame::decode(&frame) {
                Ok(f) => {
                    if !gate.lock().unwrap().accept(f.seq) {
                        continue; // reordered straggler
                    }
                    match f.payload {
                        Payload::Mouse(m) => {
                            let r = encode_report(m.buttons, m.dx, m.dy, m.wheel, m.pan);
                            input.lock().set_value(&r).notify();
                        }
                        Payload::Leave => {
                            // Release everything. A drag that crosses a screen
                            // boundary must not strand a held button here.
                            let r = encode_report(0, 0, 0, 0, 0);
                            input.lock().set_value(&r).notify();
                        }
                        Payload::Enter { edge, ratio } => {
                            log::info!("enter via {edge:?} at {ratio}");
                        }
                        Payload::Ping | Payload::Pong => {}
                    }
                }
                Err(e) => log::debug!("bad frame: {e:?}"),
            }
        }
    }

    Ok(())
}

/// Mirrors CONFIG_BT_NIMBLE_MAX_CONNECTIONS in sdkconfig.defaults.
const CONFIG_MAX_HOSTS: usize = 4;
