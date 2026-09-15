#include "scurry_display.h"

#include <signal.h>
#include <stdio.h>
#include <string.h>

#include "driver/i2c_master.h"
#include "esp_log.h"
#include "freertos/FreeRTOS.h"
#include "freertos/task.h"

static const char *TAG = "SCURRY_DISP";

/* --- The board -----------------------------------------------------------
 *
 * An ESP32-C3 with a 0.42" OLED soldered on. The two pins are fixed by the
 * board, not chosen by us; on a variant that wires them elsewhere these are
 * the two lines to change.
 *
 * GPIO9 is deliberately not among them -- that is BOOT, which scurry_button.c
 * owns and which is the entire input budget of this device. */
#define DISP_SDA_GPIO   5
#define DISP_SCL_GPIO   6
#define DISP_I2C_ADDR   0x3C
#define DISP_I2C_HZ     400000

/* --- The panel -----------------------------------------------------------
 *
 * 72x40 visible pixels sitting inside an SSD1306 that always thinks it is
 * driving 128x64. The visible window is centred in the controller's RAM, so
 * every write is displaced by a column offset: writing to column 0 puts the
 * pixel off the left-hand edge of the glass, where it is perfectly set and
 * entirely invisible. (128 - 72) / 2 = 28.
 *
 * The rows need no such offset -- 40 rows is exactly five 8-row pages
 * starting from the top -- but the multiplex ratio must be told that there
 * are 40 of them and not 64, or the panel scans rows that do not exist and
 * the image is compressed into the top of the glass. */
#define DISP_W          72
#define DISP_H          40
#define DISP_PAGES      (DISP_H / 8)
#define DISP_COL_OFFSET 28

/* Rotated 180 degrees, because the panel is mounted upside down in the case.
 * The controller does both flips on the way out of RAM, so the framebuffer
 * and the centring arithmetic are unchanged. Both are needed: one alone
 * mirrors rather than rotates. DISP_COL_OFFSET is unaffected -- the visible
 * window is centred, so it is 28 from either end. */
#define DISP_SEG_REMAP  0xA0
#define DISP_COM_SCAN   0xC0

/* Twelve characters per line: 72 / 6, where 6 is a 5-pixel glyph plus one
   column of gap. Five lines, one per page. */
#define DISP_COLS       (DISP_W / 6)
#define DISP_ROWS       DISP_PAGES

_Static_assert(SCURRY_DISPLAY_MAX_NODES == 4,
               "the link screen draws one row per node and has four to spare");
/* The header line is "LINK n/m XXX", twelve characters, and it is drawn at
   single size precisely because that is the width of the panel. If the panel
   ever narrows, that line is what breaks first. */
_Static_assert(DISP_COLS >= 12, "the link header is twelve characters wide");

/* Control bytes prefixed to every I2C write: the SSD1306 distinguishes
   commands from pixel data by the first byte, not by a separate pin. */
#define DISP_CTRL_CMD   0x00
#define DISP_CTRL_DATA  0x40

static i2c_master_bus_handle_t s_bus;
static i2c_master_dev_handle_t s_dev;
static bool s_present;
static scurry_display_poll_t s_poll;

/* One byte per 8-pixel column-slice, five pages deep. 360 bytes, which is
   cheap enough to redraw wholesale every frame and skip dirty-rectangle
   bookkeeping entirely. */
static uint8_t s_fb[DISP_PAGES][DISP_W];

/* --- 5x7 font ------------------------------------------------------------
 *
 * Five columns per glyph, each byte a vertical slice with bit 0 at the top.
 * Only 0x20..0x7E; anything outside prints as a space, which is what the
 * status strings want anyway since none of them can contain one. */
