/*
 * SPDX-FileCopyrightText: 2021-2025 Espressif Systems (Shanghai) CO LTD
 *
 * SPDX-License-Identifier: Unlicense OR CC0-1.0
 */

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include "freertos/FreeRTOS.h"
#include "freertos/task.h"
#include "freertos/event_groups.h"
#include "esp_system.h"
#include "esp_wifi.h"
#include "esp_event.h"
#include "esp_log.h"
#include "nvs_flash.h"
#include "esp_bt.h"

#include "esp_hidd_prf_api.h"
#include "esp_bt_defs.h"
#include "esp_gap_ble_api.h"
#include "esp_gatts_api.h"
#include "esp_gatt_defs.h"
#include "esp_bt_main.h"
#include "esp_bt_device.h"
#include "driver/gpio.h"
#include "hid_dev.h"
#include "driver/usb_serial_jtag.h"
#include "esp_mac.h"
#include "nvs.h"
#include "scurry_ffi.h"

/**
 * Brief:
 * This example Implemented BLE HID device profile related functions, in which the HID device
 * has 4 Reports (1 is mouse, 2 is keyboard and LED, 3 is Consumer Devices, 4 is Vendor devices).
 * Users can choose different reports according to their own application scenarios.
 * BLE HID profile inheritance and USB HID class.
 */

/**
 * Note:
 * 1. Win10 does not support vendor report , So SUPPORT_REPORT_VENDOR is always set to FALSE, it defines in hidd_le_prf_int.h
 * 2. Update connection parameters are not allowed during iPhone HID encryption, slave turns
 * off the ability to automatically update connection parameters during encryption.
 * 3. After our HID device is connected, the iPhones write 1 to the Report Characteristic Configuration Descriptor,
 * even if the HID encryption is not completed. This should actually be written 1 after the HID encryption is completed.
 * we modify the permissions of the Report Characteristic Configuration Descriptor to `ESP_GATT_PERM_READ | ESP_GATT_PERM_WRITE_ENCRYPTED`.
 * if you got `GATT_INSUF_ENCRYPTION` error, please ignore.
 */

#define HID_DEMO_TAG "HID_DEMO"


static uint16_t hid_conn_id = 0;

/* scurry spike: the demo kept exactly one conn_id and overwrote it on every
   connect, so a second host silently displaced the first. Track them all --
   the point of this spike is to see how many survive at once. */
#define SCURRY_MAX_HOSTS 4
static uint16_t      scurry_conns[SCURRY_MAX_HOSTS];
static bool          scurry_conn_used[SCURRY_MAX_HOSTS];
/* The disconnect event reports only remote_bda, never conn_id (connect gives
   both). So the peer address is the only key that works across both events,
   and it has to be stored on connect to be usable on disconnect. */
static esp_bd_addr_t scurry_conn_bda[SCURRY_MAX_HOSTS];

static int scurry_conn_count(void)
{
    int n = 0;
    for (int i = 0; i < SCURRY_MAX_HOSTS; i++) {
        if (scurry_conn_used[i]) n++;
    }
    return n;
}

/* Recompute which nodes the layout may route to. Node ids are slot + 1. */
static void scurry_publish_available(void)
{
    uint32_t mask = 1; /* node 0, the controller's own screen, always */
    for (int i = 0; i < SCURRY_MAX_HOSTS; i++) {
        if (scurry_conn_used[i]) {
            mask |= 1u << (i + 1);
        }
    }
    scurry_layout_set_available(mask);
}

static void scurry_conn_add(uint16_t conn_id, esp_bd_addr_t bda)
{
    for (int i = 0; i < SCURRY_MAX_HOSTS; i++) {
        if (!scurry_conn_used[i]) {
            scurry_conn_used[i] = true;
            scurry_conns[i] = conn_id;
            memcpy(scurry_conn_bda[i], bda, sizeof(esp_bd_addr_t));
            scurry_publish_available();
            return;
        }
    }
    ESP_LOGW(HID_DEMO_TAG, "scurry: connection table full, dropping conn_id %d", conn_id);
}

