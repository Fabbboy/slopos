#ifndef MM_PROCESS_VM_H
#define MM_PROCESS_VM_H

#include <stdint.h>
#include "paging.h"

/* Process memory allocation flags */
#define VM_FLAG_READ   0x01  /* Page is readable */
#define VM_FLAG_WRITE  0x02  /* Page is writable */
#define VM_FLAG_EXEC   0x04  /* Page is executable */
#define VM_FLAG_USER   0x08  /* Page accessible from user mode */
#define VM_FLAG_SHARED 0x10  /* Page is shared between processes */

int init_process_vm(void);
uint32_t create_process_vm(void);
int destroy_process_vm(uint32_t process_id);
void get_process_vm_stats(uint32_t *total_processes, uint32_t *active_processes);
uint64_t process_vm_alloc(uint32_t process_id, uint64_t size, uint32_t flags);
int process_vm_free(uint32_t process_id, uint64_t vaddr, uint64_t size);
process_page_dir_t *process_vm_get_page_dir(uint32_t process_id);

#endif /* MM_PROCESS_VM_H */

