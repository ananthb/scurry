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

/* Negotiated ATT MTU, which bounds a single notification at mtu-3. Frames
 * larger than that are split; the controller reassembles from the byte stream
 * exactly as the dongle does for writes coming the other way. */
static uint16_t ctl_mtu = 23;

/* Connected controllers.
 *
 * Nothing is recorded on connect: at connect time there is no way to tell a
 * controller from a target, since both are just centrals opening a link. An
 * entry appears once a peer proves it is authorised, by subscribing or writing. */
struct ctl_conn {
    bool used;
    bool subscribed;
    uint16_t conn_id;
    esp_bd_addr_t bda;
};
static struct ctl_conn conns[SCURRY_MAX_CONTROLLERS];

/* Which of them is driving, or -1. */
static int active = -1;
/* Bumped on every change of driver; see the header. */
static uint32_t generation;
/* The connection whose request is being handled, so an answer goes back to
 * whoever asked rather than to whoever happens to be driving. */
static int replying_to = -1;

/* Authorised controllers, whether or not they are here. */
static esp_bd_addr_t pins[SCURRY_MAX_CONTROLLERS];
static int pin_count;
static int64_t pairing_until_us;

static bool same(const esp_bd_addr_t a, const esp_bd_addr_t b)
{
    return memcmp(a, b, sizeof(esp_bd_addr_t)) == 0;
}

static void pins_save(void)
{
    nvs_handle_t h;
    if (nvs_open(NVS_NAMESPACE, NVS_READWRITE, &h) != ESP_OK) {
        ESP_LOGW(TAG, "could not open NVS to store controller addresses");
        return;
    }
    if (pin_count == 0) {
        nvs_erase_key(h, NVS_KEY_CTL_BDA);
    } else {
        nvs_set_blob(h, NVS_KEY_CTL_BDA, pins, pin_count * sizeof(esp_bd_addr_t));
    }
    nvs_commit(h);
    nvs_close(h);
}

static void pins_load(void)
{
    nvs_handle_t h;
    if (nvs_open(NVS_NAMESPACE, NVS_READONLY, &h) != ESP_OK) {
        return;
    }
    size_t len = sizeof(pins);
    if (nvs_get_blob(h, NVS_KEY_CTL_BDA, pins, &len) == ESP_OK) {
        /* A six-byte blob is what earlier firmware wrote for its single
         * controller. It decodes here as one entry, so an existing pairing
         * survives the upgrade rather than silently needing to be redone. */
        pin_count = (int)(len / sizeof(esp_bd_addr_t));
        if (pin_count > SCURRY_MAX_CONTROLLERS) {
            pin_count = SCURRY_MAX_CONTROLLERS;
        }
        for (int i = 0; i < pin_count; i++) {
            ESP_LOGI(TAG, "controller %d authorised: %02x:%02x:%02x:%02x:%02x:%02x", i, pins[i][0],
                     pins[i][1], pins[i][2], pins[i][3], pins[i][4], pins[i][5]);
        }
    }
    nvs_close(h);
}

bool scurry_ctl_svc_is_pinned(const esp_bd_addr_t bda)
{
    for (int i = 0; i < pin_count; i++) {
        if (same(pins[i], bda)) {
            return true;
        }
    }
    return false;
}

int scurry_ctl_svc_pin_count(void)
{
    return pin_count;
}

bool scurry_ctl_svc_pin_at(int index, esp_bd_addr_t out)
{
    if (index < 0 || index >= pin_count) {
        return false;
    }
    memcpy(out, pins[index], sizeof(esp_bd_addr_t));
    return true;
}

static bool pairing_open(void)
{
    return pairing_until_us != 0 && esp_timer_get_time() < pairing_until_us;
}

void scurry_ctl_svc_open_pairing(uint32_t seconds)
{
    pairing_until_us = esp_timer_get_time() + (int64_t)seconds * 1000000;
    ESP_LOGI(TAG, "pairing window open for %us -- the next controller to turn up is authorised",
             (unsigned)seconds);
}

