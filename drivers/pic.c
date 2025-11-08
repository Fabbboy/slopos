/*
 * SlopOS Programmable Interrupt Controller (PIC) Driver
 * Basic PIC initialization and interrupt control for legacy hardware
 */

#include <stdint.h>
#include <stddef.h>
#include "serial.h"
#include "../boot/constants.h"
#include "../boot/log.h"

/* ========================================================================
 * PIC CONSTANTS AND DEFINITIONS
 * ======================================================================== */

/* 8259 PIC I/O ports */
#define PIC1_COMMAND    0x20    /* Master PIC command port */
#define PIC1_DATA       0x21    /* Master PIC data port */
#define PIC2_COMMAND    0xA0    /* Slave PIC command port */
#define PIC2_DATA       0xA1    /* Slave PIC data port */

/* PIC commands */
#define PIC_EOI         0x20    /* End-of-interrupt command */
#define PIC_INIT        0x11    /* Initialize command */
#define PIC_MODE_8086   0x01    /* 8086/88 mode */

/* Default IRQ base vectors */
#define IRQ0_VECTOR     32      /* Timer interrupt */
#define IRQ8_VECTOR     40      /* Real-time clock */

/* PIC interrupt mask values */
#define PIC_DISABLE_ALL 0xFF    /* Disable all interrupts */

/* ========================================================================
 * PIC I/O FUNCTIONS
 * ======================================================================== */

/*
 * Write byte to I/O port
 */
static inline void outb(uint16_t port, uint8_t value) {
    __asm__ volatile ("outb %0, %1" : : "a"(value), "Nd"(port));
}

/*
 * Read byte from I/O port
 */
static inline uint8_t inb(uint16_t port) {
    uint8_t value;
    __asm__ volatile ("inb %1, %0" : "=a"(value) : "Nd"(port));
    return value;
}

/*
 * I/O delay for slow hardware
 */
static inline void io_wait(void) {
    /* Write to unused port 0x80 for delay */
    outb(0x80, 0);
}

/* ========================================================================
 * PIC INITIALIZATION FUNCTIONS
 * ======================================================================== */

/*
 * Initialize the PIC (Programmable Interrupt Controller)
 * Remaps IRQs to avoid conflicts with CPU exceptions
 */
void init_pic(void) {
    boot_log_debug("Initializing Programmable Interrupt Controller (PIC)...");

    /* Save current interrupt masks */
    uint8_t mask1 = inb(PIC1_DATA);
    uint8_t mask2 = inb(PIC2_DATA);

    BOOT_LOG_BLOCK(BOOT_LOG_LEVEL_DEBUG, {
        kprint("Current PIC masks: Master=");
        kprint_hex(mask1);
        kprint(" Slave=");
        kprint_hex(mask2);
        kprintln("");
    });

    /* Start initialization sequence */
    outb(PIC1_COMMAND, PIC_INIT);   /* Initialize master PIC */
    io_wait();
    outb(PIC2_COMMAND, PIC_INIT);   /* Initialize slave PIC */
    io_wait();

    /* Set IRQ base vectors (remap to avoid conflict with exceptions) */
    outb(PIC1_DATA, IRQ0_VECTOR);   /* Master PIC vector offset (IRQ 0-7 -> 32-39) */
    io_wait();
    outb(PIC2_DATA, IRQ8_VECTOR);   /* Slave PIC vector offset (IRQ 8-15 -> 40-47) */
    io_wait();

    /* Configure PIC cascade */
    outb(PIC1_DATA, 4);             /* Tell master PIC that slave is at IRQ2 */
    io_wait();
    outb(PIC2_DATA, 2);             /* Tell slave PIC its cascade identity */
    io_wait();

    /* Set 8086 mode */
    outb(PIC1_DATA, PIC_MODE_8086);
    io_wait();
    outb(PIC2_DATA, PIC_MODE_8086);
    io_wait();

    /* Disable all interrupts initially for safety */
    outb(PIC1_DATA, PIC_DISABLE_ALL);
    io_wait();
    outb(PIC2_DATA, PIC_DISABLE_ALL);
    io_wait();

    boot_log_debug("PIC initialization complete");
    BOOT_LOG_BLOCK(BOOT_LOG_LEVEL_DEBUG, {
        kprint("IRQ remapping: IRQ 0-7 -> vectors ");
        kprint_hex(IRQ0_VECTOR);
        kprint("-");
        kprint_hex(IRQ0_VECTOR + 7);
        kprintln("");
        kprint("IRQ remapping: IRQ 8-15 -> vectors ");
        kprint_hex(IRQ8_VECTOR);
        kprint("-");
        kprint_hex(IRQ8_VECTOR + 7);
        kprintln("");
        kprintln("All IRQs disabled for safety");
    });
}

