/*
 * SlopOS Boot Constants
 * All magic values, addresses, and bit patterns used throughout the boot process
 * NO MAGIC NUMBERS - Everything must be defined here with clear explanations
 */

#ifndef BOOT_CONSTANTS_H
#define BOOT_CONSTANTS_H

/* Framebuffer constants */
#define FRAMEBUFFER_PREFERRED_WIDTH   1024     /* Preferred width in pixels */
#define FRAMEBUFFER_PREFERRED_HEIGHT  768      /* Preferred height in pixels */
#define FRAMEBUFFER_PREFERRED_DEPTH   32       /* Preferred bits per pixel */

/* ========================================================================
 * MEMORY LAYOUT CONSTANTS
 * ======================================================================== */

/* Boot stack and early memory locations */
#define BOOT_STACK_SIZE               0x4000   /* 16KB boot stack */
#define BOOT_STACK_PHYS_ADDR          0x20000  /* 128KB physical address */

/* Page table locations in low memory for early boot */
#define EARLY_PML4_PHYS_ADDR          0x30000  /* 192KB - Page Map Level 4 */
#define EARLY_PDPT_PHYS_ADDR          0x31000  /* 196KB - Page Directory Pointer Table */
#define EARLY_PD_PHYS_ADDR            0x32000  /* 200KB - Page Directory */

/* Higher-half kernel mapping */
#define KERNEL_VIRTUAL_BASE           0xFFFFFFFF80000000ULL  /* Higher-half base */
#define KERNEL_PML4_INDEX             511      /* PML4[511] for higher-half */
#define KERNEL_PDPT_INDEX             510      /* PDPT[510] for 0x80000000 part */

/* Page sizes */
#define PAGE_SIZE_4KB                 0x1000   /* 4KB page */
#define PAGE_SIZE_2MB                 0x200000 /* 2MB page */
#define PAGE_SIZE_1GB                 0x40000000 /* 1GB page */

/* Exception handling stack configuration */
#define EXCEPTION_STACK_REGION_BASE   0xFFFFFFFFB0000000ULL  /* Reserved region for IST stacks */
#define EXCEPTION_STACK_REGION_STRIDE 0x00010000ULL          /* 64KB spacing per stack */
#define EXCEPTION_STACK_GUARD_SIZE    PAGE_SIZE_4KB          /* Single guard page */
#define EXCEPTION_STACK_PAGES         8                      /* Data pages per stack (32KB) */
#define EXCEPTION_STACK_SIZE          (EXCEPTION_STACK_PAGES * PAGE_SIZE_4KB)
#define EXCEPTION_STACK_TOTAL_SIZE    (EXCEPTION_STACK_GUARD_SIZE + EXCEPTION_STACK_SIZE)

/* Memory alignment */
#define MULTIBOOT_HEADER_ALIGN        8        /* Multiboot2 header alignment */
#define PAGE_ALIGN                    0x1000   /* Page alignment boundary */
#define STACK_ALIGN                   16       /* Stack alignment boundary */

/* ========================================================================
 * CPU STATE AND CONTROL REGISTER CONSTANTS
 * ======================================================================== */

/* EFLAGS bits */
#define EFLAGS_ID_BIT                 0x00200000  /* ID bit for CPUID detection */

/* CR0 bits */
#define CR0_PG_BIT                    0x80000000  /* Paging enable (bit 31) */
#define CR0_PE_BIT                    0x00000001  /* Protection enable (bit 0) */

/* CR4 bits */
#define CR4_PAE_BIT                   0x00000020  /* Physical Address Extension (bit 5) */

/* EFER MSR */
#define EFER_MSR                      0xC0000080  /* Extended Feature Enable Register */
#define EFER_LME_BIT                  0x00000100  /* Long Mode Enable (bit 8) */

/* CPUID function numbers */
#define CPUID_EXTENDED_FEATURES       0x80000000  /* Highest extended function */
#define CPUID_EXTENDED_FEATURE_INFO   0x80000001  /* Extended feature information */
#define CPUID_LONG_MODE_BIT           0x20000000  /* Long mode bit in EDX (bit 29) */

/* ========================================================================
 * GDT (GLOBAL DESCRIPTOR TABLE) CONSTANTS
 * ======================================================================== */

/* GDT segment selectors */
#define GDT_NULL_SELECTOR             0x00     /* Null selector (required first entry) */
#define GDT_CODE_SELECTOR             0x08     /* Code segment selector */
#define GDT_DATA_SELECTOR             0x10     /* Data segment selector */
#define GDT_TSS_SELECTOR              0x18     /* Task State Segment selector */

