// smeltr metal-hook.dylib — DYLD_INSERT_LIBRARIES + Metal method swizzling.
//
// Swizzled methods (per concrete class, lazy install via dispatch_once):
//   -[MTLDevice newCommandQueue]            — installed in constructor
//   -[MTLCommandQueue commandBuffer]        — installed on first queue creation
//   -[MTLCommandBuffer commit]              — installed on first CB
//   -[MTLCommandBuffer computeCommandEncoder*]   — installed on first CB
//   -[MTLComputeCommandEncoder setComputePipelineState:]   — lazy, first encoder
//   -[MTLComputeCommandEncoder dispatchThreadgroups:*]     — lazy, first encoder
//   -[MTLComputeCommandEncoder dispatchThreads:*]          — lazy, first encoder (MLX 0.31+)
//
// On commit: emit MetalCbCommitted; register scheduled/completed handlers to
// emit MetalCbScheduled and MetalCbCompleted (with status, error.code,
// error.domain, in_flight_ns). Queue depth tracked per-queue via objc
// associated objects holding an atomic counter.
//
// Op attribution (Phase 2.5a): PSO pointer hash + threadgroup dims are
// captured per dispatch. At CB completion, dispatches are bucketed by
// (pso, tg) signature and in_flight_ns is distributed pro-rata to emit
// MetalCbOps frames. Name format: K_<pso_hash16>_<w>x<h>x<d>.

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

static BOOL g_op_capture_enabled = YES;  // toggled by SMELTR_HOOK_NO_OPS

/* Associated-object keys for CB/encoder tracking. */
static const void *kSmeltrEncodersKey    = &kSmeltrEncodersKey;   // NSMutableArray of encoders per CB
static const void *kSmeltrEncoderPsoKey  = &kSmeltrEncoderPsoKey; // current PSO on encoder (NSValue wrapping uintptr_t)
static const void *kSmeltrEncoderDispKey = &kSmeltrEncoderDispKey; // NSMutableArray of dispatch records per encoder

/* Keys for saved original IMPs (stored on the encoder class object). */
static const void *kSmeltrOrigSetPSO              = &kSmeltrOrigSetPSO;
static const void *kSmeltrOrigDispatchTG          = &kSmeltrOrigDispatchTG;
static const void *kSmeltrOrigDispatchTGI         = &kSmeltrOrigDispatchTGI;
static const void *kSmeltrOrigDispatchThreads     = &kSmeltrOrigDispatchThreads;
// MTL4-specific indirect dispatch variants (no indirectBufferOffset: parameter).
static const void *kSmeltrOrigDispatchTGNoOffset  = &kSmeltrOrigDispatchTGNoOffset;
static const void *kSmeltrOrigDispatchThrIndirect = &kSmeltrOrigDispatchThrIndirect;

/* Forward declaration — defined later, used in smeltr_swizzle_device_class. */
static void smeltr_emit_metal_hook_skipped(const char *reason);

static BOOL smeltr_trace_enabled(void) {
    static BOOL cached = NO;
    static BOOL checked = NO;
    if (!checked) {
        const char *v = getenv("SMELTR_HOOK_TRACE");
        cached = (v != NULL && v[0] != '\0' && v[0] != '0');
        checked = YES;
    }
    return cached;
}

#define SMELTR_TRACE(fmt, ...) do { \
    if (smeltr_trace_enabled()) { \
        fprintf(stderr, "[smeltr-hook trace] " fmt "\n", ##__VA_ARGS__); \
    } \
} while (0)

static void smeltr_dump_command_queue_methods(void) {
    if (!smeltr_trace_enabled()) return;

    const char *class_names[] = {
        "AGXG14XFamilyCommandQueue",
        "AGXG13XFamilyCommandQueue",
        "AGXG15FamilyCommandQueue",
        "AGXG14SDevice",
        "MTLCommandQueue",
        "_MTLCommandQueue",
        NULL,
    };
    for (int i = 0; class_names[i] != NULL; i++) {
        Class cls = objc_getClass(class_names[i]);
        if (cls == nil) {
            SMELTR_TRACE("class %s: NOT FOUND", class_names[i]);
            continue;
        }
        unsigned int count = 0;
        Method *methods = class_copyMethodList(cls, &count);
        SMELTR_TRACE("class %s: %u methods", class_names[i], count);
        for (unsigned int j = 0; j < count; j++) {
            SEL sel = method_getName(methods[j]);
            const char *name = sel_getName(sel);
            if (strstr(name, "ommandBuffer") != NULL ||
                strstr(name, "ommit") != NULL) {
                SMELTR_TRACE("  - %s", name);
            }
        }
        if (methods) free(methods);
    }
}

static const void *kSmeltrQueueDepthKey = &kSmeltrQueueDepthKey;
static const void *kSmeltrCbCommitTsKey = &kSmeltrCbCommitTsKey;

/* In-flight CB tracking for warning timer. Keys: NSNumber(cb_id). Values:
   NSNumber(commit_ts) — set to 0 when already warned (one-shot). Always
   accessed via g_inflight_q. */
static NSMutableDictionary<NSNumber *, NSNumber *> *g_inflight = nil;
static dispatch_queue_t g_inflight_q = NULL;
static dispatch_source_t g_warn_timer = NULL;

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

/* ============ Dispatch/PSO swizzle wrappers (forward declarations) ============ */

static void smeltr_setComputePipelineState_swz(id, SEL, id);
static void smeltr_dispatchThreadgroups_swz(id, SEL, MTLSize, MTLSize);
static void smeltr_dispatchThreadgroupsIndirect_swz(id, SEL, id<MTLBuffer>, NSUInteger, MTLSize);
static void smeltr_dispatchThreads_swz(id, SEL, MTLSize, MTLSize);
// MTL4-specific: indirect dispatch without indirectBufferOffset parameter.
static void smeltr_dispatchTGNoOffset_swz(id, SEL, id<MTLBuffer>, MTLSize);
static void smeltr_dispatchThrIndirect_swz(id, SEL, id<MTLBuffer>);

/// Look up a saved original IMP stored on the encoder's concrete class.
static IMP smeltr_orig_imp(id enc, const void *key) {
    Class ec = object_getClass(enc);
    NSValue *v = objc_getAssociatedObject((id)ec, key);
    return v ? (IMP)[v pointerValue] : NULL;
}