/*
 * Disable PIC entirely (for APIC systems)
 */
void disable_pic(void) {
    boot_log_debug("Disabling legacy PIC...");

    /* Mask all interrupts on both PICs */
    outb(PIC1_DATA, PIC_DISABLE_ALL);
    outb(PIC2_DATA, PIC_DISABLE_ALL);

    boot_log_debug("Legacy PIC disabled");
}

/* ========================================================================
 * PIC INTERRUPT CONTROL
 * ======================================================================== */

/*
 * Send End-of-Interrupt (EOI) signal to PIC
 */
void pic_send_eoi(uint8_t irq) {
    if (irq >= 8) {
        /* IRQ came from slave PIC, send EOI to both */
        outb(PIC2_COMMAND, PIC_EOI);
    }
    /* Always send EOI to master PIC */
    outb(PIC1_COMMAND, PIC_EOI);
}

/*
 * Enable a specific IRQ line
 */
void pic_enable_irq(uint8_t irq) {
    uint16_t port;
    uint8_t value;

    if (irq < 8) {
        /* Master PIC */
        port = PIC1_DATA;
    } else {
        /* Slave PIC */
        port = PIC2_DATA;
        irq -= 8;
    }

    /* Clear the bit for the IRQ to enable it */
    value = inb(port) & ~(1 << irq);
    outb(port, value);

    BOOT_LOG_BLOCK(BOOT_LOG_LEVEL_DEBUG, {
        kprint("Enabled IRQ ");
        kprint_hex(irq);
        kprintln("");
    });
}

/*
 * Disable a specific IRQ line
 */
void pic_disable_irq(uint8_t irq) {
    uint16_t port;
    uint8_t value;

    if (irq < 8) {
        /* Master PIC */
        port = PIC1_DATA;
    } else {
        /* Slave PIC */
        port = PIC2_DATA;
        irq -= 8;
    }

    /* Set the bit for the IRQ to disable it */
    value = inb(port) | (1 << irq);
    outb(port, value);

    kprint("Disabled IRQ ");
    kprint_hex(irq);
    kprintln("");
}

/*
 * Get current IRQ mask for master PIC
 */
uint8_t pic_get_master_mask(void) {
    return inb(PIC1_DATA);
}

/*
 * Get current IRQ mask for slave PIC
 */
uint8_t pic_get_slave_mask(void) {
    return inb(PIC2_DATA);
}

/*
 * Set IRQ mask for master PIC
 */
void pic_set_master_mask(uint8_t mask) {
    outb(PIC1_DATA, mask);
    BOOT_LOG_BLOCK(BOOT_LOG_LEVEL_DEBUG, {
        kprint("Set master PIC mask to ");
        kprint_hex(mask);
        kprintln("");
    });
}

/*
 * Set IRQ mask for slave PIC
 */
void pic_set_slave_mask(uint8_t mask) {
    outb(PIC2_DATA, mask);
    BOOT_LOG_BLOCK(BOOT_LOG_LEVEL_DEBUG, {
        kprint("Set slave PIC mask to ");
        kprint_hex(mask);
        kprintln("");
    });
}

/* ========================================================================
 * PIC STATUS AND DEBUGGING
 * ======================================================================== */

/*
 * Read the In-Service Register (ISR) from PIC
 */
static uint16_t pic_read_isr(void) {
    /* Send command to read ISR */
    outb(PIC1_COMMAND, 0x0B);
    outb(PIC2_COMMAND, 0x0B);

    /* Read ISR values */
    uint8_t isr1 = inb(PIC1_COMMAND);
    uint8_t isr2 = inb(PIC2_COMMAND);

    return (isr2 << 8) | isr1;
}

/*
 * Read the Interrupt Request Register (IRR) from PIC
 */
