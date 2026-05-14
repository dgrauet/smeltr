// smeltr metal-hook.dylib — DYLD_INSERT_LIBRARIES + Metal method swizzling.
//
// Swizzled methods (per concrete class, lazy install via dispatch_once):
//   -[MTLDevice newCommandQueue]            — installed in constructor
//   -[MTLCommandQueue commandBuffer]        — installed on first queue creation
//   -[MTLCommandBuffer commit]              — installed on first CB
//
// On commit: emit MetalCbCommitted; register scheduled/completed handlers to
// emit MetalCbScheduled and MetalCbCompleted (with status, error.code,
// error.domain, in_flight_ns). Queue depth tracked per-queue via objc
// associated objects holding an atomic counter.

#import <Foundation/Foundation.h>
#import <Metal/Metal.h>
#include <objc/runtime.h>
#include <objc/message.h>
#include <stdatomic.h>
#include <errno.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include "smeltr_ring.h"
#include "smeltr_ring_writer.h"

static smeltr_ring_t *g_ring = NULL;
static atomic_bool    g_enabled = false;

static const void *kSmeltrQueueDepthKey = &kSmeltrQueueDepthKey;
static const void *kSmeltrCbCommitTsKey = &kSmeltrCbCommitTsKey;

@interface SmeltrAtomicU64 : NSObject {
    @public _Atomic uint64_t value;
}
+ (instancetype)withValue:(uint64_t)v;
@end
@implementation SmeltrAtomicU64
+ (instancetype)withValue:(uint64_t)v {
    SmeltrAtomicU64 *o = [[SmeltrAtomicU64 alloc] init];
    if (o) atomic_store_explicit(&o->value, v, memory_order_relaxed);
    return o;
}
@end

static SmeltrAtomicU64 *queue_depth_of(id queue) {
    SmeltrAtomicU64 *box = objc_getAssociatedObject(queue, kSmeltrQueueDepthKey);
    if (!box) {
        box = [SmeltrAtomicU64 withValue:0];
        objc_setAssociatedObject(queue, kSmeltrQueueDepthKey, box,
                                  OBJC_ASSOCIATION_RETAIN_NONATOMIC);
    }
    return box;
}

static int smeltr_log(const char *fmt, ...) {
    va_list ap;
    va_start(ap, fmt);
    fprintf(stderr, "[smeltr-hook] ");
    int n = vfprintf(stderr, fmt, ap);
    fputc('\n', stderr);
    va_end(ap);
    return n;
}

static BOOL swizzle_instance(Class cls, SEL original, SEL replacement) {
    Method o = class_getInstanceMethod(cls, original);
    Method r = class_getInstanceMethod(cls, replacement);
    if (!o || !r) return NO;
    method_exchangeImplementations(o, r);
    return YES;
}

/* ============ Category: replacement methods ============ */

@interface NSObject (SmeltrMetalHook)
- (id<MTLCommandBuffer>)smeltr_commandBuffer;
- (void)smeltr_commit;
- (id<MTLCommandQueue>)smeltr_newCommandQueue;
@end

/* Forward decls for lazy-install fns */
static void smeltr_install_cb_swizzle(id<MTLCommandBuffer> cb);

@implementation NSObject (SmeltrMetalHook)

/* After exchange, calling [self smeltr_commandBuffer] from inside this method
   invokes the ORIGINAL -[XXX commandBuffer] (because exchange swapped both). */
- (id<MTLCommandBuffer>)smeltr_commandBuffer {
    id<MTLCommandBuffer> cb = [self smeltr_commandBuffer];
    if (cb && atomic_load_explicit(&g_enabled, memory_order_relaxed)) {
        smeltr_install_cb_swizzle(cb);
    }
    return cb;
}