static const uint8_t FONT5X7[][5] = {
    {0x00, 0x00, 0x00, 0x00, 0x00}, /* space */
    {0x00, 0x00, 0x5F, 0x00, 0x00}, /* ! */
    {0x00, 0x07, 0x00, 0x07, 0x00}, /* " */
    {0x14, 0x7F, 0x14, 0x7F, 0x14}, /* # */
    {0x24, 0x2A, 0x7F, 0x2A, 0x12}, /* $ */
    {0x23, 0x13, 0x08, 0x64, 0x62}, /* % */
    {0x36, 0x49, 0x55, 0x22, 0x50}, /* & */
    {0x00, 0x05, 0x03, 0x00, 0x00}, /* ' */
    {0x00, 0x1C, 0x22, 0x41, 0x00}, /* ( */
    {0x00, 0x41, 0x22, 0x1C, 0x00}, /* ) */
    {0x14, 0x08, 0x3E, 0x08, 0x14}, /* * */
    {0x08, 0x08, 0x3E, 0x08, 0x08}, /* + */
    {0x00, 0x50, 0x30, 0x00, 0x00}, /* , */
    {0x08, 0x08, 0x08, 0x08, 0x08}, /* - */
    {0x00, 0x60, 0x60, 0x00, 0x00}, /* . */
    {0x20, 0x10, 0x08, 0x04, 0x02}, /* / */
    {0x3E, 0x51, 0x49, 0x45, 0x3E}, /* 0 */
    {0x00, 0x42, 0x7F, 0x40, 0x00}, /* 1 */
    {0x42, 0x61, 0x51, 0x49, 0x46}, /* 2 */
    {0x21, 0x41, 0x45, 0x4B, 0x31}, /* 3 */
    {0x18, 0x14, 0x12, 0x7F, 0x10}, /* 4 */
    {0x27, 0x45, 0x45, 0x45, 0x39}, /* 5 */
    {0x3C, 0x4A, 0x49, 0x49, 0x30}, /* 6 */
    {0x01, 0x71, 0x09, 0x05, 0x03}, /* 7 */
    {0x36, 0x49, 0x49, 0x49, 0x36}, /* 8 */
    {0x06, 0x49, 0x49, 0x29, 0x1E}, /* 9 */
    {0x00, 0x36, 0x36, 0x00, 0x00}, /* : */
    {0x00, 0x56, 0x36, 0x00, 0x00}, /* ; */
    {0x00, 0x08, 0x14, 0x22, 0x41}, /* < */
    {0x14, 0x14, 0x14, 0x14, 0x14}, /* = */
    {0x41, 0x22, 0x14, 0x08, 0x00}, /* > */
    {0x02, 0x01, 0x51, 0x09, 0x06}, /* ? */
    {0x32, 0x49, 0x79, 0x41, 0x3E}, /* @ */
    {0x7E, 0x11, 0x11, 0x11, 0x7E}, /* A */
    {0x7F, 0x49, 0x49, 0x49, 0x36}, /* B */
    {0x3E, 0x41, 0x41, 0x41, 0x22}, /* C */
    {0x7F, 0x41, 0x41, 0x22, 0x1C}, /* D */
    {0x7F, 0x49, 0x49, 0x49, 0x41}, /* E */
    {0x7F, 0x09, 0x09, 0x01, 0x01}, /* F */
    {0x3E, 0x41, 0x41, 0x51, 0x32}, /* G */
    {0x7F, 0x08, 0x08, 0x08, 0x7F}, /* H */
    {0x00, 0x41, 0x7F, 0x41, 0x00}, /* I */
    {0x20, 0x40, 0x41, 0x3F, 0x01}, /* J */
    {0x7F, 0x08, 0x14, 0x22, 0x41}, /* K */
    {0x7F, 0x40, 0x40, 0x40, 0x40}, /* L */
    {0x7F, 0x02, 0x04, 0x02, 0x7F}, /* M */
    {0x7F, 0x04, 0x08, 0x10, 0x7F}, /* N */
    {0x3E, 0x41, 0x41, 0x41, 0x3E}, /* O */
    {0x7F, 0x09, 0x09, 0x09, 0x06}, /* P */
    {0x3E, 0x41, 0x51, 0x21, 0x5E}, /* Q */
    {0x7F, 0x09, 0x19, 0x29, 0x46}, /* R */
    {0x46, 0x49, 0x49, 0x49, 0x31}, /* S */
    {0x01, 0x01, 0x7F, 0x01, 0x01}, /* T */
    {0x3F, 0x40, 0x40, 0x40, 0x3F}, /* U */
    {0x1F, 0x20, 0x40, 0x20, 0x1F}, /* V */
    {0x7F, 0x20, 0x18, 0x20, 0x7F}, /* W */
    {0x63, 0x14, 0x08, 0x14, 0x63}, /* X */
    {0x03, 0x04, 0x78, 0x04, 0x03}, /* Y */
    {0x61, 0x51, 0x49, 0x45, 0x43}, /* Z */
    {0x00, 0x7F, 0x41, 0x41, 0x00}, /* [ */
    {0x02, 0x04, 0x08, 0x10, 0x20}, /* backslash */
    {0x00, 0x41, 0x41, 0x7F, 0x00}, /* ] */
    {0x04, 0x02, 0x01, 0x02, 0x04}, /* ^ */
    {0x40, 0x40, 0x40, 0x40, 0x40}, /* _ */
    {0x00, 0x01, 0x02, 0x04, 0x00}, /* ` */
    {0x20, 0x54, 0x54, 0x54, 0x78}, /* a */
    {0x7F, 0x48, 0x44, 0x44, 0x38}, /* b */
    {0x38, 0x44, 0x44, 0x44, 0x20}, /* c */
    {0x38, 0x44, 0x44, 0x48, 0x7F}, /* d */
    {0x38, 0x54, 0x54, 0x54, 0x18}, /* e */
    {0x08, 0x7E, 0x09, 0x01, 0x02}, /* f */
    {0x0C, 0x52, 0x52, 0x52, 0x3E}, /* g */
    {0x7F, 0x08, 0x04, 0x04, 0x78}, /* h */
    {0x00, 0x44, 0x7D, 0x40, 0x00}, /* i */
    {0x20, 0x40, 0x44, 0x3D, 0x00}, /* j */
    {0x7F, 0x10, 0x28, 0x44, 0x00}, /* k */
    {0x00, 0x41, 0x7F, 0x40, 0x00}, /* l */
    {0x7C, 0x04, 0x18, 0x04, 0x78}, /* m */
    {0x7C, 0x08, 0x04, 0x04, 0x78}, /* n */
    {0x38, 0x44, 0x44, 0x44, 0x38}, /* o */
    {0x7C, 0x14, 0x14, 0x14, 0x08}, /* p */
    {0x08, 0x14, 0x14, 0x18, 0x7C}, /* q */
    {0x7C, 0x08, 0x04, 0x04, 0x08}, /* r */
    {0x48, 0x54, 0x54, 0x54, 0x20}, /* s */
    {0x04, 0x3F, 0x44, 0x40, 0x20}, /* t */
    {0x3C, 0x40, 0x40, 0x20, 0x7C}, /* u */
    {0x1C, 0x20, 0x40, 0x20, 0x1C}, /* v */
    {0x3C, 0x40, 0x30, 0x40, 0x3C}, /* w */
    {0x44, 0x28, 0x10, 0x28, 0x44}, /* x */
    {0x0C, 0x50, 0x50, 0x50, 0x3C}, /* y */
    {0x44, 0x64, 0x54, 0x4C, 0x44}, /* z */
    {0x00, 0x08, 0x36, 0x41, 0x00}, /* { */
    {0x00, 0x00, 0x7F, 0x00, 0x00}, /* | */
    {0x00, 0x41, 0x36, 0x08, 0x00}, /* } */
    {0x08, 0x04, 0x08, 0x10, 0x08}, /* ~ */
};