static uint16_t pic_read_irr(void) {
    /* Send command to read IRR */
    outb(PIC1_COMMAND, 0x0A);
    outb(PIC2_COMMAND, 0x0A);

    /* Read IRR values */
    uint8_t irr1 = inb(PIC1_COMMAND);
    uint8_t irr2 = inb(PIC2_COMMAND);

    return (irr2 << 8) | irr1;
}

/*
 * Display current PIC status
 */
void pic_dump_status(void) {
    kprintln("PIC Status:");

    /* Read current masks */
    uint8_t mask1 = pic_get_master_mask();
    uint8_t mask2 = pic_get_slave_mask();

    kprint("  Master mask: ");
    kprint_hex(mask1);
    kprintln("");
    kprint("  Slave mask:  ");
    kprint_hex(mask2);
    kprintln("");

    /* Read interrupt registers */
    uint16_t isr = pic_read_isr();
    uint16_t irr = pic_read_irr();

    kprint("  ISR (In-Service): ");
    kprint_hex(isr);
    kprintln("");
    kprint("  IRR (Requests):   ");
    kprint_hex(irr);
    kprintln("");

    /* Check for active interrupts */
    if (isr != 0) {
        kprintln("  WARNING: Interrupts still in service!");
    }
    if (irr != 0) {
        kprintln("  WARNING: Pending interrupt requests!");
    }
}

/*
 * Test PIC functionality by checking register access
 */
int pic_self_test(void) {
    kprintln("Running PIC self-test...");

    /* Save current state */
    uint8_t orig_mask1 = pic_get_master_mask();
    uint8_t orig_mask2 = pic_get_slave_mask();

    /* Test mask register access */
    pic_set_master_mask(0xAA);
    if (pic_get_master_mask() != 0xAA) {
        kprintln("ERROR: Master PIC mask test failed");
        pic_set_master_mask(orig_mask1);
        return -1;
    }

    pic_set_slave_mask(0x55);
    if (pic_get_slave_mask() != 0x55) {
        kprintln("ERROR: Slave PIC mask test failed");
        pic_set_master_mask(orig_mask1);
        pic_set_slave_mask(orig_mask2);
        return -1;
    }

    /* Restore original state */
    pic_set_master_mask(orig_mask1);
    pic_set_slave_mask(orig_mask2);

    boot_log_debug("PIC self-test passed");
    return 0;
}

/* ========================================================================
 * PIC UTILITY FUNCTIONS
 * ======================================================================== */

/*
 * Check if an IRQ number is valid
 */
int pic_is_valid_irq(uint8_t irq) {
    return (irq < 16);
}

/*
 * Convert IRQ number to interrupt vector
 */
uint8_t pic_irq_to_vector(uint8_t irq) {
    if (irq < 8) {
        return IRQ0_VECTOR + irq;
    } else if (irq < 16) {
        return IRQ8_VECTOR + (irq - 8);
    }
    return 0;  /* Invalid IRQ */
}

/*
 * Convert interrupt vector to IRQ number
 */
uint8_t pic_vector_to_irq(uint8_t vector) {
    if (vector >= IRQ0_VECTOR && vector < IRQ0_VECTOR + 8) {
        return vector - IRQ0_VECTOR;
    } else if (vector >= IRQ8_VECTOR && vector < IRQ8_VECTOR + 8) {
        return 8 + (vector - IRQ8_VECTOR);
    }
    return 0xFF;  /* Invalid vector */
}

/*
 * Enable all safe IRQs for testing (timer and keyboard)
 */
void pic_enable_safe_irqs(void) {
    boot_log_debug("Enabling safe IRQs for testing...");

    /* Enable timer (IRQ0), keyboard (IRQ1), and COM1 serial (IRQ4) */
    uint8_t master_mask = 0xEC;  /* Bits 0,1,4 cleared -> IRQS enabled */
    uint8_t slave_mask = 0xFF;   /* Keep all slave IRQs disabled */

    pic_set_master_mask(master_mask);
    pic_set_slave_mask(slave_mask);

    boot_log_debug("Safe IRQs enabled: Timer (IRQ 0), Keyboard (IRQ 1), Serial COM1 (IRQ 4)");
}

/*
 * Wrapper function matching header declaration
 */
void pic_init(void) {
    init_pic();
}

/*
 * Wrapper function matching header declaration
 */
void pic_dump_state(void) {
    pic_dump_status();
}
