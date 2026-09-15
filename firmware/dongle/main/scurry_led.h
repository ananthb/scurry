/* The dongle's WS2812 status LED.
 *
 * Blinking: the pairing window is open. Solid: at least one target is
 * connected. Otherwise dark.
 */

#pragma once

#include <stdbool.h>

#include "scurry_display.h"

/* Bring up the LED and start its task. Takes the same state snapshot the
 * display does.
 *
 * Returns false if the RMT channel could not be claimed; the dongle then runs
 * dark, as it does without a panel. */
bool scurry_led_start(scurry_display_poll_t poll);
