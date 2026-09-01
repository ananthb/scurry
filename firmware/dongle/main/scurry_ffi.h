/* C ABI over the Rust layout engine (crates/scurry-ffi).
 *
 * Routing runs the same code the controller is tested against rather than a
 * hand-written reimplementation. The proportional-handoff arithmetic in
 * particular already produced one subtle bug; having a single implementation
 * with host tests is worth the build plumbing.
 */
#pragma once

#include <stdint.h>
#include <stddef.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct {
    uint8_t  crossed;  /* 1 when the pointer changed screens */
    uint8_t  from;     /* node being left; only when crossed */
    uint8_t  to;       /* node now holding the pointer; 0 = controller's own */
    uint8_t  edge;     /* arrival edge: 0=L 1=R 2=T 3=B; only when crossed */
    uint16_t ratio;    /* position along that edge; only when crossed */
    /* Pointer position within `to`, normalised to 0..=32767, sent as an
       absolute HID coordinate. Relative motion would require dead reckoning,
       which breaks as soon as anything else moves the remote pointer or the
       target applies its own acceleration to our deltas. */
} scurry_route_t;

/* Install a layout from a CONFIG payload (count byte, then that many screens).
   0 on success, negative on error. Validation happens here, so an invalid
   layout can never reach storage. */
int32_t scurry_layout_load(const uint8_t *data, size_t len);

/* Serialise the current layout back to a CONFIG payload.
   Returns bytes written, or negative on error. */
int32_t scurry_layout_save(uint8_t *out, size_t cap);

/* A mouse report after the target's input profile has been applied. */
typedef struct {
    uint8_t buttons;
    int16_t dx;
    int16_t dy;
    int8_t  wheel;
    int8_t  pan;
} scurry_mouse_t;

/* Apply a target's mouse profile: scaling, axis inversion, scroll direction,
   button swapping. Kept in Rust so the arithmetic stays under host tests. */
int32_t scurry_map_mouse(uint8_t node, uint8_t buttons, int16_t dx, int16_t dy,
                         int8_t wheel, int8_t pan, scurry_mouse_t *out);

/* Translate a host modifier byte into the one this target expects -- the Mac
   sends Cmd where Linux and ChromeOS want Ctrl. */
uint8_t scurry_map_modifiers(uint8_t node, uint8_t host);

/* The node pinned to this Bluetooth address, or -1 if none is. Lets a
   reconnecting machine keep the same node id instead of taking whichever slot
   was free, which silently swapped screens across a reboot. */
int32_t scurry_layout_node_for_address(const uint8_t *bda);

/* Declare which nodes can receive input, as a bitmask where bit N means node N.
   Node 0 is always available. Without this the pointer crosses onto machines
   that are not connected, which looks like it sticking to an edge and dying. */
void scurry_layout_set_available(uint32_t mask);

/* Apply a pending availability change and rescue a stranded pointer. Returns 1
   when the pointer had to move, filling `out` as scurry_layout_advance does.
   Must be called from the task that handles input. */
int32_t scurry_layout_settle(scurry_route_t *out);

/* 0 until a layout is installed. Until then there is nowhere to route input. */
uint8_t scurry_layout_ready(void);

/* Feed relative motion in; learn where it landed. 0 on success. */
int32_t scurry_layout_advance(int32_t dx, int32_t dy, scurry_route_t *out);

#ifdef __cplusplus
}
#endif