/* --- Talking to the panel ------------------------------------------------ */

static esp_err_t disp_cmd(const uint8_t *cmds, size_t n)
{
    uint8_t buf[24];
    if (n + 1 > sizeof(buf)) {
        return ESP_ERR_INVALID_SIZE;
    }
    buf[0] = DISP_CTRL_CMD;
    memcpy(buf + 1, cmds, n);
    return i2c_master_transmit(s_dev, buf, n + 1, 100);
}

static esp_err_t disp_cmd1(uint8_t c)
{
    return disp_cmd(&c, 1);
}

/* Push the whole framebuffer. One transfer per page: 73 bytes each, which
   fits comfortably inside the driver's buffer and keeps a partial failure
   confined to a single row rather than corrupting the frame. */
static void disp_flush(void)
{
    for (int page = 0; page < DISP_PAGES; page++) {
        uint8_t addr[] = {
            (uint8_t)(0xB0 | page),                            /* page start */
            (uint8_t)(DISP_COL_OFFSET & 0x0F),                 /* column low */
            (uint8_t)(0x10 | ((DISP_COL_OFFSET >> 4) & 0x0F)), /* column high */
        };
        if (disp_cmd(addr, sizeof(addr)) != ESP_OK) {
            return;
        }
        uint8_t buf[1 + DISP_W];
        buf[0] = DISP_CTRL_DATA;
        memcpy(buf + 1, s_fb[page], DISP_W);
        if (i2c_master_transmit(s_dev, buf, sizeof(buf), 100) != ESP_OK) {
            return;
        }
    }
}

/* --- Drawing ------------------------------------------------------------- */

static void fb_clear(void)
{
    memset(s_fb, 0, sizeof(s_fb));
}

/* --- Scaled text ---------------------------------------------------------
 *
 * The character-cell routines above are page-aligned, which is what makes
 * them cheap: a glyph is five whole bytes memcpy'd into a row. A scaled glyph
 * is not page-aligned -- doubled, a 7-pixel character is 14 tall and lands
 * across two pages at an arbitrary offset -- so it has to go in a pixel at a
 * time. Slower, and it does not matter: this draws at most eleven characters,
 * five times a second, on a core that is otherwise waiting for the radio. */
