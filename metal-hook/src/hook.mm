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
    struct stat st;
    if (stat(ring_path, &st) != 0) {
        smeltr_log("SMELTR_RING_PATH=%s does not exist (errno=%d)", ring_path, errno);
        return;
    }
    smeltr_log("loaded; ring=%s size=%lld", ring_path, (long long)st.st_size);
}
