# Wire protocol

What the controller and the dongle say to each other. Version 3.

This is *not* the protocol the targets see — they see ordinary BLE HID reports
and have no idea scurry exists. This is the link between the machine you are
typing on and the dongle.

## Frame

An 8-byte header, little-endian, then a length-prefixed payload.

```text
0       magic 0x53
1       version
2       kind
3       flags (reserved, zero)
4..6    seq (u16)
6..8    payload length (u16)
8..     payload
```

A mouse update carries an 8-byte payload, so the hot path is exactly 16 bytes —
the length prefix costs nothing there, and lets a config message be arbitrarily
long. Payloads are capped at 512 bytes, which bounds the dongle's reassembly
buffer: it has no allocator to grow one.

## Kinds

| | | |
|---|---|---|
| `0x01` | `MOUSE` | controller → dongle. Raw pointer motion. |
| `0x02` | `KEY` | controller → dongle. Full keyboard state. |
| `0x04` | `PING` | either direction. |
| `0x05` | `PONG` | the answer to one. |
| `0x06` | `FOCUS` | dongle → controller. Which node holds the pointer. |
| `0x10` | `GET_CONFIG` | controller → dongle. |
| `0x11` | `CONFIG` | either direction. A full layout. |
| `0x12` | `SET_CONFIG` | controller → dongle. Replace and persist the layout. |
| `0x13` | `GET_STATUS` | controller → dongle. |
| `0x14` | `STATUS` | dongle → controller. Bonded targets and their links. |
| `0x15` | `ACK` | dongle → controller. The result of a request. |
| `0x16` | `GET_WIRELESS` | controller → dongle. |
| `0x17` | `WIRELESS` | dongle → controller. Authorised controllers, and which drives. |
| `0x18` | `SET_WIRELESS` | controller → dongle. Open the pairing window, or revoke. |
| `0x19` | `GET_FIRMWARE` | controller → dongle. |
| `0x1a` | `FIRMWARE` | dongle → controller. Version, and whether it can be updated. |
| `0x1b` | `GET_IDENTITY` | controller → dongle. |
| `0x1c` | `IDENTITY` | dongle → controller. Which dongle this is: name and address. |
| `0x20` | `OTA_BEGIN` | controller → dongle. Length and SHA-256 of an image. |
| `0x21` | `OTA_DATA` | controller → dongle. One chunk, offset first. |
| `0x22` | `OTA_END` | controller → dongle. Verify and stage the image. |
| `0x23` | `OTA_ABORT` | either direction. Abandon the update. |
| `0x24` | `OTA_STATUS` | dongle → controller. Bytes written, and the state. |

Control kinds start at `0x10` so a reader can tell the classes apart by
magnitude, and the split is enforced by a test: an overlap would route a config
message into the pointer path.

## Firmware updates stay inside version 3

The update kinds were added without bumping the version byte, and that is
deliberate rather than lazy. The dongle rejects any frame whose version is not
exactly its own, so a controller that bumped to 4 could not open a
conversation with the firmware it was trying to replace — the update path would
work on every dongle except the ones that needed it.

An older build answers `OTA_BEGIN` with `BAD_REQUEST`, which the controller
reports as "this firmware cannot be updated over the wire, use the cable".

The image is described before it is sent:

```text
OTA_BEGIN   0..4    image length (u32)
            4..36   SHA-256 of the image
```

The digest goes first so an image the dongle was never going to accept is
refused before a flash erase and thirty seconds of radio time have been spent
receiving it.

```text
OTA_DATA    0..4    byte offset (u32)
            4..     image bytes, at most 508
```

Every `OTA_DATA` is answered with an `OTA_STATUS`, so the reply is the flow
control as much as the progress:

```text
OTA_STATUS  0..4    bytes written so far (u32)
            4       state: 0 idle, 1 receiving, 2 verifying, 3 ready, 4 failed
```

Answering each chunk halves the theoretical throughput and is still the right
trade. Without it the controller outruns the dongle's flash writes and
overflows a reassembly buffer that has no allocator to grow. Over the cable the
round trip is 0.3ms and the whole image takes a few seconds; over the air it is
closer to a minute, which is a fine price for not having to find the cable.

The offset is explicit because the dongle writes to flash sequentially and
cannot seek. A chunk arriving out of order has to be refused, and it could not
be refused if nothing said where it belonged.

`OTA_END` carries no payload. The dongle checks the digest, stages the image,
acks, and then reboots into it.

## Updating over the air needs authorisation

