# 003 — What each link costs

**Verdict: the cable is free, the radio costs about 17ms, and the variance is
what you feel.** [001](001-concurrent-bonded-hosts.md) left latency as the
design's known weak point and the README called it unmeasured. It is measured.

Run on an ESP32-C3 (QFN32) rev v0.4, ESP-IDF v5.5.2, against macOS. Thirty
`PING`/`PONG` round trips per transport, 20ms apart, two targets connected
throughout.

## Numbers

| | min | median | p90 | max | mean |
|---|---|---|---|---|---|
| Cable (USB CDC) | 0.2 ms | **0.3 ms** | 0.3 ms | 0.3 ms | 0.3 ms |
| Wireless (BLE) | 13.4 ms | **16.8 ms** | 30.4 ms | 34.0 ms | 19.5 ms |

Reproduce with `scurry-ctl latency` and `scurry-ctl --wireless latency`.

## Reading them

**This is a round trip on the control path, and it is the optimistic half of the
answer.** A real pointer report is one way, but then waits on the dongle's
*second* radio hop out to the target, which nothing on the controller can
observe. Roughly: half of 16.8 for the first hop, plus a target connection
interval, puts a report at something like 18ms typical and 30ms worst case.

**The wireless figure is mostly a readout of what macOS negotiated, not of the
C3.** Apple's accessory guidelines put the connection interval floor at 15ms in
15ms steps, and a host picks the interval. A 16.8ms median against a 15ms floor
means essentially one interval of delay and almost no processing on top. The
dongle is not the bottleneck and a faster MCU would not move this number.

**The spread is the part that is felt.** The p90 is nearly double the median: a
report that misses its connection event waits a whole further interval. Reported
subjectively as "a bit jittery but not unusable", which is what an
almost-bimodal 17/30ms distribution should feel like. Uniform delay is easy to
adapt to; irregular delay is not.

## Consequences

- **Not for gaming**, which was never the claim.
- **The cable is not a legacy path.** Two orders of magnitude is not a
  preference, it is a different kind of pointer. Wireless is for when the dongle
  needs to live somewhere else.
- **Coalescing is load-bearing, not an optimisation.** A 125Hz pointer produces a
  report every 8ms against a link that drains one every 15–30ms. Without
  collapsing bursts the queue grows without bound and the lag climbs forever
  rather than settling. Motion is a delta and buttons are absolute, which is the
  only reason collapsing is lossless.

## Not established

- **What the interval actually is.** Inferred from the round trip and Apple's
  published floor rather than read off the link; neither CoreBluetooth nor
  `btleplug` exposes it, and the dongle has not been asked.
- **Whether requesting different connection parameters helps.** A peripheral can
  ask; macOS is free to refuse.
- **The second hop**, measured directly rather than inferred.
- **Behaviour under interference**, or at range, or with four targets.