/// Lazily install setComputePipelineState: + dispatchThreadgroups:* swizzles
/// on the concrete encoder class the first time we see an encoder.
static void smeltr_install_dispatch_swizzles(id<MTLComputeCommandEncoder> enc) {
    static dispatch_once_t once;
    dispatch_once(&once, ^{
        Class ec = object_getClass(enc);
        SEL sps  = @selector(setComputePipelineState:);
        SEL dtg  = @selector(dispatchThreadgroups:threadsPerThreadgroup:);
        SEL dtgi = @selector(dispatchThreadgroupsWithIndirectBuffer:indirectBufferOffset:threadsPerThreadgroup:);
        SEL dtt  = @selector(dispatchThreads:threadsPerThreadgroup:);
        Method m;
        if ((m = class_getInstanceMethod(ec, sps))) {
            IMP orig = method_setImplementation(m, (IMP)smeltr_setComputePipelineState_swz);
            objc_setAssociatedObject((id)ec, kSmeltrOrigSetPSO,
                                      [NSValue valueWithPointer:(void *)orig],
                                      OBJC_ASSOCIATION_RETAIN_NONATOMIC);
            smeltr_log("swizzled %s.setComputePipelineState:", class_getName(ec));
        }
        if ((m = class_getInstanceMethod(ec, dtg))) {
            IMP orig = method_setImplementation(m, (IMP)smeltr_dispatchThreadgroups_swz);
            objc_setAssociatedObject((id)ec, kSmeltrOrigDispatchTG,
                                      [NSValue valueWithPointer:(void *)orig],
                                      OBJC_ASSOCIATION_RETAIN_NONATOMIC);
            smeltr_log("swizzled %s.dispatchThreadgroups:threadsPerThreadgroup:", class_getName(ec));
        }
        if ((m = class_getInstanceMethod(ec, dtgi))) {
            IMP orig = method_setImplementation(m, (IMP)smeltr_dispatchThreadgroupsIndirect_swz);
            objc_setAssociatedObject((id)ec, kSmeltrOrigDispatchTGI,
                                      [NSValue valueWithPointer:(void *)orig],
                                      OBJC_ASSOCIATION_RETAIN_NONATOMIC);
            smeltr_log("swizzled %s.dispatchThreadgroupsWithIndirectBuffer:offset:threadsPerThreadgroup:",
                       class_getName(ec));
        }
        // MLX 0.31+ uses dispatchThreads:threadsPerThreadgroup: (non-fixed-threadgroup-count
        // variant). Swizzle it so we record the same (PSO pointer, 0x0x0) sentinel that
        // smeltr_record_dispatch uses for indirect dispatches — total-thread dims are
        // not known at encode time, so we treat them uniformly as size (0,0,0).
        if ((m = class_getInstanceMethod(ec, dtt))) {
            IMP orig = method_setImplementation(m, (IMP)smeltr_dispatchThreads_swz);
            objc_setAssociatedObject((id)ec, kSmeltrOrigDispatchThreads,
                                      [NSValue valueWithPointer:(void *)orig],
                                      OBJC_ASSOCIATION_RETAIN_NONATOMIC);
            smeltr_log("swizzled %s.dispatchThreads:threadsPerThreadgroup:", class_getName(ec));
        }
    });
}

/* ============ Dispatch/PSO wrapper bodies ============ */

static void smeltr_setComputePipelineState_swz(id self, SEL cmd, id pso) {
    // Stash current PSO pointer on the encoder for the next dispatch.
    objc_setAssociatedObject(self, kSmeltrEncoderPsoKey,
                              [NSValue valueWithPointer:(__bridge void *)pso],
                              OBJC_ASSOCIATION_RETAIN_NONATOMIC);
    IMP orig = smeltr_orig_imp(self, kSmeltrOrigSetPSO);
    if (orig) ((void (*)(id, SEL, id))orig)(self, cmd, pso);
}

static void smeltr_record_dispatch(id enc, MTLSize tg) {
    if (!g_op_capture_enabled) return;
    SMELTR_TRACE("smeltr_record_dispatch enc=%p class=%s tg=%lux%lux%lu",
                 enc, class_getName(object_getClass(enc)),
                 (unsigned long)tg.width, (unsigned long)tg.height, (unsigned long)tg.depth);
    NSValue *psov = objc_getAssociatedObject(enc, kSmeltrEncoderPsoKey);
    uint64_t pso_ptr = psov ? (uint64_t)(uintptr_t)[psov pointerValue] : 0;
    NSMutableArray *list = objc_getAssociatedObject(enc, kSmeltrEncoderDispKey);
    if (!list) {
        // Encoder obtained before our smeltr_computeCommandEncoder swizzle was
        // in place (e.g. first CB on MLX 0.31+). Lazily create the dispatch list
        // and associate this encoder with its parent CB.
        list = [NSMutableArray new];
        objc_setAssociatedObject(enc, kSmeltrEncoderDispKey, list,
                                  OBJC_ASSOCIATION_RETAIN_NONATOMIC);
        // Link encoder → parent CB so smeltr_emit_cb_ops_pso can find it.
        // MTLComputeCommandEncoder doesn't expose commandBuffer in its public
        // protocol, but the concrete AGX classes implement it. Use message-send
        // via objc_msgSend to avoid a compile-time protocol error.
        SEL cmdBufSel = sel_registerName("commandBuffer");
        if ([enc respondsToSelector:cmdBufSel]) {
            id cb = ((id (*)(id, SEL))objc_msgSend)(enc, cmdBufSel);
            if (cb) {
                NSMutableArray *encs = objc_getAssociatedObject(cb, kSmeltrEncodersKey);
                if (!encs) {
                    encs = [NSMutableArray new];
                    objc_setAssociatedObject(cb, kSmeltrEncodersKey, encs,
                                              OBJC_ASSOCIATION_RETAIN_NONATOMIC);
                }
                [encs addObject:enc];
            }
        }
    }
    // Record (pso, tg.w, tg.h, tg.d) as a 4-element NSArray of NSNumbers.
    [list addObject:@[ @(pso_ptr), @(tg.width), @(tg.height), @(tg.depth) ]];
}

static void smeltr_dispatchThreadgroups_swz(id self, SEL cmd, MTLSize tg, MTLSize tpt) {
    smeltr_record_dispatch(self, tg);
    IMP orig = smeltr_orig_imp(self, kSmeltrOrigDispatchTG);
    if (orig) ((void (*)(id, SEL, MTLSize, MTLSize))orig)(self, cmd, tg, tpt);
}

static void smeltr_dispatchThreadgroupsIndirect_swz(id self, SEL cmd,
        id<MTLBuffer> buf, NSUInteger off, MTLSize tpt) {
    // For indirect dispatch, we don't know the threadgroup count at encode time.
    // Record with a sentinel tg=(0,0,0) so attribution still happens.
    smeltr_record_dispatch(self, MTLSizeMake(0, 0, 0));
    IMP orig = smeltr_orig_imp(self, kSmeltrOrigDispatchTGI);
    if (orig) ((void (*)(id, SEL, id<MTLBuffer>, NSUInteger, MTLSize))orig)(self, cmd, buf, off, tpt);
}