static void fb_pixel(int x, int y)
{
    if (x < 0 || x >= DISP_W || y < 0 || y >= DISP_H) {
        return;
    }
    s_fb[y >> 3][x] |= (uint8_t)(1u << (y & 7));
}

static void fb_char_scaled(int x, int y, char ch, int scale)
{
    if (ch < 0x20 || ch > 0x7E) {
        ch = ' ';
    }
    const uint8_t *g = FONT5X7[(int)ch - 0x20];
    for (int col = 0; col < 5; col++) {
        for (int row = 0; row < 7; row++) {
            if (!(g[col] & (1u << row))) {
                continue;
            }
            for (int dx = 0; dx < scale; dx++) {
                for (int dy = 0; dy < scale; dy++) {
                    fb_pixel(x + col * scale + dx, y + row * scale + dy);
                }
            }
        }
    }
}

/* Width of n characters at a scale, less the trailing inter-character gap --
   which is real space when you are centring and would push the text a pixel
   or two left of true if it were counted. */
static int text_width(int n, int scale)
{
    return n <= 0 ? 0 : n * 6 * scale - scale;
}

static void fb_text_scaled_at(int x, int y, const char *s, int scale)
{
    for (int i = 0; s[i] != '\0'; i++) {
        fb_char_scaled(x + i * 6 * scale, y, s[i], scale);
    }
}

static void fb_text_scaled_centre(int y, const char *s, int scale)
{
    int x = (DISP_W - text_width((int)strlen(s), scale)) / 2;
    fb_text_scaled_at(x < 0 ? 0 : x, y, s, scale);
}

/* Centre single-size text on a page boundary.
 *
 * Centred by pixel rather than by character cell. Cell centring can only place
 * a string on a multiple of six pixels, so an odd-length line sat up to three
 * pixels left of true -- invisible on its own and obvious the moment it is
 * stacked above a line centred by any other means. On a panel 72 pixels wide,
 * three is most of a character. */
static void fb_text_centre(int row, const char *s)
{
    fb_text_scaled_centre(row * 8, s, 1);
}

/* A horizontal run of pixels, for underlining a row of text. */
static void fb_underline(int y, int x0, int x1)
{
    for (int x = x0; x <= x1; x++) {
        fb_pixel(x, y);
    }
}

/* A solid rule across a row, drawn straight into the page rather than as a
   glyph so it spans the full 72 pixels instead of stopping at the last
   character cell. Bit 3 puts it in the middle of the 8-row band. */
static void fb_rule(int row)
{
    if (row < 0 || row >= DISP_ROWS) {
        return;
    }
    memset(s_fb[row], 0x08, DISP_W);
}

/* --- The screens ---------------------------------------------------------
 *
 * Four of them, in strict priority order: whatever is most time-critical and
 * most likely to be why somebody is looking at the dongle at all. A passkey
 * beats an open window beats ordinary status. Every line below is at most
 * twelve characters, and several are exactly twelve. */

/* The passkey, as large as six digits can be made on this panel.
 *
 * Double size, and that is the ceiling rather than a choice: a digit is five
 * pixels wide plus one of gap, so six of them at double size span 70 of the 72
 * pixels there are. Triple would need 105. The limit is the width of the
 * glass, not the height, which is why nothing is gained by giving the digits
 * more rows.
 *
 * What the rows did gain is everything around them. This screen used to spend
 * two of its five lines on horizontal rules, which decorated a number that
 * somebody is squinting at from wherever the dongle happens to be sitting.
 * They are gone, and the digits took the space.
 *
 * Of everything on this display, these six characters are the only ones that
 * have to be read correctly the first time and typed somewhere else. */
static void screen_passkey(const scurry_display_state_t *st)
{
    char line[DISP_COLS + 1];

    fb_text_scaled_centre(0, "PASSKEY", 1);

    /* Zero-padded: the stack's passkey is a number in 0..999999 and a leading
       zero is part of what the peer is being asked to confirm. Printing
       "12345" for 012345 would fail the comparison and look like a bug in the
       controller rather than in this line. */
    snprintf(line, sizeof(line), "%06lu", (unsigned long)st->passkey);
    fb_text_scaled_centre(12, line, 2);

    snprintf(line, sizeof(line), "%lus left", (unsigned long)st->pairing_left_s);
    fb_text_scaled_centre(32, line, 1);
}

static void screen_pairing(const scurry_display_state_t *st)
{
    char line[DISP_COLS + 1];

    fb_text_centre(0, "PAIRING");
    fb_rule(1);
    snprintf(line, sizeof(line), "open %lus", (unsigned long)st->pairing_left_s);
    fb_text_centre(2, line);
    /* The way out, on the screen, because a window opened by accident is
       exactly the moment somebody needs to be told there is one. */
    fb_text_centre(3, "press x3");
    fb_text_centre(4, "to cancel");
}

