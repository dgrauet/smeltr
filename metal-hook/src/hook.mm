// smeltr metal-hook.dylib — top-level entry, env var reading.
// Swizzling is added in later tasks; this file currently only validates the
// build toolchain and env-var control flow.

#import <Foundation/Foundation.h>
#include <errno.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include "smeltr_ring.h"
#include "smeltr_ring_writer.h"

static smeltr_ring_t *g_ring = NULL;

static int smeltr_log(const char *fmt, ...) {
    va_list ap;
    va_start(ap, fmt);
    fprintf(stderr, "[smeltr-hook] ");
    int n = vfprintf(stderr, fmt, ap);
    fputc('\n', stderr);
    va_end(ap);
    return n;
}

__attribute__((constructor))
static void smeltr_hook_init(void) {
    const char *disabled = getenv("SMELTR_HOOK_DISABLE");
    if (disabled && strcmp(disabled, "1") == 0) {
        smeltr_log("disabled via SMELTR_HOOK_DISABLE=1");
        return;
    }
    const char *ring_path = getenv("SMELTR_RING_PATH");
    if (!ring_path || ring_path[0] == '\0') {
        smeltr_log("no SMELTR_RING_PATH set; remaining inert");
        return;
    }
    g_ring = smeltr_ring_open(ring_path);
    if (!g_ring) {
        smeltr_log("failed to open ring at %s (errno=%d)", ring_path, errno);
        return;
    }
    smeltr_log("loaded; ring=%s", ring_path);
    // Demo write — validates the C writer end-to-end. To be removed in Task 8.
    smeltr_write_cb_scheduled(g_ring, smeltr_mono_ns(), 0xdead, 0xbeef);
}

__attribute__((destructor))
static void smeltr_hook_fini(void) {
    if (g_ring) { smeltr_ring_close(g_ring); g_ring = NULL; }
}
