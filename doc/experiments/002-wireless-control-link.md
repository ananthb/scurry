# 002 — A wireless controller

**Verdict: it works.** The cable between the controller and the dongle is
optional. A Mac drove two bonded targets with the dongle on a USB charger and no
data connection to the machine at all — mouse and keyboard both.

Run on an ESP32-C3 (QFN32) rev v0.4, ESP-IDF v5.5.2, Bluedroid, against macOS
using CoreBluetooth through `btleplug`.

## What was in question

[001](001-concurrent-bonded-hosts.md) established that one C3 can hold several
bonded HID connections at once. This asks a different thing: whether the
*controller* can be one more connection on the same radio, so the dongle can sit
between the machines rather than hanging off one of them.

Three ways it could have failed:

- the C3 has one radio and a fixed link budget, so a controller connection is
  one fewer target;
- macOS hides some services from applications and might have hidden ours;
- an input link that anything in range can bond with is a remote keystroke
  injection device, so it had to be possible to authorise one controller and
  refuse the rest.

## Shape

The dongle keeps its HID service and gains a second, custom one beside it. It is
a peripheral to everything: targets connect to the HID service, the controller
connects to the control service, one GATT database.

```text
[controller: macOS]
        |  BLE, custom GATT service ("scurry LINK")
   [ESP32-C3 dongle]  — powered from anything with 5V
        |  BLE HID (HOGP), one bonded connection per target
    /        \
[Pixel]   [Chromebook]
```

The control service carries the same framed protocol the cable carries, so
nothing above the transport knows which one it got.

## Evidence

Dongle on a charger, no serial port present on the Mac:

```text
$ ls /dev/cu.usbmodem*
no matches found

$ scurry-ctl --wireless status
dongle: Scurry 629X (wireless)
node   connected   address
1      yes         62:c2:78:1b:00:ed
2      yes         f0:68:e3:e5:d1:b1

$ scurry-ctl run --wireless
dongle: Scurry 629X (wireless)
capturing; move the pointer off a screen edge to hand over
```

The pointer crossed onto both targets and keyboard input followed it. Reported
as "a bit jittery but not unusable", which matches [003](003-link-latency.md):
the p90 is close to double the median, and irregular motion reads worse than
uniformly delayed motion.

The whole control plane works over the air — `ping`, `status`, and a layout read
and written back — which exercises reassembly in both directions.

## Three targets, not four

`CONFIG_BT_ACL_CONNECTIONS` is 4 and the controller holds one of them. So
wireless mode is three targets; the cable keeps all four. Advertising is bounded
by the link budget rather than by the screen count, or the dongle would keep
soliciting connections it has no room for.

## What had to change

| Change | Why |
|---|---|
| Register the GATT app *after* `esp_hidd_register_callbacks` | That call installs the GATTS callback. An app registered before it has its registration event delivered to nobody: `app_register` returns `ESP_OK` and the service then silently never exists. Cost the better part of a debugging session. |
| Frame reassembly on both transports | A BLE write is bounded by the negotiated MTU and no config payload fits inside one, so a frame arrives in pieces — the same problem the cable already had with log text interleaved into the stream. One framer per transport, because interleaving two streams into one buffer splices frames together. |
| Hand GATT writes to the reader task | The write callback runs on the Bluedroid task. Parsing there would put a second thread on the layout engine, which holds the pointer position in a static, and on the shared transmit buffer and sequence counter. |
| Floor the reader loop's timeout at one tick | The tick is 10ms, so `5 / portTICK_PERIOD_MS` truncated to **zero**, the read stopped blocking, and the task spun at 100% until the watchdog fired. |
| Never seat the controller as a target | A controller connects as an ordinary central and looks exactly like a target. Observed: a Mac bonding for the control link took node 1 and displaced the phone pinned to it — which would have routed the pointer at the machine the pointer came from. |
| Coalesce queued writes | The write queue is unbounded, so a radio that cannot keep up with a 125Hz pointer falls behind without limit rather than settling at some offset. Lossless here only because motion is a delta and buttons are absolute. |

## Authorisation

Bonding is Just Works — the dongle has no display or keypad — so anything in
range can hold an encrypted link. That is fine for a mouse and not fine for
something that can type.

So encryption is necessary but not sufficient: the controller's address must
also be **pinned**, and pinning only happens inside a window. The window opens
two ways, and both mean somebody is standing at the dongle:

- **three presses of its button**, or
- a request **over the cable**, which the dongle refuses if it arrives over the
  air.

The window closes on a timeout or the moment something pairs. The pin survives
reboot in NVS. Physical access still grants control, exactly as when the cable
was the only path.

Three presses rather than one: BOOT is the button people mash by reflex when a
flash goes wrong, and what sits behind it authorises a device that can type on
every machine in the layout.

## Surprises worth recording

- **CoreBluetooth never discloses a peer's address.** Every device on macOS
  reports `00:00:00:00:00:00`, so the controller finds the dongle by name.
  Pinning is therefore necessarily dongle-side, which is where it belongs.
- **macOS hides the HID service from applications** but not custom ones. Only
  the battery service and ours are visible to `btleplug`.
- **macOS caches a bonded peripheral's service list** and will serve a stale one
  indefinitely. A dongle that gains a service after the Mac has bonded appears
  not to have it. It cleared here on the next reflash; failing that, forgetting
  the device in Bluetooth settings is the way out.
- **macOS decorates a bonded HID device's advertised name**, presenting
  `Scurry 629X` as `HID [Scurry 629X]`, so a prefix match on the name fails
  where a substring match works.

## Not established

- **A second controller.** One pinned address; what happens when a phone wants
  to be the input device instead has not been tried.
- **Range.** Tested across a desk.
- **Reconnect.** The controller does not yet re-establish a dropped wireless
  link on its own; the tray only looks for a serial port.
- **Power draw**, which now matters, because the dongle is on a battery rather
  than a host.
