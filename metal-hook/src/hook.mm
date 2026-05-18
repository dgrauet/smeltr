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

static BOOL g_stage_sampling_enabled = NO;
// AtDispatchBoundary support (M3+ devices). Probed once at init; the
// per-dispatch path is only used when the env var SMELTR_HOOK_DISPATCH_BOUNDARY=1
// is also set. When enabled, replaces the encoder-level stage-boundary
// timing with exact per-dispatch ns instead of pro-rata.
static BOOL g_dispatch_sampling_supported = NO;
static BOOL g_dispatch_sampling_enabled = NO;
// Opt-in MTL4 machine-learning encoder visibility (macOS 26). Only swizzles
// `dispatchNetworkWithIntermediatesHeap:` on existing ML encoder classes;
// `setPipelineState:` is deliberately untouched (replacing it crashes
// Apple's ML proxy machinery). Off by default.
static BOOL g_ml_encoder_enabled = NO;
static id<MTLCounterSet> g_timestamp_counter_set = nil;
// Holds the bit pattern of the current double ns-per-tick ratio. Stored as
// _Atomic so the optional recalibration timer can update it concurrently
// without tearing reads on the hot path (smeltr_ticks_to_ns).
static _Atomic(uint64_t) g_ns_per_tick_bits = 0;
static dispatch_source_t g_recal_timer = NULL;
static id<MTLDevice> g_recal_device = nil;
static _Atomic(uint64_t) g_recal_rejected_total = 0;

static inline double smeltr_load_ns_per_tick(void) {
    uint64_t bits = atomic_load_explicit(&g_ns_per_tick_bits, memory_order_relaxed);
    double v;
    memcpy(&v, &bits, sizeof(v));
    return v;
}

static inline void smeltr_store_ns_per_tick(double v) {
    uint64_t bits;
    memcpy(&bits, &v, sizeof(bits));
    atomic_store_explicit(&g_ns_per_tick_bits, bits, memory_order_relaxed);
}

/// Detect MTLCounterSamplingPointAtStageBoundary support and find the
/// timestamp counter set. Called once at hook init.
static void smeltr_detect_stage_sampling(id<MTLDevice> device) {
    for (id<MTLCounterSet> cs in [device counterSets]) {
        if ([[cs name] isEqualToString:MTLCommonCounterSetTimestamp]) {
            g_timestamp_counter_set = cs;
            break;
        }
    }
    if (!g_timestamp_counter_set) return;
    if (![device supportsCounterSampling:MTLCounterSamplingPointAtStageBoundary]) {
        g_timestamp_counter_set = nil;
        return;
    }
    g_stage_sampling_enabled = YES;
    g_dispatch_sampling_supported =
        [device supportsCounterSampling:MTLCounterSamplingPointAtDispatchBoundary];
}

/// Take two timestamp samples 50ms apart and compute CPU-ns / GPU-tick.
/// Returns YES on a sane ratio, NO otherwise (caller decides what to do).
static BOOL smeltr_measure_tick_ratio(id<MTLDevice> device, double *out_ratio) {
    MTLTimestamp cpu1 = 0, gpu1 = 0, cpu2 = 0, gpu2 = 0;
    [device sampleTimestamps:&cpu1 gpuTimestamp:&gpu1];
    struct timespec ts = { .tv_sec = 0, .tv_nsec = 50 * 1000 * 1000 };
    nanosleep(&ts, NULL);
    [device sampleTimestamps:&cpu2 gpuTimestamp:&gpu2];
    if (gpu2 <= gpu1) return NO;
    double ratio = (double)(cpu2 - cpu1) / (double)(gpu2 - gpu1);
    if (ratio < 0.1 || ratio > 100.0) return NO;
    *out_ratio = ratio;
    return YES;
}

/// Initial calibration. On failure, disables stage-boundary sampling
/// entirely (the per-encoder timings would be meaningless without it).
static void smeltr_calibrate_ticks(id<MTLDevice> device) {
    double ratio = 0.0;
    if (!smeltr_measure_tick_ratio(device, &ratio)) {
        g_stage_sampling_enabled = NO;
        return;
    }
    smeltr_store_ns_per_tick(ratio);
}

static inline uint64_t smeltr_ticks_to_ns(uint64_t ticks) {
    return (uint64_t)((double)ticks * smeltr_load_ns_per_tick());
}

// Forward decls — full definitions appear later in this file.
static int smeltr_log(const char *fmt, ...) __attribute__((format(printf, 1, 2)));
static void smeltr_emit_metal_hook_skipped(const char *reason);

