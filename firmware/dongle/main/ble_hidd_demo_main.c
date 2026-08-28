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

static void scurry_conn_add(uint16_t conn_id, esp_bd_addr_t bda)
{
    for (int i = 0; i < SCURRY_MAX_HOSTS; i++) {
        if (!scurry_conn_used[i]) {
            scurry_conn_used[i] = true;
            scurry_conns[i] = conn_id;
            memcpy(scurry_conn_bda[i], bda, sizeof(esp_bd_addr_t));
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
 * scurry frame reader
 *
 * The controller writes 16-byte scurry frames at us over USB Serial/JTAG. This
 * is the SECOND implementation of that wire format -- crates/scurry-proto is
 * the first, in Rust. They must not drift. doc/protocol.md is the normative
 * description; golden vectors in the Rust crate's tests pin the byte layout.
 *
 * Logs flow the other way on the same CDC pipe. That is fine: it is full
 * duplex, the host writes binary and reads text.
 * ------------------------------------------------------------------------- */

#define SCURRY_MAGIC        0x53
#define SCURRY_VERSION      1
#define SCURRY_FRAME_LEN    16

#define SCURRY_KIND_MOUSE   1
#define SCURRY_KIND_ENTER   2
#define SCURRY_KIND_LEAVE   3
#define SCURRY_KIND_PING    4
#define SCURRY_KIND_PONG    5

static bool     scurry_seq_seen = false;
static uint16_t scurry_seq_last = 0;

/* Sequence numbers wrap at u16, so "newer" is signed distance on the wrapped
   circle, not `>`. A naive comparison would stall the pointer for 32k frames
   every time the counter wrapped. Mirrors SeqGate in scurry-proto. */
static bool scurry_seq_accept(uint16_t seq)
{
    if (!scurry_seq_seen) {
        scurry_seq_seen = true;
        scurry_seq_last = seq;
        return true;
    }
    int16_t delta = (int16_t)(seq - scurry_seq_last);
    if (delta > 0) {
        scurry_seq_last = seq;
        return true;
    }
    return false;
}

/* Map a scurry node id onto a live BLE connection. Node ids index the
   connection table in bond order; -1 means that target is not connected. */
static int scurry_conn_for_node(uint8_t node)
{
    /* Node 0 is the controller's own screen. The controller never transmits
       it, so node N addresses connection slot N-1. Keeping 0 reserved means a
       zeroed or truncated frame cannot accidentally address a real target. */
    if (node == 0 || node > SCURRY_MAX_HOSTS) {
        return -1;
    }
    uint8_t slot = node - 1;
    return scurry_conn_used[slot] ? (int)scurry_conns[slot] : -1;
}

static void scurry_handle_frame(const uint8_t *f)
{
    uint8_t  kind = f[2];
    uint8_t  node = f[3];
    uint16_t seq  = (uint16_t)f[4] | ((uint16_t)f[5] << 8);

    if (!scurry_seq_accept(seq)) {
        return; /* reordered straggler */
    }

    int conn = scurry_conn_for_node(node);
    if (conn < 0 && kind != SCURRY_KIND_PING) {
        return; /* nothing bonded on that slot yet */
    }

    switch (kind) {
    case SCURRY_KIND_MOUSE: {
        uint8_t buttons = f[6];
        int16_t dx = (int16_t)((uint16_t)f[8]  | ((uint16_t)f[9]  << 8));
        int16_t dy = (int16_t)((uint16_t)f[10] | ((uint16_t)f[11] << 8));
        int8_t  wheel = (int8_t)f[12];
        int8_t  pan   = (int8_t)f[13];
        esp_hidd_send_mouse_report((uint16_t)conn, buttons, dx, dy, wheel, pan);
        break;
    }
    case SCURRY_KIND_LEAVE:
        /* Release everything. A drag that crosses a screen boundary must not
           strand a held button on the machine being departed. */
        esp_hidd_send_mouse_report((uint16_t)conn, 0, 0, 0, 0, 0);
        break;
    case SCURRY_KIND_ENTER:
        ESP_LOGI(HID_DEMO_TAG, "scurry: enter node=%d edge=%d", node, f[6]);
        break;
    case SCURRY_KIND_PING:
        ESP_LOGI(HID_DEMO_TAG, "scurry: ping seq=%u, %d host(s)", seq, scurry_conn_count());
        break;
    default:
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
    ESP_LOGI(HID_DEMO_TAG, "scurry: reading frames from USB Serial/JTAG");

    static uint8_t buf[SCURRY_FRAME_LEN * 8];
    size_t filled = 0;

    while (1) {
        int n = usb_serial_jtag_read_bytes(buf + filled, sizeof(buf) - filled,
                                           20 / portTICK_PERIOD_MS);
        if (n <= 0) {
            continue;
        }
        filled += (size_t)n;

        size_t off = 0;
        while (filled - off >= SCURRY_FRAME_LEN) {
            /* Resynchronise byte-by-byte on the magic. A dropped or spurious
               byte otherwise desynchronises the stream permanently. */
            if (buf[off] != SCURRY_MAGIC || buf[off + 1] != SCURRY_VERSION) {
                off++;
                continue;
            }
            scurry_handle_frame(buf + off);
            off += SCURRY_FRAME_LEN;
        }
        /* Keep the unconsumed tail. */
        if (off > 0) {
            memmove(buf, buf + off, filled - off);
            filled -= off;
        } else if (filled == sizeof(buf)) {
            filled = 0; /* no magic anywhere in a full buffer: drop it */
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
