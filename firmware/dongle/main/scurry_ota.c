#include "scurry_ota.h"

#include <string.h>

#include "esp_app_desc.h"
#include "esp_ota_ops.h"
#include "esp_partition.h"
#include "esp_timer.h"
#include "esp_log.h"
#include "mbedtls/sha256.h"

static const char *TAG = "SCURRY_OTA";

/* Ack codes, mirroring scurry-proto's ack module. Duplicated rather than
   shared because this file has no business including the protocol header. */
#define ACK_OK             0
#define ACK_BAD_REQUEST    1
#define ACK_OTA_FAILED     5

static esp_ota_handle_t s_handle;
static const esp_partition_t *s_target;
static mbedtls_sha256_context s_sha;
static bool s_sha_live;

static uint32_t s_expect_len;
static uint32_t s_received;
static uint8_t  s_expect_sha[32];
static uint8_t  s_state = SCURRY_OTA_IDLE;

/* How long the firmware must stay up before it is considered worth keeping.
 *
 * Long enough to have got past everything that fails immediately -- a bad
 * link, a panic in a callback, a boot loop -- and short enough that nobody
 * updating over the air is left waiting to find out. The failures this catches
 * are the ones that happen at once; a bug that takes an hour to show is not
 * something a bootloader timer was ever going to save anyone from. */
#define SCURRY_OTA_SELF_CHECK_S 20

bool scurry_ota_supported(void)
{
    /* The question is whether there is a second slot at all, which is a fact
       about the partition table rather than about this build. A dongle flashed
       with the old single-app layout answers no, and the controller reports
       that rather than starting a transfer it cannot finish. */
    return esp_ota_get_next_update_partition(NULL) != NULL;
}

const char *scurry_ota_running_version(void)
{
    const esp_app_desc_t *d = esp_app_get_description();
    return d != NULL ? d->version : "";
}

bool scurry_ota_pending_verify(void)
{
    const esp_partition_t *running = esp_ota_get_running_partition();
    esp_ota_img_states_t state;
    if (running == NULL || esp_ota_get_state_partition(running, &state) != ESP_OK) {
        return false;
    }
    return state == ESP_OTA_IMG_PENDING_VERIFY;
}

void scurry_ota_mark_valid(void)
{
    if (!scurry_ota_pending_verify()) {
        return;
    }
    if (esp_ota_mark_app_valid_cancel_rollback() == ESP_OK) {
        ESP_LOGI(TAG, "running image confirmed; the previous one is no longer held");
    } else {
        ESP_LOGW(TAG, "could not confirm the running image");
    }
}

static void self_check_fired(void *arg)
{
    (void)arg;
    scurry_ota_mark_valid();
}

void scurry_ota_start_self_check(void)
{
    if (!scurry_ota_pending_verify()) {
        return;
    }
    ESP_LOGW(TAG, "this image is on probation: confirming in %ds, reverting on a reset before then",
             SCURRY_OTA_SELF_CHECK_S);
    const esp_timer_create_args_t args = {
        .callback = self_check_fired,
        .name = "scurry_ota_ok",
    };
    esp_timer_handle_t t;
    if (esp_timer_create(&args, &t) != ESP_OK) {
        /* Without a timer the image would never confirm itself and every
           reset would roll it back -- worse than not having rollback at all.
           Confirm now and lose the probation rather than the firmware. */
        ESP_LOGW(TAG, "no timer for the self check; confirming immediately");
        scurry_ota_mark_valid();
        return;
    }
    esp_timer_start_once(t, (uint64_t)SCURRY_OTA_SELF_CHECK_S * 1000000);
}

static void ota_reset(uint8_t state)
{
    if (s_sha_live) {
        mbedtls_sha256_free(&s_sha);
        s_sha_live = false;
    }
    s_handle = 0;
    s_target = NULL;
    s_received = 0;
    s_expect_len = 0;
    s_state = state;
}

uint8_t scurry_ota_begin(uint32_t len, const uint8_t sha256[32])
{
    if (s_state == SCURRY_OTA_RECEIVING) {
        /* A second begin abandons the first. A controller that lost its reply
           and started again is far more likely than two controllers updating
           at once, and the alternative -- refusing until a timeout -- strands
           the dongle for whoever retries. */
        ESP_LOGW(TAG, "a transfer was already in progress; abandoning it");
        esp_ota_abort(s_handle);
        ota_reset(SCURRY_OTA_IDLE);
    }

    s_target = esp_ota_get_next_update_partition(NULL);
    if (s_target == NULL) {
        ESP_LOGE(TAG, "no second slot: this layout cannot take an update");
        s_state = SCURRY_OTA_FAILED;
        return ACK_BAD_REQUEST;
    }
    if (len == 0 || len > s_target->size) {
        ESP_LOGE(TAG, "image is %lu bytes, slot holds %lu",
                 (unsigned long)len, (unsigned long)s_target->size);
        s_state = SCURRY_OTA_FAILED;
        return ACK_BAD_REQUEST;
    }

    /* Erases the slot, which takes a moment and is why the digest was checked
       for plausibility first. */
    esp_err_t err = esp_ota_begin(s_target, len, &s_handle);
    if (err != ESP_OK) {
        ESP_LOGE(TAG, "esp_ota_begin: %s", esp_err_to_name(err));
        s_state = SCURRY_OTA_FAILED;
        return ACK_OTA_FAILED;
    }

    mbedtls_sha256_init(&s_sha);
    if (mbedtls_sha256_starts(&s_sha, 0) != 0) {
        mbedtls_sha256_free(&s_sha);
        esp_ota_abort(s_handle);
        s_state = SCURRY_OTA_FAILED;
        return ACK_OTA_FAILED;
    }
    s_sha_live = true;

    memcpy(s_expect_sha, sha256, sizeof(s_expect_sha));
    s_expect_len = len;
    s_received = 0;
    s_state = SCURRY_OTA_RECEIVING;
    ESP_LOGI(TAG, "receiving %lu bytes into %s", (unsigned long)len, s_target->label);
    return ACK_OK;
}

