// hidprobe.m — probe Dell's HID->I2C tunnel (WriteI2C_MC framing). READ-ONLY.
// HID report: [0x92|0x93, i2cAddr, len, moreFlag, data...] padded 0xFF to 64 bytes.
#import <Foundation/Foundation.h>
#import <IOKit/hid/IOHIDManager.h>

static uint8_t g_in[256]; static volatile int g_inLen = 0;

static void inputCB(void *ctx, IOReturn res, void *sender, IOHIDReportType type,
                    uint32_t rid, uint8_t *report, CFIndex len) {
    if (len > 0 && len < (CFIndex)sizeof g_in) { memcpy(g_in, report, len); g_inLen = (int)len; }
    CFRunLoopStop(CFRunLoopGetCurrent());
}

static int propInt(IOHIDDeviceRef d, CFStringRef key) {
    CFTypeRef v = IOHIDDeviceGetProperty(d, key); int n = 0;
    if (v && CFGetTypeID(v) == CFNumberGetTypeID()) CFNumberGetValue(v, kCFNumberIntType, &n);
    return n;
}

static void hexdump(const char *tag, uint8_t *b, int n) {
    printf("      %s:", tag); for (int i = 0; i < n; i++) printf(" %02X", b[i]); printf("\n");
}

// Build DDC payload with Dell checksum (seed 0x6E over whole buf incl. leading 0x51)
static int ddcFrame(uint8_t *out, uint8_t *body, int blen) {
    int n = 0; out[n++] = 0x51; out[n++] = 0x80 | blen;
    memcpy(out + n, body, blen); n += blen;
    uint8_t ck = 0x6E; for (int i = 0; i < n; i++) ck ^= out[i];
    out[n++] = ck; return n;
}

static void tryDevice(IOHIDDeviceRef dev, int vid, int pid, int usagePage) {
    printf("\n--- VID 0x%04X PID 0x%04X usagePage 0x%02X ---\n", vid, pid, usagePage);
    if (IOHIDDeviceOpen(dev, kIOHIDOptionsTypeNone) != kIOReturnSuccess) {
        printf("      cannot open (permission or in use)\n"); return;
    }
    static uint8_t inbuf[256];
    IOHIDDeviceRegisterInputReportCallback(dev, inbuf, sizeof inbuf, inputCB, NULL);
    IOHIDDeviceScheduleWithRunLoop(dev, CFRunLoopGetCurrent(), kCFRunLoopDefaultMode);

    struct { const char *name; uint8_t body[4]; int blen; } tests[] = {
        { "std VCP get 0x10 (brightness)", {0x01, 0x10}, 2 },
        { "std VCP capabilities 0xF3",     {0xF3, 0x00, 0x00}, 3 },
        { "Dell vendor get 0x62 (input)",  {0xEB, 0x06, 0x62}, 3 },
        { "Dell vendor get 0x7A (PxP)",    {0xEB, 0x06, 0x7A}, 3 },
    };

    for (unsigned t = 0; t < sizeof tests / sizeof *tests; t++) {
        uint8_t ddc[64]; int dlen = ddcFrame(ddc, tests[t].body, tests[t].blen);
        // both hidapi conventions: reportID carried in byte 0, and reportID 0
        for (int mode = 0; mode < 2; mode++) {
            uint8_t rpt[64]; memset(rpt, 0xFF, sizeof rpt);
            rpt[0] = 0x92; rpt[1] = 0x37; rpt[2] = dlen; rpt[3] = 0x00;
            memcpy(rpt + 4, ddc, dlen);
            uint32_t rid = mode ? 0x92 : 0;
            uint8_t *body = mode ? rpt + 1 : rpt;
            size_t blen = mode ? 63 : 64;
            IOReturn r = IOHIDDeviceSetReport(dev, kIOHIDReportTypeOutput, rid, body, blen);
            if (r != kIOReturnSuccess) continue;
            // request read
            uint8_t rd[64]; memset(rd, 0xFF, sizeof rd);
            rd[0] = 0x93; rd[1] = 0x37; rd[2] = 0x20; rd[3] = 0x00;
            IOHIDDeviceSetReport(dev, kIOHIDReportTypeOutput, rid, mode ? rd + 1 : rd, blen);
            g_inLen = 0;
            CFRunLoopRunInMode(kCFRunLoopDefaultMode, 0.6, true);
            if (g_inLen > 0) {
                printf("   *** REPLY  %s (reportID mode %d)\n", tests[t].name, mode);
                hexdump("in", g_in, g_inLen < 24 ? g_inLen : 24);
            }
        }
    }
    IOHIDDeviceClose(dev, kIOHIDOptionsTypeNone);
}

int main(void) { @autoreleasepool {
    IOHIDManagerRef mgr = IOHIDManagerCreate(kCFAllocatorDefault, kIOHIDOptionsTypeNone);
    IOHIDManagerSetDeviceMatching(mgr, NULL);
    IOHIDManagerOpen(mgr, kIOHIDOptionsTypeNone);
    CFSetRef set = IOHIDManagerCopyDevices(mgr);
    if (!set) { printf("no HID devices\n"); return 1; }
    CFIndex n = CFSetGetCount(set);
    IOHIDDeviceRef *devs = malloc(sizeof(IOHIDDeviceRef) * n);
    CFSetGetValues(set, (const void **)devs);
    printf("scanning %ld HID devices for vendor-defined pages...\n", (long)n);
    for (CFIndex i = 0; i < n; i++) {
        int up  = propInt(devs[i], CFSTR(kIOHIDPrimaryUsagePageKey));
        int vid = propInt(devs[i], CFSTR(kIOHIDVendorIDKey));
        int pid = propInt(devs[i], CFSTR(kIOHIDProductIDKey));
        if (up >= 0xFF || vid == 0x413C || vid == 0x0424)   // vendor-defined, Dell, or Microchip
            tryDevice(devs[i], vid, pid, up);
    }
    free(devs);
    return 0;
}}
