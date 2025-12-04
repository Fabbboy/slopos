/*
 * SlopOS CPU State Verification
 * Validates CPU state and system configuration during early boot
 */

#include <stdint.h>
#include <stddef.h>

#include "kernel_panic.h"

/*
 * Read CPU control register CR0
 */
static uint64_t read_cr0(void) {
    uint64_t cr0;
    __asm__ volatile ("movq %%cr0, %0" : "=r" (cr0));
    return cr0;
}

/*
 * Read CPU control register CR4
 */
static uint64_t read_cr4(void) {
    uint64_t cr4;
    __asm__ volatile ("movq %%cr4, %0" : "=r" (cr4));
    return cr4;
}

/*
 * Read Extended Feature Enable Register (EFER)
 */
static uint64_t read_efer(void) {
    uint32_t eax, edx;
    __asm__ volatile (
        "movl $0xC0000080, %%ecx\n\t"  // EFER MSR
        "rdmsr"
        : "=a" (eax), "=d" (edx)
        :
        : "ecx"
    );
    return ((uint64_t)edx << 32) | eax;
}

/*
 * Get current stack pointer
 */
static uint64_t get_stack_pointer(void) {
    uint64_t rsp;
    __asm__ volatile ("movq %%rsp, %0" : "=r" (rsp));
    return rsp;
}

/*
 * Verify that the CPU is in the expected state for 64-bit operation
 */
void verify_cpu_state(void) {
    uint64_t cr0 = read_cr0();
    uint64_t cr4 = read_cr4();
    uint64_t efer = read_efer();

    // Check CR0 register
    // Bit 31 (PG) - Paging must be enabled
    if (!(cr0 & (1ULL << 31))) {
        kernel_panic("Paging not enabled in CR0");
    }

    // Bit 0 (PE) - Protected mode must be enabled
    if (!(cr0 & (1ULL << 0))) {
        kernel_panic("Protected mode not enabled in CR0");
    }

    // Check CR4 register
    // Bit 5 (PAE) - Physical Address Extension must be enabled
    if (!(cr4 & (1ULL << 5))) {
        kernel_panic("PAE not enabled in CR4");
    }

    // Check EFER register
    // Bit 8 (LME) - Long Mode Enable must be set
    if (!(efer & (1ULL << 8))) {
        kernel_panic("Long mode not enabled in EFER");
    }

    // Bit 10 (LMA) - Long Mode Active should be set
    if (!(efer & (1ULL << 10))) {
        kernel_panic("Long mode not active in EFER");
    }
}

/*
 * Verify memory layout and addressing is correct
 */
void verify_memory_layout(void) {
    // Get address of this function
    void *current_addr = (void*)verify_memory_layout;
    uint64_t addr = (uint64_t)current_addr;

    // Verify we're running in higher-half kernel space
    if (addr < 0xFFFFFFFF80000000ULL) {
        kernel_panic("Kernel not running in higher-half virtual memory");
    }

    // Verify we're not in user space
    if (addr < 0xFFFF800000000000ULL) {
        kernel_panic("Kernel running in user space address range");
    }

    // Additional validation: check that we can access kernel symbols
    // This is a basic test that our memory mapping is working
    extern char _start;  // Symbol from linker script
    volatile char test_read = _start;
    (void)test_read;  // Suppress unused variable warning
}

/*
 * Verify stack health and configuration
 */
void check_stack_health(void) {
    uint64_t rsp = get_stack_pointer();

    // Basic stack pointer validation
    if (rsp == 0) {
        kernel_panic("Stack pointer is null");
    }

    // Check stack alignment (should be 16-byte aligned for x86_64)
    if (rsp & 0xF) {
        kernel_panic("Stack pointer not properly aligned");
    }

    // Verify stack is in a reasonable memory range
    // Should be either in identity-mapped region or higher-half
    if (rsp < 0x1000) {
        kernel_panic("Stack pointer too low (possible corruption)");
    }

    // If in higher-half, should be in kernel space
    if (rsp >= 0xFFFFFFFF80000000ULL) {
        // Stack is in higher-half kernel space - this is good
        return;
    }

    // If not in higher-half, should be in identity-mapped region (below 1GB)
    if (rsp >= 0x40000000ULL) {
        kernel_panic("Stack pointer in invalid memory region");
    }

    // Stack appears to be in valid identity-mapped region
}

/*
 * Perform additional CPU feature checks
 */
void verify_cpu_features(void) {
    uint32_t eax, ebx, ecx, edx;

    // Check for basic required features
    // CPUID function 1: Feature Information
    __asm__ volatile (
        "movl $1, %%eax\n\t"
        "cpuid"
        : "=a" (eax), "=b" (ebx), "=c" (ecx), "=d" (edx)
    );

    // Check for required features in EDX
    // Bit 6: PAE (Physical Address Extension)
    if (!(edx & (1U << 6))) {
        kernel_panic("CPU does not support PAE");
    }

    // Bit 13: PGE (Page Global Enable)
    if (!(edx & (1U << 13))) {
        kernel_panic("CPU does not support PGE");
    }

    // Check extended features (CPUID function 0x80000001)
    __asm__ volatile (
        "movl $0x80000001, %%eax\n\t"
        "cpuid"
        : "=a" (eax), "=b" (ebx), "=c" (ecx), "=d" (edx)
    );

    // Verify long mode support (should already be enabled, but double-check)
    if (!(edx & (1U << 29))) {
        kernel_panic("CPU does not support long mode");
    }

    // Check for Execute Disable bit support
    if (!(edx & (1U << 20))) {
        // NX bit not supported - not fatal, but note it
        // Could implement warning system here
    }
}

/*
 * Complete CPU and system state verification
 */
void complete_system_verification(void) {
    verify_cpu_state();
    verify_memory_layout();
    check_stack_health();
    verify_cpu_features();
}