uint8_t scurry_ota_write(uint32_t offset, const uint8_t *data, size_t len)
{
    if (s_state != SCURRY_OTA_RECEIVING) {
        return ACK_BAD_REQUEST;
    }
    /* Flash is written forwards and cannot be seeked, so a chunk that is not
       the next one is not something to store for later -- it is a transfer
       that has already gone wrong. Say so now, while the controller still has
       the image and can start again, rather than at the digest check when the
       only evidence left is that the answer is wrong. */
    if (offset != s_received) {
        ESP_LOGE(TAG, "chunk at %lu, expected %lu", (unsigned long)offset,
                 (unsigned long)s_received);
        esp_ota_abort(s_handle);
        ota_reset(SCURRY_OTA_FAILED);
        return ACK_OTA_FAILED;
    }
    if (len == 0 || s_received + len > s_expect_len) {
        esp_ota_abort(s_handle);
        ota_reset(SCURRY_OTA_FAILED);
        return ACK_BAD_REQUEST;
    }

    esp_err_t err = esp_ota_write(s_handle, data, len);
    if (err != ESP_OK) {
        ESP_LOGE(TAG, "esp_ota_write: %s", esp_err_to_name(err));
        esp_ota_abort(s_handle);
        ota_reset(SCURRY_OTA_FAILED);
        return ACK_OTA_FAILED;
    }
    mbedtls_sha256_update(&s_sha, data, len);
    s_received += len;
    return ACK_OK;
}

uint8_t scurry_ota_end(void)
{
    if (s_state != SCURRY_OTA_RECEIVING) {
        return ACK_BAD_REQUEST;
    }
    s_state = SCURRY_OTA_VERIFYING;

    if (s_received != s_expect_len) {
        ESP_LOGE(TAG, "short image: %lu of %lu bytes",
                 (unsigned long)s_received, (unsigned long)s_expect_len);
        esp_ota_abort(s_handle);
        ota_reset(SCURRY_OTA_FAILED);
        return ACK_OTA_FAILED;
    }

    uint8_t got[32];
    mbedtls_sha256_finish(&s_sha, got);
    if (memcmp(got, s_expect_sha, sizeof(got)) != 0) {
        /* The bytes arrived and are not the bytes that were promised. This is
           the check that catches a corrupted transfer; esp_ota_end's own
           validation catches a malformed image, which is a different thing. */
        ESP_LOGE(TAG, "digest mismatch: the image that arrived is not the one described");
        esp_ota_abort(s_handle);
        ota_reset(SCURRY_OTA_FAILED);
        return ACK_OTA_FAILED;
    }

    esp_err_t err = esp_ota_end(s_handle);
    s_handle = 0;
    if (err != ESP_OK) {
        ESP_LOGE(TAG, "esp_ota_end: %s", esp_err_to_name(err));
        ota_reset(SCURRY_OTA_FAILED);
        return ACK_OTA_FAILED;
    }

    err = esp_ota_set_boot_partition(s_target);
    if (err != ESP_OK) {
        ESP_LOGE(TAG, "esp_ota_set_boot_partition: %s", esp_err_to_name(err));
        ota_reset(SCURRY_OTA_FAILED);
        return ACK_OTA_FAILED;
    }

    if (s_sha_live) {
        mbedtls_sha256_free(&s_sha);
        s_sha_live = false;
    }
    s_state = SCURRY_OTA_READY;
    ESP_LOGW(TAG, "staged %s; rebooting into it", s_target->label);
    return ACK_OK;
}

void scurry_ota_abort(void)
{
    if (s_state == SCURRY_OTA_RECEIVING) {
        esp_ota_abort(s_handle);
        ESP_LOGW(TAG, "transfer abandoned at %lu bytes", (unsigned long)s_received);
    }
    ota_reset(SCURRY_OTA_IDLE);
}

void scurry_ota_progress(uint32_t *received, uint8_t *state)
{
    if (received != NULL) {
        *received = s_received;
    }
    if (state != NULL) {
        *state = s_state;
    }
}

uint8_t scurry_ota_percent(void)
{
    if (s_expect_len == 0) {
        return 0;
    }
    uint64_t pct = (uint64_t)s_received * 100 / s_expect_len;
    return pct > 100 ? 100 : (uint8_t)pct;
}