static void smeltr_dispatchThreads_swz(id self, SEL cmd, MTLSize threads, MTLSize tpt) {
    // dispatchThreads:threadsPerThreadgroup: — used by MLX 0.31+.
    // The total thread count is not a threadgroup count; record sentinel (0,0,0)
    // for the tg dims so the key is (pso, 0, 0, 0). This distinguishes it from
    // dispatchThreadgroups but still groups by PSO.
    smeltr_record_dispatch(self, MTLSizeMake(0, 0, 0));
    IMP orig = smeltr_orig_imp(self, kSmeltrOrigDispatchThreads);
    if (orig) ((void (*)(id, SEL, MTLSize, MTLSize))orig)(self, cmd, threads, tpt);
}

static void smeltr_dispatchTGNoOffset_swz(id self, SEL cmd, id<MTLBuffer> buf, MTLSize tpt) {
    // MTL4: dispatchThreadgroupsWithIndirectBuffer:threadsPerThreadgroup:
    // (no indirectBufferOffset parameter — different from the legacy variant).
    smeltr_record_dispatch(self, MTLSizeMake(0, 0, 0));
    IMP orig = smeltr_orig_imp(self, kSmeltrOrigDispatchTGNoOffset);
    if (orig) ((void (*)(id, SEL, id<MTLBuffer>, MTLSize))orig)(self, cmd, buf, tpt);
}

static void smeltr_dispatchThrIndirect_swz(id self, SEL cmd, id<MTLBuffer> buf) {
    // MTL4: dispatchThreadsWithIndirectBuffer: (no threadsPerThreadgroup).
    smeltr_record_dispatch(self, MTLSizeMake(0, 0, 0));
    IMP orig = smeltr_orig_imp(self, kSmeltrOrigDispatchThrIndirect);
    if (orig) ((void (*)(id, SEL, id<MTLBuffer>))orig)(self, cmd, buf);
}

/* ============ CB-completion helper: emit MetalCbOps from PSO+tg buckets ============ */

/// Aggregate dispatch records per CB into (pso, tg) buckets, compute
/// pro-rata gpu_ns from in_flight_ns, emit one MetalCbOps frame.
static void smeltr_emit_cb_ops_pso(id<MTLCommandBuffer> done_cb, uint64_t cb_id,
                                    uint64_t in_flight_ns) {
    if (!g_op_capture_enabled || !g_ring || in_flight_ns == 0) return;
    NSMutableArray *encs = objc_getAssociatedObject(done_cb, kSmeltrEncodersKey);
    if (encs.count == 0) return;

    // Aggregate (pso, tg.w, tg.h, tg.d) → dispatch_count across all encoders.
    NSMutableDictionary<NSArray *, NSNumber *> *agg = [NSMutableDictionary new];
    uint64_t total_dispatches = 0;
    for (id enc in encs) {
        NSArray *list = objc_getAssociatedObject(enc, kSmeltrEncoderDispKey);
        for (NSArray *d in list) {
            total_dispatches++;
            NSNumber *cur = agg[d];
            agg[d] = @([cur unsignedLongLongValue] + 1);
        }
    }
    if (total_dispatches == 0) return;

    // Build C arrays for smeltr_write_cb_ops.
    uint32_t n = (uint32_t)agg.count;
    char **names_buf  = (char **)malloc(sizeof(char *) * n);
    uint64_t *gpu_ns_arr = (uint64_t *)malloc(sizeof(uint64_t) * n);
    uint32_t *counts  = (uint32_t *)malloc(sizeof(uint32_t) * n);
    if (!names_buf || !gpu_ns_arr || !counts) {
        free(names_buf); free(gpu_ns_arr); free(counts);
        return;
    }
    uint32_t i = 0;
    for (NSArray *key in agg) {
        uint64_t pso   = [key[0] unsignedLongLongValue];
        unsigned long w     = [key[1] unsignedLongValue];
        unsigned long h     = [key[2] unsignedLongValue];
        unsigned long depth = [key[3] unsignedLongValue];
        uint16_t pso_short = (uint16_t)(pso & 0xFFFF);
        char *name = (char *)malloc(48);
        snprintf(name, 48, "K_%04x_%lux%lux%lu", pso_short, w, h, depth);
        names_buf[i] = name;
        uint64_t dcount = [agg[key] unsignedLongLongValue];
        counts[i] = (uint32_t)dcount;
        // Pro-rata: kernel_ns = in_flight_ns * dcount / total_dispatches
        gpu_ns_arr[i] = (in_flight_ns * dcount) / total_dispatches;
        i++;
    }
    smeltr_write_cb_ops(g_ring, smeltr_mono_ns(), cb_id,
                        (const char *const *)names_buf, gpu_ns_arr, counts, n);
    for (uint32_t k = 0; k < n; k++) free(names_buf[k]);
    free(names_buf);
    free(gpu_ns_arr);
    free(counts);
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
    SMELTR_TRACE("swizzled_commandBuffer hit on class=%s",
                 class_getName([self class]));
    id<MTLCommandBuffer> cb = [self smeltr_commandBuffer];
    if (cb && atomic_load_explicit(&g_enabled, memory_order_relaxed)) {
        smeltr_install_cb_swizzle(cb);
    }
    return cb;
}

- (void)smeltr_commit {
    SMELTR_TRACE("swizzled_commit hit on class=%s cb=%p",
                 class_getName([self class]), self);
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
                smeltr_emit_cb_ops_pso(done_cb, captured_cb_id, in_flight);
                id<MTLCommandQueue> q2 = [done_cb commandQueue];
                if (q2) {
                    atomic_fetch_sub_explicit(&queue_depth_of(q2)->value, 1,
                        memory_order_relaxed);
                }
                if (g_inflight_q) {
                    dispatch_async(g_inflight_q, ^{
                        [g_inflight removeObjectForKey:@(captured_cb_id)];
                    });
                }
            }];
            // Track in-flight for warning timer.
            if (g_inflight_q) {
                uint64_t cb_id_capture = cb_id;
                uint64_t ts_capture = commit_ts;
                dispatch_async(g_inflight_q, ^{
                    g_inflight[@(cb_id_capture)] = @(ts_capture);
                });
            }
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

/* ============ MTLCommandBuffer swizzles for computeCommandEncoder creation ============ */

@interface NSObject (SmeltrCbEncoderHook)
- (id)smeltr_computeCommandEncoder;
- (id)smeltr_computeCommandEncoderWithDescriptor:(id)desc;
- (id)smeltr_computeCommandEncoderWithDispatchType:(NSUInteger)dt;
@end

