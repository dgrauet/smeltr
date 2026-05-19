#include "smeltr_ring.h"
#include "smeltr_ring_writer.h"

#include <fcntl.h>
#include <mach/mach_time.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <unistd.h>

struct smeltr_ring {
    int fd;
    uint8_t *map;
    size_t map_len;
    uint64_t capacity;
    uint64_t mask;
    smeltr_ring_header_t *hdr;
    uint8_t *data;
};

static inline _Atomic uint64_t *as_atomic64(void *p) { return (_Atomic uint64_t *)p; }
static inline size_t round8(size_t n) { return (n + 7) & ~(size_t)7; }

smeltr_ring_t *smeltr_ring_open(const char *path) {
    int fd = open(path, O_RDWR);
    if (fd < 0) return NULL;
    struct stat st;
    if (fstat(fd, &st) != 0) { close(fd); return NULL; }
    void *map = mmap(NULL, (size_t)st.st_size, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    if (map == MAP_FAILED) { close(fd); return NULL; }
    smeltr_ring_header_t *hdr = (smeltr_ring_header_t *)map;
    if (hdr->magic != SMELTR_RING_MAGIC || hdr->version != SMELTR_RING_VERSION) {
        munmap(map, (size_t)st.st_size); close(fd); return NULL;
    }
    smeltr_ring_t *r = (smeltr_ring_t *)calloc(1, sizeof(*r));
    if (!r) { munmap(map, (size_t)st.st_size); close(fd); return NULL; }
    r->fd = fd;
    r->map = (uint8_t *)map;
    r->map_len = (size_t)st.st_size;
    r->capacity = hdr->capacity;
    r->mask = r->capacity - 1;
    r->hdr = hdr;
    r->data = r->map + sizeof(smeltr_ring_header_t);
    return r;
}

void smeltr_ring_close(smeltr_ring_t *r) {
    if (!r) return;
    if (r->map) munmap(r->map, r->map_len);
    if (r->fd >= 0) close(r->fd);
    free(r);
}

uint64_t smeltr_mono_ns(void) {
    static mach_timebase_info_data_t tb = {0, 0};
    if (tb.denom == 0) mach_timebase_info(&tb);
    return mach_absolute_time() * tb.numer / tb.denom;
}

static void write_frame(smeltr_ring_t *r, uint32_t kind, uint64_t ts,
                        const uint8_t *payload, size_t payload_len) {
    if (!r) return;
    size_t frame_len = round8(sizeof(smeltr_frame_header_t) + payload_len);

    _Atomic uint64_t *p_head    = as_atomic64(&r->hdr->head);
    _Atomic uint64_t *p_tail    = as_atomic64(&r->hdr->tail);
    _Atomic uint64_t *p_dropped = as_atomic64(&r->hdr->dropped);

    uint64_t head = atomic_load_explicit(p_head, memory_order_relaxed);
    uint64_t tail = atomic_load_explicit(p_tail, memory_order_acquire);
    uint64_t free_bytes = r->capacity - (head - tail);
    if ((uint64_t)frame_len > free_bytes) {
        atomic_fetch_add_explicit(p_dropped, 1, memory_order_relaxed);
        return;
    }

    size_t offset = (size_t)(head & r->mask);
    size_t to_end = (size_t)r->capacity - offset;
    if (frame_len > to_end) {
        if (to_end < sizeof(smeltr_frame_header_t)) {
            atomic_fetch_add_explicit(p_dropped, 1, memory_order_relaxed);
            return;
        }
        smeltr_frame_header_t pad = { (uint32_t)to_end, SMELTR_KIND_PAD, ts };
        memcpy(r->data + offset, &pad, sizeof(pad));
        atomic_store_explicit(p_head, head + (uint64_t)to_end, memory_order_release);
        write_frame(r, kind, ts, payload, payload_len);
        return;
    }

    smeltr_frame_header_t hdr = { (uint32_t)frame_len, kind, ts };
    memcpy(r->data + offset, &hdr, sizeof(hdr));
    memcpy(r->data + offset + sizeof(hdr), payload, payload_len);
    if (frame_len > sizeof(hdr) + payload_len) {
        memset(r->data + offset + sizeof(hdr) + payload_len, 0,
               frame_len - sizeof(hdr) - payload_len);
    }
    atomic_store_explicit(p_head, head + (uint64_t)frame_len, memory_order_release);
}

#define BUF_PUSH(buf, off, val, sz) do { memcpy((buf) + (off), &(val), (sz)); (off) += (sz); } while (0)
#define BUF_PUSH_U32(buf, off, v) do { uint32_t _t = (v); BUF_PUSH(buf, off, _t, 4); } while (0)
#define BUF_PUSH_I32(buf, off, v) do { int32_t  _t = (v); BUF_PUSH(buf, off, _t, 4); } while (0)
#define BUF_PUSH_U64(buf, off, v) do { uint64_t _t = (v); BUF_PUSH(buf, off, _t, 8); } while (0)
#define BUF_PUSH_I64(buf, off, v) do { int64_t  _t = (v); BUF_PUSH(buf, off, _t, 8); } while (0)
static void push_label(uint8_t *buf, size_t *off, const char *s) {
    if (!s) { uint32_t z = 0; memcpy(buf + *off, &z, 4); *off += 4; return; }
    uint32_t n = (uint32_t)strlen(s);
    memcpy(buf + *off, &n, 4); *off += 4;
    memcpy(buf + *off, s, n); *off += n;
}

void smeltr_write_cb_committed(smeltr_ring_t *r, uint64_t ts,
    uint64_t cb_id, uint64_t queue_id, uint32_t queue_depth, const char *label)
{
    uint8_t buf[512]; size_t off = 0;
    BUF_PUSH_U64(buf, off, cb_id); BUF_PUSH_U64(buf, off, queue_id);
    BUF_PUSH_U32(buf, off, queue_depth);
    push_label(buf, &off, label);
    write_frame(r, SMELTR_KIND_CB_COMMITTED, ts, buf, off);
}

void smeltr_write_cb_scheduled(smeltr_ring_t *r, uint64_t ts,
    uint64_t cb_id, uint64_t queue_id)
{
    uint8_t buf[16]; size_t off = 0;
    BUF_PUSH_U64(buf, off, cb_id); BUF_PUSH_U64(buf, off, queue_id);
    write_frame(r, SMELTR_KIND_CB_SCHEDULED, ts, buf, off);
}

void smeltr_write_cb_completed(smeltr_ring_t *r, uint64_t ts,
    uint64_t cb_id, uint64_t queue_id, uint32_t status,
    int32_t error_code_present, int64_t error_code,
    const char *domain, uint64_t in_flight_ns)
{
    uint8_t buf[256]; size_t off = 0;
    BUF_PUSH_U64(buf, off, cb_id); BUF_PUSH_U64(buf, off, queue_id);
    BUF_PUSH_U32(buf, off, status);
    BUF_PUSH_I32(buf, off, error_code_present);
    BUF_PUSH_I64(buf, off, error_code);
    push_label(buf, &off, domain);
    BUF_PUSH_U64(buf, off, in_flight_ns);
    write_frame(r, SMELTR_KIND_CB_COMPLETED, ts, buf, off);
}

void smeltr_write_cb_warning(smeltr_ring_t *r, uint64_t ts,
    uint64_t cb_id, uint64_t queue_id, uint64_t elapsed_ns)
{
    uint8_t buf[24]; size_t off = 0;
    BUF_PUSH_U64(buf, off, cb_id); BUF_PUSH_U64(buf, off, queue_id); BUF_PUSH_U64(buf, off, elapsed_ns);
    write_frame(r, SMELTR_KIND_CB_WARNING, ts, buf, off);
}

void smeltr_write_heap_alloc(smeltr_ring_t *r, uint64_t ts,
    uint64_t heap_id, uint64_t size_bytes, const char *label)
{
    uint8_t buf[512]; size_t off = 0;
    BUF_PUSH_U64(buf, off, heap_id); BUF_PUSH_U64(buf, off, size_bytes);
    push_label(buf, &off, label);
    write_frame(r, SMELTR_KIND_HEAP_ALLOC, ts, buf, off);
}

void smeltr_write_heap_free(smeltr_ring_t *r, uint64_t ts, uint64_t heap_id) {
    uint8_t buf[8]; size_t off = 0; BUF_PUSH_U64(buf, off, heap_id);
    write_frame(r, SMELTR_KIND_HEAP_FREE, ts, buf, off);
}

void smeltr_write_buffer_alloc(smeltr_ring_t *r, uint64_t ts,
    uint64_t buffer_id, int32_t heap_id_present, uint64_t heap_id,
    uint64_t size_bytes, const char *label)
{
    uint8_t buf[512]; size_t off = 0;
    BUF_PUSH_U64(buf, off, buffer_id);
    BUF_PUSH_I32(buf, off, heap_id_present);
    BUF_PUSH_U64(buf, off, heap_id);
    BUF_PUSH_U64(buf, off, size_bytes);
    push_label(buf, &off, label);
    write_frame(r, SMELTR_KIND_BUFFER_ALLOC, ts, buf, off);
}

void smeltr_write_buffer_free(smeltr_ring_t *r, uint64_t ts, uint64_t buffer_id) {
    uint8_t buf[8]; size_t off = 0; BUF_PUSH_U64(buf, off, buffer_id);
    write_frame(r, SMELTR_KIND_BUFFER_FREE, ts, buf, off);
}

void smeltr_write_texture_alloc(smeltr_ring_t *r, uint64_t ts,
    uint64_t texture_id, int32_t heap_id_present, uint64_t heap_id,
    uint64_t size_bytes, const char *label)
{
    uint8_t buf[512]; size_t off = 0;
    BUF_PUSH_U64(buf, off, texture_id);
    BUF_PUSH_I32(buf, off, heap_id_present);
    BUF_PUSH_U64(buf, off, heap_id);
    BUF_PUSH_U64(buf, off, size_bytes);
    push_label(buf, &off, label);
    write_frame(r, SMELTR_KIND_TEXTURE_ALLOC, ts, buf, off);
}

void smeltr_write_texture_free(smeltr_ring_t *r, uint64_t ts, uint64_t texture_id) {
    uint8_t buf[8]; size_t off = 0; BUF_PUSH_U64(buf, off, texture_id);
    write_frame(r, SMELTR_KIND_TEXTURE_FREE, ts, buf, off);
}

void smeltr_write_skipped(smeltr_ring_t *r, uint64_t ts, const char *reason) {
    uint8_t buf[512]; size_t off = 0;
    push_label(buf, &off, reason);
    write_frame(r, SMELTR_KIND_SKIPPED, ts, buf, off);
}

void smeltr_write_cb_ops(smeltr_ring_t *r, uint64_t ts,
    uint64_t cb_id,
    const char *const *names,
    const char *const *symbols,
    const uint64_t *gpu_ns,
    const uint32_t *counts,
    uint32_t op_count)
{
    /* per-op upper bound: 4 (name_len) + 1024 (name) + 4 (symbol_len) +
       1024 (symbol) + 8 (gpu_ns) + 4 (count). Names/symbols are bounded
       in practice by MLX primitive/kernel names (~30 / ~80 chars), but
       cap each at 1024 for safety to keep buffer size sane. */
    size_t cap = 16;
    for (uint32_t i = 0; i < op_count; i++) {
        size_t nl = names[i] ? strlen(names[i]) : 0;
        if (nl > 1024) nl = 1024;
        size_t sl = 0;
        if (symbols != NULL && symbols[i] != NULL) {
            sl = strlen(symbols[i]);
            if (sl > 1024) sl = 1024;
        }
        cap += 4 + nl + 4 + sl + 8 + 4;
    }
    uint8_t *buf = (uint8_t *)malloc(cap);
    if (!buf) return;
    size_t off = 0;
    BUF_PUSH_U64(buf, off, cb_id);
    BUF_PUSH_U32(buf, off, op_count);
    for (uint32_t i = 0; i < op_count; i++) {
        const char *name = names[i] ? names[i] : "";
        size_t nl = strlen(name);
        if (nl > 1024) nl = 1024;
        uint32_t nl32 = (uint32_t)nl;
        BUF_PUSH_U32(buf, off, nl32);
        memcpy(buf + off, name, nl); off += nl;

        if (symbols != NULL && symbols[i] != NULL) {
            size_t sl = strlen(symbols[i]);
            if (sl > 1024) sl = 1024;
            uint32_t sl32 = (uint32_t)sl;
            BUF_PUSH_U32(buf, off, sl32);
            memcpy(buf + off, symbols[i], sl); off += sl;
        } else {
            BUF_PUSH_U32(buf, off, SMELTR_CB_OPS_SYMBOL_LEN_NONE);
        }

        BUF_PUSH_U64(buf, off, gpu_ns[i]);
        BUF_PUSH_U32(buf, off, counts[i]);
    }
    write_frame(r, SMELTR_KIND_CB_OPS, ts, buf, off);
    free(buf);
}

void smeltr_write_device_mem_sample(smeltr_ring_t *r, uint64_t ts,
    uint64_t allocated_bytes,
    uint64_t recommended_max_bytes,
    const char *at_event)
{
    const char *evt = at_event ? at_event : "";
    size_t el = strlen(evt);
    if (el > 64) el = 64; /* sanity cap */
    size_t cap = 8 + 8 + 4 + el;
    uint8_t *buf = (uint8_t *)malloc(cap);
    if (!buf) return;
    size_t off = 0;
    BUF_PUSH_U64(buf, off, allocated_bytes);
    BUF_PUSH_U64(buf, off, recommended_max_bytes);
    BUF_PUSH_U32(buf, off, (uint32_t)el);
    memcpy(buf + off, evt, el);
    off += el;
    write_frame(r, SMELTR_KIND_DEVICE_MEM_SAMPLE, ts, buf, off);
    free(buf);
}