static void scurry_conn_remove(esp_bd_addr_t bda)
{
    for (int i = 0; i < SCURRY_MAX_HOSTS; i++) {
        if (scurry_conn_used[i] &&
            memcmp(scurry_conn_bda[i], bda, sizeof(esp_bd_addr_t)) == 0) {
            scurry_conn_used[i] = false;
            scurry_publish_available();
            return;
        }
    }
}
static bool sec_conn = false;
static bool send_volum_up = false;
#define CHAR_DECLARATION_SIZE   (sizeof(uint8_t))

static void hidd_event_callback(esp_hidd_cb_event_t event, esp_hidd_cb_param_t *param);

/* Advertised as "Scurry XXXX", where XXXX is derived from this board's BT MAC.
   Stable across reboots and reflashes, distinct per board, so several dongles
   are tellable apart in a Bluetooth picker. Filled in by
   scurry_make_device_name() before the name is ever set. */
static char HIDD_DEVICE_NAME[16] = "Scurry";

static void scurry_make_device_name(void)
{
    uint8_t mac[6] = {0};
    if (esp_read_mac(mac, ESP_MAC_BT) != ESP_OK) {
        ESP_LOGW(HID_DEMO_TAG, "scurry: no BT MAC, name stays %s", HIDD_DEVICE_NAME);
        return;
    }

    /* FNV-1a over all six bytes rather than just printing the tail. Espressif's
       OUI occupies the first three, so slicing the MAC directly would leave
       boards differing only in the last character or two. Hashing spreads the
       difference across every position. */
    uint32_t h = 2166136261u;
    for (int i = 0; i < 6; i++) {
        h ^= mac[i];
        h *= 16777619u;
    }

    /* Crockford base32: no I, L, O or U, so the id cannot be misread or
       mistyped when someone reads it off a screen. 4 chars = 20 bits. */
    static const char alpha[] = "0123456789ABCDEFGHJKMNPQRSTVWXYZ";
    char id[5];
    for (int i = 0; i < 4; i++) {
        id[i] = alpha[(h >> (i * 5)) & 0x1F];
    }
    id[4] = '\0';

    snprintf(HIDD_DEVICE_NAME, sizeof(HIDD_DEVICE_NAME), "Scurry %s", id);
    ESP_LOGI(HID_DEMO_TAG, "scurry: advertising as \"%s\" (mac %02x:%02x:%02x:%02x:%02x:%02x)",
             HIDD_DEVICE_NAME, mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]);
}
static uint8_t hidd_service_uuid128[] = {
    /* LSB <--------------------------------------------------------------------------------> MSB */
    //first uuid, 16bit, [12],[13] is the value
    0xfb, 0x34, 0x9b, 0x5f, 0x80, 0x00, 0x00, 0x80, 0x00, 0x10, 0x00, 0x00, 0x12, 0x18, 0x00, 0x00,
};

static esp_ble_adv_data_t hidd_adv_data = {
    .set_scan_rsp = false,
    .include_name = true,
    .include_txpower = true,
    .min_interval = 0x0006, //slave connection min interval, Time = min_interval * 1.25 msec
    .max_interval = 0x0010, //slave connection max interval, Time = max_interval * 1.25 msec
    /* 0x03C2 = HID Mouse. Was 0x03C0 (Generic HID), which leaves the host to
       pick an icon for an unknown HID device -- ChromeOS renders that as a game
       controller. Must match the GAP local icon set in hid_device_le_prf.c:
       hosts differ over whether they trust the advertisement or the connected
       device's GAP service, and a mismatch shows one icon while scanning and
       another once paired. */
    .appearance = ESP_BLE_APPEARANCE_HID_MOUSE,
    .manufacturer_len = 0,
    .p_manufacturer_data =  NULL,
    .service_data_len = 0,
    .p_service_data = NULL,
    .service_uuid_len = sizeof(hidd_service_uuid128),
    .p_service_uuid = hidd_service_uuid128,
    .flag = 0x6,
};

static esp_ble_adv_params_t hidd_adv_params = {
    .adv_int_min        = 0x20,
    .adv_int_max        = 0x30,
    .adv_type           = ADV_TYPE_IND,
    .own_addr_type      = BLE_ADDR_TYPE_PUBLIC,
    //.peer_addr            =
    //.peer_addr_type       =
    .channel_map        = ADV_CHNL_ALL,
    .adv_filter_policy = ADV_FILTER_ALLOW_SCAN_ANY_CON_ANY,
};