@implementation NSObject (SmeltrCbEncoderHook)

- (id)smeltr_computeCommandEncoder {
    id enc = [self smeltr_computeCommandEncoder]; // original (after swap)
    if (enc && g_op_capture_enabled) {
        id<MTLCommandBuffer> cb = (id<MTLCommandBuffer>)self;
        smeltr_install_dispatch_swizzles((id<MTLComputeCommandEncoder>)enc);
        objc_setAssociatedObject(enc, kSmeltrEncoderDispKey,
                                  [NSMutableArray new],
                                  OBJC_ASSOCIATION_RETAIN_NONATOMIC);
        NSMutableArray *encs = objc_getAssociatedObject(cb, kSmeltrEncodersKey);
        if (!encs) {
            encs = [NSMutableArray new];
            objc_setAssociatedObject(cb, kSmeltrEncodersKey, encs,
                                      OBJC_ASSOCIATION_RETAIN_NONATOMIC);
        }
        [encs addObject:enc];
    }
    return enc;
}

- (id)smeltr_computeCommandEncoderWithDescriptor:(id)desc {
    id enc = [self smeltr_computeCommandEncoderWithDescriptor:desc]; // original
    if (enc && g_op_capture_enabled) {
        id<MTLCommandBuffer> cb = (id<MTLCommandBuffer>)self;
        smeltr_install_dispatch_swizzles((id<MTLComputeCommandEncoder>)enc);
        objc_setAssociatedObject(enc, kSmeltrEncoderDispKey,
                                  [NSMutableArray new],
                                  OBJC_ASSOCIATION_RETAIN_NONATOMIC);
        NSMutableArray *encs = objc_getAssociatedObject(cb, kSmeltrEncodersKey);
        if (!encs) {
            encs = [NSMutableArray new];
            objc_setAssociatedObject(cb, kSmeltrEncodersKey, encs,
                                      OBJC_ASSOCIATION_RETAIN_NONATOMIC);
        }
        [encs addObject:enc];
    }
    return enc;
}

- (id)smeltr_computeCommandEncoderWithDispatchType:(NSUInteger)dt {
    id enc = [self smeltr_computeCommandEncoderWithDispatchType:dt]; // original
    if (enc && g_op_capture_enabled) {
        id<MTLCommandBuffer> cb = (id<MTLCommandBuffer>)self;
        smeltr_install_dispatch_swizzles((id<MTLComputeCommandEncoder>)enc);
        objc_setAssociatedObject(enc, kSmeltrEncoderDispKey,
                                  [NSMutableArray new],
                                  OBJC_ASSOCIATION_RETAIN_NONATOMIC);
        NSMutableArray *encs = objc_getAssociatedObject(cb, kSmeltrEncodersKey);
        if (!encs) {
            encs = [NSMutableArray new];
            objc_setAssociatedObject(cb, kSmeltrEncodersKey, encs,
                                      OBJC_ASSOCIATION_RETAIN_NONATOMIC);
        }
        [encs addObject:enc];
    }
    return enc;
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
        if (g_op_capture_enabled) {
            if (swizzle_instance(cbcls,
                                  @selector(computeCommandEncoder),
                                  @selector(smeltr_computeCommandEncoder))) {
                smeltr_log("swizzled %s.computeCommandEncoder", class_getName(cbcls));
            } else {
                smeltr_log("failed to swizzle %s.computeCommandEncoder",
                           class_getName(cbcls));
            }
            if (swizzle_instance(cbcls,
                                  @selector(computeCommandEncoderWithDescriptor:),
                                  @selector(smeltr_computeCommandEncoderWithDescriptor:))) {
                smeltr_log("swizzled %s.computeCommandEncoderWithDescriptor:",
                           class_getName(cbcls));
            } else {
                smeltr_log("failed to swizzle %s.computeCommandEncoderWithDescriptor:",
                           class_getName(cbcls));
            }
            if (swizzle_instance(cbcls,
                                  @selector(computeCommandEncoderWithDispatchType:),
                                  @selector(smeltr_computeCommandEncoderWithDispatchType:))) {
                smeltr_log("swizzled %s.computeCommandEncoderWithDispatchType:",
                           class_getName(cbcls));
            } else {
                smeltr_log("failed to swizzle %s.computeCommandEncoderWithDispatchType:",
                           class_getName(cbcls));
            }
        }
    });
}

/* ============ Alloc/free tracking ============ */

static IMP g_orig_buffer_dealloc  = NULL;
static IMP g_orig_texture_dealloc = NULL;
static IMP g_orig_heap_dealloc    = NULL;

static void smeltr_buffer_dealloc_replacement(__unsafe_unretained id self, SEL _cmd) {
    if (atomic_load_explicit(&g_enabled, memory_order_relaxed) && g_ring) {
        @try {
            smeltr_write_buffer_free(g_ring, smeltr_mono_ns(),
                                     (uint64_t)(uintptr_t)self);
        } @catch (NSException *e) {
            smeltr_log("buffer dealloc hook exc: %s", e.reason.UTF8String);
        }
    }
    ((void (*)(__unsafe_unretained id, SEL))g_orig_buffer_dealloc)(self, _cmd);
}

static void smeltr_texture_dealloc_replacement(__unsafe_unretained id self, SEL _cmd) {
    if (atomic_load_explicit(&g_enabled, memory_order_relaxed) && g_ring) {
        @try {
            smeltr_write_texture_free(g_ring, smeltr_mono_ns(),
                                      (uint64_t)(uintptr_t)self);
        } @catch (NSException *e) {
            smeltr_log("texture dealloc hook exc: %s", e.reason.UTF8String);
        }
    }
    ((void (*)(__unsafe_unretained id, SEL))g_orig_texture_dealloc)(self, _cmd);
}

static void smeltr_heap_dealloc_replacement(__unsafe_unretained id self, SEL _cmd) {
    if (atomic_load_explicit(&g_enabled, memory_order_relaxed) && g_ring) {
        @try {
            smeltr_write_heap_free(g_ring, smeltr_mono_ns(),
                                   (uint64_t)(uintptr_t)self);
        } @catch (NSException *e) {
            smeltr_log("heap dealloc hook exc: %s", e.reason.UTF8String);
        }
    }
    ((void (*)(__unsafe_unretained id, SEL))g_orig_heap_dealloc)(self, _cmd);
}