/* GDT segment descriptor values for 64-bit mode */
#define GDT_NULL_DESCRIPTOR           0x0000000000000000ULL
#define GDT_CODE_DESCRIPTOR_64        0x00AF9A000000FFFFULL  /* 64-bit code segment */
#define GDT_DATA_DESCRIPTOR_64        0x00AF92000000FFFFULL  /* 64-bit data segment */

/* GDT descriptor bit breakdown explanation:
 * Code segment (0x00AF9A000000FFFF):
 *   - Base: 0x00000000 (ignored in 64-bit mode)
 *   - Limit: 0xFFFFF (ignored in 64-bit mode)
 *   - Access: 0x9A = 10011010b
 *     - Present: 1, DPL: 00, Type: 1, Executable: 1, Direction: 0, Readable: 1, Accessed: 0
 *   - Flags: 0xA = 1010b
 *     - Granularity: 1, Size: 0 (must be 0 for 64-bit), Long: 1, Available: 0
 *
 * Data segment (0x00AF92000000FFFF):
 *   - Base: 0x00000000 (ignored in 64-bit mode)
 *   - Limit: 0xFFFFF (ignored in 64-bit mode)
 *   - Access: 0x92 = 10010010b
 *     - Present: 1, DPL: 00, Type: 1, Executable: 0, Direction: 0, Writable: 1, Accessed: 0
 *   - Flags: 0xA = 1010b (same as code segment)
 */

/* ========================================================================
 * PAGE TABLE ENTRY FLAGS
 * ======================================================================== */

/* Page table entry flags for all levels (PML4, PDPT, PD, PT) */
#define PAGE_PRESENT                  0x001    /* Page is present in memory */
#define PAGE_WRITABLE                 0x002    /* Page is writable */
#define PAGE_USER                     0x004    /* Page accessible from user mode */
#define PAGE_WRITE_THROUGH            0x008    /* Write-through caching */
#define PAGE_CACHE_DISABLE            0x010    /* Disable caching for this page */
#define PAGE_ACCESSED                 0x020    /* Page has been accessed */
#define PAGE_DIRTY                    0x040    /* Page has been written to */
#define PAGE_SIZE                     0x080    /* Large page (2MB/1GB) */
#define PAGE_GLOBAL                   0x100    /* Global page (not flushed on CR3 reload) */

/* Combined flags for common page types */
#define PAGE_KERNEL_RW                (PAGE_PRESENT | PAGE_WRITABLE)  /* Kernel read-write */
#define PAGE_KERNEL_RO                (PAGE_PRESENT)                  /* Kernel read-only */
#define PAGE_USER_RW                  (PAGE_PRESENT | PAGE_WRITABLE | PAGE_USER)  /* User read-write */
#define PAGE_USER_RO                  (PAGE_PRESENT | PAGE_USER)      /* User read-only */
#define PAGE_LARGE_KERNEL_RW          (PAGE_PRESENT | PAGE_WRITABLE | PAGE_SIZE)  /* 2MB kernel page */

/* ========================================================================
 * SERIAL PORT CONSTANTS
 * ======================================================================== */

/* COM port base addresses */
#define COM1_BASE                     0x3F8    /* COM1 base I/O address */
#define COM2_BASE                     0x2F8    /* COM2 base I/O address */
#define COM3_BASE                     0x3E8    /* COM3 base I/O address */
#define COM4_BASE                     0x2E8    /* COM4 base I/O address */

/* COM port register offsets */
#define SERIAL_DATA_REG               0        /* Data register (read/write) */
#define SERIAL_INT_ENABLE_REG         1        /* Interrupt enable register */
#define SERIAL_FIFO_CTRL_REG          2        /* FIFO control register */
#define SERIAL_INT_IDENT_REG          2        /* Interrupt identification register */
#define SERIAL_LINE_CTRL_REG          3        /* Line control register */
#define SERIAL_MODEM_CTRL_REG         4        /* Modem control register */
#define SERIAL_LINE_STATUS_REG        5        /* Line status register */
#define SERIAL_MODEM_STATUS_REG       6        /* Modem status register */
#define SERIAL_SCRATCH_REG            7        /* Scratch register */