static void hidd_event_callback(esp_hidd_cb_event_t event, esp_hidd_cb_param_t *param)
{
    switch(event) {
        case ESP_HIDD_EVENT_REG_FINISH: {
            if (param->init_finish.state == ESP_HIDD_INIT_OK) {
                //esp_bd_addr_t rand_addr = {0x04,0x11,0x11,0x11,0x11,0x05};
                esp_ble_gap_set_device_name(HIDD_DEVICE_NAME);
                esp_ble_gap_config_adv_data(&hidd_adv_data);

            }
            break;
        }
        case ESP_BAT_EVENT_REG: {
            break;
        }
        case ESP_HIDD_EVENT_DEINIT_FINISH:
	     break;
		case ESP_HIDD_EVENT_BLE_CONNECT: {
            ESP_LOGI(HID_DEMO_TAG, "ESP_HIDD_EVENT_BLE_CONNECT");
            hid_conn_id = param->connect.conn_id;
            scurry_conn_add(param->connect.conn_id, param->connect.remote_bda);
            ESP_LOGW(HID_DEMO_TAG, "scurry: conn_id=%d, %d host(s) connected",
                     param->connect.conn_id, scurry_conn_count());
            /* Keep advertising so a second target can find and bond with us.
               Without this the dongle stops being discoverable at one host and
               could never reach a second -- which would fail the spike for a
               reason that has nothing to do with whether BLE supports it. */
            if (scurry_conn_count() < SCURRY_MAX_HOSTS) {
                esp_ble_gap_start_advertising(&hidd_adv_params);
            }
            break;
        }
        case ESP_HIDD_EVENT_BLE_DISCONNECT: {
            sec_conn = false;
            ESP_LOGI(HID_DEMO_TAG, "ESP_HIDD_EVENT_BLE_DISCONNECT");
            scurry_conn_remove(param->disconnect.remote_bda);
            ESP_LOGW(HID_DEMO_TAG, "scurry: disconnect, %d host(s) remain",
                     scurry_conn_count());
            esp_ble_gap_start_advertising(&hidd_adv_params);
            break;
        }
        case ESP_HIDD_EVENT_BLE_VENDOR_REPORT_WRITE_EVT: {
            ESP_LOGI(HID_DEMO_TAG, "%s, ESP_HIDD_EVENT_BLE_VENDOR_REPORT_WRITE_EVT", __func__);
            ESP_LOG_BUFFER_HEX(HID_DEMO_TAG, param->vendor_write.data, param->vendor_write.length);
            break;
        }
        case ESP_HIDD_EVENT_BLE_LED_REPORT_WRITE_EVT: {
            ESP_LOGI(HID_DEMO_TAG, "ESP_HIDD_EVENT_BLE_LED_REPORT_WRITE_EVT");
            ESP_LOG_BUFFER_HEX(HID_DEMO_TAG, param->led_write.data, param->led_write.length);
            break;
        }
        default:
            break;
    }
    return;
}

static void gap_event_handler(esp_gap_ble_cb_event_t event, esp_ble_gap_cb_param_t *param)
{
    switch (event) {
    case ESP_GAP_BLE_ADV_DATA_SET_COMPLETE_EVT:
        esp_ble_gap_start_advertising(&hidd_adv_params);
        break;
     case ESP_GAP_BLE_SEC_REQ_EVT:
        for(int i = 0; i < ESP_BD_ADDR_LEN; i++) {
             ESP_LOGD(HID_DEMO_TAG, "%x:",param->ble_security.ble_req.bd_addr[i]);
        }
        esp_ble_gap_security_rsp(param->ble_security.ble_req.bd_addr, true);
	 break;
     case ESP_GAP_BLE_AUTH_CMPL_EVT:
        esp_bd_addr_t bd_addr;
        memcpy(bd_addr, param->ble_security.auth_cmpl.bd_addr, sizeof(esp_bd_addr_t));
        ESP_LOGI(HID_DEMO_TAG, "remote BD_ADDR: %08x%04x",\
                (bd_addr[0] << 24) + (bd_addr[1] << 16) + (bd_addr[2] << 8) + bd_addr[3],
                (bd_addr[4] << 8) + bd_addr[5]);
        ESP_LOGI(HID_DEMO_TAG, "address type = %d", param->ble_security.auth_cmpl.addr_type);
        ESP_LOGI(HID_DEMO_TAG, "pair status = %s",param->ble_security.auth_cmpl.success ? "success" : "fail");
        if (param->ble_security.auth_cmpl.success) {
            sec_conn = true;
            ESP_LOGI(HID_DEMO_TAG, "secure connection established.");
        } else {
            ESP_LOGE(HID_DEMO_TAG, "pairing failed, reason = 0x%x",
                     param->ble_security.auth_cmpl.fail_reason);
        }
        break;
    default:
        break;
    }
}