static void install_dealloc_hook(Class cls, IMP *orig_slot, IMP replacement) {
    if (!cls || *orig_slot != NULL) return;
    SEL sel = sel_registerName("dealloc");
    // Find dealloc method DIRECTLY on this class (not inherited). If it lives
    // on a superclass (e.g. NSObject), replacing it would clobber ALL objc
    // objects' dealloc. In that case, add a fresh dealloc method on this class
    // that calls the superclass implementation after emitting the event.
    unsigned int count = 0;
    Method *methods = class_copyMethodList(cls, &count);
    Method direct = NULL;
    for (unsigned int i = 0; i < count; i++) {
        if (method_getName(methods[i]) == sel) { direct = methods[i]; break; }
    }
    if (methods) free(methods);
    if (direct) {
        *orig_slot = method_setImplementation(direct, replacement);
        smeltr_log("installed dealloc hook on %s", class_getName(cls));
    } else {
        // No direct dealloc — calling super's IMP raw is fragile in practice
        // (ARC pool teardown crashed on some Metal classes). Skip; we'll miss
        // free events for these objects but won't destabilize the host.
        smeltr_log("skipping dealloc hook on %s (no direct dealloc)", class_getName(cls));
    }
}

static dispatch_once_t g_heap_suballoc_once;

static void smeltr_install_heap_suballoc_swizzles(id<MTLHeap> heap);

static void smeltr_on_buffer_alloc(id<MTLBuffer> buf, id<MTLHeap> heap /* nullable */) {
    if (!g_ring) return;
    @try {
        install_dealloc_hook(object_getClass(buf), &g_orig_buffer_dealloc,
                              (IMP)smeltr_buffer_dealloc_replacement);
        int32_t hp  = heap ? 1 : 0;
        uint64_t hid = heap ? (uint64_t)(uintptr_t)heap : 0;
        uint64_t sz  = (uint64_t)[buf length];
        NSString *lbl = [buf label];
        const char *lbl_c = lbl ? [lbl UTF8String] : NULL;
        smeltr_write_buffer_alloc(g_ring, smeltr_mono_ns(),
            (uint64_t)(uintptr_t)buf, hp, hid, sz, lbl_c);
    } @catch (NSException *e) {
        smeltr_log("buffer alloc hook exc: %s", e.reason.UTF8String);
    }
}

static void smeltr_on_texture_alloc(id<MTLTexture> tex, id<MTLHeap> heap) {
    if (!g_ring) return;
    @try {
        install_dealloc_hook(object_getClass(tex), &g_orig_texture_dealloc,
                              (IMP)smeltr_texture_dealloc_replacement);
        int32_t hp  = heap ? 1 : 0;
        uint64_t hid = heap ? (uint64_t)(uintptr_t)heap : 0;
        uint64_t sz  = (uint64_t)[tex allocatedSize];
        NSString *lbl = [tex label];
        const char *lbl_c = lbl ? [lbl UTF8String] : NULL;
        smeltr_write_texture_alloc(g_ring, smeltr_mono_ns(),
            (uint64_t)(uintptr_t)tex, hp, hid, sz, lbl_c);
    } @catch (NSException *e) {
        smeltr_log("texture alloc hook exc: %s", e.reason.UTF8String);
    }
}

static void smeltr_on_heap_alloc(id<MTLHeap> heap) {
    if (!g_ring) return;
    @try {
        smeltr_install_heap_suballoc_swizzles(heap);
        install_dealloc_hook(object_getClass(heap), &g_orig_heap_dealloc,
                              (IMP)smeltr_heap_dealloc_replacement);
        uint64_t sz = (uint64_t)[heap size];
        NSString *lbl = [heap label];
        const char *lbl_c = lbl ? [lbl UTF8String] : NULL;
        smeltr_write_heap_alloc(g_ring, smeltr_mono_ns(),
            (uint64_t)(uintptr_t)heap, sz, lbl_c);
    } @catch (NSException *e) {
        smeltr_log("heap alloc hook exc: %s", e.reason.UTF8String);
    }
}

/* MTLDevice swizzles for newBufferWithLength:options: and newHeapWithDescriptor: */
@interface NSObject (SmeltrDeviceAllocHook)
- (id<MTLBuffer>)smeltr_newBufferWithLength:(NSUInteger)length options:(MTLResourceOptions)opts;
- (id<MTLHeap>)smeltr_newHeapWithDescriptor:(MTLHeapDescriptor *)desc;
@end

@implementation NSObject (SmeltrDeviceAllocHook)
- (id<MTLBuffer>)smeltr_newBufferWithLength:(NSUInteger)length options:(MTLResourceOptions)opts {
    id<MTLBuffer> b = [self smeltr_newBufferWithLength:length options:opts];
    if (b) smeltr_on_buffer_alloc(b, nil);
    return b;
}
- (id<MTLHeap>)smeltr_newHeapWithDescriptor:(MTLHeapDescriptor *)desc {
    id<MTLHeap> h = [self smeltr_newHeapWithDescriptor:desc];
    if (h) smeltr_on_heap_alloc(h);
    return h;
}
@end

/* MTLHeap swizzles for sub-allocations. Same selector names as on the device
   category, but they live on a different concrete class, which is fine. */
@interface NSObject (SmeltrHeapAllocHook)
- (id<MTLBuffer>)smeltr_heap_newBufferWithLength:(NSUInteger)length options:(MTLResourceOptions)opts;
- (id<MTLTexture>)smeltr_heap_newTextureWithDescriptor:(MTLTextureDescriptor *)desc;
@end

@implementation NSObject (SmeltrHeapAllocHook)
- (id<MTLBuffer>)smeltr_heap_newBufferWithLength:(NSUInteger)length options:(MTLResourceOptions)opts {
    id<MTLBuffer> b = [self smeltr_heap_newBufferWithLength:length options:opts]; // original
    if (b) smeltr_on_buffer_alloc(b, (id<MTLHeap>)self);
    return b;
}
- (id<MTLTexture>)smeltr_heap_newTextureWithDescriptor:(MTLTextureDescriptor *)desc {
    id<MTLTexture> t = [self smeltr_heap_newTextureWithDescriptor:desc]; // original
    if (t) smeltr_on_texture_alloc(t, (id<MTLHeap>)self);
    return t;
}
@end

static void smeltr_install_heap_suballoc_swizzles(id<MTLHeap> heap) {
    dispatch_once(&g_heap_suballoc_once, ^{
        Class hcls = object_getClass(heap);
        if (swizzle_instance(hcls, @selector(newBufferWithLength:options:),
                                    @selector(smeltr_heap_newBufferWithLength:options:))) {
            smeltr_log("swizzled %s.newBufferWithLength:options: (heap)", class_getName(hcls));
        }
        if (swizzle_instance(hcls, @selector(newTextureWithDescriptor:),
                                    @selector(smeltr_heap_newTextureWithDescriptor:))) {
            smeltr_log("swizzled %s.newTextureWithDescriptor: (heap)", class_getName(hcls));
        }
    });
}

