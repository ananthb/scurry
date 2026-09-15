#include "scurry_led.h"

#include <string.h>

#include "driver/rmt_tx.h"
#include "esp_log.h"
#include "freertos/FreeRTOS.h"
#include "freertos/task.h"

static const char *TAG = "SCURRY_LED";

/* One WS2812 on GPIO8. GPIO5/6 are the panel, GPIO9 is BOOT. */
#define LED_GPIO        8

/* WS2812 bit timing in 0.1us ticks. The >50us reset that latches a frame is
 * not emitted: the channel idles low and the next write is a tick away. */
#define LED_T0H         3  /* 0.3us */
#define LED_T0L         9  /* 0.9us */
#define LED_T1H         9  /* 0.9us */
#define LED_T1L         3  /* 0.3us */

#define LED_RESOLUTION_HZ 10000000 /* 10MHz, so one tick is 0.1us */

#define LED_TICK_MS     50
#define LED_BLINK_MS    500 /* half a second on, half off */

/* Brightness out of 255. Low: a WS2812 at full scale is a desk torch. */
#define LED_LEVEL       12

static rmt_channel_handle_t s_chan;
static rmt_encoder_handle_t s_encoder;
static scurry_display_poll_t s_poll;

/* Push one colour. The WS2812 takes green, red, blue, MSB first. */
static void led_write(uint8_t r, uint8_t g, uint8_t b)
{
    uint8_t grb[3] = {g, r, b};
    rmt_transmit_config_t cfg = {.loop_count = 0};
    if (rmt_transmit(s_chan, s_encoder, grb, sizeof(grb), &cfg) != ESP_OK) {
        return;
    }
    /* An overlapping transmit would be refused and the colour lost. */
    rmt_tx_wait_all_done(s_chan, 100);
}

static void led_task(void *arg)
{
    (void)arg;
    uint32_t elapsed_ms = 0;
    /* What the LED is showing, so an unchanged state costs no bus traffic. */
    int shown = -1;

    while (true) {
        scurry_display_state_t st;
        memset(&st, 0, sizeof(st));
        s_poll(&st);

        /* Pairing outranks connected, as it does on the glass. */
        int want;
        if (st.pairing_left_s > 0) {
            want = (elapsed_ms / LED_BLINK_MS) % 2 == 0 ? 1 : 0;
        } else if (st.links > 0) {
            want = 2;
        } else {
            want = 0;
        }

        if (want != shown) {
            switch (want) {
            case 1: /* amber: pairing */
                led_write(LED_LEVEL, LED_LEVEL / 2, 0);
                break;
            case 2: /* green: connected */
                led_write(0, LED_LEVEL, 0);
                break;
            default:
                led_write(0, 0, 0);
                break;
            }
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

    rmt_tx_channel_config_t chan_cfg = {
        .gpio_num = LED_GPIO,
        .clk_src = RMT_CLK_SRC_DEFAULT,
        .resolution_hz = LED_RESOLUTION_HZ,
        /* 24 symbols per frame; 48 is the driver minimum and means a
           transmit never refills partway through. A stretched bit is a
           wrong colour. */
        .mem_block_symbols = 48,
        .trans_queue_depth = 1,
    };
    if (rmt_new_tx_channel(&chan_cfg, &s_chan) != ESP_OK) {
        ESP_LOGW(TAG, "no RMT channel for the LED on GPIO%d -- running dark", LED_GPIO);
        return false;
    }

    rmt_bytes_encoder_config_t enc_cfg = {
        .bit0 = {.level0 = 1, .duration0 = LED_T0H, .level1 = 0, .duration1 = LED_T0L},
        .bit1 = {.level0 = 1, .duration0 = LED_T1H, .level1 = 0, .duration1 = LED_T1L},
        .flags.msb_first = 1,
    };
    if (rmt_new_bytes_encoder(&enc_cfg, &s_encoder) != ESP_OK) {
        ESP_LOGW(TAG, "could not build the LED encoder -- running dark");
        rmt_del_channel(s_chan);
        s_chan = NULL;
        return false;
    }

    if (rmt_enable(s_chan) != ESP_OK) {
        ESP_LOGW(TAG, "could not enable the LED channel -- running dark");
        return false;
    }

    /* Clear whatever the LED came up holding, before the first tick. */
    led_write(0, 0, 0);

    /* 2048 is generous for a loop with no locals to speak of. */
    if (xTaskCreate(led_task, "scurry_led", 2048, NULL, 4, NULL) != pdPASS) {
        ESP_LOGW(TAG, "could not start the LED task -- running dark");
        return false;
    }

    ESP_LOGI(TAG, "status LED on GPIO%d", LED_GPIO);
    return true;
}