/* ---------------------------------------------------------------------------
 * scurry protocol v2
 *
 * 8-byte header then a length-prefixed payload. The controller sends *raw*
 * pointer motion and has no opinion about which machine it lands on -- routing
 * happens here, through the Rust layout engine in scurry-ffi, so the same
 * tested code runs whether or not a controller exists at all.
 *
 * The wire format is defined in crates/scurry-proto and described in
 * doc/protocol.md. This file must not drift from it.
 *
 * Logs share the pipe in the other direction. That is fine -- it is full duplex,
 * and the host resynchronises on the magic byte.
 * ------------------------------------------------------------------------- */


#define SCURRY_MAGIC        0x53
#define SCURRY_VERSION      2
#define SCURRY_HEADER_LEN   8
#define SCURRY_MAX_PAYLOAD  256

#define SCURRY_KIND_MOUSE       0x01
#define SCURRY_KIND_PING        0x04
#define SCURRY_KIND_PONG        0x05
#define SCURRY_KIND_FOCUS       0x06
#define SCURRY_KIND_GET_CONFIG  0x10
#define SCURRY_KIND_CONFIG      0x11
#define SCURRY_KIND_SET_CONFIG  0x12
#define SCURRY_KIND_GET_STATUS  0x13
#define SCURRY_KIND_STATUS      0x14
#define SCURRY_KIND_ACK         0x15

#define SCURRY_ACK_OK              0
#define SCURRY_ACK_BAD_REQUEST     1
#define SCURRY_ACK_INVALID_LAYOUT  2
#define SCURRY_ACK_STORAGE_FAILED  3

#define SCURRY_NVS_NAMESPACE  "scurry"
#define SCURRY_NVS_KEY_LAYOUT "layout"

static uint16_t scurry_tx_seq = 0;
static uint8_t  scurry_focus_node = 0;

static bool     scurry_seq_seen = false;
static uint16_t scurry_seq_last = 0;
static uint8_t  scurry_seq_rejected = 0;

/* Consecutive rejections after which the gate re-anchors. A restarted
   controller counts from zero again while we still remember a high sequence
   from the previous session, so every message looks like an ancient straggler
   and input dies silently until the counter climbs past the old value --
   thousands of messages later. Real reordering is a handful at most, so a
   sustained run of rejects means the peer restarted. */
#define SCURRY_SEQ_RESYNC_AFTER 8

/* Sequence numbers wrap at u16, so "newer" is signed distance on the wrapped
   circle, not `>`. A naive comparison would stall the stream for 32k messages
   every time the counter wrapped. Mirrors SeqGate in scurry-proto. */
static bool scurry_seq_accept(uint16_t seq)
{
    if (!scurry_seq_seen) {
        scurry_seq_seen = true;
        scurry_seq_last = seq;
        scurry_seq_rejected = 0;
        return true;
    }
    if ((int16_t)(seq - scurry_seq_last) > 0) {
        scurry_seq_last = seq;
        scurry_seq_rejected = 0;
        return true;
    }
    if (scurry_seq_rejected >= SCURRY_SEQ_RESYNC_AFTER) {
        ESP_LOGW(HID_DEMO_TAG, "scurry: controller restarted, re-anchoring at seq %u", seq);
        scurry_seq_last = seq;
        scurry_seq_rejected = 0;
        return true;
    }
    scurry_seq_rejected++;
    return false;
}

