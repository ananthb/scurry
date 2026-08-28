# Spike: concurrent multi-host BLE HID on the ESP32-C3

**Verdict: it works.** One C3 holds simultaneous *bonded* HID connections to
multiple hosts. Switching machines is choosing a `conn_id`, not paying a BLE
reconnect. The architecture stands, and the dongle role needs no S3.

Run on an ESP32-C3 (QFN32) rev v0.4, ESP-IDF v5.5.2, Bluedroid.

## Evidence

```text
W (25209) HID_DEMO: scurry: conn_id=0, 1 host(s) connected
I (25829) HID_DEMO: remote BD_ADDR: 842f...
I (25829) HID_DEMO: pair status = success
I (25829) HID_DEMO: secure connection established.

W (96729) HID_DEMO: scurry: conn_id=1, 2 host(s) connected
I (97039) HID_DEMO: remote BD_ADDR: f068...
I (97039) HID_DEMO: pair status = success
I (97039) HID_DEMO: secure connection established.
```

Two distinct peer addresses, two distinct `conn_id`s, both bonded rather than
merely connected. **No disconnect event fired when the second host arrived** —
that is the load-bearing observation. The demo loop kept running for 40s after,
so the first link was not silently dead.

## What had to change to get here

Espressif's `ble_hid_device_demo` cannot do this as shipped. Three fixes, two of
which are latent upstream bugs that stay invisible while `HID_MAX_APPS` is 1:

| Change | Why |
|---|---|
| `HID_MAX_APPS` 1 → 4 | `hidd_clcb[]` was sized for a single connection. |
| Fix `hidd_clcb_dealloc` | It ignored `conn_id`, clearing the *first* slot unconditionally. With a real array, one host disconnecting wipes another's state. |
| Key the connection table on `remote_bda` | The disconnect event carries only the peer address; `conn_id` appears on connect but never on disconnect. |
| Keep advertising after connect | The demo stopped advertising at one host, so a second could never discover it. Without this the spike fails for a reason unrelated to BLE. |

The profile underneath was always built for this: `hidd_clcb_alloc` scans for a
free slot and every send function already takes a `conn_id`. Only the bound and
the teardown path were wrong.

## Resource cost

| | Used | Total | % |
|---|---|---|---|
| DRAM | 103,110 B | 321,296 B | 32% |

Bluedroid plus four connection slots leaves 218KB free. RAM was the expected
constraint on a 400KB part and it turned out to be comfortable.

## Not yet established

The spike proves connections coexist. It does **not** yet show:

- **Latency.** The granted connection interval is still unmeasured. This is the
  design's known weak point — macOS is expected to clamp to 15-30ms.
- **Per-connection routing.** The demo still sends to a single `hid_conn_id`.
  Writing a report to a *chosen* host is the next thing to prove.
- **Scale past two.** Configured for 4; two were tested.
- **Bond persistence across reboot.**
