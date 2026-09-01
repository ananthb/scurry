#include <string.h>

#include "esp_log.h"
#include "esp_timer.h"
#include "nvs.h"
#include "nvs_flash.h"

#include "scurry_ctl_svc.h"

#define TAG "SCURRY_CTL"

#define NVS_NAMESPACE "scurry"
#define NVS_KEY_CTL_BDA "ctl_bda"

/* 73637572-7279-4c49-4e4b-0000000000xx -- "scurry" and "LINK" in ASCII, so the
 * service is recognisable in a scanner without a lookup table. Written
 * least-significant byte first, which is the order the stack wants. */
#define SCURRY_UUID_BASE 0x4b, 0x4e, 0x49, 0x4c, 0x79, 0x72, 0x72, 0x75, 0x63, 0x73

static const uint8_t svc_uuid[16] = {0x01, 0x00, 0x00, 0x00, 0x00, 0x00, SCURRY_UUID_BASE};
static const uint8_t control_uuid[16] = {0x02, 0x00, 0x00, 0x00, 0x00, 0x00, SCURRY_UUID_BASE};
static const uint8_t event_uuid[16] = {0x03, 0x00, 0x00, 0x00, 0x00, 0x00, SCURRY_UUID_BASE};

enum {
    SCURRY_IDX_SVC,
    SCURRY_IDX_CONTROL_CHAR,
    SCURRY_IDX_CONTROL_VAL,
    SCURRY_IDX_EVENT_CHAR,
    SCURRY_IDX_EVENT_VAL,
    SCURRY_IDX_EVENT_CCC,
    SCURRY_IDX_NB,
};

static const uint16_t primary_service_uuid = ESP_GATT_UUID_PRI_SERVICE;
static const uint16_t character_declaration_uuid = ESP_GATT_UUID_CHAR_DECLARE;
static const uint16_t character_client_config_uuid = ESP_GATT_UUID_CHAR_CLIENT_CONFIG;

/* Write and write-without-response both offered. The pointer stream wants the
 * unacknowledged form -- an ack per mouse report would double the radio events
 * for no benefit, since a dropped report is repaired by the next one. Config
 * writes want the acknowledged form. */
static const uint8_t prop_write = ESP_GATT_CHAR_PROP_BIT_WRITE | ESP_GATT_CHAR_PROP_BIT_WRITE_NR;
static const uint8_t prop_notify = ESP_GATT_CHAR_PROP_BIT_NOTIFY;

/* One MTU's worth. Frames larger than this arrive in several writes and are
 * reassembled by the caller, so nothing here needs to hold a whole config. */
#define CTL_VAL_MAX 244

static uint8_t control_val[CTL_VAL_MAX];
static uint8_t event_val[CTL_VAL_MAX];
static uint8_t event_ccc[2] = {0, 0};

static const esp_gatts_attr_db_t scurry_ctl_db[SCURRY_IDX_NB] = {
    [SCURRY_IDX_SVC] = {{ESP_GATT_AUTO_RSP},
                        {ESP_UUID_LEN_16, (uint8_t *)&primary_service_uuid, ESP_GATT_PERM_READ,
                         sizeof(svc_uuid), sizeof(svc_uuid), (uint8_t *)svc_uuid}},

    [SCURRY_IDX_CONTROL_CHAR] = {{ESP_GATT_AUTO_RSP},
                                 {ESP_UUID_LEN_16, (uint8_t *)&character_declaration_uuid,
                                  ESP_GATT_PERM_READ, sizeof(uint8_t), sizeof(uint8_t),
                                  (uint8_t *)&prop_write}},
    /* Encrypted, so an unbonded peer cannot write here at all. Bonding is Just
     * Works and therefore not proof of anything on its own -- see the pinning
     * check in the write handler, which is what actually gates control. */
    [SCURRY_IDX_CONTROL_VAL] = {{ESP_GATT_AUTO_RSP},
                                {ESP_UUID_LEN_128, (uint8_t *)control_uuid,
                                 ESP_GATT_PERM_WRITE_ENCRYPTED, CTL_VAL_MAX, 0, control_val}},

    [SCURRY_IDX_EVENT_CHAR] = {{ESP_GATT_AUTO_RSP},
                               {ESP_UUID_LEN_16, (uint8_t *)&character_declaration_uuid,
                                ESP_GATT_PERM_READ, sizeof(uint8_t), sizeof(uint8_t),
                                (uint8_t *)&prop_notify}},
    [SCURRY_IDX_EVENT_VAL] = {{ESP_GATT_AUTO_RSP},
                              {ESP_UUID_LEN_128, (uint8_t *)event_uuid,
                               ESP_GATT_PERM_READ_ENCRYPTED, CTL_VAL_MAX, 0, event_val}},
    [SCURRY_IDX_EVENT_CCC] = {{ESP_GATT_AUTO_RSP},
                              {ESP_UUID_LEN_16, (uint8_t *)&character_client_config_uuid,
                               ESP_GATT_PERM_READ | ESP_GATT_PERM_WRITE_ENCRYPTED,
                               sizeof(event_ccc), sizeof(event_ccc), event_ccc}},
};