static void smeltr_swizzle_device_class(void) {
    id<MTLDevice> d = MTLCreateSystemDefaultDevice();
    if (!d) { smeltr_log("no Metal device available"); return; }

    const char *no_ops = getenv("SMELTR_HOOK_NO_OPS");
    if (no_ops && strcmp(no_ops, "1") == 0) {
        g_op_capture_enabled = NO;
        smeltr_emit_metal_hook_skipped("op-level capture disabled: SMELTR_HOOK_NO_OPS=1");
    }

    Class dcls = object_getClass(d);
    if (swizzle_instance(dcls, @selector(newCommandQueue),
                                @selector(smeltr_newCommandQueue))) {
        smeltr_log("swizzled %s.newCommandQueue", class_getName(dcls));
    } else {
        smeltr_log("failed to swizzle %s.newCommandQueue", class_getName(dcls));
    }
    if (swizzle_instance(dcls, @selector(newBufferWithLength:options:),
                                @selector(smeltr_newBufferWithLength:options:))) {
        smeltr_log("swizzled %s.newBufferWithLength:options:", class_getName(dcls));
    }
    if (swizzle_instance(dcls, @selector(newHeapWithDescriptor:),
                                @selector(smeltr_newHeapWithDescriptor:))) {
        smeltr_log("swizzled %s.newHeapWithDescriptor:", class_getName(dcls));
    }
}

static void smeltr_warn_init(void) {
    g_inflight = [NSMutableDictionary new];
    g_inflight_q = dispatch_queue_create("smeltr.inflight", DISPATCH_QUEUE_SERIAL);
    g_warn_timer = dispatch_source_create(DISPATCH_SOURCE_TYPE_TIMER, 0, 0, g_inflight_q);
    dispatch_source_set_timer(g_warn_timer,
        dispatch_time(DISPATCH_TIME_NOW, NSEC_PER_SEC),
        NSEC_PER_SEC,
        100 * NSEC_PER_MSEC);
    dispatch_source_set_event_handler(g_warn_timer, ^{
        if (!g_ring) return;
        uint64_t now = smeltr_mono_ns();
        // Snapshot keys to allow safe mutation below.
        NSArray<NSNumber *> *keys = [g_inflight.allKeys copy];
        for (NSNumber *k in keys) {
            NSNumber *tsBox = g_inflight[k];
            if (!tsBox) continue;
            uint64_t t0 = tsBox.unsignedLongLongValue;
            if (t0 == 0) continue; // already warned
            uint64_t elapsed = (now > t0) ? (now - t0) : 0;
            if (elapsed >= 5ULL * 1000000000ULL) {
                uint64_t cb_id = k.unsignedLongLongValue;
                smeltr_write_cb_warning(g_ring, now, cb_id, 0, elapsed);
                g_inflight[k] = @0; // sentinel: warned
            }
        }
    });
    dispatch_resume(g_warn_timer);
}

/* ============ _MTLCommandQueue swizzles (Plan 11) ============
 *
 * MLX 0.31+ on macOS 26 creates command buffers via private
 * _MTLCommandQueue methods. The AGX*FamilyCommandQueue classes targeted by
 * the Plan 3 swizzles aren't loaded at hook constructor time (they're
 * registered lazily by the Metal stack after first device init), so those
 * swizzle attempts silently no-op. _MTLCommandQueue is the private parent
 * class registered by libMetal.dylib early enough to be visible at our
 * DYLD_INSERT_LIBRARIES constructor. Swizzling here catches calls
 * regardless of the GPU-family-specific subclass via ObjC dispatch. */

static id   (*orig_cmdBufferWithDescriptor)(id, SEL, id) = NULL;
static void (*orig_commitCommandBufferWake)(id, SEL, id, BOOL) = NULL;

static uint32_t smeltr_queue_depth(id queue) {
    SEL sel = sel_registerName("numCommandBuffers");
    if (![queue respondsToSelector:sel]) return 0;
    NSUInteger (*impl)(id, SEL) =
        (NSUInteger (*)(id, SEL))[queue methodForSelector:sel];
    return (uint32_t)impl(queue, sel);
}

static id smeltr_swz_cmdBufferWithDescriptor(id self, SEL _cmd, id desc) {
    id cb = orig_cmdBufferWithDescriptor(self, _cmd, desc);
    SMELTR_TRACE("_MTLCommandQueue.commandBufferWithDescriptor: queue=%p cb=%p",
                 self, cb);
    if (cb && atomic_load_explicit(&g_enabled, memory_order_relaxed)) {
        smeltr_install_cb_swizzle((id<MTLCommandBuffer>)cb);
    }
    return cb;
}