/// Opt-in periodic ticks→ns recalibration, guarded by
/// `SMELTR_HOOK_RECALIBRATE_SEC=<n>`. EMA-smoothed (alpha=0.2) so a single
/// bad sample doesn't move the ratio sharply. Rejections (sanity-check
/// failures) bump an atomic counter and emit a throttled diagnostic on
/// the MetalHookSkipped channel.
static void smeltr_recalibration_init(id<MTLDevice> device) {
    const char *raw = getenv("SMELTR_HOOK_RECALIBRATE_SEC");
    if (!raw || raw[0] == '\0') return;
    char *end = NULL;
    long secs = strtol(raw, &end, 10);
    if (end == raw || *end != '\0' || secs <= 0 || secs > 86400) {
        smeltr_log("SMELTR_HOOK_RECALIBRATE_SEC=%s: ignored (out of range 1..86400)", raw);
        return;
    }

    g_recal_device = device;  // retained for the lifetime of the timer
    dispatch_queue_t q = dispatch_queue_create("smeltr.recal", DISPATCH_QUEUE_SERIAL);
    g_recal_timer = dispatch_source_create(DISPATCH_SOURCE_TYPE_TIMER, 0, 0, q);
    uint64_t interval_ns = (uint64_t)secs * NSEC_PER_SEC;
    dispatch_source_set_timer(g_recal_timer,
        dispatch_time(DISPATCH_TIME_NOW, (int64_t)interval_ns),
        interval_ns,
        NSEC_PER_SEC);
    dispatch_source_set_event_handler(g_recal_timer, ^{
        double sample = 0.0;
        if (!smeltr_measure_tick_ratio(g_recal_device, &sample)) {
            uint64_t n = atomic_fetch_add_explicit(&g_recal_rejected_total, 1,
                                                   memory_order_relaxed) + 1;
            // Throttle: first occurrence, then every 16th.
            if (n == 1 || (n % 16) == 0) {
                char buf[96];
                snprintf(buf, sizeof(buf),
                         "tick recalibration rejected (total=%llu)",
                         (unsigned long long)n);
                smeltr_emit_metal_hook_skipped(buf);
            }
            return;
        }
        double cur = smeltr_load_ns_per_tick();
        const double alpha = 0.2;
        double ema = alpha * sample + (1.0 - alpha) * cur;
        smeltr_store_ns_per_tick(ema);
    });
    dispatch_resume(g_recal_timer);
    smeltr_log("recalibration enabled (interval=%lds, ema_alpha=0.2)", secs);
}

/* Associated-object keys for CB/encoder tracking. */
static const void *kSmeltrEncodersKey    = &kSmeltrEncodersKey;   // NSMutableArray of encoders per CB
static const void *kSmeltrEncoderPsoKey  = &kSmeltrEncoderPsoKey; // current PSO on encoder (NSValue wrapping uintptr_t)
static const void *kSmeltrEncoderDispKey = &kSmeltrEncoderDispKey; // NSMutableArray of dispatch records per encoder
static const void *kSmeltrEncoderSBKey   = &kSmeltrEncoderSBKey;  // id<MTLCounterSampleBuffer> per encoder (stage or dispatch)
static const void *kSmeltrEncoderSBIdxKey   = &kSmeltrEncoderSBIdxKey;   // NSNumber: next free dispatch-sample slot
static const void *kSmeltrEncoderDispIdxKey = &kSmeltrEncoderDispIdxKey; // NSMutableArray<NSNumber>: per-dispatch start_idx (-1 if not sampled), parallel to kSmeltrEncoderDispKey

// 512 dispatches max per encoder (2 samples per dispatch). Beyond this,
// extra dispatches contribute to a pro-rata pool of the remaining
// encoder-level time, like the stage-boundary path.
static const NSUInteger kDispatchBoundarySampleCap = 1024;

static const NSUInteger kStageBoundarySampleCount = 2;  // start + end

/* Keys for saved original IMPs (stored on the encoder class object). */
static const void *kSmeltrOrigSetPSO              = &kSmeltrOrigSetPSO;
static const void *kSmeltrOrigDispatchTG          = &kSmeltrOrigDispatchTG;
static const void *kSmeltrOrigDispatchTGI         = &kSmeltrOrigDispatchTGI;
static const void *kSmeltrOrigDispatchThreads     = &kSmeltrOrigDispatchThreads;
// MTL4-specific indirect dispatch variants (no indirectBufferOffset: parameter).
static const void *kSmeltrOrigDispatchTGNoOffset  = &kSmeltrOrigDispatchTGNoOffset;
static const void *kSmeltrOrigDispatchThrIndirect = &kSmeltrOrigDispatchThrIndirect;
// MTL4 ML encoder: dispatchNetworkWithIntermediatesHeap: (opt-in).
static const void *kSmeltrOrigDispatchNetwork     = &kSmeltrOrigDispatchNetwork;

