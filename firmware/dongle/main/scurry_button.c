#include "scurry_button.h"

#include "esp_log.h"
#include "esp_timer.h"
#include "freertos/FreeRTOS.h"
#include "freertos/task.h"

#define TAG "SCURRY_BTN"

/* Polled rather than interrupt-driven. A mechanical button bounces for a
 * millisecond or two and an ISR would see every one of those edges, so it would
 * need debouncing anyway; at 10ms the bounce is simply invisible, and the cost
 * is one low-priority task doing a register read a hundred times a second. */
#define POLL_MS 10

/* How long after the last release a burst is considered finished. Long enough
 * that a deliberate triple press is never cut in two, short enough that the
 * feedback does not feel detached from the act. */
#define BURST_GAP_MS 400

/* An upper bound on one burst, so holding the button down or a stuck switch
 * cannot accumulate presses forever. */
#define BURST_MAX_MS 3000

static scurry_button_cb_t press_cb;

static void button_task(void *arg)
{
    gpio_num_t pin = (gpio_num_t)(intptr_t)arg;

    /* Active low: the pull-up holds it high and the button pulls it down. */
    gpio_config_t cfg = {
        .pin_bit_mask = 1ULL << pin,
        .mode = GPIO_MODE_INPUT,
        .pull_up_en = GPIO_PULLUP_ENABLE,
        .pull_down_en = GPIO_PULLDOWN_DISABLE,
        .intr_type = GPIO_INTR_DISABLE,
    };
    if (gpio_config(&cfg) != ESP_OK) {
        ESP_LOGE(TAG, "could not configure GPIO %d", (int)pin);
        vTaskDelete(NULL);
        return;
    }
    ESP_LOGI(TAG, "button on GPIO %d", (int)pin);

    bool was_down = false;
    int presses = 0;
    int64_t last_edge_us = 0;
    int64_t burst_started_us = 0;

    while (1) {
        vTaskDelay(pdMS_TO_TICKS(POLL_MS));

        bool down = gpio_get_level(pin) == 0;
        int64_t now = esp_timer_get_time();

        if (down && !was_down) {
            if (presses == 0) {
                burst_started_us = now;
            }
            presses++;
            last_edge_us = now;
        } else if (!down && was_down) {
            last_edge_us = now;
        }
        was_down = down;

        /* A burst ends when the button has been left alone long enough, or when
           it has gone on too long to be one gesture. Reported on release only:
           counting a burst while a finger is still down would fire mid-gesture
           and make a triple press look like a double. */
        if (presses > 0 && !down) {
            bool settled = (now - last_edge_us) > (int64_t)BURST_GAP_MS * 1000;
            bool overlong = (now - burst_started_us) > (int64_t)BURST_MAX_MS * 1000;
            if (settled || overlong) {
                int count = presses;
                presses = 0;
                if (press_cb) {
                    press_cb(count);
                }
            }
        }
    }
}

void scurry_button_start(gpio_num_t pin, scurry_button_cb_t cb)
{
    press_cb = cb;
    /* Low priority: nothing here is urgent, and it must never sit in front of
       the reader task or the Bluetooth stack. */
    xTaskCreate(button_task, "scurry_btn", 2560, (void *)(intptr_t)pin, 2, NULL);
}
