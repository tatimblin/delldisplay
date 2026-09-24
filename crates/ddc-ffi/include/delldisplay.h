/* delldisplay: C ABI for DDC/CI monitor control.
 * Link against libdelldisplay.dylib or .a (built from crates/ddc-ffi).
 *
 * Every call that talks to the monitor blocks, typically 100ms or more per
 * request and several seconds if the panel is re-syncing. Call from a
 * background thread.
 *
 * A handle is not thread-safe: use each DdHandle from one thread at a time.
 *
 * int-returning functions return DD_OK (0) or a negative DD_ERR_* code,
 * except dd_count and dd_capabilities, which return a count or length on
 * success. */
#ifndef DELLDISPLAY_H
#define DELLDISPLAY_H
#include <stdint.h>
#include <stddef.h>
#ifdef __cplusplus
extern "C" {
#endif

/* Return codes */
#define DD_OK               0
#define DD_ERR_ARG          (-1)  /* NULL handle or bad argument */
#define DD_ERR_IO           (-2)  /* no reply, I2C failure, or display gone */
#define DD_ERR_REFUSED      (-3)  /* the panel declined the VCP code */
#define DD_ERR_UNSUPPORTED  (-4)  /* not available on this platform */
#define DD_ERR_PANIC        (-5)  /* internal bug; the handle may be unusable */

typedef struct DdHandle DdHandle;

/* Number of external displays, or a DD_ERR_* code. */
int        dd_count(void);
/* Open display `index` (0 = first). NULL if it is not there. */
DdHandle  *dd_open(size_t index);
/* Free a handle. NULL is ignored. */
void       dd_close(DdHandle *h);
/* Send each request twice (on = 1, the default). Dell panels need it. */
int        dd_set_double_write(DdHandle *h, int on);
/* Read a VCP code. `current` and `max` may be NULL. */
int        dd_get(DdHandle *h, uint8_t vcp, uint16_t *current, uint16_t *max);
/* Write a VCP code. Unacknowledged: read back to confirm. */
int        dd_set(DdHandle *h, uint8_t vcp, uint16_t value);
/* Copy the capabilities string into `buf`, NUL-terminated and truncated to
 * fit, like snprintf. Returns the full length excluding the NUL; if that is
 * >= len, call again with a bigger buffer. `buf` may be NULL when len is 0. */
int        dd_capabilities(DdHandle *h, char *buf, size_t len);

/* VCP codes verified on a Dell U4323QE */
#define DD_VCP_BRIGHTNESS   0x10
#define DD_VCP_CONTRAST     0x12
#define DD_VCP_INPUT        0x60
#define DD_VCP_VOLUME       0x62
#define DD_VCP_POWER        0xD6
#define DD_VCP_PIP          0xE9

/* Values for DD_VCP_INPUT */
#define DD_INPUT_DP         0x0F
#define DD_INPUT_HDMI1      0x11
#define DD_INPUT_HDMI2      0x12
#define DD_INPUT_DP2        0x13
#define DD_INPUT_USBC       0x1B

/* Values for DD_VCP_PIP */
#define DD_PIP_OFF          0x00
#define DD_PIP_ON           0x21

#ifdef __cplusplus
}
#endif
#endif
