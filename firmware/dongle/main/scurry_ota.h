/* Firmware updates over the same link that carries the pointer.
 *
 * The dongle is the one piece of this system that is not a file on somebody's
 * laptop, and until now the only way to change it was a cable, a held BOOT
 * button and a tapped RST -- a sequence that is fiddly on the best board and
 * genuinely awkward on one sitting on a charger between two machines, which is
 * exactly where the wireless link was built to let it sit.
 *
 * So: an image arrives in chunks over the protocol, is written straight to the
 * inactive slot, checked, and booted. If anything about it is wrong the running
 * firmware is untouched, because it was never the thing being written to.
 *
 * Two things make that claim hold.
 *
 * The image is described before it is sent -- length and SHA-256 up front --
 * so one that was never going to be accepted is refused before a flash erase
 * and a minute of radio time have been spent on it.
 *
 * And the bootloader can undo the result. A freshly booted image is marked
 * pending, and if it does not declare itself healthy it is reverted on the next
 * reset. That matters most in the case that is otherwise unrecoverable: an
 * update delivered over the air, to a dongle nobody is standing next to, which
 * boots into something that cannot talk.
 */

#pragma once

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

/* Mirrors scurry-proto's ota_state. */
#define SCURRY_OTA_IDLE      0
#define SCURRY_OTA_RECEIVING 1
#define SCURRY_OTA_VERIFYING 2
#define SCURRY_OTA_READY     3
#define SCURRY_OTA_FAILED    4

/* True when this build has somewhere to put a second image. False for one
 * flashed into the old single-app layout, which cannot be updated this way and
 * says so rather than failing halfway through. */
bool scurry_ota_supported(void);

/* The running image's version string, as built. Never NULL. */
const char *scurry_ota_running_version(void);

/* True when the running image has not yet confirmed itself, so a reset now
 * would return the dongle to the previous one. */
bool scurry_ota_pending_verify(void);

/* Declare the running image healthy, so the bootloader stops holding the
 * previous one in reserve. Idempotent. */
void scurry_ota_mark_valid(void);

/* Start the timer that will call scurry_ota_mark_valid() once the firmware has
 * stayed up long enough to be worth keeping. Called once, at the end of
 * startup. */
void scurry_ota_start_self_check(void);

/* The transfer. Each returns a scurry-proto ack code: 0 is OK. */
uint8_t scurry_ota_begin(uint32_t len, const uint8_t sha256[32]);
uint8_t scurry_ota_write(uint32_t offset, const uint8_t *data, size_t len);
uint8_t scurry_ota_end(void);
void    scurry_ota_abort(void);

/* Bytes written so far, and one of the states above. */
void scurry_ota_progress(uint32_t *received, uint8_t *state);

/* 0..100, for the screen. */
uint8_t scurry_ota_percent(void);
