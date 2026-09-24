// session.c — establish a Dell session token (requires on-screen user approval),
// then exercise the privileged getters. All calls go through Dell's own SDK.
#include <stdio.h>
#include <string.h>
#include <dlfcn.h>
#include <stdint.h>
#include <stdlib.h>

#define SDK "/Applications/DDPM/DDPM.app/Contents/Frameworks/libDellMonitorSdkLib.dylib"
static void *h;
static int (*GetLastErrorCode)(void);
static void *sym(const char *n) { return dlsym(h, n); }

static void show(const char *name, int rc, unsigned char *b) {
    printf("  %-20s rc=%-4d err=0x%04X  ", name, rc,
           GetLastErrorCode ? GetLastErrorCode() : -1);
    for (int i = 0; i < 8; i++) printf("%02X ", b[i]);
    printf("\n"); fflush(stdout);
}

int main(int argc, char **argv) {
    h = dlopen(SDK, RTLD_NOW | RTLD_LOCAL);
    if (!h) { fprintf(stderr, "dlopen: %s\n", dlerror()); return 1; }
    GetLastErrorCode = sym("GetLastErrorCode");

    void (*InitHandles)(void)             = sym("InitHandles");
    int  (*Initialize)(void)              = sym("Initialize");
    int  (*OpenDevice)(int)               = sym("OpenDevice");
    int  (*GetAvailableMonitors)(void *)  = sym("GetAvailableMonitors");
    int  (*ConnectMonitor)(unsigned char) = sym("ConnectMonitor");
    int  (*StartSession)(int, void *, int)= sym("StartSession");
    int  (*HelperIsTokenValid)(void)      = sym("HelperIsTokenValid");

    unsigned char *buf = calloc(1, 8192);

    if (InitHandles) InitHandles();
    if (Initialize)  Initialize();
    if (OpenDevice)  printf("OpenDevice(0) -> %d\n", OpenDevice(0));
    if (GetAvailableMonitors) { memset(buf,0,256); GetAvailableMonitors(buf);
                                printf("monitors = %d\n", buf[0]); }
    if (ConnectMonitor) printf("ConnectMonitor(0) -> %d\n", ConnectMonitor(0));
    if (HelperIsTokenValid) printf("HelperIsTokenValid -> %d\n", HelperIsTokenValid());

    int mode = (argc > 1) ? atoi(argv[1]) : 0;
    printf("\n**** LOOK AT THE MONITOR — APPROVE THE PROMPT (up to ~30s) ****\n");
    fflush(stdout);
    memset(buf, 0, 8192);
    int rc = StartSession ? StartSession(mode, buf, 0) : -99;
    printf("StartSession(%d,...) -> %d  err=0x%04X\n", mode, rc,
           GetLastErrorCode ? GetLastErrorCode() : -1);
    if (HelperIsTokenValid) printf("HelperIsTokenValid -> %d\n", HelperIsTokenValid());
    fflush(stdout);

    printf("\n--- privileged getters after session ---\n");
    const char *names[] = { "GetBrightness", "GetPowerState", "GetVideoInput",
                            "GetVideoInputCaps", "GetAutoSelect", "GetPxPMode",
                            "GetPxPLayout", "GetPxPSubInput", "GetUSBAssociation",
                            "GetVersionFirmware" };
    for (unsigned i = 0; i < sizeof names/sizeof *names; i++) {
        int (*fn)(void *) = sym(names[i]);
        if (!fn) { printf("  %-20s (missing)\n", names[i]); continue; }
        memset(buf, 0, 512);
        show(names[i], fn(buf), buf);
    }
    return 0;
}