static void smeltr_swz_commitCommandBufferWake(id self, SEL _cmd, id cb, BOOL wake) {
    SMELTR_TRACE("_MTLCommandQueue.commitCommandBuffer:wake: queue=%p cb=%p wake=%d",
                 self, cb, (int)wake);
    if (atomic_load_explicit(&g_enabled, memory_order_relaxed) && g_ring && cb) {
        @try {
            uint64_t cb_id = (uint64_t)(uintptr_t)cb;
            uint64_t q_id  = (uint64_t)(uintptr_t)self;
            uint64_t commit_ts = smeltr_mono_ns();
            uint32_t depth = smeltr_queue_depth(self);
            // Stash commit timestamp on the CB so the completion callback can
            // compute in_flight_ns even when Apple's startTime is unavailable.
            objc_setAssociatedObject(cb, kSmeltrCbCommitTsKey,
                [SmeltrAtomicU64 withValue:commit_ts],
                OBJC_ASSOCIATION_RETAIN_NONATOMIC);
            // Fallback: install encoder swizzles at commit time for CB families
            // not reached by the commandBuffer / commandBufferWithDescriptor paths
            // (e.g. MLX 0.31 on macOS 14+). dispatch_once inside is idempotent.
            smeltr_install_cb_swizzle((id<MTLCommandBuffer>)cb);
            NSString *label = nil;
            if ([cb respondsToSelector:@selector(label)]) {
                label = [(id<MTLCommandBuffer>)cb label];
            }
            const char *label_c = label ? [label UTF8String] : NULL;
            smeltr_write_cb_committed(g_ring, commit_ts, cb_id, q_id,
                depth, label_c);
            if (g_inflight_q) {
                uint64_t cb_id_capture = cb_id;
                uint64_t ts_capture = commit_ts;
                dispatch_async(g_inflight_q, ^{
                    g_inflight[@(cb_id_capture)] = @(ts_capture);
                });
            }
            // Register a completion handler via the public Metal API. We do
            // this here (rather than swizzling the private
            // commandBufferDidComplete:startTime:completionTime:error:
            // selector, whose Apple-internal calling convention disagrees
            // with the documented (double, double, NSError*) shape and
            // crashes ARC in objc_retain on the bogus 'error' arg).
            //
            // commitCommandBuffer:wake: is invoked just before the CB is
            // submitted to the GPU, which is exactly the right moment to
            // attach a completion handler: the CB is still in the
            // NotEnqueued/Enqueued state, and Metal permits adding
            // handlers up until commit. This catches CBs created via any
            // path (commandBufferWithDescriptor:, commandBuffer,
            // commandBufferWithUnretainedReferences, etc.).
            SEL addHandler = @selector(addCompletedHandler:);
            if ([cb respondsToSelector:addHandler]) {
                uint64_t q_id_capture = q_id;
                void (^handler)(id<MTLCommandBuffer>) = ^(id<MTLCommandBuffer> done_cb) {
                    if (!g_ring) return;
                    @try {
                        uint64_t done_cb_id = (uint64_t)(uintptr_t)done_cb;
                        uint64_t done_ts = smeltr_mono_ns();
                        uint64_t in_flight = 0;
                        SmeltrAtomicU64 *box = objc_getAssociatedObject(
                            done_cb, kSmeltrCbCommitTsKey);
                        if (box) {
                            uint64_t t0 = atomic_load_explicit(
                                &box->value, memory_order_relaxed);
                            if (t0 > 0 && done_ts > t0) in_flight = done_ts - t0;
                        }
                        NSError *err = [done_cb error];
                        int32_t err_present = err ? 1 : 0;
                        int64_t err_code = err ? (int64_t)err.code : 0;
                        const char *domain = err ? [err.domain UTF8String] : NULL;
                        uint32_t status = (uint32_t)[done_cb status];
                        smeltr_write_cb_completed(g_ring, done_ts,
                            done_cb_id, q_id_capture, status,
                            err_present, err_code, domain, in_flight);
                        smeltr_emit_cb_ops_pso(done_cb, done_cb_id, in_flight);
                        if (g_inflight_q) {
                            uint64_t cb_id_capture = done_cb_id;
                            dispatch_async(g_inflight_q, ^{
                                [g_inflight removeObjectForKey:@(cb_id_capture)];
                            });
                        }
                    } @catch (NSException *e) {
                        smeltr_log("exception in addCompletedHandler: %s",
                                   e.reason.UTF8String);
                    }
                };
                [(id<MTLCommandBuffer>)cb addCompletedHandler:handler];
            } else {
                SMELTR_TRACE("cb does not respond to addCompletedHandler: cb=%p", cb);
            }
        } @catch (NSException *e) {
            smeltr_log("exception in commit (parent) hook: %s", e.reason.UTF8String);
        }
    }
    orig_commitCommandBufferWake(self, _cmd, cb, wake);
}

/// Install setComputePipelineState: + dispatchThreads/Threadgroups:* swizzles
/// directly on known Metal compute encoder classes found at load time.
///
/// This is needed because smeltr_install_dispatch_swizzles() is only
/// triggered lazily when smeltr_computeCommandEncoder() is called (i.e., when
/// MLX obtains an encoder after the CB class swizzle is in place). On MLX 0.31+
/// the first CB's encoding happens before that window. By swizzling the concrete
/// encoder class here (at constructor time) we catch dispatches regardless of
/// when the encoder was obtained.
static void smeltr_install_encoder_dispatch_swizzles_eager(void) {
    if (!g_op_capture_enabled) return;
    // Known concrete classes for Metal compute/ML encoders on Apple Silicon.
    // _MTL4ComputeCommandEncoder: MTL4 path used by MLX 0.31+ on macOS 15+.
    // MTLLegacySVComputeCommandEncoder: legacy path used on earlier macOS/MLX.
    // _MTL4MachineLearningCommandEncoder: ML-hardware path used by MLX 0.31+
    //   on macOS 26 (Darwin 25.x) with Apple Silicon ANE/ML hardware; uses
    //   setPipelineState: + dispatchNetworkWithIntermediatesHeap: instead of
    //   standard compute dispatch APIs.
    static const char *encoder_class_names[] = {
        // MTL4 path on Apple Silicon (macOS 15+):
        // The concrete hardware classes are the AGXG14XFamily* contexts.
        // _MTL4ComputeCommandEncoder is a user-facing wrapper; the hardware
        // dispatch path goes through AGXG14XFamilyComputeContext_mtlnext.
        "AGXG14XFamilyComputeContext_mtlnext",
        "AGXG14XFamilyComputeContext",
        // Legacy/standard Metal path (older macOS or non-MTL4 dispatch):
        "MTLLegacySVComputeCommandEncoder",
        // User-facing wrapper classes (may delegate to the above):
        "_MTL4ComputeCommandEncoder",
        "_MTL4MachineLearningCommandEncoder",
        // Wrapper/debug/tools variants — used when Metal GPU frame capture or
        // diagnostic tools are active.
        "MTL4ToolsComputeCommandEncoder",
        "MTL4DebugComputeCommandEncoder",
        "MTL4GPUDebugComputeCommandEncoder",
        "MTL4ToolsMachineLearningCommandEncoder",
        "MTL4DebugMachineLearningCommandEncoder",
        "MTLDebugComputeCommandEncoder",
        "MTLGPUDebugComputeCommandEncoder",
        "MTLToolsComputeCommandEncoder",
        NULL,
    };
    for (int ci = 0; encoder_class_names[ci]; ci++) {
        Class ec = objc_getClass(encoder_class_names[ci]);
        if (!ec) {
            SMELTR_TRACE("encoder class %s: not found", encoder_class_names[ci]);
            continue;
        }
        // Build a table of (selector_string, IMP_wrapper, key) to avoid repetition.
        struct { const char *sel_name; IMP wrapper; const void *key; } entries[] = {
            { "setComputePipelineState:",
              (IMP)smeltr_setComputePipelineState_swz, kSmeltrOrigSetPSO },
            { "dispatchThreadgroups:threadsPerThreadgroup:",
              (IMP)smeltr_dispatchThreadgroups_swz, kSmeltrOrigDispatchTG },
            { "dispatchThreadgroupsWithIndirectBuffer:indirectBufferOffset:threadsPerThreadgroup:",
              (IMP)smeltr_dispatchThreadgroupsIndirect_swz, kSmeltrOrigDispatchTGI },
            { "dispatchThreads:threadsPerThreadgroup:",
              (IMP)smeltr_dispatchThreads_swz, kSmeltrOrigDispatchThreads },
            // MTL4-specific indirect variants (no indirectBufferOffset parameter).
            { "dispatchThreadgroupsWithIndirectBuffer:threadsPerThreadgroup:",
              (IMP)smeltr_dispatchTGNoOffset_swz, kSmeltrOrigDispatchTGNoOffset },
            { "dispatchThreadsWithIndirectBuffer:",
              (IMP)smeltr_dispatchThrIndirect_swz, kSmeltrOrigDispatchThrIndirect },
            // MTL4 ML encoder: setPipelineState: (not setComputePipelineState:).
            // Reuse smeltr_setComputePipelineState_swz — same ABI (id self, SEL, id pso).
            { "setPipelineState:",
              (IMP)smeltr_setComputePipelineState_swz, kSmeltrOrigSetPSO },
            // MTL4 ML encoder dispatch: dispatchNetworkWithIntermediatesHeap:
            // Reuse smeltr_dispatchThrIndirect_swz — ignores heap arg, records sentinel dispatch.
            { "dispatchNetworkWithIntermediatesHeap:",
              (IMP)smeltr_dispatchThrIndirect_swz, kSmeltrOrigDispatchThrIndirect },
            { NULL, NULL, NULL },
        };
        for (int ei = 0; entries[ei].sel_name; ei++) {
            SEL sel = sel_registerName(entries[ei].sel_name);
            Method m = class_getInstanceMethod(ec, sel);
            if (!m) continue;
            IMP cur = method_getImplementation(m);
            if (cur == entries[ei].wrapper) continue; // already swizzled
            IMP orig = method_setImplementation(m, entries[ei].wrapper);
            objc_setAssociatedObject((id)ec, entries[ei].key,
                                      [NSValue valueWithPointer:(void *)orig],
                                      OBJC_ASSOCIATION_RETAIN_NONATOMIC);
            smeltr_log("eager-swizzled %s.%s", encoder_class_names[ci], entries[ei].sel_name);
        }
    }
}