/* The links, and only the links that exist.
 *
 * This used to draw four rows come what may, three of them "----" on a desk
 * with one target on it. Reserving space for absent machines is the panel's
 * scarcest resource spent on the least information: an empty slot is not news,
 * and there are only five lines to begin with.
 *
 * So the rows are the connections, and the text grows into whatever they do
 * not use. With one or two targets -- which is what most desks have -- the
 * addresses come up at double size and are legible from across the room
 * instead of from arm's length. The header stays single size throughout
 * because "LINK 2/4 USB" is twelve characters, which is exactly the width at
 * single size and twice the width at double. */
static void screen_links(const scurry_display_state_t *st)
{
    char line[DISP_COLS + 1];

    const char *who = st->driver < 0 ? "---" : (st->driver == 0 ? "USB" : "AIR");
    snprintf(line, sizeof(line), "LINK %d/%d %s", st->links, st->max_links, who);
    fb_text_centre(0, line);

    /* Row 0 is the header; everything below it belongs to the entries. */
    const int top = 8;
    const int avail = DISP_H - top;

    if (st->links == 0) {
        fb_text_scaled_centre(top + (avail - 14) / 2, "no", 2);
        fb_text_scaled_centre(top + (avail - 14) / 2 + 16, "targets", 1);
        return;
    }

    /* An entry is "1>A1B2": six characters, which is 69 pixels at double size
       and 105 at triple -- so double is the ceiling here regardless of how much
       vertical room is going spare. Two of them fit in the 32 pixels below the
       header; a third does not, and everything drops to single size. */
    int scale = st->links <= 2 ? 2 : 1;
    int line_h = 7 * scale;
    /* One row of gap is the underline's; the rest keeps it off the next line. */
    int gap = scale == 2 ? 3 : 2;
    int block = st->links * line_h + (st->links - 1) * gap;
    int y = top + (avail - block) / 2;
    if (y < top) {
        y = top;
    }

    for (int i = 0; i < SCURRY_DISPLAY_MAX_NODES; i++) {
        if (!st->node_up[i]) {
            continue;
        }
        /* Node ids are slot + 1, matching the layout engine and the config
           file, so what is on the glass is what goes in scurry.toml. */
        int node = i + 1;
        /* '>' marks where the pointer is now. Node 0 is the controller's own
           screen, so during normal local use nothing is marked -- which is
           itself the answer to "where did my mouse go". */
        /* The name if the layout has one, the address tail if it does not.
           A dongle that has never been configured still has to say something,
           and before a layout exists the address is the only name a machine
           has. Room is what the scale leaves: four characters after the node
           number at double size, ten at single. */
        const char *name = st->node_name[i];
        int x0, width;

        if (scale == 2) {
            /* The number is set small and the name large, on one row.
               
               A uniform row spends a whole double-width cell on a digit and
               another on the stop that keeps it from reading as the name's
               first letter -- a third of the row, to label it. At single size
               the pair costs eleven pixels instead of twenty-four, which buys
               the name back the character the stop took and puts the emphasis
               where the reading happens. */
            char num[4];
            char body[DISP_COLS + 1];
            snprintf(num, sizeof(num), "%d.", node);
            if (name[0] != '\0') {
                snprintf(body, sizeof(body), "%.5s", name);
            } else {
                snprintf(body, sizeof(body), "%02X%02X",
                         st->node_tail[i][0], st->node_tail[i][1]);
            }

            int wn = text_width((int)strlen(num), 1);
            int wb = text_width((int)strlen(body), 2);
            width = wn + 2 + wb;
            x0 = (DISP_W - width) / 2;
            if (x0 < 0) {
                x0 = 0;
            }
            /* Bottom-aligned against the tall glyphs, so the number sits on the
               same line the name stands on rather than floating at its top. */
            fb_text_scaled_at(x0, y + (line_h - 7), num, 1);
            fb_text_scaled_at(x0 + wn + 2, y, body, 2);
        } else {
            /* Three or more links: everything is single size, so there is no
               emphasis to arrange and one string does. */
            if (name[0] != '\0') {
                snprintf(line, sizeof(line), "%d. %.*s", node, 9, name);
            } else {
                snprintf(line, sizeof(line), "%d. %02X%02X", node,
                         st->node_tail[i][0], st->node_tail[i][1]);
            }
            width = text_width((int)strlen(line), 1);
            x0 = (DISP_W - width) / 2;
            if (x0 < 0) {
                x0 = 0;
            }
            fb_text_scaled_at(x0, y, line, 1);
        }

        /* Focus is an underline rather than a marker character.
           
           A '>' cost one of the six characters a double-size row has, spending
           a sixth of the name to say something a line under the row says
           better -- you see which row is underlined without reading any of
           them, where a chevron has to be found first. Spans the number too:
           the row is what has focus, not the name. */
        if (st->focus_node == node) {
            fb_underline(y + line_h, x0, x0 + width - 1);
        }
        y += line_h + gap;
    }
}