// Top byte marker stored in the per-encoder pso slot to flag this encoder
// as an MTL4 ML-network encoder. User-space heap pointers on macOS arm64
// never have 0xFF in the top byte, so this is collision-free with real
// MTLComputePipelineState pointers.
static const uint64_t kSmeltrMLEncoderPsoMarker = 0xFF00000000000000ULL;

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
// MTL4 ML encoder: `-[T dispatchNetworkWithIntermediatesHeap:(id<MTLHeap>)]`.
// Defined later; installed only when SMELTR_HOOK_ML_ENCODER=1.
static void smeltr_dispatchNetwork_swz(id, SEL, id);

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
        // Guard: if the eager-swizzle pass already installed our wrappers on this
        // class, skip — overwriting would store our OWN IMP as "orig", causing
        // infinite recursion on the next dispatch call.
        if ((m = class_getInstanceMethod(ec, sps))) {
            IMP cur = method_getImplementation(m);
            if (cur != (IMP)smeltr_setComputePipelineState_swz) {
                IMP orig = method_setImplementation(m, (IMP)smeltr_setComputePipelineState_swz);
                objc_setAssociatedObject((id)ec, kSmeltrOrigSetPSO,
                                          [NSValue valueWithPointer:(void *)orig],
                                          OBJC_ASSOCIATION_RETAIN_NONATOMIC);
                smeltr_log("swizzled %s.setComputePipelineState:", class_getName(ec));
            }
        }
        if ((m = class_getInstanceMethod(ec, dtg))) {
            IMP cur = method_getImplementation(m);
            if (cur != (IMP)smeltr_dispatchThreadgroups_swz) {
                IMP orig = method_setImplementation(m, (IMP)smeltr_dispatchThreadgroups_swz);
                objc_setAssociatedObject((id)ec, kSmeltrOrigDispatchTG,
                                          [NSValue valueWithPointer:(void *)orig],
                                          OBJC_ASSOCIATION_RETAIN_NONATOMIC);
                smeltr_log("swizzled %s.dispatchThreadgroups:threadsPerThreadgroup:", class_getName(ec));
            }
        }
        if ((m = class_getInstanceMethod(ec, dtgi))) {
            IMP cur = method_getImplementation(m);
            if (cur != (IMP)smeltr_dispatchThreadgroupsIndirect_swz) {
                IMP orig = method_setImplementation(m, (IMP)smeltr_dispatchThreadgroupsIndirect_swz);
                objc_setAssociatedObject((id)ec, kSmeltrOrigDispatchTGI,
                                          [NSValue valueWithPointer:(void *)orig],
                                          OBJC_ASSOCIATION_RETAIN_NONATOMIC);
                smeltr_log("swizzled %s.dispatchThreadgroupsWithIndirectBuffer:offset:threadsPerThreadgroup:",
                           class_getName(ec));
            }
        }
        // MLX 0.31+ uses dispatchThreads:threadsPerThreadgroup: (non-fixed-threadgroup-count
        // variant). Swizzle it so we record the same (PSO pointer, 0x0x0) sentinel that
        // smeltr_record_dispatch uses for indirect dispatches — total-thread dims are
        // not known at encode time, so we treat them uniformly as size (0,0,0).
        if ((m = class_getInstanceMethod(ec, dtt))) {
            IMP cur = method_getImplementation(m);
            if (cur != (IMP)smeltr_dispatchThreads_swz) {
                IMP orig = method_setImplementation(m, (IMP)smeltr_dispatchThreads_swz);
                objc_setAssociatedObject((id)ec, kSmeltrOrigDispatchThreads,
                                          [NSValue valueWithPointer:(void *)orig],
                                          OBJC_ASSOCIATION_RETAIN_NONATOMIC);
                smeltr_log("swizzled %s.dispatchThreads:threadsPerThreadgroup:", class_getName(ec));
            }
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

/// Allocate a per-encoder MTLCounterSampleBuffer sized for dispatch-boundary
/// sampling. Failure path mirrors stage-boundary: throttle the log and
/// auto-disable g_dispatch_sampling_enabled after 16 consecutive failures
/// (subsequent encoders fall back to stage-boundary timing).
static void smeltr_attach_dispatch_sample_buffer(id enc, id<MTLCommandBuffer> cb) {
    if (!g_dispatch_sampling_enabled) return;
    MTLCounterSampleBufferDescriptor *sbd = [MTLCounterSampleBufferDescriptor new];
    sbd.counterSet = g_timestamp_counter_set;
    sbd.storageMode = MTLStorageModeShared;
    sbd.sampleCount = kDispatchBoundarySampleCap;
    NSError *err = nil;
    id<MTLCounterSampleBuffer> sb =
        [[cb device] newCounterSampleBufferWithDescriptor:sbd error:&err];
    if (!sb) {
        static atomic_int g_disp_alloc_failures = 0;
        static atomic_bool g_disp_alloc_logged = false;
        int n = atomic_fetch_add(&g_disp_alloc_failures, 1) + 1;
        if (!atomic_exchange(&g_disp_alloc_logged, true)) {
            smeltr_log("dispatch sample buffer alloc failed: %s "
                       "(further failures silenced)",
                       err ? [[err localizedDescription] UTF8String] : "(no error)");
        }
        if (n >= 16) {
            BOOL was_enabled = g_dispatch_sampling_enabled;
            g_dispatch_sampling_enabled = NO;
            if (was_enabled) {
                smeltr_emit_metal_hook_skipped(
                    "dispatch sampling disabled after sustained alloc failures "
                    "(stage-boundary fallback)");
            }
        }
        return;
    }
    objc_setAssociatedObject(enc, kSmeltrEncoderSBKey, sb,
                             OBJC_ASSOCIATION_RETAIN_NONATOMIC);
    objc_setAssociatedObject(enc, kSmeltrEncoderSBIdxKey, @(0u),
                             OBJC_ASSOCIATION_RETAIN_NONATOMIC);
}

/// Insert a "start" GPU timestamp before a dispatch. Returns the sample
/// slot index, or -1 if sampling isn't active or capacity is exhausted.
static NSInteger smeltr_dispatch_sample_pre(id enc) {
    if (!g_dispatch_sampling_enabled) return -1;
    NSNumber *idxBox = objc_getAssociatedObject(enc, kSmeltrEncoderSBIdxKey);
    if (!idxBox) return -1;
    NSUInteger idx = [idxBox unsignedIntegerValue];
    if (idx + 2 > kDispatchBoundarySampleCap) return -1;  // overflow → pro-rata
    id<MTLCounterSampleBuffer> sb = objc_getAssociatedObject(enc, kSmeltrEncoderSBKey);
    if (!sb) return -1;
    SEL sel = sel_registerName("sampleCountersInBuffer:atSampleIndex:withBarrier:");
    if (![enc respondsToSelector:sel]) return -1;
    typedef void (*sampler_imp)(id, SEL, id<MTLCounterSampleBuffer>, NSUInteger, BOOL);
    sampler_imp imp = (sampler_imp)[(id)enc methodForSelector:sel];
    imp(enc, sel, sb, idx, NO);
    objc_setAssociatedObject(enc, kSmeltrEncoderSBIdxKey, @(idx + 2),
                             OBJC_ASSOCIATION_RETAIN_NONATOMIC);
    return (NSInteger)idx;
}

/// Insert the matching "end" GPU timestamp after a dispatch. No-op when
/// start_idx is -1 (sampling not active for this dispatch).
static void smeltr_dispatch_sample_post(id enc, NSInteger start_idx) {
    if (start_idx < 0) return;
    id<MTLCounterSampleBuffer> sb = objc_getAssociatedObject(enc, kSmeltrEncoderSBKey);
    if (!sb) return;
    SEL sel = sel_registerName("sampleCountersInBuffer:atSampleIndex:withBarrier:");
    if (![enc respondsToSelector:sel]) return;
    typedef void (*sampler_imp)(id, SEL, id<MTLCounterSampleBuffer>, NSUInteger, BOOL);
    sampler_imp imp = (sampler_imp)[(id)enc methodForSelector:sel];
    imp(enc, sel, sb, (NSUInteger)(start_idx + 1), NO);
}

/// Append a dispatch record and (when dispatch sampling is on) insert the
/// "start" GPU timestamp sample. Returns the sample slot index for the
/// caller to pair with smeltr_dispatch_sample_post; -1 if no sample was
/// inserted (sampling off, capacity exhausted, or op-capture disabled).
static NSInteger smeltr_record_dispatch(id enc, MTLSize tg) {
    if (!g_op_capture_enabled) return -1;
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

    // Per-dispatch start_idx tracked in a parallel array. Always populated
    // (even when sampling is off — value -1 — so completion can iterate by
    // index rather than zipping two lists of potentially different lengths).
    NSMutableArray *idx_list = objc_getAssociatedObject(enc, kSmeltrEncoderDispIdxKey);
    if (!idx_list) {
        idx_list = [NSMutableArray new];
        objc_setAssociatedObject(enc, kSmeltrEncoderDispIdxKey, idx_list,
                                  OBJC_ASSOCIATION_RETAIN_NONATOMIC);
    }
    NSInteger start_idx = smeltr_dispatch_sample_pre(enc);
    [idx_list addObject:@(start_idx)];
    return start_idx;
}

static void smeltr_dispatchThreadgroups_swz(id self, SEL cmd, MTLSize tg, MTLSize tpt) {
    NSInteger idx = smeltr_record_dispatch(self, tg);
    IMP orig = smeltr_orig_imp(self, kSmeltrOrigDispatchTG);
    if (orig) ((void (*)(id, SEL, MTLSize, MTLSize))orig)(self, cmd, tg, tpt);
    smeltr_dispatch_sample_post(self, idx);
}

static void smeltr_dispatchThreadgroupsIndirect_swz(id self, SEL cmd,
        id<MTLBuffer> buf, NSUInteger off, MTLSize tpt) {
    // For indirect dispatch, we don't know the threadgroup count at encode time.
    // Record with a sentinel tg=(0,0,0) so attribution still happens.
    NSInteger idx = smeltr_record_dispatch(self, MTLSizeMake(0, 0, 0));
    IMP orig = smeltr_orig_imp(self, kSmeltrOrigDispatchTGI);
    if (orig) ((void (*)(id, SEL, id<MTLBuffer>, NSUInteger, MTLSize))orig)(self, cmd, buf, off, tpt);
    smeltr_dispatch_sample_post(self, idx);
}

static void smeltr_dispatchThreads_swz(id self, SEL cmd, MTLSize threads, MTLSize tpt) {
    // dispatchThreads:threadsPerThreadgroup: — used by MLX 0.31+.
    // The total thread count is not a threadgroup count; record sentinel (0,0,0)
    // for the tg dims so the key is (pso, 0, 0, 0). This distinguishes it from
    // dispatchThreadgroups but still groups by PSO.
    NSInteger idx = smeltr_record_dispatch(self, MTLSizeMake(0, 0, 0));
    IMP orig = smeltr_orig_imp(self, kSmeltrOrigDispatchThreads);
    if (orig) ((void (*)(id, SEL, MTLSize, MTLSize))orig)(self, cmd, threads, tpt);
    smeltr_dispatch_sample_post(self, idx);
}

static void smeltr_dispatchTGNoOffset_swz(id self, SEL cmd, id<MTLBuffer> buf, MTLSize tpt) {
    // MTL4: dispatchThreadgroupsWithIndirectBuffer:threadsPerThreadgroup:
    // (no indirectBufferOffset parameter — different from the legacy variant).
    NSInteger idx = smeltr_record_dispatch(self, MTLSizeMake(0, 0, 0));
    IMP orig = smeltr_orig_imp(self, kSmeltrOrigDispatchTGNoOffset);
    if (orig) ((void (*)(id, SEL, id<MTLBuffer>, MTLSize))orig)(self, cmd, buf, tpt);
    smeltr_dispatch_sample_post(self, idx);
}

static void smeltr_dispatchThrIndirect_swz(id self, SEL cmd, id<MTLBuffer> buf) {
    // MTL4: dispatchThreadsWithIndirectBuffer: (no threadsPerThreadgroup).
    NSInteger idx = smeltr_record_dispatch(self, MTLSizeMake(0, 0, 0));
    IMP orig = smeltr_orig_imp(self, kSmeltrOrigDispatchThrIndirect);
    if (orig) ((void (*)(id, SEL, id<MTLBuffer>))orig)(self, cmd, buf);
    smeltr_dispatch_sample_post(self, idx);
}

/// MTL4 ML encoder dispatch (one network = one dispatch from our point of
/// view). Stamps the encoder with a marker pso so the name builder formats
/// the bucket as `K_MLNet_<addr>` instead of `K_0000_0x0x0`. The network's
/// internal op decomposition is opaque to us; we just record that an ML
/// dispatch happened and forward to the original IMP.
static void smeltr_dispatchNetwork_swz(id self, SEL cmd, id heap) {
    uintptr_t enc_addr = (uintptr_t)(__bridge void *)self;
    uintptr_t marker = (enc_addr & 0x00FFFFFFFFFFFFFFULL) | kSmeltrMLEncoderPsoMarker;
    objc_setAssociatedObject(self, kSmeltrEncoderPsoKey,
                             [NSValue valueWithPointer:(void *)marker],
                             OBJC_ASSOCIATION_RETAIN_NONATOMIC);
    NSInteger idx = smeltr_record_dispatch(self, MTLSizeMake(0, 0, 0));
    IMP orig = smeltr_orig_imp(self, kSmeltrOrigDispatchNetwork);
    if (orig) ((void (*)(id, SEL, id))orig)(self, cmd, heap);
    smeltr_dispatch_sample_post(self, idx);
}

/// Install `dispatchNetworkWithIntermediatesHeap:` on any MTL4 ML encoder
/// classes present on this OS. Deliberately does NOT swizzle
/// `setPipelineState:` or any other selector on these classes — replacing
/// those crashes Apple's ML proxy machinery. No-op if no class is found.
static void smeltr_install_ml_encoder_swizzle(void) {
    static const char *const candidates[] = {
        "_MTL4MachineLearningCommandEncoder",
        "_MTL4DebugMachineLearningCommandEncoder",
        "_MTL4ToolsMachineLearningCommandEncoder",
        NULL,
    };
    SEL sel = sel_registerName("dispatchNetworkWithIntermediatesHeap:");
    int installed = 0;
    for (int i = 0; candidates[i]; i++) {
        Class c = objc_getClass(candidates[i]);
        if (!c) continue;
        Method m = class_getInstanceMethod(c, sel);
        if (!m) {
            smeltr_log("ml_encoder: %s found but lacks "
                       "dispatchNetworkWithIntermediatesHeap:", candidates[i]);
            continue;
        }
        IMP cur = method_getImplementation(m);
        if (cur == (IMP)smeltr_dispatchNetwork_swz) continue;
        IMP orig = method_setImplementation(m, (IMP)smeltr_dispatchNetwork_swz);
        objc_setAssociatedObject((id)c, kSmeltrOrigDispatchNetwork,
                                 [NSValue valueWithPointer:(void *)orig],
                                 OBJC_ASSOCIATION_RETAIN_NONATOMIC);
        smeltr_log("swizzled %s.dispatchNetworkWithIntermediatesHeap:",
                   candidates[i]);
        installed++;
    }
    if (installed == 0) {
        smeltr_emit_metal_hook_skipped(
            "SMELTR_HOOK_ML_ENCODER=1: no MTL4 ML encoder classes found");
    }
}

/* ============ CB-completion helper: emit MetalCbOps from PSO+tg buckets ============ */

/// Aggregate dispatch records per CB into (pso, tg) buckets. For each encoder,
/// use the stage-boundary sample buffer (if available) for its true GPU duration;
/// fall back to pro-rata distribution of in_flight_ns for untimed encoders.
/// Emit one MetalCbOps frame.
static void smeltr_emit_cb_ops_pso(id<MTLCommandBuffer> done_cb, uint64_t cb_id,
                                    uint64_t in_flight_ns) {
    if (!g_op_capture_enabled || !g_ring) return;
    NSMutableArray *encs = objc_getAssociatedObject(done_cb, kSmeltrEncodersKey);
    if (encs.count == 0) return;

    NSUInteger n_encs = encs.count;
    uint64_t *enc_gpu_ns = (uint64_t *)calloc(n_encs, sizeof(uint64_t));
    BOOL    *enc_is_disp = (BOOL *)calloc(n_encs, sizeof(BOOL));
    NSMutableArray<NSData *> *enc_disp_raw = [NSMutableArray new];
    if (!enc_gpu_ns || !enc_is_disp) {
        free(enc_gpu_ns); free(enc_is_disp);
        return;
    }
    NSMutableArray<NSArray *> *enc_dispatches = [NSMutableArray new];
    NSMutableArray<NSArray *> *enc_disp_idxs  = [NSMutableArray new];
    BOOL all_encoders_timed = YES;

    // First pass: classify each encoder (stage-boundary vs dispatch-boundary)
    // and compute its encoder-level GPU time when stage-sampled. For
    // dispatch-sampled encoders we keep the raw NSData for the second pass.
    for (NSUInteger i = 0; i < n_encs; i++) {
        id enc = encs[i];
        NSArray *list = objc_getAssociatedObject(enc, kSmeltrEncoderDispKey);
        if (!list) list = @[];
        [enc_dispatches addObject:list];
        NSArray *idx_list = objc_getAssociatedObject(enc, kSmeltrEncoderDispIdxKey);
        if (!idx_list) idx_list = @[];
        [enc_disp_idxs addObject:idx_list];

        // Was this encoder dispatch-sampled? Marker: at least one start_idx >= 0.
        BOOL is_disp = NO;
        if (idx_list.count == list.count && list.count > 0) {
            for (NSNumber *n in (NSArray<NSNumber *> *)idx_list) {
                if ([n integerValue] >= 0) { is_disp = YES; break; }
            }
        }
        enc_is_disp[i] = is_disp;

        id<MTLCounterSampleBuffer> sb = objc_getAssociatedObject(enc, kSmeltrEncoderSBKey);
        if (is_disp && sb) {
            NSNumber *next_box = objc_getAssociatedObject(enc, kSmeltrEncoderSBIdxKey);
            NSUInteger used = next_box ? [next_box unsignedIntegerValue] : 0;
            NSData *raw = used > 0
                ? [sb resolveCounterRange:NSMakeRange(0, used)]
                : nil;
            [enc_disp_raw addObject:(raw ?: (NSData *)[NSData data])];
            continue;
        }
        // Placeholder so enc_disp_raw is index-aligned with encs.
        [enc_disp_raw addObject:(NSData *)[NSData data]];

        if (sb && g_stage_sampling_enabled) {
            NSData *raw = [sb resolveCounterRange:NSMakeRange(0, kStageBoundarySampleCount)];
            if (raw.length >= kStageBoundarySampleCount * sizeof(MTLCounterResultTimestamp)) {
                const MTLCounterResultTimestamp *ts =
                    (const MTLCounterResultTimestamp *)raw.bytes;
                uint64_t s = ts[0].timestamp;
                uint64_t e = ts[1].timestamp;
                if (s != MTLCounterErrorValue && e != MTLCounterErrorValue && e > s) {
                    enc_gpu_ns[i] = smeltr_ticks_to_ns(e - s);
                    continue;
                }
            }
        }
        all_encoders_timed = NO;
        // enc_gpu_ns[i] stays 0 — filled below via fallback (stage path only).
    }

    // Fill encoders without sample buffers via pro-rata of the leftover
    // in_flight_ns budget. Dispatch-sampled encoders are excluded: they
    // attribute time per-dispatch in the second pass and shouldn't share
    // the global pro-rata pool.
    if (!all_encoders_timed) {
        uint64_t accounted = 0;
        uint64_t untimed_dispatches = 0;
        for (NSUInteger i = 0; i < n_encs; i++) {
            if (enc_is_disp[i]) continue;
            if (enc_gpu_ns[i] > 0) {
                accounted += enc_gpu_ns[i];
            } else {
                untimed_dispatches += ((NSArray *)enc_dispatches[i]).count;
            }
        }
        uint64_t leftover = (in_flight_ns > accounted) ? (in_flight_ns - accounted) : 0;
        for (NSUInteger i = 0; i < n_encs; i++) {
            if (enc_is_disp[i]) continue;
            if (enc_gpu_ns[i] == 0 && untimed_dispatches > 0) {
                uint64_t dcount = ((NSArray *)enc_dispatches[i]).count;
                enc_gpu_ns[i] = (leftover * dcount) / untimed_dispatches;
            }
        }
    }

    // Second pass: aggregate per encoder into global (pso, tg) → (gpu_ns, count).
    // - Dispatch-sampled encoders: exact per-dispatch ns from the resolved
    //   sample buffer; overflow dispatches (cap exceeded) contribute 0.
    // - Stage-sampled encoders: encoder time distributed pro-rata by dispatch
    //   count within the encoder (unchanged from Phase 2.5b).
    NSMutableDictionary<NSArray *, NSArray *> *agg = [NSMutableDictionary new];
    for (NSUInteger i = 0; i < n_encs; i++) {
        NSArray *list = enc_dispatches[i];
        if (list.count == 0) continue;

        if (enc_is_disp[i]) {
            NSData *raw = enc_disp_raw[i];
            const MTLCounterResultTimestamp *ts =
                (const MTLCounterResultTimestamp *)raw.bytes;
            NSUInteger n_ts = raw.length / sizeof(MTLCounterResultTimestamp);
            NSArray<NSNumber *> *idx_list = enc_disp_idxs[i];
            for (NSUInteger j = 0; j < list.count; j++) {
                NSArray *d = list[j];
                uint64_t kernel_ns = 0;
                if (j < idx_list.count) {
                    NSInteger sidx = [idx_list[j] integerValue];
                    if (sidx >= 0 && (NSUInteger)(sidx + 1) < n_ts) {
                        uint64_t s = ts[sidx].timestamp;
                        uint64_t e = ts[sidx + 1].timestamp;
                        if (s != MTLCounterErrorValue
                            && e != MTLCounterErrorValue
                            && e > s) {
                            kernel_ns = smeltr_ticks_to_ns(e - s);
                        }
                    }
                }
                NSArray *cur = agg[d] ?: @[@(0ULL), @(0ULL)];
                agg[d] = @[
                    @([cur[0] unsignedLongLongValue] + kernel_ns),
                    @([cur[1] unsignedLongLongValue] + 1),
                ];
            }
            continue;
        }

        uint64_t enc_ns = enc_gpu_ns[i];
        NSMutableDictionary<NSArray *, NSNumber *> *enc_buckets = [NSMutableDictionary new];
        for (NSArray *d in list) {
            NSNumber *cur = enc_buckets[d];
            enc_buckets[d] = @([cur unsignedLongLongValue] + 1);
        }
        for (NSArray *key in enc_buckets) {
            uint64_t dcount = [enc_buckets[key] unsignedLongLongValue];
            uint64_t kernel_ns = (enc_ns * dcount) / list.count;
            NSArray *cur = agg[key] ?: @[@(0ULL), @(0ULL)];
            agg[key] = @[
                @([cur[0] unsignedLongLongValue] + kernel_ns),
                @([cur[1] unsignedLongLongValue] + dcount),
            ];
        }
    }
    free(enc_gpu_ns);
    free(enc_is_disp);

    if (agg.count == 0) return;

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
        uint64_t pso        = [key[0] unsignedLongLongValue];
        unsigned long w     = [key[1] unsignedLongValue];
        unsigned long h     = [key[2] unsignedLongValue];
        unsigned long depth = [key[3] unsignedLongValue];
        char *name = (char *)malloc(48);
        if ((pso & 0xFF00000000000000ULL) == kSmeltrMLEncoderPsoMarker) {
            // MTL4 ML encoder dispatch — name format K_MLNet_<encoder_addr>.
            uint64_t addr = pso & 0x00FFFFFFFFFFFFFFULL;
            snprintf(name, 48, "K_MLNet_%llx", (unsigned long long)addr);
        } else {
            uint16_t pso_short = (uint16_t)(pso & 0xFFFF);
            snprintf(name, 48, "K_%04x_%lux%lux%lu", pso_short, w, h, depth);
        }
        names_buf[i]  = name;
        gpu_ns_arr[i] = [agg[key][0] unsignedLongLongValue];
        counts[i]     = (uint32_t)[agg[key][1] unsignedLongLongValue];
        i++;
    }
    smeltr_write_cb_ops(g_ring, smeltr_mono_ns(), cb_id,
                        (const char *const *)names_buf,
                        NULL,  /* symbols — wired in Task 8 */
                        gpu_ns_arr, counts, n);
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
        smeltr_attach_dispatch_sample_buffer(enc, cb);
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
        smeltr_attach_dispatch_sample_buffer(enc, cb);
    }
    return enc;
}