- (void)smeltr_commit {
    if (atomic_load_explicit(&g_enabled, memory_order_relaxed) && g_ring) {
        @try {
            id<MTLCommandBuffer> cb = (id<MTLCommandBuffer>)self;
            id<MTLCommandQueue> q = [cb commandQueue];
            uint64_t cb_id = (uint64_t)(uintptr_t)cb;
            uint64_t q_id  = (uint64_t)(uintptr_t)q;
            uint64_t commit_ts = smeltr_mono_ns();
            uint32_t new_depth = (uint32_t)(atomic_fetch_add_explicit(
                &queue_depth_of(q)->value, 1, memory_order_relaxed) + 1);

            // Stash commit timestamp on the CB for in_flight_ns at completion.
            objc_setAssociatedObject(cb, kSmeltrCbCommitTsKey,
                [SmeltrAtomicU64 withValue:commit_ts],
                OBJC_ASSOCIATION_RETAIN_NONATOMIC);

            NSString *label = [cb label];
            const char *label_c = label ? [label UTF8String] : NULL;
            smeltr_write_cb_committed(g_ring, commit_ts, cb_id, q_id,
                new_depth, label_c);

            // Register handlers. Capture ids by value into the blocks (they
            // become __block-stable copies).
            __block uint64_t captured_cb_id = cb_id;
            __block uint64_t captured_q_id  = q_id;
            [cb addScheduledHandler:^(id<MTLCommandBuffer> _cb) {
                (void)_cb;
                if (g_ring) {
                    smeltr_write_cb_scheduled(g_ring, smeltr_mono_ns(),
                        captured_cb_id, captured_q_id);
                }
            }];
            [cb addCompletedHandler:^(id<MTLCommandBuffer> done_cb) {
                if (!g_ring) return;
                uint64_t done_ts = smeltr_mono_ns();
                SmeltrAtomicU64 *box = objc_getAssociatedObject(done_cb, kSmeltrCbCommitTsKey);
                uint64_t in_flight = 0;
                if (box) {
                    uint64_t t0 = atomic_load_explicit(&box->value, memory_order_relaxed);
                    if (t0 > 0 && done_ts > t0) in_flight = done_ts - t0;
                }
                NSError *err = [done_cb error];
                int32_t err_present = err ? 1 : 0;
                int64_t err_code = err ? (int64_t)err.code : 0;
                const char *domain = err ? [err.domain UTF8String] : NULL;
                uint32_t status = (uint32_t)[done_cb status];
                smeltr_write_cb_completed(g_ring, done_ts,
                    captured_cb_id, captured_q_id, status,
                    err_present, err_code, domain, in_flight);
                id<MTLCommandQueue> q2 = [done_cb commandQueue];
                if (q2) {
                    atomic_fetch_sub_explicit(&queue_depth_of(q2)->value, 1,
                        memory_order_relaxed);
                }
            }];
        } @catch (NSException *e) {
            smeltr_log("exception in commit hook: %s", e.reason.UTF8String);
        }
    }
    // Tail call: invoke original commit.
    [self smeltr_commit];
}

- (id<MTLCommandQueue>)smeltr_newCommandQueue {
    id<MTLCommandQueue> q = [self smeltr_newCommandQueue]; // original
    if (q && atomic_load_explicit(&g_enabled, memory_order_relaxed)) {
        // Install commandBuffer swizzle once on the concrete queue class.
        static dispatch_once_t qonce;
        dispatch_once(&qonce, ^{
            Class qcls = object_getClass(q);
            if (swizzle_instance(qcls, @selector(commandBuffer),
                                       @selector(smeltr_commandBuffer))) {
                smeltr_log("swizzled %s.commandBuffer", class_getName(qcls));
            } else {
                smeltr_log("failed to swizzle %s.commandBuffer", class_getName(qcls));
            }
        });
    }
    return q;
}

@end

static void smeltr_install_cb_swizzle(id<MTLCommandBuffer> cb) {
    static dispatch_once_t once;
    dispatch_once(&once, ^{
        Class cbcls = object_getClass(cb);
        if (swizzle_instance(cbcls, @selector(commit), @selector(smeltr_commit))) {
            smeltr_log("swizzled %s.commit", class_getName(cbcls));
        } else {
            smeltr_log("failed to swizzle %s.commit", class_getName(cbcls));
        }
    });
}

static void smeltr_swizzle_device_class(void) {
    id<MTLDevice> d = MTLCreateSystemDefaultDevice();
    if (!d) { smeltr_log("no Metal device available"); return; }
    Class dcls = object_getClass(d);
    if (swizzle_instance(dcls, @selector(newCommandQueue),
                                @selector(smeltr_newCommandQueue))) {
        smeltr_log("swizzled %s.newCommandQueue", class_getName(dcls));
    } else {
        smeltr_log("failed to swizzle %s.newCommandQueue", class_getName(dcls));
    }
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
    atomic_store_explicit(&g_enabled, true, memory_order_release);
    smeltr_swizzle_device_class();
    smeltr_log("loaded; ring=%s, swizzles installed", ring_path);
}

__attribute__((destructor))
static void smeltr_hook_fini(void) {
    if (g_ring) { smeltr_ring_close(g_ring); g_ring = NULL; }
}