static uint16_t handles[SCURRY_IDX_NB];
static esp_gatt_if_t ctl_gatts_if = ESP_GATT_IF_NONE;
static scurry_ctl_rx_cb_t rx_cb;

/* The controller's connection, once one has identified itself by writing or
 * subscribing. Not recorded on connect: at connect time there is no way to tell
 * a controller from a target, since both are just centrals opening a link. */
static bool ctl_have_conn;
static uint16_t ctl_conn_id;
static esp_bd_addr_t ctl_bda;
static bool ctl_subscribed;

/* Negotiated ATT MTU, which bounds a single notification at mtu-3. Frames
 * larger than that are split; the controller reassembles from the byte stream
 * exactly as the dongle does for writes coming the other way. */
static uint16_t ctl_mtu = 23;

static bool pin_valid;
static esp_bd_addr_t pin_bda;
static int64_t pairing_until_us;

static void pin_save(const esp_bd_addr_t bda)
{
    memcpy(pin_bda, bda, sizeof(esp_bd_addr_t));
    pin_valid = true;

    nvs_handle_t h;
    if (nvs_open(NVS_NAMESPACE, NVS_READWRITE, &h) != ESP_OK) {
        ESP_LOGW(TAG, "could not open NVS to store the controller address");
        return;
    }
    if (nvs_set_blob(h, NVS_KEY_CTL_BDA, bda, sizeof(esp_bd_addr_t)) == ESP_OK) {
        nvs_commit(h);
    }
    nvs_close(h);
}

static void pin_load(void)
{
    nvs_handle_t h;
    if (nvs_open(NVS_NAMESPACE, NVS_READONLY, &h) != ESP_OK) {
        return;
    }
    size_t len = sizeof(esp_bd_addr_t);
    if (nvs_get_blob(h, NVS_KEY_CTL_BDA, pin_bda, &len) == ESP_OK && len == sizeof(esp_bd_addr_t)) {
        pin_valid = true;
        ESP_LOGI(TAG, "controller pinned to %02x:%02x:%02x:%02x:%02x:%02x", pin_bda[0], pin_bda[1],
                 pin_bda[2], pin_bda[3], pin_bda[4], pin_bda[5]);
    }
    nvs_close(h);
}

static bool pairing_open(void)
{
    return pairing_until_us != 0 && esp_timer_get_time() < pairing_until_us;
}

void scurry_ctl_svc_open_pairing(uint32_t seconds)
{
    pairing_until_us = esp_timer_get_time() + (int64_t)seconds * 1000000;
    ESP_LOGI(TAG, "pairing window open for %us -- the next controller to subscribe is pinned",
             (unsigned)seconds);
}

uint32_t scurry_ctl_svc_pairing_remaining(void)
{
    if (!pairing_open()) {
        return 0;
    }
    return (uint32_t)((pairing_until_us - esp_timer_get_time()) / 1000000) + 1;
}

bool scurry_ctl_svc_pinned(esp_bd_addr_t out)
{
    if (!pin_valid) {
        return false;
    }
    memcpy(out, pin_bda, sizeof(esp_bd_addr_t));
    return true;
}

void scurry_ctl_svc_forget(void)
{
    pin_valid = false;
    memset(pin_bda, 0, sizeof(pin_bda));
    pairing_until_us = 0;
    ctl_have_conn = false;
    ctl_subscribed = false;

    nvs_handle_t h;
    if (nvs_open(NVS_NAMESPACE, NVS_READWRITE, &h) == ESP_OK) {
        nvs_erase_key(h, NVS_KEY_CTL_BDA);
        nvs_commit(h);
        nvs_close(h);
    }
    ESP_LOGI(TAG, "wireless controller forgotten");
}

/* Whether this peer may drive the machine.
 *
 * Encryption alone is not enough: Just Works bonding means anything in range
 * can hold an encrypted link. The address must also be the pinned one, and the
 * only way to become the pinned one is to turn up while a window opened over
 * the cable is still running. */
static bool authorised(const esp_bd_addr_t bda)
{
    if (pin_valid && memcmp(pin_bda, bda, sizeof(esp_bd_addr_t)) == 0) {
        return true;
    }
    if (pairing_open()) {
        pin_save(bda);
        pairing_until_us = 0;
        ESP_LOGI(TAG, "controller %02x:%02x:%02x:%02x:%02x:%02x pinned", bda[0], bda[1], bda[2],
                 bda[3], bda[4], bda[5]);
        return true;
    }
    return false;
}

void scurry_ctl_svc_init(scurry_ctl_rx_cb_t on_rx)
{
    rx_cb = on_rx;
    pin_load();
}

bool scurry_ctl_svc_ready(void)
{
    return ctl_have_conn && ctl_subscribed;
}

bool scurry_ctl_svc_conn(uint16_t *conn_id, esp_bd_addr_t bda)
{
    if (!ctl_have_conn) {
        return false;
    }
    if (conn_id) {
        *conn_id = ctl_conn_id;
    }
    if (bda) {
        memcpy(bda, ctl_bda, sizeof(esp_bd_addr_t));
    }
    return true;
}

