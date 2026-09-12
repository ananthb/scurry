/* The dongle's 0.42" OLED.
 *
 * Until now the dongle was headless, and two things suffered for it. The
 * pairing window was invisible -- you triple-pressed the button and then had
 * to trust that something had happened, because the only confirmation was a
 * log line on a cable you may well not have been holding. And the control
 * link bonded Just Works, because a device with no display cannot prove to a
 * controller that the controller is talking to it and not to something in the
 * middle.
 *
 * A screen fixes both. `scurry_ctl_svc.h` already described the second one in
 * detail and stopped short of doing it for want of hardware; this is that
 * hardware.
 *
 * The panel is 72x40 -- twelve characters by five lines in a 5x7 font. That is
 * not much, and it is the reason the screens below say "LINK 2/4" rather than
 * anything more generous. Every string here has been counted.
 */

#pragma once

#include <stdbool.h>
#include <stdint.h>

/* Mirrors SCURRY_MAX_HOSTS. Kept as its own name so the driver does not have
   to include the BLE main file to know how many rows to draw. A build-time
   check in scurry_display.c ties the two together. */
#define SCURRY_DISPLAY_MAX_NODES 4

/* Everything the screens can show, gathered in one snapshot.
 *
 * The driver polls for this rather than being pushed at from the BLE
 * callbacks. Pushing would mean touching a dozen call sites and would put
 * I2C writes -- which block for milliseconds -- inside the Bluedroid callback
 * thread, where a stall costs connection events and therefore pointer
 * latency. Polling keeps all of that on the display's own task. */
typedef struct {
    /* Advertised name, e.g. "Scurry M4K2". */
    const char *name;
    uint8_t     mac[6];

    /* Targets currently connected, and the ceiling. */
    int         links;
    int         max_links;

    /* Which node the pointer is on. 0 is the controller's own screen, so it
       means "not on any target here"; node N is slot N-1. */
    uint8_t     focus_node;

    /* Per slot: connected, and the last two bytes of the peer address. Two
       bytes because four hex digits is what fits beside a node number, and
       the tail is the half that differs between machines. */
    bool        node_up[SCURRY_DISPLAY_MAX_NODES];
    uint8_t     node_tail[SCURRY_DISPLAY_MAX_NODES][2];
    /* The screen's name from the stored layout, empty when there is no layout
       or the node is not in it. What somebody actually calls the machine beats
       four hex digits from its address, which identify it correctly and
       describe it not at all. */
    char        node_name[SCURRY_DISPLAY_MAX_NODES][12];

    /* Who holds the wheel: -1 nobody, 0 the cable, >0 a wireless controller. */
    int         driver;

    /* Seconds left in the pairing window, 0 when it is closed. */
    uint32_t    pairing_left_s;

    /* The six digits the peer must confirm, valid only while the stack has
       given us one. Held separately from the window because the window opens
       first and the passkey only arrives once a controller actually turns up. */
    bool        passkey_valid;
    uint32_t    passkey;

    /* A firmware update in progress: one of SCURRY_OTA_*, and how far it has
       got. This outranks everything else on the glass, because it is the one
       state where somebody needs to know not to unplug the thing. */
    uint8_t     ota_state;
    uint8_t     ota_percent;
} scurry_display_state_t;

/* Fill `out` with the current state. Called from the display task, so it must
   not block on anything the BLE callbacks hold. */
typedef void (*scurry_display_poll_t)(scurry_display_state_t *out);

/* Bring up I2C and the panel, then start the refresh task.
 *
 * Returns false if the panel did not answer -- a board without one still runs,
 * it just runs headless, exactly as it did before. That is deliberate: the
 * same firmware image is flashed to both, and losing mouse control because a
 * screen is missing would be a poor trade for a status readout. */
bool scurry_display_start(scurry_display_poll_t poll);

/* True when a panel answered at boot. */
bool scurry_display_present(void);

/* Show the next screen now, and restart its dwell.
 *
 * Wired to a single press of the button. One press has to stay harmless --
 * it is exactly what somebody does to a button when they are not sure what it
 * does -- and changing which of two read-only screens is up is about as
 * harmless as an input gets. The three-press pairing ceremony is unaffected:
 * the button reports a whole burst at once, so a triple press arrives as one
 * callback saying "three", never as three saying "one". */
void scurry_display_next(void);

/* Summon the Bluetooth address screen, held for the same dwell a carousel
 * screen gets. Wired to a double press.
 *
 * It is on demand rather than in the rotation because of how often it is
 * wanted, which is twice: once when a machine is first pinned to a node, and
 * again whenever somebody edits scurry.toml. A screen wanted twice does not
 * deserve a third of the rotation for the rest of the dongle's life. */
void scurry_display_show_mac(void);