/* The home screen: the dongle's name, and nothing else.
 *
 * The name is "Scurry XXXX" and the two halves are not equally interesting.
 * "Scurry" is on every board ever built; the four characters after it are the
 * entire answer to "which one is this". So the prefix is set small and the id
 * as large as will fit -- at triple size four characters span 69 of the 72
 * pixels available, which is as close to filling the glass as this panel gets.
 *
 * The scale is chosen rather than fixed, because the id is only four
 * characters by present convention and a convention is not a guarantee. A
 * longer one steps down to double and then to single size instead of running
 * off the edge.
 *
 * There used to be a "ready" under all this. It was removed because it was
 * never true or false: the screen only draws at all once the panel is up,
 * which is after everything else has started, so the word could not report a
 * state that had any other value. A status line that cannot say anything but
 * "fine" is worse than no status line, because it looks like one. */
static void screen_identity(const scurry_display_state_t *st)
{
    const char *name = st->name ? st->name : "Scurry";
    const char *space = strchr(name, ' ');

    /* No space to break at: nothing to make small, so give the whole string
       the largest size it fits in and centre it. */
    if (space == NULL || space[1] == '\0') {
        int len = (int)strlen(name);
        int scale = text_width(len, 3) <= DISP_W ? 3 : (text_width(len, 2) <= DISP_W ? 2 : 1);
        fb_text_scaled_centre((DISP_H - 7 * scale) / 2, name, scale);
        return;
    }

    char head[DISP_COLS + 1];
    size_t n = (size_t)(space - name);
    if (n > sizeof(head) - 1) {
        n = sizeof(head) - 1;
    }
    memcpy(head, name, n);
    head[n] = '\0';

    const char *id = space + 1;
    int id_len = (int)strlen(id);
    int scale = text_width(id_len, 3) <= DISP_W ? 3 : (text_width(id_len, 2) <= DISP_W ? 2 : 1);

    /* 7 for the small line, a 5-pixel gap, then the id. Centred as a block so
       dropping a scale does not leave it hanging from the top. */
    int block = 7 + 5 + 7 * scale;
    int top = (DISP_H - block) / 2;
    fb_text_scaled_centre(top, head, 1);
    fb_text_scaled_centre(top + 12, id, scale);
}

/* The Bluetooth address, on its own screen behind a double press.
 *
 * It earns a screen rather than a corner of one because of what it is for:
 * this is the string somebody copies into scurry.toml to pin a machine to a
 * node. Drawn at single size with its colons intact -- when the point is
 * transcribing twelve hex digits without error, being able to read them in
 * pairs beats being able to read them from across the room. */
static void screen_mac(const scurry_display_state_t *st)
{
    char line[DISP_COLS + 1];

    fb_text_centre(0, "BT ADDRESS");
    fb_rule(1);
    snprintf(line, sizeof(line), "%02X:%02X:%02X", st->mac[0], st->mac[1], st->mac[2]);
    fb_text_centre(2, line);
    snprintf(line, sizeof(line), "%02X:%02X:%02X", st->mac[3], st->mac[4], st->mac[5]);
    fb_text_centre(3, line);
}

/* An update, in the only terms that matter while one is running: how far, and
 * whether it is safe to walk away. The bar is drawn rather than written as a
 * number because the question being asked of it is "is it still moving". */
static void screen_ota(const scurry_display_state_t *st)
{
    char line[DISP_COLS + 1];

    fb_text_centre(0, "UPDATING");

    /* A frame the full width of the glass, filled to the percentage. Two rows
       of the framebuffer, drawn as pixels so it has an outline rather than
       being a row of block characters. */
    int filled = (DISP_W - 4) * (int)st->ota_percent / 100;
    for (int x = 0; x < DISP_W; x++) {
        fb_pixel(x, 14);
        fb_pixel(x, 23);
    }
    for (int y = 14; y <= 23; y++) {
        fb_pixel(0, y);
        fb_pixel(DISP_W - 1, y);
    }
    for (int x = 0; x < filled; x++) {
        for (int y = 17; y <= 20; y++) {
            fb_pixel(2 + x, y);
        }
    }

    switch (st->ota_state) {
    case 2: /* verifying */
        fb_text_centre(4, "checking");
        break;
    case 3: /* ready */
        fb_text_centre(4, "rebooting");
        break;
    case 4: /* failed */
        fb_text_centre(4, "FAILED");
        break;
    default:
        snprintf(line, sizeof(line), "%u%%", (unsigned)st->ota_percent);
        fb_text_centre(4, line);
        break;
    }
}

