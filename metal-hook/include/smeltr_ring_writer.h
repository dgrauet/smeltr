#ifndef SMELTR_RING_WRITER_H
#define SMELTR_RING_WRITER_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct smeltr_ring smeltr_ring_t;

smeltr_ring_t *smeltr_ring_open(const char *path);
void smeltr_ring_close(smeltr_ring_t *r);

uint64_t smeltr_mono_ns(void);

void smeltr_write_cb_committed(smeltr_ring_t *r, uint64_t ts,
    uint64_t cb_id, uint64_t queue_id, uint32_t queue_depth,
    const char *label /* nullable */);
void smeltr_write_cb_scheduled(smeltr_ring_t *r, uint64_t ts,
    uint64_t cb_id, uint64_t queue_id);
void smeltr_write_cb_completed(smeltr_ring_t *r, uint64_t ts,
    uint64_t cb_id, uint64_t queue_id, uint32_t status,
    int32_t error_code_present, int64_t error_code,
    const char *domain /* nullable */, uint64_t in_flight_ns);
void smeltr_write_cb_warning(smeltr_ring_t *r, uint64_t ts,
    uint64_t cb_id, uint64_t queue_id, uint64_t elapsed_ns);
void smeltr_write_heap_alloc(smeltr_ring_t *r, uint64_t ts,
    uint64_t heap_id, uint64_t size_bytes, const char *label);
void smeltr_write_heap_free(smeltr_ring_t *r, uint64_t ts, uint64_t heap_id);
void smeltr_write_buffer_alloc(smeltr_ring_t *r, uint64_t ts,
    uint64_t buffer_id, int32_t heap_id_present, uint64_t heap_id,
    uint64_t size_bytes, const char *label);
void smeltr_write_buffer_free(smeltr_ring_t *r, uint64_t ts, uint64_t buffer_id);
void smeltr_write_texture_alloc(smeltr_ring_t *r, uint64_t ts,
    uint64_t texture_id, int32_t heap_id_present, uint64_t heap_id,
    uint64_t size_bytes, const char *label);
void smeltr_write_texture_free(smeltr_ring_t *r, uint64_t ts, uint64_t texture_id);

#ifdef __cplusplus
}
#endif

#endif /* SMELTR_RING_WRITER_H */