`OTA_BEGIN` is refused with `NOT_PERMITTED` unless it arrives over the cable or
from a controller that has already been authorised. Writing firmware is a
persistent compromise where typing is only a live one, so it is held to at
least the standard that authorising a controller already is.

## The controller does not route

The layout lives on the dongle, so the controller sends *raw* pointer motion and
has no opinion about which machine it lands on. That is what lets the same
firmware run standalone, and it is why there is no node id in the header — an
earlier version had one, from when the controller decided.

It is also why `FOCUS` is required rather than informational. Since the dongle
owns the layout, the controller cannot otherwise tell whether to swallow local
input and hide its cursor: it has no idea where the pointer went.

## Buttons are absolute, motion is relative

Neither transport retransmits. Frames can be dropped.

If button presses and releases were separate events, a dropped release would
leave a button held down on the target with no way to recover — a stuck mouse
button on a machine you are not sitting at, which is the worst failure this
protocol could have. So every frame carries the **full button bitmask**, and the
next frame to arrive repairs the state. Keyboard is the same: the whole modifier
byte and the whole set of held keys, every time.

Motion is a delta, because the opposite tradeoff applies. A dropped motion frame
costs a few pixels and self-corrects the moment the mouse moves again.

This is also what makes it safe to **collapse a burst** of queued frames into
one when a link cannot keep up: summing deltas lands the pointer in the same
place, and the last button state is the truth.

## Focus is repeated, not announced once

`FOCUS` was originally sent only when the node changed. One lost frame then left
the controller feeding input to its own machine while the dongle routed the same
input to a target — both pointers moving, permanently, with nothing to repair
it.

It is absolute state, like buttons, and gets the same treatment: re-sent every
500ms of activity, so a lost announcement heals within half a second.

## Sequence numbers

Every frame carries one, so a receiver can drop reordered stragglers instead of
applying motion backwards. `SeqGate` compares by signed distance on the wrapped
circle, not by `>`; a naive comparison would stall the pointer for 32k frames
every time the counter wrapped.

A sustained run of rejections means the peer restarted rather than that frames
are arriving out of order, so the gate re-anchors after eight. Without that, a
restarted controller counting from zero would have every frame dropped as
ancient until it climbed back past the old value.

**One gate per source, not one per receiver.** Two controllers numbering
independently each look like ancient stragglers to the other, and the resync
logic thrashes instead of either working.

## Two transports, one framing

The controller reaches the dongle either over the **cable** (USB CDC) or over
the **radio** (a custom GATT service; see
[experiment 002](experiments/002-wireless-control-link.md)). Both carry exactly
this protocol.

Neither delivers messages — both deliver a byte stream. The cable interleaves
the firmware's log output with the protocol, and a BLE write is bounded by the
negotiated MTU, which no config payload fits inside. So both ends resynchronise
on the magic byte and reassemble, and **each source gets its own buffer**:
splicing two streams into one produces frames neither sender wrote.

Anything that is not a valid frame on the cable is log text, and is printed
rather than discarded — it is the only window into the firmware while the port
is held open.

## One driver at a time

Several controllers may be authorised, and the cable counts as one of them. Any
of them may query the dongle or push a layout at any time. Only one may move the
pointer.

The wheel is claimed by **sending input**, not by connecting or subscribing —
hosts reconnect bonded devices in the background, so a laptop waking in another
room would otherwise take control from the device in your hand. Another source
takes over once the current one has been quiet for 250ms.

`FOCUS` goes to *every* connected controller, not only the driver. One that is
idle may take the wheel at any moment, and would take it believing the pointer
is wherever it was when it last heard.

## Leaving a screen releases everything

When the pointer crosses off a machine, the dongle sends that machine a report
with no buttons held and no keys down, before the pointer arrives anywhere else.
Otherwise a drag across the boundary strands a held button, and a held modifier
is worse still: it changes what every later keystroke does over there.

## Proportional handoff

This happens inside the dongle's layout engine rather than on the wire, but it
is the reason handoff feels continuous between mismatched displays.

The crossing point is measured as a ratio along the edge being **left**, then
applied to the edge being entered. Leaving 30% of the way down a 4K display
arrives 30% of the way down a 1080p one. Measuring it on the destination instead
would just reinterpret the same absolute coordinate in a different frame, which
is the bug the mechanism exists to avoid.

The arriving pointer is placed slightly *inside* the destination edge. Landing
exactly on the boundary meant one pixel of reverse motion crossed straight back,
and focus oscillated between two machines every 20ms.
