/* The dongle's one usable button.
 *
 * A C3 devkit has two: BOOT and RESET. RESET is wired to chip enable and cannot
 * be read, so BOOT is the whole input budget. It is a strapping pin -- held low
 * across a reset it selects download mode -- but after boot it is an ordinary
 * input with a pull-up, which is what makes it usable as a user button at all.
 *
 * Counted presses rather than a hold, because a hold is what people do to the
 * BOOT button by reflex when flashing goes wrong, and a triple press is not
 * something anyone does by accident.
 */

#pragma once

#include <stdint.h>

#include "driver/gpio.h"

/* Reported once a burst has settled: how many presses it contained. */
typedef void (*scurry_button_cb_t)(int presses);

/* BOOT on every ESP32-C3 devkit worth naming. If a board puts it elsewhere,
 * this is the one line to change. */
#define SCURRY_BUTTON_GPIO GPIO_NUM_9

void scurry_button_start(gpio_num_t pin, scurry_button_cb_t cb);
