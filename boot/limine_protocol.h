#ifndef SLOPOS_LIMINE_PROTOCOL_H
#define SLOPOS_LIMINE_PROTOCOL_H

#include <stdint.h>
#include "../third_party/limine/limine.h"

int init_limine_protocol(void);
int get_framebuffer_info(uint64_t *addr, uint32_t *width, uint32_t *height,
                         uint32_t *pitch, uint8_t *bpp);
int is_framebuffer_available(void);
uint64_t get_total_memory(void);
uint64_t get_available_memory(void);
int is_memory_map_available(void);
uint64_t get_hhdm_offset(void);
int is_hhdm_available(void);
uint64_t get_kernel_phys_base(void);
uint64_t get_kernel_virt_base(void);
const char *get_kernel_cmdline(void);
int is_rsdp_available(void);
uint64_t get_rsdp_phys_address(void);
const void *get_rsdp_address(void);

const struct limine_memmap_response *limine_get_memmap_response(void);
const struct limine_hhdm_response *limine_get_hhdm_response(void);

#endif /* SLOPOS_LIMINE_PROTOCOL_H */
