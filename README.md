# scurry

Share one mouse and keyboard across machines. The pointer crosses a screen edge
and lands on the next machine.

Unlike Synergy or Barrier, the targets run **nothing**. Each target sees a
plain mouse, because as far as it can tell that is what it has. That means it
works on a machine you cannot install software on, and onto a locked login
screen.

## How it works

```
[controller: macOS/Linux]
        |  USB CDC
   [ESP32-C3 dongle]
        |  BLE HID (HOGP), one bonded connection per target
    /       |       \
[Linux]  [Windows]  [Mac]
```

The controller captures local input and owns the virtual desktop layout. The
dongle presents itself to each target as an ordinary Bluetooth mouse. Crossing
a screen edge switches which bonded connection the reports are written to.

One dongle. No per-target hardware, no access point, no network to join, and
nothing to install anywhere.

## Status

Mouse and keyboard both work, across two targets, driven from macOS.

Keyboard was never merely more of the same — the controller sees OS keysyms,
the dongle must emit raw HID usage codes, and the target applies its own keymap
on top. Cmd against a PC-style target is a per-screen setting rather than a
guess.

| Piece | State |
|---|---|
| `scurry-proto` wire format | done, tested, `no_std` verified on riscv32imc |
| `scurry-ctl` layout engine | done, tested |
| `scurry-ctl` input capture | macOS event tap, working on device |
| dongle BLE HID firmware | working: mouse and keyboard reach bonded targets |
| wireless controller link | experimental; works, see [002](doc/experiments/002-wireless-control-link.md) |
| Linux and Windows capture | not started |

Everything that was not obviously going to work is written up in
[`doc/experiments/`](doc/experiments/), verdict first.

### Two ways to reach the dongle

The cable is the one that always works: USB CDC, 0.3ms round trip, and the only
path that can authorise a wireless controller.

The wireless link is experimental. The dongle carries the same protocol over a
custom GATT service, so it can sit on a charger between the machines instead of
hanging off one of them — at 16.8ms median and 30.4ms p90, and one fewer target,
because the controller takes one of the radio's four links. Authorising one
takes physical presence: three presses of the dongle's button, or a request over
the cable, which is refused if it arrives over the air.

### The spike

The architecture rests on one unverified claim: that a single ESP32-C3 can hold
**concurrent bonded HID connections** to several hosts at once.

**It can.** Two machines held simultaneous bonded HID connections on a C3, with
no disconnect when the second arrived. See `doc/experiments/001-concurrent-bonded-hosts.md`. Switching is
therefore a choice of `conn_id`, not a reconnect, and the dongle role needs no
S3.

Latency remains unmeasured, and that is the design's known weak point.

## Hardware

One ESP32-C3. That is the whole bill of materials.

The C3 is enough: a mouse report is 7 bytes at ~125Hz, so this is latency-bound
on the BLE connection interval, not throughput-bound on the CPU. A second core
would not help. The resource worth watching is RAM — NimBLE with several
connections on ~400KB is the tighter constraint.

### Why not USB HID

An earlier design put an ESP32-S3 in each target's USB port presenting as a USB
HID mouse. That is lower latency (~1-2ms versus 15-30ms) and works before the
OS boots, in a BIOS or a bootloader, which BLE cannot do.

It was dropped because it needs one board per target, and because BLE HID keeps
the property that actually matters — targets install nothing — at a fraction of
the hardware. If BLE latency turns out to be intolerable, this is the fallback.

Note that a C3 **cannot** serve that fallback. Its USB Serial/JTAG is
fixed-function: it enumerates as CDC-ACM and its descriptors are not
programmable. Presenting as a mouse needs a real USB OTG controller, which only
the S2, S3, and P4 have.

## Layout

- `crates/scurry-proto` — the wire format. Zero dependencies, `no_std`, shared
  by the controller and the firmware so the format cannot drift between them.
- `crates/scurry-ctl` — the controller daemon.
- `firmware/dongle` — ESP32-C3 firmware. A separate workspace: it cross-compiles
  with `build-std` and must not inherit the host workspace's dependencies.

## Build

```sh
nix develop
cargo test          # host crates

# Firmware. esp-idf-sys fetches and builds ESP-IDF into .embuild on first run,
# which takes a while. ldproxy is not packaged in nixpkgs.
cargo install ldproxy --root .cargo-tools
export PATH="$PWD/.cargo-tools/bin:$PATH"
cd firmware/dongle && cargo build
```
