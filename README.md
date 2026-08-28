# scurry

Share one mouse and keyboard across machines. The pointer crosses a screen edge
and lands on the next machine.

Unlike Synergy or Barrier, the targets run **nothing**. Each target sees a
plain USB mouse, because that is literally what is plugged into it. That means
it works on a machine you cannot install software on, through a KVM, into a
BIOS screen, or onto a locked login window.

## How it works

```
[controller: macOS/Linux]
        |  USB CDC
   [ESP32-C3 dongle]
        |  ESP-NOW  (~1-2ms, no access point, no infrastructure)
    /       |       \
[S3]      [S3]      [S3]
  | USB HID  |         |
[Linux]  [Windows]   [Mac]
```

The controller captures local input and owns the virtual desktop layout. The
dongle is a radio bridge — the controller's OS cannot speak ESP-NOW, so the
dongle translates USB CDC to it. Each node presents as a USB HID mouse to the
machine it is plugged into.

ESP-NOW means there is no access point, no network to join, and no credentials
to provision. It works on a plane.

## Hardware

| Role | Chip | Why |
|---|---|---|
| Dongle | ESP32-C3 | USB Serial/JTAG is enough to reach the host. RISC-V, so stock Rust builds it. |
| Node | ESP32-S3 | Needs USB OTG to present arbitrary HID descriptors. |

**The node cannot be a C3.** The C3's USB Serial/JTAG is fixed-function: it
enumerates as CDC-ACM and its descriptors are not programmable. Presenting as a
mouse requires a real USB OTG controller, which only the S2, S3, and P4 have.
This is why the two roles use different chips.

The S3 is Xtensa, so node firmware needs the esp-rs rustc fork via `espup`. The
host and dongle toolchain stays on stock Rust from the flake.

## Status

Mouse first; keyboard comes after. Keyboard is not merely more of the same —
the controller sees OS keysyms, the node must emit raw HID usage codes, and the
target applies its own keymap on top.

| Piece | State |
|---|---|
| `scurry-proto` wire format | done, tested, `no_std` verified on riscv32imc |
| `scurry-ctl` layout engine | done, tested |
| `scurry-ctl` input capture | not started |
| dongle firmware | not started |
| node firmware | blocked on S3 hardware |

## Layout

- `crates/scurry-proto` — the wire format. Zero dependencies, `no_std`, shared
  by the controller and both firmwares so the format cannot drift between them.
- `crates/scurry-ctl` — the controller daemon.
- `firmware/` — a separate workspace; `no_std`, cross-compiled, and must not
  inherit host dev-dependencies.

## Build

```sh
nix develop
cargo test
```