static void scurry_send(uint8_t kind, const uint8_t *payload, uint16_t len)
{
    uint8_t header[SCURRY_HEADER_LEN] = {0};
    header[0] = SCURRY_MAGIC;
    header[1] = SCURRY_VERSION;
    header[2] = kind;
    header[4] = (uint8_t)(scurry_tx_seq & 0xFF);
    header[5] = (uint8_t)(scurry_tx_seq >> 8);
    header[6] = (uint8_t)(len & 0xFF);
    header[7] = (uint8_t)(len >> 8);
    scurry_tx_seq++;

    usb_serial_jtag_write_bytes(header, sizeof(header), 100 / portTICK_PERIOD_MS);
    if (len > 0 && payload != NULL) {
        usb_serial_jtag_write_bytes(payload, len, 100 / portTICK_PERIOD_MS);
    }
}

static void scurry_send_ack(uint8_t code)
{
    scurry_send(SCURRY_KIND_ACK, &code, 1);
}

/* Tell the controller where the pointer went. Without this it cannot know
   whether to swallow local input, because it no longer owns the layout. */
static void scurry_announce_focus(uint8_t node)
{
    if (node == scurry_focus_node) {
        return;
    }
    scurry_focus_node = node;
    scurry_send(SCURRY_KIND_FOCUS, &node, 1);
    ESP_LOGI(HID_DEMO_TAG, "scurry: focus -> node %d", node);
}

static esp_err_t scurry_nvs_save(const uint8_t *buf, size_t len)
{
    nvs_handle_t h;
    esp_err_t err = nvs_open(SCURRY_NVS_NAMESPACE, NVS_READWRITE, &h);
    if (err != ESP_OK) {
        return err;
    }

    /* Skip the write if nothing changed. Flash endurance is not a real concern
       at human rates -- a 67-byte blob across a 24KB wear-levelled partition is
       millions of writes -- but anything that ends up storing per-frame, such
       as a calibration loop, would chew through it. Comparing first costs a
       read and removes the whole class of problem. */
    uint8_t existing[SCURRY_MAX_PAYLOAD];
    size_t existing_len = sizeof(existing);
    if (nvs_get_blob(h, SCURRY_NVS_KEY_LAYOUT, existing, &existing_len) == ESP_OK &&
        existing_len == len && memcmp(existing, buf, len) == 0) {
        nvs_close(h);
        ESP_LOGI(HID_DEMO_TAG, "scurry: layout unchanged, not rewriting");
        return ESP_OK;
    }

    err = nvs_set_blob(h, SCURRY_NVS_KEY_LAYOUT, buf, len);
    if (err == ESP_OK) {
        err = nvs_commit(h);
    }
    nvs_close(h);
    return err;
}

/* Restore the stored layout at boot. Absent storage is not an error: a dongle
   that has never been configured simply has nowhere to route yet, and says so
   rather than inventing a layout. */
static void scurry_nvs_load(void)
{
    nvs_handle_t h;
    if (nvs_open(SCURRY_NVS_NAMESPACE, NVS_READONLY, &h) != ESP_OK) {
        ESP_LOGI(HID_DEMO_TAG, "scurry: no stored layout yet");
        return;
    }
    uint8_t buf[SCURRY_MAX_PAYLOAD];
    size_t len = sizeof(buf);
    esp_err_t err = nvs_get_blob(h, SCURRY_NVS_KEY_LAYOUT, buf, &len);
    nvs_close(h);
    if (err != ESP_OK) {
        ESP_LOGI(HID_DEMO_TAG, "scurry: no stored layout yet");
        return;
    }
    int32_t rc = scurry_layout_load(buf, len);
    if (rc != 0) {
        ESP_LOGW(HID_DEMO_TAG, "scurry: stored layout rejected (%ld)", (long)rc);
        return;
    }
    ESP_LOGI(HID_DEMO_TAG, "scurry: layout restored from NVS (%u bytes)", (unsigned)len);
    /* Nothing is connected yet at boot; the layout must not route anywhere. */
    scurry_publish_available();
}

/* Map a scurry node id onto a live BLE connection. Node 0 is the controller's
   own screen and is never transmitted, so node N addresses slot N-1. Reserving
   0 means a zeroed or truncated message cannot address a real target. */
static int scurry_conn_for_node(uint8_t node)
{
    if (node == 0 || node > SCURRY_MAX_HOSTS) {
        return -1;
    }
    uint8_t slot = node - 1;
    return scurry_conn_used[slot] ? (int)scurry_conns[slot] : -1;
}

