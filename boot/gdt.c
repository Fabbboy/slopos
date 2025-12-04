/*
 * SlopOS Global Descriptor Table (GDT) and Task State Segment (TSS)
 * Sets up segmentation for long mode and exposes IST configuration helpers
 */

#include "gdt.h"
#include "constants.h"
#include "log.h"
#include "../drivers/serial.h"
#include "../lib/memory.h"

#include <stdint.h>
#include <stddef.h>

/* Symbols exported from boot/limine_entry.s */
extern uint8_t kernel_stack_top;

/* 64-bit Task State Segment definition */
struct tss64 {
    uint32_t reserved0;
    uint64_t rsp0;
    uint64_t rsp1;
    uint64_t rsp2;
    uint64_t reserved1;
    uint64_t ist[7];
    uint64_t reserved2;
    uint16_t reserved3;
    uint16_t iomap_base;
} __attribute__((packed));

/* 64-bit GDT TSS descriptor */
struct gdt_tss_entry {
    uint16_t limit_low;
    uint16_t base_low;
    uint8_t base_mid;
    uint8_t access;
    uint8_t granularity;
    uint8_t base_high;
    uint32_t base_upper;
    uint32_t reserved;
} __attribute__((packed));

/* GDT layout: null, code, data descriptors + TSS descriptor */
struct gdt_layout {
    uint64_t entries[3];
    struct gdt_tss_entry tss_entry;
} __attribute__((packed));

/* Descriptor passed to lgdt */
struct gdt_descriptor {
    uint16_t limit;
    uint64_t base;
} __attribute__((packed));

static struct gdt_layout gdt_table;
static struct tss64 kernel_tss;

static void load_gdt(const struct gdt_descriptor *descriptor) {
    __asm__ volatile ("lgdt %0" : : "m" (*descriptor));

    __asm__ volatile (
        "pushq %[code]\n\t"
        "leaq 1f(%%rip), %%rax\n\t"
        "pushq %%rax\n\t"
        "lretq\n"
        "1:\n\t"
        "movw %[data], %%ax\n\t"
        "movw %%ax, %%ds\n\t"
        "movw %%ax, %%es\n\t"
        "movw %%ax, %%ss\n\t"
        "movw %%ax, %%fs\n\t"
        "movw %%ax, %%gs\n\t"
        :
        : [code] "i" (GDT_CODE_SELECTOR),
          [data] "i" (GDT_DATA_SELECTOR)
        : "rax", "memory"
    );
}

static void load_tss(void) {
    uint16_t selector = GDT_TSS_SELECTOR;
    __asm__ volatile ("ltr %0" : : "r" (selector) : "memory");
}

void gdt_init(void) {
    boot_log_debug("GDT: Initializing descriptor tables");

    memset(&gdt_table, 0, sizeof(gdt_table));
    memset(&kernel_tss, 0, sizeof(kernel_tss));

    gdt_table.entries[0] = GDT_NULL_DESCRIPTOR;
    gdt_table.entries[1] = GDT_CODE_DESCRIPTOR_64;
    gdt_table.entries[2] = GDT_DATA_DESCRIPTOR_64;

    uint64_t tss_base = (uint64_t)&kernel_tss;
    uint16_t tss_limit = sizeof(kernel_tss) - 1;

    struct gdt_tss_entry *tss_entry = &gdt_table.tss_entry;
    tss_entry->limit_low = tss_limit & 0xFFFF;
    tss_entry->base_low = tss_base & 0xFFFF;
    tss_entry->base_mid = (tss_base >> 16) & 0xFF;
    tss_entry->access = 0x89; /* Present | type=64-bit available TSS */
    tss_entry->granularity = (uint8_t)((tss_limit >> 16) & 0x0F);
    tss_entry->base_high = (tss_base >> 24) & 0xFF;
    tss_entry->base_upper = (uint32_t)(tss_base >> 32);
    tss_entry->reserved = 0;

    kernel_tss.iomap_base = sizeof(struct tss64);
    kernel_tss.rsp0 = (uint64_t)&kernel_stack_top;

    struct gdt_descriptor descriptor = {
        .limit = (uint16_t)(sizeof(gdt_table) - 1),
        .base = (uint64_t)&gdt_table
    };

    load_gdt(&descriptor);
    load_tss();

    boot_log_debug("GDT: Initialized with TSS loaded");
}

void gdt_set_ist(uint8_t index, uint64_t stack_top) {
    if (index == 0 || index > 7) {
        return;
    }
    kernel_tss.ist[index - 1] = stack_top;
}