static void smeltr_install_mtl_command_queue_swizzles(void) {
    Class cls = objc_getClass("_MTLCommandQueue");
    if (cls == nil) {
        smeltr_log("_MTLCommandQueue not found at init; skipping parent-class swizzles");
        return;
    }
    struct {
        const char *name;
        IMP wrapper;
        IMP *orig;
    } entries[] = {
        { "commandBufferWithDescriptor:",
          (IMP)smeltr_swz_cmdBufferWithDescriptor,
          (IMP *)&orig_cmdBufferWithDescriptor },
        { "commitCommandBuffer:wake:",
          (IMP)smeltr_swz_commitCommandBufferWake,
          (IMP *)&orig_commitCommandBufferWake },
    };
    for (size_t i = 0; i < sizeof(entries) / sizeof(entries[0]); i++) {
        SEL sel = sel_registerName(entries[i].name);
        Method m = class_getInstanceMethod(cls, sel);
        if (m == NULL) {
            smeltr_log("_MTLCommandQueue.%s not found", entries[i].name);
            continue;
        }
        *entries[i].orig = (IMP)method_getImplementation(m);
        method_setImplementation(m, entries[i].wrapper);
        smeltr_log("swizzled _MTLCommandQueue.%s", entries[i].name);
    }
}

static int smeltr_detect_os_major(void) {
    const char *ovr = getenv("SMELTR_HOOK_FORCE_OS_MAJOR");
    if (ovr) {
        int n = atoi(ovr);
        if (n > 0) return n;
    }
    NSOperatingSystemVersion v = [[NSProcessInfo processInfo] operatingSystemVersion];
    return (int)v.majorVersion;
}

static const int kMinSupportedMacOSMajor = 14;

/// Returns YES if the current macOS major version is below the supported
/// minimum and the caller should skip swizzling.
static BOOL smeltr_macos_too_old(char *reason_out, size_t cap) {
    int major = smeltr_detect_os_major();
    if (major >= kMinSupportedMacOSMajor) {
        return NO;
    }
    NSOperatingSystemVersion v = [[NSProcessInfo processInfo] operatingSystemVersion];
    snprintf(reason_out, cap,
        "macOS %ld.%ld.%ld unsupported (need >= %d)",
        (long)v.majorVersion, (long)v.minorVersion, (long)v.patchVersion,
        kMinSupportedMacOSMajor);
    return YES;
}

/// Emit a MetalHookSkipped frame to the ring (if open) and log to stderr.
static void smeltr_emit_metal_hook_skipped(const char *reason) {
    if (g_ring) {
        smeltr_write_skipped(g_ring, smeltr_mono_ns(), reason);
    }
    smeltr_log("skipped: %s", reason);
}

__attribute__((constructor))
static void smeltr_hook_init(void) {
    // Diagnostic: dump AGX command-queue method lists if trace is enabled.
    // Runs unconditionally so it's useful even when the hook stays inert.
    smeltr_dump_command_queue_methods();

    // Open the ring early so the Skipped frame can be emitted on any skip path.
    const char *ring_path = getenv("SMELTR_RING_PATH");
    if (ring_path && ring_path[0] != '\0') {
        g_ring = smeltr_ring_open(ring_path);
        if (!g_ring) {
            smeltr_log("failed to open ring at %s (errno=%d)", ring_path, errno);
        }
    }

    const char *disabled = getenv("SMELTR_HOOK_DISABLE");
    if (disabled && strcmp(disabled, "1") == 0) {
        smeltr_emit_metal_hook_skipped("SMELTR_HOOK_DISABLE set");
        return;
    }

    char reason[160];
    if (smeltr_macos_too_old(reason, sizeof(reason))) {
        smeltr_emit_metal_hook_skipped(reason);
        return;
    }

    if (!ring_path || ring_path[0] == '\0') {
        smeltr_log("no SMELTR_RING_PATH set; remaining inert");
        return;
    }
    if (!g_ring) {
        // Ring path was set but open failed above; remain inert.
        return;
    }
    atomic_store_explicit(&g_enabled, true, memory_order_release);
    smeltr_swizzle_device_class();
    smeltr_warn_init();
    smeltr_install_mtl_command_queue_swizzles();
    // Install dispatch swizzles on known encoder classes immediately so that
    // the first CB's dispatches are captured even before smeltr_commandBuffer
    // fires for it (MLX 0.31+ encodes into the first CB before our lazy-install
    // path has a chance to run).
    smeltr_install_encoder_dispatch_swizzles_eager();
    smeltr_log("loaded; ring=%s, swizzles installed", ring_path);
}

__attribute__((destructor))
static void smeltr_hook_fini(void) {
    if (g_ring) { smeltr_ring_close(g_ring); g_ring = NULL; }
}