static void scurry_handle_mouse(const uint8_t *p, uint16_t len)
{
    if (len < 8) {
        return;
    }
    uint8_t buttons = p[0];
    int16_t dx = (int16_t)((uint16_t)p[1] | ((uint16_t)p[2] << 8));
    int16_t dy = (int16_t)((uint16_t)p[3] | ((uint16_t)p[4] << 8));
    int8_t  wheel = (int8_t)p[5];
    int8_t  pan   = (int8_t)p[6];
    /* dx/dy feed the layout only. What goes out is the absolute position the
       layout computes, never the raw delta. */

    if (!scurry_layout_ready()) {
        return; /* nowhere to route: drop rather than guess */
    }

    scurry_route_t route;
    if (scurry_layout_advance(dx, dy, &route) != 0) {
        return;
    }

    if (route.crossed) {
        /* Release on the machine being left before the pointer arrives
           anywhere else, or a drag across the boundary strands a held button
           on a machine the user is no longer sitting at. */
        int from_conn = scurry_conn_for_node(route.from);
        if (from_conn >= 0) {
            esp_hidd_send_mouse_report((uint16_t)from_conn, 0, 0, 0, 0, 0);
        }
        scurry_announce_focus(route.to);

        /* Re-anchor. With relative motion the dongle is dead reckoning, and its
           model drifts: the target applies its own pointer acceleration to our
           deltas, and anything else touching that machine moves the pointer
           without telling us. Slamming hard against the arrival edge pins the
           pointer somewhere known -- the target clamps at its own boundary --
           so every crossing starts from a fixed reference instead of
           accumulating error forever. */
        int to_conn = scurry_conn_for_node(route.to);
        if (to_conn >= 0) {
            int16_t sx = 0, sy = 0;
            switch (route.edge) {
            case 0: sx = -20000; break;  /* arriving at the left edge   */
            case 1: sx =  20000; break;  /* ... right                   */
            case 2: sy = -20000; break;  /* ... top                     */
            default: sy = 20000; break;  /* ... bottom                  */
            }
            for (int i = 0; i < 3; i++) {
                esp_hidd_send_mouse_report((uint16_t)to_conn, 0, sx, sy, 0, 0);
            }
            ESP_LOGI(HID_DEMO_TAG, "scurry: re-anchored node %d against edge %d",
                     route.to, route.edge);
        }
    }

    /* Node 0 is the controller's own screen. Sending anything here would mean
       two pointers moving at once, which must never happen. */
    if (route.to == 0) {
        return;
    }

    int conn = scurry_conn_for_node(route.to);
    if (conn < 0) {
        static uint32_t unrouted = 0;
        if ((unrouted++ % 120) == 0) {
            ESP_LOGW(HID_DEMO_TAG, "scurry: node %d has no connection", route.to);
        }
        return;
    }

    /* Throttled: enough to tell "not sending" from "sending and ignored",
       without flooding the link we are also using for messages. */
    static uint32_t sent = 0;
    if ((sent++ % 120) == 0) {
        ESP_LOGI(HID_DEMO_TAG, "scurry: report -> node %d conn %d d(%d,%d) btn %02x",
                 route.to, conn, dx, dy, buttons);
    }
    esp_hidd_send_mouse_report((uint16_t)conn, buttons, dx, dy, wheel, pan);
}

static void scurry_send_config(void)
{
    uint8_t buf[SCURRY_MAX_PAYLOAD];
    int32_t n = scurry_layout_save(buf, sizeof(buf));
    if (n < 0) {
        /* Not configured yet: an empty config is a valid answer, and lets the
           controller distinguish "none stored" from "link is broken". */
        uint8_t empty = 0;
        scurry_send(SCURRY_KIND_CONFIG, &empty, 1);
        return;
    }
    scurry_send(SCURRY_KIND_CONFIG, buf, (uint16_t)n);
}