- (id)smeltr_computeCommandEncoderWithDispatchType:(NSUInteger)dt {
    id<MTLCommandBuffer> cb = (id<MTLCommandBuffer>)self;
    id enc;
    id<MTLCounterSampleBuffer> sb_for_this_encoder = nil;

    // Dispatch-boundary path (M3+, opt-in via SMELTR_HOOK_DISPATCH_BOUNDARY=1):
    // skip the stage-boundary descriptor substitution and attach a per-encoder
    // sample buffer explicitly. Dispatch swizzles will record per-dispatch
    // start/end samples via sampleCountersInBuffer:atSampleIndex:withBarrier:.
    if (g_op_capture_enabled && g_dispatch_sampling_enabled) {
        enc = [self smeltr_computeCommandEncoderWithDispatchType:dt]; // original
        // sb_for_this_encoder stays nil; smeltr_attach_dispatch_sample_buffer
        // associates the SB directly on the encoder below.
    } else if (g_op_capture_enabled && g_stage_sampling_enabled) {
        // Substitute WithDispatchType: with WithDescriptor: so we can attach a
        // per-encoder stage-boundary counter sample buffer.
        MTLComputePassDescriptor *desc = [MTLComputePassDescriptor computePassDescriptor];
        desc.dispatchType = (MTLDispatchType)dt;

        MTLCounterSampleBufferDescriptor *sbd = [MTLCounterSampleBufferDescriptor new];
        sbd.counterSet = g_timestamp_counter_set;
        sbd.storageMode = MTLStorageModeShared;
        sbd.sampleCount = kStageBoundarySampleCount;
        NSError *err = nil;
        sb_for_this_encoder = [[cb device] newCounterSampleBufferWithDescriptor:sbd error:&err];
        if (sb_for_this_encoder) {
            MTLComputePassSampleBufferAttachmentDescriptor *att =
                desc.sampleBufferAttachments[0];
            att.sampleBuffer = sb_for_this_encoder;
            att.startOfEncoderSampleIndex = 0;
            att.endOfEncoderSampleIndex   = 1;
        } else {
            // Stage sample buffer allocation can fail under sustained load
            // (device has a per-process quota of these). Log the first failure
            // and after N consecutive failures disable stage sampling for the
            // rest of the session; from then on the analyzer falls back to
            // Phase 2.5a per-CB pro-rata attribution.
            static atomic_int g_stage_alloc_failures = 0;
            static atomic_bool g_stage_alloc_logged = false;
            int n = atomic_fetch_add(&g_stage_alloc_failures, 1) + 1;
            if (!atomic_exchange(&g_stage_alloc_logged, true)) {
                smeltr_log("stage sample buffer alloc failed: %s (further failures silenced)",
                           err ? [[err localizedDescription] UTF8String] : "(no error)");
            }
            if (n >= 16) {
                BOOL was_enabled = g_stage_sampling_enabled;
                g_stage_sampling_enabled = NO;
                if (was_enabled) {
                    smeltr_emit_metal_hook_skipped(
                        "stage sampling disabled after sustained alloc failures (pro-rata fallback)");
                }
            }
        }
        // Forward to the original WithDescriptor: (after swap, this calls the
        // real Metal implementation for computeCommandEncoderWithDescriptor:).
        enc = [self smeltr_computeCommandEncoderWithDescriptor:desc];
    } else {
        // Fallback: call the original WithDispatchType: as before.
        enc = [self smeltr_computeCommandEncoderWithDispatchType:dt]; // original (after swap)
    }

    if (enc && g_op_capture_enabled) {
        if (sb_for_this_encoder) {
            objc_setAssociatedObject(enc, kSmeltrEncoderSBKey, sb_for_this_encoder,
                                      OBJC_ASSOCIATION_RETAIN_NONATOMIC);
        }
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
        smeltr_attach_dispatch_sample_buffer(enc, cb);
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

    // Detect + calibrate stage-boundary counter sampling BEFORE the NO_OPS
    // check so we know whether to use per-encoder timing.
    smeltr_detect_stage_sampling(d);
    if (g_stage_sampling_enabled) {
        smeltr_calibrate_ticks(d);
        if (g_stage_sampling_enabled) {
            smeltr_log("stage_sampling enabled (ns_per_tick=%.6f)",
                       smeltr_load_ns_per_tick());
            const char *db = getenv("SMELTR_HOOK_DISPATCH_BOUNDARY");
            if (db && strcmp(db, "1") == 0) {
                if (g_dispatch_sampling_supported) {
                    g_dispatch_sampling_enabled = YES;
                    smeltr_log("dispatch_sampling enabled (per-dispatch timing)");
                } else {
                    smeltr_log("SMELTR_HOOK_DISPATCH_BOUNDARY=1 ignored: "
                               "AtDispatchBoundary not supported on this device");
                }
            }
            smeltr_recalibration_init(d);
            const char *ml = getenv("SMELTR_HOOK_ML_ENCODER");
            if (ml && strcmp(ml, "1") == 0) {
                g_ml_encoder_enabled = YES;
                smeltr_install_ml_encoder_swizzle();
            }
        } else {
            smeltr_log("stage_sampling: calibration failed, falling back to 2.5a pro-rata");
        }
    } else {
        smeltr_log("stage_sampling not supported on this device, using 2.5a pro-rata");
    }

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
        // Only the three concrete compute encoder classes that MLX actually
        // instantiates. Debug/Tools wrappers are Apple-internal proxies with
        // different semantics; swizzling them crashes non-debug workloads.
        // MachineLearning encoders use a different dispatch shape
        // (dispatchNetworkWithIntermediatesHeap:) that our wrappers cannot
        // handle.
        "AGXG14XFamilyComputeContext",
        "AGXG14XFamilyComputeContext_mtlnext",
        "_MTL4ComputeCommandEncoder",
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
