# Experiments

Things that were not obviously going to work, and what happened when they were
tried on real hardware.

Each one states a verdict first, shows the evidence, and ends with what it did
*not* establish — because the last section is the one that stops a result being
read as broader than it is.

| # | Question | Verdict |
|---|---|---|
| [001](001-concurrent-bonded-hosts.md) | Can one ESP32-C3 hold bonded HID connections to several hosts at once? | Yes. Switching machines is choosing a `conn_id`, not paying a reconnect. |
| [002](002-wireless-control-link.md) | Can the controller be one more connection on that radio, so the cable is optional? | Yes, mouse and keyboard, with the dongle on a charger. Costs one target slot. |
| [003](003-link-latency.md) | What does each link cost? | Cable 0.3ms, wireless 16.8ms median and 30.4ms p90. The variance is what you feel. |

Numbers throughout are from an ESP32-C3 (QFN32) rev v0.4 on ESP-IDF v5.5.2 with
Bluedroid, driven from macOS.
