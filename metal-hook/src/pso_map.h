#ifndef SMELTR_PSO_MAP_H
#define SMELTR_PSO_MAP_H

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* Thread-safe map: pso_addr (uintptr_t) -> strdup'd UTF-8 function name.
 *
 * The map is process-wide and has no eviction — entries live for the
 * lifetime of the dylib. MLX programs typically allocate < 5k unique PSOs;
 * the memory cost is bounded (~150 bytes per entry).
 *
 * Insert is idempotent — re-inserting the same address with a different
 * name is allowed but unusual; the first insertion wins. The stored
 * string is strdup'd, so the caller's pointer does not need to remain
 * valid.
 */
void smeltr_pso_map_init(void);
void smeltr_pso_map_insert(uintptr_t pso_addr, const char *function_name);

/* Lookup returns a borrowed pointer that remains valid for the process
 * lifetime (we never evict). Returns NULL if the address is not present. */
const char *smeltr_pso_map_lookup(uintptr_t pso_addr);

/* Test-only — clear the map and free all stored names. Production code
 * never calls this. */
void smeltr_pso_map_reset_for_tests(void);

#ifdef __cplusplus
}
#endif

#endif /* SMELTR_PSO_MAP_H */
