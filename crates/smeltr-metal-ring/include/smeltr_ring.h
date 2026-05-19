/* SMELTR ring buffer wire format. Mirrored by crates/smeltr-metal-ring/src/wire.rs.
 * Any change here MUST be reflected in wire.rs AND in header_matches.rs. */
#ifndef SMELTR_RING_H
#define SMELTR_RING_H

#include <stdint.h>

#define SMELTR_RING_MAGIC   0x534D4C52u   /* "SMLR" */
#define SMELTR_RING_VERSION 3u

#define SMELTR_KIND_PAD            0u
#define SMELTR_KIND_CB_COMMITTED   1u
#define SMELTR_KIND_CB_SCHEDULED   2u
#define SMELTR_KIND_CB_COMPLETED   3u
#define SMELTR_KIND_CB_WARNING     4u
#define SMELTR_KIND_HEAP_ALLOC     5u
#define SMELTR_KIND_HEAP_FREE      6u
#define SMELTR_KIND_BUFFER_ALLOC   7u
#define SMELTR_KIND_BUFFER_FREE    8u
#define SMELTR_KIND_TEXTURE_ALLOC  9u
#define SMELTR_KIND_TEXTURE_FREE   10u
#define SMELTR_KIND_DROPPED        11u
#define SMELTR_KIND_SKIPPED        12u
#define SMELTR_KIND_CB_OPS         13u
#define SMELTR_KIND_DEVICE_MEM_SAMPLE 14u

/* Sentinel for the per-op symbol_len field in CB_OPS frames: no symbol present. */
#define SMELTR_CB_OPS_SYMBOL_LEN_NONE 0xFFFFFFFFu

typedef struct {
    uint32_t magic;
    uint32_t version;
    uint64_t capacity;
    uint64_t head;
    uint64_t tail;
    uint64_t dropped;
} smeltr_ring_header_t; /* size = 40 */

typedef struct {
    uint32_t len;
    uint32_t kind;
    uint64_t ts_mono_ns;
} smeltr_frame_header_t; /* size = 16 */

#endif /* SMELTR_RING_H */
