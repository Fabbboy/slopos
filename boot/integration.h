/*
 * SlopOS Kernel Integration Header
 * Function prototypes and integration points for all subsystems
 * Used by kernel-architect for coordinating component integration
 */

#ifndef BOOT_INTEGRATION_H
#define BOOT_INTEGRATION_H

#include <stdint.h>
#include <stddef.h>

/* ========================================================================
 * MEMORY MANAGEMENT INTEGRATION (COMPLETE)
 * ======================================================================== */

/* Memory management initialization functions */
extern void init_paging(void);
extern void init_kernel_memory_layout(void);
extern void parse_multiboot2_info(uint64_t multiboot_info_addr);

/* Memory allocation functions available for other subsystems */
extern void *kmalloc(size_t size);
extern void kfree(void *ptr);
extern uint64_t alloc_physical_page(void);
extern void free_physical_page(uint64_t phys_addr);

/* ========================================================================
 * VIDEO SUBSYSTEM INTEGRATION (PENDING - video-pipeline-architect)
 * ======================================================================== */

/* Video initialization function to be implemented */
extern void init_video_subsystem(void);

/* Framebuffer functions to be implemented */
extern int setup_framebuffer(void);
extern void *get_framebuffer_address(void);
extern uint32_t get_framebuffer_width(void);
extern uint32_t get_framebuffer_height(void);
extern uint32_t get_framebuffer_pitch(void);

/* Graphics primitives to be implemented */
extern void draw_pixel(uint32_t x, uint32_t y, uint32_t color);
extern void draw_rectangle(uint32_t x, uint32_t y, uint32_t width, uint32_t height, uint32_t color);
extern void clear_screen(uint32_t color);
extern void draw_string(uint32_t x, uint32_t y, const char *str, uint32_t color);

/* ========================================================================
 * SCHEDULER SUBSYSTEM INTEGRATION (PENDING - kernel-scheduler-manager)
 * ======================================================================== */

/* Scheduler initialization function to be implemented */
extern void init_scheduler_subsystem(void);

/* Task management functions to be implemented */
extern uint32_t create_task(void (*entry_point)(void), void *stack, size_t stack_size);
extern void yield_task(void);
extern void schedule_next_task(void);
extern void terminate_current_task(void);

/* Task information functions to be implemented */
extern uint32_t get_current_task_id(void);
extern uint32_t get_task_count(void);

/* ========================================================================
 * INTERRUPT AND EXCEPTION HANDLING INTEGRATION (COMPLETE)
 * ======================================================================== */

/* IDT initialization and management functions */
extern void init_idt(void);
extern void load_idt(void);
extern int is_idt_initialized(void);
extern void dump_idt(void);
extern int verify_idt_integrity(void);

/* Exception handling functions */
extern void verify_memory_mapping(uint64_t virtual_addr);
extern void analyze_page_fault(uint64_t fault_addr, uint64_t error_code);
extern void dump_stack_trace(uint64_t rbp, uint64_t rip);

/* ========================================================================
 * INTEGRATION HELPER MACROS AND CONSTANTS
 * ======================================================================== */

/* Integration status flags */
#define SUBSYSTEM_MEMORY_READY    0x01
#define SUBSYSTEM_VIDEO_READY     0x02
#define SUBSYSTEM_SCHEDULER_READY 0x04
#define SUBSYSTEM_IDT_READY       0x08
#define SUBSYSTEM_ALL_READY       0x0F

/* Color definitions for video subsystem */
#define COLOR_BLACK     0x00000000
#define COLOR_WHITE     0x00FFFFFF
#define COLOR_RED       0x00FF0000
#define COLOR_GREEN     0x0000FF00
#define COLOR_BLUE      0x000000FF
#define COLOR_YELLOW    0x00FFFF00
#define COLOR_CYAN      0x0000FFFF
#define COLOR_MAGENTA   0x00FF00FF

/* Task stack sizes */
#define TASK_STACK_SIZE_SMALL     4096     /* 4KB for simple tasks */
#define TASK_STACK_SIZE_NORMAL    8192     /* 8KB for normal tasks */
#define TASK_STACK_SIZE_LARGE     16384    /* 16KB for complex tasks */

/* ========================================================================
 * INTEGRATION COORDINATION FUNCTIONS
 * ======================================================================== */

/* Subsystem status checking */
extern int is_memory_subsystem_ready(void);
extern int is_video_subsystem_ready(void);
extern int is_scheduler_subsystem_ready(void);
extern int are_all_subsystems_ready(void);

/* Integration testing functions */
extern void test_memory_integration(void);
extern void test_video_integration(void);
extern void test_scheduler_integration(void);
extern void test_full_system_integration(void);

/* ========================================================================
 * RANDOMNESS AND KERNEL ROULETTE INTEGRATION
 * ======================================================================== */

/* Randomness driver functions */
extern void random_init(void);
extern uint32_t random_next(void);
extern uint32_t random_range(uint32_t max);
extern uint32_t random_range_inclusive(uint32_t min, uint32_t max);

/* Kernel roulette: The wheel of fate */
extern void kernel_roulette(void);

#endif /* BOOT_INTEGRATION_H */