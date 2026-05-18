#import "pso_map.h"

#import <Foundation/Foundation.h>
#import <os/lock.h>
#import <stdlib.h>
#import <string.h>

static os_unfair_lock g_pso_map_lock = OS_UNFAIR_LOCK_INIT;
// NSMutableDictionary<NSNumber* (uintptr_t boxed), NSValue* (const char* boxed)>.
// The const char* is strdup'd and owned by the map for the process lifetime.
static NSMutableDictionary *g_pso_map = nil;

void smeltr_pso_map_init(void) {
    os_unfair_lock_lock(&g_pso_map_lock);
    if (g_pso_map == nil) {
        g_pso_map = [[NSMutableDictionary alloc] initWithCapacity:1024];
    }
    os_unfair_lock_unlock(&g_pso_map_lock);
}

void smeltr_pso_map_insert(uintptr_t pso_addr, const char *function_name) {
    if (function_name == NULL || pso_addr == 0) return;
    os_unfair_lock_lock(&g_pso_map_lock);
    if (g_pso_map == nil) {
        g_pso_map = [[NSMutableDictionary alloc] initWithCapacity:1024];
    }
    NSNumber *key = @((unsigned long long)pso_addr);
    if (g_pso_map[key] == nil) {
        char *copy = strdup(function_name);
        if (copy != NULL) {
            g_pso_map[key] = [NSValue valueWithPointer:copy];
        }
    }
    os_unfair_lock_unlock(&g_pso_map_lock);
}

const char *smeltr_pso_map_lookup(uintptr_t pso_addr) {
    if (pso_addr == 0) return NULL;
    os_unfair_lock_lock(&g_pso_map_lock);
    const char *out = NULL;
    if (g_pso_map != nil) {
        NSValue *v = g_pso_map[@((unsigned long long)pso_addr)];
        if (v != nil) {
            out = (const char *)[v pointerValue];
        }
    }
    os_unfair_lock_unlock(&g_pso_map_lock);
    return out;
}

void smeltr_pso_map_reset_for_tests(void) {
    os_unfair_lock_lock(&g_pso_map_lock);
    if (g_pso_map != nil) {
        [g_pso_map enumerateKeysAndObjectsUsingBlock:^(id, NSValue *v, BOOL *) {
            char *p = (char *)[v pointerValue];
            if (p) free(p);
        }];
        [g_pso_map removeAllObjects];
    }
    os_unfair_lock_unlock(&g_pso_map_lock);
}