void scurry_ctl_svc_close_pairing(void)
{
    if (pairing_until_us != 0) {
        pairing_until_us = 0;
        ESP_LOGI(TAG, "pairing window closed");
    }
}

uint32_t scurry_ctl_svc_pairing_remaining(void)
{
    if (!pairing_open()) {
        return 0;
    }
    return (uint32_t)((pairing_until_us - esp_timer_get_time()) / 1000000) + 1;
}

void scurry_ctl_svc_forget(void)
{
    pin_count = 0;
    memset(pins, 0, sizeof(pins));
    pairing_until_us = 0;
    memset(conns, 0, sizeof(conns));
    active = -1;
    generation++;
    pins_save();
    ESP_LOGI(TAG, "all wireless controllers forgotten");
}

/* Whether this peer may drive the machine.
 *
 * Encryption alone is not enough: Just Works bonding means anything in range can
 * hold an encrypted link. The address must also be authorised, and the only way
 * to become authorised is to turn up while a window is open -- which takes
 * either the dongle's button or the cable. */
static bool authorised(const esp_bd_addr_t bda)
{
    if (scurry_ctl_svc_is_pinned(bda)) {
        return true;
    }
    if (!pairing_open()) {
        return false;
    }
    if (pin_count >= SCURRY_MAX_CONTROLLERS) {
        /* Refuse rather than evict. Silently dropping whichever controller
           happened to be oldest would take the machine somebody is sitting at
           out from under them. */
        ESP_LOGW(TAG, "pairing window open but all %d controller slots are taken; forget one first",
                 SCURRY_MAX_CONTROLLERS);
        return false;
    }
    memcpy(pins[pin_count++], bda, sizeof(esp_bd_addr_t));
    pins_save();
    scurry_ctl_svc_close_pairing();
    ESP_LOGI(TAG, "controller %02x:%02x:%02x:%02x:%02x:%02x authorised (%d of %d)", bda[0], bda[1],
             bda[2], bda[3], bda[4], bda[5], pin_count, SCURRY_MAX_CONTROLLERS);
    return true;
}

/* Find or make this peer's connection entry. */
static int conn_slot(uint16_t conn_id, const esp_bd_addr_t bda)
{
    for (int i = 0; i < SCURRY_MAX_CONTROLLERS; i++) {
        if (conns[i].used && same(conns[i].bda, bda)) {
            conns[i].conn_id = conn_id;
            return i;
        }
    }
    for (int i = 0; i < SCURRY_MAX_CONTROLLERS; i++) {
        if (!conns[i].used) {
            conns[i].used = true;
            conns[i].subscribed = false;
            conns[i].conn_id = conn_id;
            memcpy(conns[i].bda, bda, sizeof(esp_bd_addr_t));
            return i;
        }
    }
    return -1;
}

/* Hand the wheel to this controller.
 *
 * Driven by writing, not by connecting or subscribing. That distinction is the
 * whole point: macOS reconnects bonded devices in the background, so a laptop
 * waking in another room would otherwise take control away from the phone in
 * your hand without anybody touching anything. Whoever is actually sending
 * input is the one driving. */
static void make_active(int slot)
{
    if (slot < 0 || active == slot) {
        return;
    }
    if (active >= 0) {
        const uint8_t *b = conns[slot].bda;
        ESP_LOGI(TAG, "controller %02x:%02x:%02x:%02x:%02x:%02x took over", b[0], b[1], b[2], b[3],
                 b[4], b[5]);
    }
    active = slot;
    generation++;
}

uint32_t scurry_ctl_svc_generation(void)
{
    return generation;
}

void scurry_ctl_svc_init(scurry_ctl_rx_cb_t on_rx)
{
    rx_cb = on_rx;
    pins_load();
}

bool scurry_ctl_svc_ready(void)
{
    return active >= 0 && conns[active].used && conns[active].subscribed;
}