static void scurry_send_status(void)
{
    uint8_t buf[1 + SCURRY_MAX_HOSTS * 8];
    buf[0] = SCURRY_MAX_HOSTS;
    for (int i = 0; i < SCURRY_MAX_HOSTS; i++) {
        uint8_t *e = &buf[1 + i * 8];
        e[0] = (uint8_t)i;
        e[1] = scurry_conn_used[i] ? 1 : 0;
        memcpy(&e[2], scurry_conn_bda[i], 6);
    }
    scurry_send(SCURRY_KIND_STATUS, buf, sizeof(buf));
}

static void scurry_handle_set_config(const uint8_t *p, uint16_t len)
{
    /* Validate before storing. scurry_layout_load runs the same checks the
       controller's tests cover -- overlaps, duplicate nodes, a missing local
       screen -- so an unusable layout can never reach NVS and brick routing
       across a reboot. */
    int32_t rc = scurry_layout_load(p, len);
    if (rc != 0) {
        ESP_LOGW(HID_DEMO_TAG, "scurry: rejected layout (%ld)", (long)rc);
        scurry_send_ack(SCURRY_ACK_INVALID_LAYOUT);
        return;
    }
    if (scurry_nvs_save(p, len) != ESP_OK) {
        scurry_send_ack(SCURRY_ACK_STORAGE_FAILED);
        return;
    }
    ESP_LOGI(HID_DEMO_TAG, "scurry: layout stored (%u bytes)", (unsigned)len);
    scurry_send_ack(SCURRY_ACK_OK);
}

static void scurry_handle(uint8_t kind, uint16_t seq, const uint8_t *payload, uint16_t len)
{
    /* Only the pointer stream is sequence-gated. Control messages are rare and
       must not be dropped for arriving out of order behind a burst of motion. */
    if (kind == SCURRY_KIND_MOUSE) {
        if (!scurry_seq_accept(seq)) {
            return;
        }
        scurry_handle_mouse(payload, len);
        return;
    }

    switch (kind) {
    case SCURRY_KIND_PING:
        scurry_send(SCURRY_KIND_PONG, NULL, 0);
        break;
    case SCURRY_KIND_GET_CONFIG:
        scurry_send_config();
        break;
    case SCURRY_KIND_SET_CONFIG:
        scurry_handle_set_config(payload, len);
        break;
    case SCURRY_KIND_GET_STATUS:
        scurry_send_status();
        break;
    default:
        ESP_LOGD(HID_DEMO_TAG, "scurry: unhandled kind 0x%02x", kind);
        scurry_send_ack(SCURRY_ACK_BAD_REQUEST);
        break;
    }
}

void scurry_reader_task(void *pvParameters)
{
    usb_serial_jtag_driver_config_t cfg = USB_SERIAL_JTAG_DRIVER_CONFIG_DEFAULT();
    cfg.rx_buffer_size = 1024;
    cfg.tx_buffer_size = 1024;
    if (usb_serial_jtag_driver_install(&cfg) != ESP_OK) {
        ESP_LOGE(HID_DEMO_TAG, "scurry: usb_serial_jtag driver install failed");
        vTaskDelete(NULL);
        return;
    }
    ESP_LOGI(HID_DEMO_TAG, "scurry: reading protocol v%d from USB Serial/JTAG", SCURRY_VERSION);

    static uint8_t buf[SCURRY_HEADER_LEN + SCURRY_MAX_PAYLOAD];
    size_t filled = 0;

    while (1) {
        int n = usb_serial_jtag_read_bytes(buf + filled, sizeof(buf) - filled,
                                           20 / portTICK_PERIOD_MS);
        if (n <= 0) {
            continue;
        }
        filled += (size_t)n;

        size_t off = 0;
        while (filled - off >= SCURRY_HEADER_LEN) {
            const uint8_t *h = buf + off;
            /* Resynchronise a byte at a time on the magic. A dropped or
               spurious byte otherwise desynchronises the stream permanently. */
            if (h[0] != SCURRY_MAGIC || h[1] != SCURRY_VERSION) {
                off++;
                continue;
            }
            uint16_t len = (uint16_t)h[6] | ((uint16_t)h[7] << 8);
            if (len > SCURRY_MAX_PAYLOAD) {
                off++; /* corrupt: treat as a false magic rather than trusting it */
                continue;
            }
            if (filled - off < (size_t)SCURRY_HEADER_LEN + len) {
                break; /* payload still in flight */
            }
            uint16_t seq = (uint16_t)h[4] | ((uint16_t)h[5] << 8);
            scurry_handle(h[2], seq, h + SCURRY_HEADER_LEN, len);
            off += SCURRY_HEADER_LEN + len;
        }

        if (off > 0) {
            memmove(buf, buf + off, filled - off);
            filled -= off;
        } else if (filled == sizeof(buf)) {
            /* A full buffer with no parsable message: drop it rather than
               deadlock with no room to read more. */
            filled = 0;
        }
    }
}


