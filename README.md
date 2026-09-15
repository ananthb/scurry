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
| 0.42" OLED status display | working: identity, links, pairing window, passkey |
| firmware updates over the link | working: verified over the cable, onto a live dongle |
| WS2812 status LED | written, not yet verified on hardware |
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
the cable, which is refused if it arrives over the air. On a board with the
screen fitted the window also shows a six-digit passkey the controller must
confirm, so the link is no longer merely encrypted but unauthenticated.

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

### Updating the dongle

The dongle is the one part of this that is not a file on somebody's laptop, and
until recently the only way to change it was a cable, a held BOOT button and a
tapped RST. That is awkward on the best board and worse on one deliberately
parked on a charger between two machines, which is where the wireless link was
built to let it sit.

So an image now goes over the same protocol the pointer uses, over whichever
transport is to hand:

```sh
scurry-ctl firmware          # what it runs, and what the latest release is
scurry-ctl flash             # install the latest release
scurry-ctl flash --file f.bin # install something you built
```

The tray has the same thing under **Firmware**, including dropping a `.bin`
onto the window.

CI publishes `scurry-dongle-esp32c3.bin` with every release and the clients
fetch it from there, so the normal path involves no files at all. Only the app
image is published: an update writes the inactive app slot and nothing else.

Three things keep it from being a way to brick the dongle:

- **Two app slots.** The image is written to the one that is not running, so a
  transfer that fails at 90% has damaged nothing.
- **The image is described before it is sent** — length and SHA-256 up front —
  so one that was never going to be accepted is refused before a flash erase
  and a minute of radio time have been spent on it.
- **The bootloader can undo it.** A freshly booted image is on probation and
  reverts on the next reset unless it stays up for twenty seconds. That matters
  precisely in the case that is otherwise unrecoverable: an update sent over the
  air, to a dongle nobody is standing next to, that boots into something unable
  to talk.

Writing firmware is held to the same standard as authorising a controller, and
for a sharper reason: a controller that can type is a live compromise, one that
can flash is a permanent one. So an image is accepted over the cable, or from a
controller that has already been authorised, and refused otherwise.

Note that a dongle running firmware older than this has a single `factory`
partition and nowhere to put a second image. It says so when asked, and needs
one cable flash to gain the two-slot layout — after which it can update itself.
NVS keeps its offset across that change, so bonds and the stored layout survive.

### The status LED

The board's second LED — the addressable one, not the power light — blinks
while the pairing window is open and sits solid green while at least one target
is connected. Dark otherwise. It answers the same question the screen does, for
a dongle parked where the screen cannot be read, or a board with no screen
soldered on.

### The optional screen

A 0.42" SSD1306 OLED, if the board has one soldered on -- the same firmware
runs on a board without it and simply logs one line and stays headless, because
losing the mouse over a missing screen would be a poor trade for a status
readout.

Fitted, it is 72x40: twelve characters by five lines, or six at double size.
Two screens alternate every fifteen seconds.

The first is the dongle's name. "Scurry" is on every board ever built and the
four characters after it are the whole answer to "which one is this", so the
prefix is set small and the id as large as fits -- at triple size four
characters span 69 of the 72 pixels available.

The second is the links, and only the links that exist. Drawing a fixed four
rows spent the panel's scarcest resource on its least information: on a desk
with one target, three of those rows said "----", which is not news. So the
rows are the connections and the text grows into whatever they leave. One or
two targets -- which is most desks -- puts the addresses up at double size,
legible across a room rather than at arm's length.

Everything is centred by pixel rather than by character cell. Cell centring can
only place a string on a multiple of six pixels, which left odd-length lines up
to three pixels off true: invisible alone, obvious the moment two of them are
stacked.

The button is counted rather than held, so it carries all of this by press
count, ordered by consequence: one flips between the two screens, two summons
the Bluetooth address, three opens the pairing window. A pairing window or an
update in progress preempts the rotation, because those are the two states
where somebody needs to be told something rather than shown it.

The screen earns its place twice over. The pairing window used to be invisible:
you pressed the button three times and then had to trust that something had
happened, because the only confirmation was a log line on a cable you may not
have been holding. And a device with no display cannot prove to a controller
that the controller is talking to it and not to something in the middle, which
is why the wireless link bonded Just Works. Both of those are now fixed, and
neither is fixable without hardware.

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

## Screen sizes are logical, not physical

A screen's rectangle is in the units its own pointer moves through, which is
rarely the panel's pixel count. A Retina Mac's cursor crosses 1512x982 points,
not 3024x1964 pixels; a Chromebook with a 2560x1600 panel running at 2x moves
through 1280x800.

Get it wrong and the symptom is oddly specific: the pointer hands over to the
next machine before reaching the edge of the screen, because it reached the edge
of the *layout* at the halfway mark. `scurry.toml.example` says where to read
the right numbers from.

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