/* Serial line status register bits */
#define SERIAL_LSR_DATA_READY         0x01     /* Data ready */
#define SERIAL_LSR_OVERRUN_ERROR      0x02     /* Overrun error */
#define SERIAL_LSR_PARITY_ERROR       0x04     /* Parity error */
#define SERIAL_LSR_FRAMING_ERROR      0x08     /* Framing error */
#define SERIAL_LSR_BREAK_INTERRUPT    0x10     /* Break interrupt */
#define SERIAL_LSR_THR_EMPTY          0x20     /* Transmitter holding register empty */
#define SERIAL_LSR_TRANSMITTER_EMPTY  0x40     /* Transmitter empty */
#define SERIAL_LSR_IMPENDING_ERROR    0x80     /* Impending error */

/* Serial interrupt enable register bits */
#define SERIAL_IER_RECEIVED_DATA      0x01     /* Received data available interrupt */
#define SERIAL_IER_THR_EMPTY          0x02     /* Transmitter holding register empty interrupt */
#define SERIAL_IER_LINE_STATUS        0x04     /* Receiver line status interrupt */

/* Serial interrupt identification register bits */
#define SERIAL_IIR_NO_PENDING         0x01     /* When set, no interrupts pending */
#define SERIAL_IIR_REASON_MASK        0x0E     /* Bits indicating interrupt reason */
#define SERIAL_IIR_REASON_TX_EMPTY    0x02     /* THR empty */
#define SERIAL_IIR_REASON_RX_AVAIL    0x04     /* Received data available */
#define SERIAL_IIR_REASON_LINE_STATUS 0x06     /* Line status change */
#define SERIAL_IIR_REASON_TIMEOUT     0x0C     /* RX timeout */

/* Serial line control register values */
#define SERIAL_LCR_8N1                0x03     /* 8 data bits, no parity, 1 stop bit */
#define SERIAL_LCR_DLAB               0x80     /* Divisor latch access bit */

/* Serial baud rate divisors (for 115200 bps) */
#define SERIAL_BAUD_115200_LOW        0x01     /* Low byte of divisor for 115200 bps */
#define SERIAL_BAUD_115200_HIGH       0x00     /* High byte of divisor for 115200 bps */

/* ========================================================================
 * ERROR CODES
 * ======================================================================== */

/* Boot error codes */
#define BOOT_ERROR_NO_CPUID           0x31     /* CPUID instruction not supported */
#define BOOT_ERROR_NO_LONG_MODE       0x32     /* Long mode not supported */
#define BOOT_ERROR_INVALID_MULTIBOOT2 0x33     /* Invalid Multiboot2 structure */
#define BOOT_ERROR_NO_MEMORY_MAP      0x34     /* No memory map available */
#define BOOT_ERROR_NO_EFI_MMAP        0x35     /* No EFI memory map available */
#define BOOT_ERROR_PAGING_FAILED      0x36     /* Paging setup failed */
#define BOOT_ERROR_GDT_FAILED         0x37     /* GDT setup failed */

/* ========================================================================
 * SYSTEM LIMITS AND SIZES
 * ======================================================================== */

/* Memory map limits */
#define MAX_MEMORY_REGIONS            64       /* Maximum memory regions to track */
#define MAX_EFI_DESCRIPTORS          128      /* Maximum EFI memory descriptors */

/* String and buffer sizes */
#define PANIC_MESSAGE_MAX_LEN         256      /* Maximum panic message length */
#define BOOT_CMDLINE_MAX_LEN          512      /* Maximum command line length */

/* Page table sizes */
#define ENTRIES_PER_PAGE_TABLE        512      /* Entries per page table (512 * 8 = 4KB) */

/* Process management limits */
#define MAX_PROCESSES                 256      /* Maximum number of processes */
#define INVALID_PROCESS_ID            0xFFFFFFFF /* Invalid process ID value */

/* EFI constants */
#define EFI_PAGE_SIZE                 0x1000   /* EFI page size (4KB) */
#define EFI_CONVENTIONAL_MEMORY       7        /* EFI conventional memory type */

/* ========================================================================
 * ASSEMBLY CONSTANTS FOR USE IN .s FILES
 * ======================================================================== */

#ifdef __ASSEMBLER__

/* Assembly-friendly versions of important constants */
.set MULTIBOOT2_MAGIC, MULTIBOOT2_HEADER_MAGIC
.set BOOT_STACK_PHYS, BOOT_STACK_PHYS_ADDR
.set EARLY_PML4_PHYS, EARLY_PML4_PHYS_ADDR
.set EARLY_PDPT_PHYS, EARLY_PDPT_PHYS_ADDR
.set EARLY_PD_PHYS, EARLY_PD_PHYS_ADDR
.set CODE_SEL, GDT_CODE_SELECTOR
.set DATA_SEL, GDT_DATA_SELECTOR

#endif /* __ASSEMBLER__ */

#endif /* BOOT_CONSTANTS_H */
