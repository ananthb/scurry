# Wire protocol

Fixed 16-byte frames, little-endian. ESP-NOW allows 250 bytes, so there is room
to grow, but a fixed size means no length negotiation and no allocation on the
node.

```text
0       magic 0x53
1       version
2       kind
3       node id
4..6    seq (u16)
6..16   payload
```

## Buttons are absolute, motion is relative

ESP-NOW does not retransmit. Frames can be dropped or reordered.

If button presses and releases were separate events, a dropped release would
leave a button held down on the target with no way to recover. That is the worst
failure this protocol could have — a stuck mouse button on a machine you are not
sitting at. So every frame carries the **full button bitmask**, and the next
frame to arrive repairs the state.

Motion is a delta, because the opposite tradeoff applies. A dropped motion frame
costs a few pixels and self-corrects the moment the mouse moves again.

## Sequence numbers

Every frame carries one, so a receiver can drop reordered stragglers instead of
applying motion backwards. `SeqGate` compares by signed distance on the wrapped
circle, not by `>`; a naive comparison would stall the pointer for 32k frames
every time the counter wrapped.

## Leave releases everything

A node receiving `Leave` must release all held buttons. Otherwise a drag that
crosses a screen boundary strands a held button on the machine being departed.

## Proportional handoff

`Enter` carries the edge and a `ratio`: how far along that edge the pointer
arrived, scaled to `u16`.

The ratio is measured on the screen being **left**, then applied to the screen
being entered. This is what lets a 4K display hand off to a 1080p one without
the pointer jumping — leaving 30% of the way down arrives 30% of the way down.
Measuring it on the destination instead would just reinterpret the same absolute
coordinate in a different frame, which is the bug the mechanism exists to avoid.
