/* The wireless control link.
 *
 * A custom GATT service the controller connects to as a central, carrying the
 * same framed protocol the USB cable carries. The dongle stays a peripheral for
 * everything: targets connect to its HID service, the controller connects to
 * this one, and all of it is one radio and one GATT database.
 *
 * The cable is not replaced by this. It remains the configuration path and the
 * fallback, and -- see the pairing window below -- it is also one of the two
 * ways a controller is authorised in the first place.
 */

#pragma once

#include <stdbool.h>
#include <stdint.h>

#include "esp_gap_ble_api.h"
#include "esp_gatts_api.h"

/* One GATT app id of our own, distinct from the HID and battery profiles. */
#define SCURRY_CTL_APP_ID 0x5343

/* How many controllers may be authorised at once.
 *
 * More than one, because a laptop is not the only thing that might drive this:
 * a phone should be able to take over without the laptop having to be forgotten
 * and re-paired to get it back afterwards. Exactly one drives at a time. */
#define SCURRY_MAX_CONTROLLERS 4

/* Handed every byte the driving controller writes. The bytes are a stream, not
 * a message: BLE writes are bounded by the negotiated MTU and a config payload
 * is larger than any MTU a host will agree to, so a frame routinely arrives in
 * pieces. The caller reassembles. */
typedef void (*scurry_ctl_rx_cb_t)(const uint8_t *data, uint16_t len);

void scurry_ctl_svc_init(scurry_ctl_rx_cb_t on_rx);

/* Hooked into the single GATTS callback the stack allows; see the dispatch
 * table in hid_device_le_prf.c. */
void scurry_ctl_svc_gatts_event(esp_gatts_cb_event_t event, esp_gatt_if_t gatts_if,
                                esp_ble_gatts_cb_param_t *param);

/* True once a controller is connected, subscribed and driving. */
bool scurry_ctl_svc_ready(void);

/* Send an announcement to whichever controller is driving. Silently does
 * nothing when none is, which is the ordinary state on the cable. */
void scurry_ctl_svc_notify(const uint8_t *data, uint16_t len);

/* Send an answer to the controller whose request is being handled.
 *
 * Distinct from notify. A controller that is connected but not driving must
 * still get answers to its own queries, or opening the settings window on the
 * machine that is not currently in charge would silently time out. */
void scurry_ctl_svc_reply(const uint8_t *data, uint16_t len);

/* Bumped whenever the driving controller changes.
 *
 * The reader compares it to decide whether the bytes it is holding still belong
 * to the same stream: a half-delivered frame from the previous controller must
 * not have the next one's first bytes spliced onto it. */
uint32_t scurry_ctl_svc_generation(void);

/* Which controller is driving, if any. */
bool scurry_ctl_svc_active_bda(esp_bd_addr_t out);

/* True if this address is authorised at all, driving or not -- so no controller
 * is ever seated as a target, even while another one holds the wheel. */
bool scurry_ctl_svc_is_pinned(const esp_bd_addr_t bda);

void scurry_ctl_svc_on_disconnect(esp_bd_addr_t bda);

/* Authorisation.
 *
 * Bonding here is Just Works -- the dongle has no display or keypad, so the
 * pairing has no man-in-the-middle protection and anything in radio range can
 * bond with it. That is acceptable for a mouse. It is not acceptable for a link
 * that can type, so an encrypted connection is necessary but not sufficient:
 * the controller's address must also have been pinned, and pinning only happens
 * inside a window. The window opens two ways, and both of them mean somebody is
 * standing at the dongle: a triple press of its button, or a request over the
 * cable, which is refused if it arrives over the air.
 *
 * Physical access is therefore still what grants control, exactly as it was
 * when the cable was the only path.
 *
 * On hardware with a display this becomes a real passkey: show six digits and
 * make the controller prove it can see them. That needs the device's IO
 * capability raised to DisplayOnly, which is a global security parameter -- so
 * it has to be raised when the window opens and lowered again when it closes,
 * or every target would start being asked to type a code at a mouse. */
void scurry_ctl_svc_open_pairing(uint32_t seconds);
void scurry_ctl_svc_close_pairing(void);
/* Seconds left in the pairing window, 0 when closed. */
uint32_t scurry_ctl_svc_pairing_remaining(void);

/* How many controllers are authorised, and their addresses. */
int scurry_ctl_svc_pin_count(void);
bool scurry_ctl_svc_pin_at(int index, esp_bd_addr_t out);

/* Revoke every authorised controller. */
void scurry_ctl_svc_forget(void);
