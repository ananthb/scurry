#include "scurry_led.h"

#include <string.h>

#include "driver/gpio.h"
#include "esp_log.h"
#include "freertos/FreeRTOS.h"
#include "freertos/task.h"

static const char *TAG = "SCURRY_LED";

/* A single-colour LED on GPIO8. GPIO5/6 are the panel, GPIO9 is BOOT.
 *
 * Wired anode to 3V3, so the pin sinks the current and a low level is lit.
 * That is also the reset state of the pin, which is why the LED sits on until
 * something drives it high. */
#define LED_GPIO        8
#define LED_ON          0
#define LED_OFF         1

#define LED_TICK_MS     50
#define LED_BLINK_MS    500 /* half a second on, half off */

static scurry_display_poll_t s_poll;

static void led_task(void *arg)
{
    (void)arg;
    uint32_t elapsed_ms = 0;
    /* What the LED is showing, so an unchanged state costs nothing. */
    int shown = -1;

    while (true) {
        scurry_display_state_t st;
        memset(&st, 0, sizeof(st));
        s_poll(&st);

        /* Pairing outranks connected, as it does on the glass. */
        int want;
        if (st.pairing_left_s > 0) {
            want = (elapsed_ms / LED_BLINK_MS) % 2 == 0;
        } else {
            want = st.links > 0;
        }

        if (want != shown) {
            gpio_set_level(LED_GPIO, want ? LED_ON : LED_OFF);
            shown = want;
        }

        elapsed_ms += LED_TICK_MS;
        vTaskDelay(pdMS_TO_TICKS(LED_TICK_MS));
    }
}

bool scurry_led_start(scurry_display_poll_t poll)
{
    if (poll == NULL) {
        return false;
    }
    s_poll = poll;

    gpio_config_t cfg = {
        .pin_bit_mask = 1ULL << LED_GPIO,
        .mode = GPIO_MODE_OUTPUT,
        .pull_up_en = GPIO_PULLUP_DISABLE,
        .pull_down_en = GPIO_PULLDOWN_DISABLE,
        .intr_type = GPIO_INTR_DISABLE,
    };
    if (gpio_config(&cfg) != ESP_OK) {
        ESP_LOGW(TAG, "could not claim GPIO%d -- running dark", LED_GPIO);
        return false;
    }

    /* Off before the first tick: the pin comes up low, which is lit. */
    gpio_set_level(LED_GPIO, LED_OFF);

    /* 2048 is generous for a loop with no locals to speak of. */
    if (xTaskCreate(led_task, "scurry_led", 2048, NULL, 4, NULL) != pdPASS) {
        ESP_LOGW(TAG, "could not start the LED task -- running dark");
        gpio_set_level(LED_GPIO, LED_OFF);
        return false;
    }

    ESP_LOGI(TAG, "status LED on GPIO%d", LED_GPIO);
    return true;
}
