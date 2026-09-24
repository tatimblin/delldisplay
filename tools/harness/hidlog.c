// hidlog.c — DYLD interposer: logs outbound reports AND inbound replies.
// The SDK calls these cross-image, so they ARE interposable (unlike hid_write,
// which is internal to libDellMonitorSdkLib and reached by a direct bl).
#include <stdio.h>
#include <IOKit/hid/IOHIDDevice.h>

#define INTERPOSE(_new, _old) \
  __attribute__((used)) static struct { const void *n; const void *o; } \
  _interp_##_old __attribute__((section("__DATA,__interpose"))) = \
  { (const void *)(unsigned long)&_new, (const void *)(unsigned long)&_old };

static void dumpline(const char *tag, const uint8_t *b, long len) {
    fprintf(stderr, "[hid] %-4s len=%-3ld |", tag, len);
    long n = len > 20 ? 20 : len;
    for (long i = 0; i < n; i++) fprintf(stderr, " %02X", b[i]);
    fprintf(stderr, "\n"); fflush(stderr);
}

static IOReturn my_SetReport(IOHIDDeviceRef d, IOHIDReportType t, CFIndex rid,
                             const uint8_t *r, CFIndex len) {
    dumpline("OUT", r, (long)len);
    return IOHIDDeviceSetReport(d, t, rid, r, len);
}

// Wrap the SDK's input-report callback so we can see replies.
static IOHIDReportCallback g_orig;
static void *g_origCtx;
static void my_inputCB(void *ctx, IOReturn res, void *sender, IOHIDReportType type,
                       uint32_t rid, uint8_t *report, CFIndex len) {
    dumpline("IN", report, (long)len);
    if (g_orig) g_orig(g_origCtx, res, sender, type, rid, report, len);
}
static void my_RegisterInputReportCallback(IOHIDDeviceRef d, uint8_t *buf, CFIndex len,
                                           IOHIDReportCallback cb, void *ctx) {
    g_orig = cb; g_origCtx = ctx;
    fprintf(stderr, "[hid] (input callback hooked, buflen=%ld)\n", (long)len);
    IOHIDDeviceRegisterInputReportCallback(d, buf, len, my_inputCB, ctx);
}

INTERPOSE(my_SetReport, IOHIDDeviceSetReport)
INTERPOSE(my_RegisterInputReportCallback, IOHIDDeviceRegisterInputReportCallback)
