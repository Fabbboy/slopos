#ifndef MM_MEMORY_INIT_H
#define MM_MEMORY_INIT_H

#include <stdint.h>
#include "../third_party/limine/limine.h"

int init_memory_system(const struct limine_memmap_response *memmap,
                       uint64_t hhdm_offset);
int is_memory_system_initialized(void);
void get_memory_statistics(uint64_t *total_memory_out,
                           uint64_t *available_memory_out,
                           uint32_t *regions_count_out);

#endif /* MM_MEMORY_INIT_H */