void app_main(void)
{
    esp_err_t ret;

    // Initialize NVS.
    ret = nvs_flash_init();
    if (ret == ESP_ERR_NVS_NO_FREE_PAGES || ret == ESP_ERR_NVS_NEW_VERSION_FOUND) {
        ESP_ERROR_CHECK(nvs_flash_erase());
        ret = nvs_flash_init();
    }
    ESP_ERROR_CHECK( ret );

    /* Before anything can advertise it. Reads efuse, so it is safe this early. */
    scurry_make_device_name();

    /* NVS is up by now, so the stored layout can be restored before the first
       pointer message could possibly arrive. */
    scurry_nvs_load();

    ESP_ERROR_CHECK(esp_bt_controller_mem_release(ESP_BT_MODE_CLASSIC_BT));

    esp_bt_controller_config_t bt_cfg = BT_CONTROLLER_INIT_CONFIG_DEFAULT();
    ret = esp_bt_controller_init(&bt_cfg);
    if (ret) {
        ESP_LOGE(HID_DEMO_TAG, "%s initialize controller failed", __func__);
        return;
    }

    ret = esp_bt_controller_enable(ESP_BT_MODE_BLE);
    if (ret) {
        ESP_LOGE(HID_DEMO_TAG, "%s enable controller failed", __func__);
        return;
    }

    ret = esp_bluedroid_init();
    if (ret) {
        ESP_LOGE(HID_DEMO_TAG, "%s init bluedroid failed", __func__);
        return;
    }

    ret = esp_bluedroid_enable();
    if (ret) {
        ESP_LOGE(HID_DEMO_TAG, "%s init bluedroid failed", __func__);
        return;
    }

    if((ret = esp_hidd_profile_init()) != ESP_OK) {
        ESP_LOGE(HID_DEMO_TAG, "%s init bluedroid failed", __func__);
    }

    ///register the callback function to the gap module
    esp_ble_gap_register_callback(gap_event_handler);
    esp_hidd_register_callbacks(hidd_event_callback);

    /* set the security iocap & auth_req & key size & init key response key parameters to the stack*/
    esp_ble_auth_req_t auth_req = ESP_LE_AUTH_BOND;     //bonding with peer device after authentication
    esp_ble_io_cap_t iocap = ESP_IO_CAP_NONE;           //set the IO capability to No output No input
    uint8_t key_size = 16;      //the key size should be 7~16 bytes
    uint8_t init_key = ESP_BLE_ENC_KEY_MASK | ESP_BLE_ID_KEY_MASK;
    uint8_t rsp_key = ESP_BLE_ENC_KEY_MASK | ESP_BLE_ID_KEY_MASK;
    esp_ble_gap_set_security_param(ESP_BLE_SM_AUTHEN_REQ_MODE, &auth_req, sizeof(uint8_t));
    esp_ble_gap_set_security_param(ESP_BLE_SM_IOCAP_MODE, &iocap, sizeof(uint8_t));
    esp_ble_gap_set_security_param(ESP_BLE_SM_MAX_KEY_SIZE, &key_size, sizeof(uint8_t));
    /* If your BLE device act as a Slave, the init_key means you hope which types of key of the master should distribute to you,
    and the response key means which key you can distribute to the Master;
    If your BLE device act as a master, the response key means you hope which types of key of the slave should distribute to you,
    and the init key means which key you can distribute to the slave. */
    esp_ble_gap_set_security_param(ESP_BLE_SM_SET_INIT_KEY, &init_key, sizeof(uint8_t));
    esp_ble_gap_set_security_param(ESP_BLE_SM_SET_RSP_KEY, &rsp_key, sizeof(uint8_t));

    xTaskCreate(&scurry_reader_task, "scurry_rx", 4096, NULL, 5, NULL);
}