/* --- Choosing a screen ---------------------------------------------------
 *
 * Two screens worth showing when nothing urgent is happening, and the dongle
 * decides between them from its own state rather than by taking turns.
 *
 * They used to alternate every fifteen seconds, on the reasoning that the
 * device cannot know which one you want. It can, near enough: with a machine
 * connected the links are the only thing worth the glass, and with none the
 * only thing left to say is which dongle this is. Rotating meant that half the
 * time you looked over, the answer you wanted had just gone, and waiting for
 * it to come back around is a worse interface than pressing a button.
 *
 * So the state picks, and a press overrides the pick until something happens
 * that makes the state worth showing again. */
typedef enum {
    DISP_SCREEN_IDENTITY,
    DISP_SCREEN_LINKS,
} disp_screen_t;

/* How long a summoned address screen stays up. No longer paces anything else. */
#define DISP_DWELL_MS   15000
#define DISP_REFRESH_MS 200

static disp_screen_t s_screen;
/* True once a press has chosen a screen, so the state stops choosing. */
static bool s_held;
/* Whether anything was connected last time round, to notice the transition. */
static int s_had_links = -1;
/* Set by the button task, cleared by the display task. One flag, one writer
   each way, and a lost or duplicated press costs a screen flip -- so this is
   the rare case where sig_atomic_t really is the whole synchronisation. */
static volatile sig_atomic_t s_advance;
static volatile sig_atomic_t s_summon_mac;

void scurry_display_next(void)
{
    s_advance = 1;
}

void scurry_display_show_mac(void)
{
    s_summon_mac = 1;
}

static void scurry_display_task(void *arg)
{
    (void)arg;
    /* When the summoned address screen stops being wanted. Zero when it is not
       up. It is held for the same fifteen seconds a carousel screen gets,
       because it is asked for by somebody about to copy it down and they
       should not have to race it. */
    TickType_t mac_until = 0;

    for (;;) {
        scurry_display_state_t st;
        memset(&st, 0, sizeof(st));
        st.driver = -1;
        s_poll(&st);

        TickType_t now = xTaskGetTickCount();

        bool pressed = s_advance != 0;
        if (pressed) {
            s_advance = 0;
        }
        if (s_summon_mac != 0) {
            s_summon_mac = 0;
            mac_until = now + pdMS_TO_TICKS(DISP_DWELL_MS);
        } else if (pressed) {
            /* A single press means "show me the other thing", so it dismisses
               the address early rather than being swallowed by it. */
            mac_until = 0;
        }

        /* What the dongle's state says to show. */
        disp_screen_t from_state =
            st.links > 0 ? DISP_SCREEN_LINKS : DISP_SCREEN_IDENTITY;

        /* A machine appearing or the last one going away is news, and news
           outranks whatever was last pressed. Without this the first target of
           the day would connect behind a home screen somebody had pressed for
           hours earlier, and the dongle would sit there not mentioning it. */
        int have_links = st.links > 0;
        if (s_had_links != have_links) {
            s_had_links = have_links;
            s_held = false;
        }

        if (pressed) {
            /* One press is "show me the other one", and it stays there. The
               way back is another press, not a wait: a screen that reverts on
               its own is the rotation this replaced, just slower. */
            s_screen = (s_screen == DISP_SCREEN_LINKS) ? DISP_SCREEN_IDENTITY
                                                       : DISP_SCREEN_LINKS;
            s_held = true;
        } else if (!s_held) {
            s_screen = from_state;
        }

        fb_clear();
        /* Strict priority: a passkey beats an open window beats a summoned
           address beats the chosen screen. Anything somebody has to act on
           outranks anything they asked to see, which outranks anything merely
           on show. */
        if (st.ota_state != 0) {
            screen_ota(&st);
        } else if (st.pairing_left_s > 0 && st.passkey_valid) {
            screen_passkey(&st);
        } else if (st.pairing_left_s > 0) {
            screen_pairing(&st);
        } else if (mac_until != 0 && (int32_t)(mac_until - now) > 0) {
            screen_mac(&st);
        } else if (s_screen == DISP_SCREEN_IDENTITY) {
            mac_until = 0;
            screen_identity(&st);
        } else {
            mac_until = 0;
            screen_links(&st);
        }
        disp_flush();

        vTaskDelay(pdMS_TO_TICKS(DISP_REFRESH_MS));
    }
}