void scurry_ctl_svc_on_disconnect(esp_bd_addr_t bda)
{
    if (ctl_have_conn && memcmp(ctl_bda, bda, sizeof(esp_bd_addr_t)) == 0) {
        ctl_have_conn = false;
        ctl_subscribed = false;
        ESP_LOGI(TAG, "wireless controller disconnected");
    }
}

void scurry_ctl_svc_notify(const uint8_t *data, uint16_t len)
{
    if (!scurry_ctl_svc_ready() || ctl_gatts_if == ESP_GATT_IF_NONE || len == 0) {
        return;
    }
    uint16_t chunk = ctl_mtu > 3 ? ctl_mtu - 3 : 20;
    if (chunk > CTL_VAL_MAX) {
        chunk = CTL_VAL_MAX;
    }
    for (uint16_t off = 0; off < len; off += chunk) {
        uint16_t n = len - off;
        if (n > chunk) {
            n = chunk;
        }
        esp_ble_gatts_send_indicate(ctl_gatts_if, ctl_conn_id, handles[SCURRY_IDX_EVENT_VAL], n,
                                    (uint8_t *)data + off, false);
    }
}

void scurry_ctl_svc_gatts_event(esp_gatts_cb_event_t event, esp_gatt_if_t gatts_if,
                                esp_ble_gatts_cb_param_t *param)
{
    switch (event) {
    case ESP_GATTS_REG_EVT: {
        ESP_LOGI(TAG, "REG_EVT app_id=%04x status=%d if=%d", param->reg.app_id, param->reg.status,
                 gatts_if);
        if (param->reg.app_id != SCURRY_CTL_APP_ID) {
            break;
        }
        ctl_gatts_if = gatts_if;
        esp_err_t err = esp_ble_gatts_create_attr_tab(scurry_ctl_db, gatts_if, SCURRY_IDX_NB, 0);
        ESP_LOGI(TAG, "create_attr_tab -> %s", esp_err_to_name(err));
        break;
    }

    case ESP_GATTS_CREAT_ATTR_TAB_EVT:
        if (param->add_attr_tab.status != ESP_GATT_OK ||
            param->add_attr_tab.num_handle != SCURRY_IDX_NB) {
            ESP_LOGE(TAG, "control service attribute table failed (status %d, %d handles)",
                     param->add_attr_tab.status, param->add_attr_tab.num_handle);
            break;
        }
        memcpy(handles, param->add_attr_tab.handles, sizeof(handles));
        esp_ble_gatts_start_service(handles[SCURRY_IDX_SVC]);
        ESP_LOGI(TAG, "control service started");
        break;

    case ESP_GATTS_WRITE_EVT: {
        if (param->write.is_prep) {
            /* Frames are reassembled from the byte stream, so a long write is
             * never needed. Refusing it is better than half-supporting it. */
            break;
        }
        if (param->write.handle == handles[SCURRY_IDX_EVENT_CCC] && param->write.len == 2) {
            bool on = (param->write.value[0] | (param->write.value[1] << 8)) != 0;
            if (on && !authorised(param->write.bda)) {
                ESP_LOGW(TAG,
                         "refusing %02x:%02x:%02x:%02x:%02x:%02x -- not the pinned controller "
                         "and no pairing window is open",
                         param->write.bda[0], param->write.bda[1], param->write.bda[2],
                         param->write.bda[3], param->write.bda[4], param->write.bda[5]);
                esp_ble_gatts_close(gatts_if, param->write.conn_id);
                break;
            }
            ctl_subscribed = on;
            if (on) {
                ctl_have_conn = true;
                ctl_conn_id = param->write.conn_id;
                memcpy(ctl_bda, param->write.bda, sizeof(esp_bd_addr_t));
                ESP_LOGI(TAG, "wireless controller subscribed (conn %d)", param->write.conn_id);
            }
            break;
        }

        if (param->write.handle == handles[SCURRY_IDX_CONTROL_VAL]) {
            if (!authorised(param->write.bda)) {
                esp_ble_gatts_close(gatts_if, param->write.conn_id);
                break;
            }
            /* Record the connection here too: a controller that writes before
             * subscribing is still the controller. */
            ctl_have_conn = true;
            ctl_conn_id = param->write.conn_id;
            memcpy(ctl_bda, param->write.bda, sizeof(esp_bd_addr_t));
            if (rx_cb && param->write.len > 0) {
                rx_cb(param->write.value, param->write.len);
            }
        }
        break;
    }

    case ESP_GATTS_MTU_EVT:
        ctl_mtu = param->mtu.mtu;
        ESP_LOGI(TAG, "ATT MTU is %u", (unsigned)ctl_mtu);
        break;

    case ESP_GATTS_DISCONNECT_EVT:
        scurry_ctl_svc_on_disconnect(param->disconnect.remote_bda);
        break;

    default:
        break;
    }
}