bool scurry_ctl_svc_active_bda(esp_bd_addr_t out)
{
    if (active < 0 || !conns[active].used) {
        return false;
    }
    memcpy(out, conns[active].bda, sizeof(esp_bd_addr_t));
    return true;
}

void scurry_ctl_svc_on_disconnect(esp_bd_addr_t bda)
{
    for (int i = 0; i < SCURRY_MAX_CONTROLLERS; i++) {
        if (conns[i].used && same(conns[i].bda, bda)) {
            conns[i].used = false;
            conns[i].subscribed = false;
            if (active == i) {
                active = -1;
                generation++;
                ESP_LOGI(TAG, "the driving controller disconnected");
            }
            return;
        }
    }
}

/* Send to one connection, splitting at the negotiated MTU. */
static void notify_slot(int slot, const uint8_t *data, uint16_t len)
{
    if (slot < 0 || !conns[slot].used || !conns[slot].subscribed ||
        ctl_gatts_if == ESP_GATT_IF_NONE || len == 0) {
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
        esp_ble_gatts_send_indicate(ctl_gatts_if, conns[slot].conn_id,
                                    handles[SCURRY_IDX_EVENT_VAL], n, (uint8_t *)data + off, false);
    }
}

void scurry_ctl_svc_notify(const uint8_t *data, uint16_t len)
{
    notify_slot(active, data, len);
}

void scurry_ctl_svc_reply(const uint8_t *data, uint16_t len)
{
    notify_slot(replying_to >= 0 ? replying_to : active, data, len);
}

void scurry_ctl_svc_gatts_event(esp_gatts_cb_event_t event, esp_gatt_if_t gatts_if,
                                esp_ble_gatts_cb_param_t *param)
{
    switch (event) {
    case ESP_GATTS_REG_EVT: {
        if (param->reg.app_id != SCURRY_CTL_APP_ID) {
            break;
        }
        ctl_gatts_if = gatts_if;
        esp_err_t err = esp_ble_gatts_create_attr_tab(scurry_ctl_db, gatts_if, SCURRY_IDX_NB, 0);
        if (err != ESP_OK) {
            ESP_LOGE(TAG, "create_attr_tab -> %s", esp_err_to_name(err));
        }
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

        bool ours = param->write.handle == handles[SCURRY_IDX_EVENT_CCC] ||
                    param->write.handle == handles[SCURRY_IDX_CONTROL_VAL];
        if (!ours) {
            break;
        }
        if (!authorised(param->write.bda)) {
            ESP_LOGW(TAG,
                     "refusing %02x:%02x:%02x:%02x:%02x:%02x -- not an authorised controller "
                     "and no pairing window is open",
                     param->write.bda[0], param->write.bda[1], param->write.bda[2],
                     param->write.bda[3], param->write.bda[4], param->write.bda[5]);
            esp_ble_gatts_close(gatts_if, param->write.conn_id);
            break;
        }

        int slot = conn_slot(param->write.conn_id, param->write.bda);
        if (slot < 0) {
            ESP_LOGW(TAG, "no room to track another controller connection");
            break;
        }

        if (param->write.handle == handles[SCURRY_IDX_EVENT_CCC]) {
            if (param->write.len < 2) {
                break;
            }
            conns[slot].subscribed = (param->write.value[0] | (param->write.value[1] << 8)) != 0;
            /* Subscribing does not take the wheel; writing does. But a lone
               controller should not have to send input before it can be told
               anything, so the first one to arrive drives by default. */
            if (conns[slot].subscribed && active < 0) {
                make_active(slot);
            }
            ESP_LOGI(TAG, "controller %s (conn %d)",
                     conns[slot].subscribed ? "subscribed" : "unsubscribed", param->write.conn_id);
            break;
        }

        /* A write is somebody driving. */
        make_active(slot);
        replying_to = slot;
        if (rx_cb && param->write.len > 0) {
            rx_cb(param->write.value, param->write.len);
        }
        replying_to = -1;
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