/* --- Bring-up ------------------------------------------------------------ */

/* The init sequence. Values that differ from a stock 128x64 panel are the
   ones carrying comments; the rest is the datasheet's recommended power-on
   configuration and is not worth restating here. */
static const uint8_t DISP_INIT[] = {
    0xAE,              /* display off while we reconfigure */
    0xD5, 0x80,        /* clock divide / oscillator frequency */
    0xA8, DISP_H - 1,  /* multiplex ratio: 40 rows, not the default 64 */
    0xD3, 0x00,        /* no vertical display offset */
    0x40,              /* start line 0 */
    0x8D, 0x14,        /* charge pump on -- no panel without it, and the
                          symptom is a display that ACKs every write and
                          stays blank, which reads as a wiring fault */
    0x20, 0x02,        /* page addressing: disp_flush sets the page and column
                          explicitly per row, which horizontal mode would then
                          auto-increment past */
    DISP_SEG_REMAP,    /* the 180 rotation; change these two as a pair */
    DISP_COM_SCAN,
    0xDA, 0x12,        /* alternative COM pin config, as this panel is wired */
    0x81, 0xAF,        /* contrast */
    0xD9, 0x22,        /* pre-charge period */
    0xDB, 0x20,        /* VCOMH deselect level */
    0xA4,              /* follow RAM, not all-on */
    0xA6,              /* normal, not inverted */
    0xAF,              /* display on */
};

bool scurry_display_present(void)
{
    return s_present;
}

bool scurry_display_start(scurry_display_poll_t poll)
{
    if (poll == NULL) {
        return false;
    }
    s_poll = poll;

    i2c_master_bus_config_t bus_cfg = {
        .i2c_port = -1,
        .sda_io_num = DISP_SDA_GPIO,
        .scl_io_num = DISP_SCL_GPIO,
        .clk_source = I2C_CLK_SRC_DEFAULT,
        .glitch_ignore_cnt = 7,
        /* The board has its own pull-ups; these are belt and braces, and cost
           nothing on a bus that is two traces long. */
        .flags.enable_internal_pullup = true,
    };
    if (i2c_new_master_bus(&bus_cfg, &s_bus) != ESP_OK) {
        ESP_LOGE(TAG, "could not open the I2C bus on SDA=%d SCL=%d",
                 DISP_SDA_GPIO, DISP_SCL_GPIO);
        return false;
    }

    /* Probe before configuring. A board with no panel is a supported
       configuration, and the failure should be one log line here rather than
       a stream of transmit errors from the refresh task. */
    if (i2c_master_probe(s_bus, DISP_I2C_ADDR, 100) != ESP_OK) {
        ESP_LOGW(TAG, "no panel at 0x%02X -- running headless", DISP_I2C_ADDR);
        i2c_del_master_bus(s_bus);
        s_bus = NULL;
        return false;
    }

    i2c_device_config_t dev_cfg = {
        .dev_addr_length = I2C_ADDR_BIT_LEN_7,
        .device_address = DISP_I2C_ADDR,
        .scl_speed_hz = DISP_I2C_HZ,
    };
    if (i2c_master_bus_add_device(s_bus, &dev_cfg, &s_dev) != ESP_OK) {
        ESP_LOGE(TAG, "could not add the panel to the bus");
        i2c_del_master_bus(s_bus);
        s_bus = NULL;
        return false;
    }

    for (size_t i = 0; i < sizeof(DISP_INIT); i++) {
        if (disp_cmd1(DISP_INIT[i]) != ESP_OK) {
            ESP_LOGE(TAG, "panel rejected init byte %u (0x%02X)",
                     (unsigned)i, DISP_INIT[i]);
            return false;
        }
    }

    /* The panel powers up with whatever was in RAM last time, which after a
       warm reset is the previous image. Clear before the first frame so a
       stale screen never outlives the firmware that drew it. */
    fb_clear();
    disp_flush();

    s_present = true;
    ESP_LOGI(TAG, "0.42\" OLED up: %dx%d at 0x%02X on SDA=%d SCL=%d",
             DISP_W, DISP_H, DISP_I2C_ADDR, DISP_SDA_GPIO, DISP_SCL_GPIO);

    /* Priority 4: below the reader task at 5, which is on the pointer's
       critical path. A late frame is invisible; a late mouse report is not. */
    xTaskCreate(&scurry_display_task, "scurry_disp", 3072, NULL, 4, NULL);
    return true;
}
