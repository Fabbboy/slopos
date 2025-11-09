/*
 * SlopOS Serial Port Driver Implementation
 * COM port communication for kernel output and debugging
 */

#include "serial.h"
#include "tty.h"
#include "irq.h"
#include "../sched/scheduler.h"
#include "../boot/log.h"

/* ========================================================================
 * INTERNAL STATE AND CONFIGURATION
 * ======================================================================== */

/* Serial port configurations for each COM port */
static serial_config_t port_configs[4] = {0};

/* Default kernel output port */
static uint16_t kernel_output_port = COM1_BASE;

/* Last error for each port */
static int port_errors[4] = {0};

#define SERIAL_RX_BUFFER_SIZE 256

typedef struct serial_rx_buffer {
    char data[SERIAL_RX_BUFFER_SIZE];
    uint16_t head;
    uint16_t tail;
    uint16_t count;
} serial_rx_buffer_t;

static serial_rx_buffer_t serial_rx_buffers[4];
static uint8_t serial_irq_registered[4] = {0};

/* ========================================================================
 * LOW-LEVEL HARDWARE ACCESS
 * ======================================================================== */

/*
 * Get port index from base address (0-3 for COM1-COM4)
 * Returns -1 for invalid port
 */
static int get_port_index(uint16_t port) {
    switch (port) {
        case COM1_BASE: return 0;
        case COM2_BASE: return 1;
        case COM3_BASE: return 2;
        case COM4_BASE: return 3;
        default: return -1;
    }
}

/*
 * Set error code for a port
 */
static void set_port_error(uint16_t port, int error_code) {
    int index = get_port_index(port);
    if (index >= 0 && index < 4) {
        port_errors[index] = error_code;
    }
}

static serial_rx_buffer_t *serial_get_rx_buffer(uint16_t port) {
    int index = get_port_index(port);
    if (index < 0 || index >= 4) {
        return NULL;
    }
    return &serial_rx_buffers[index];
}

static void serial_rx_buffer_reset(serial_rx_buffer_t *buffer) {
    if (!buffer) {
        return;
    }
    buffer->head = 0;
    buffer->tail = 0;
    buffer->count = 0;
}

static void serial_rx_buffer_push(serial_rx_buffer_t *buffer, char value) {
    if (!buffer) {
        return;
    }

    if (buffer->count >= SERIAL_RX_BUFFER_SIZE) {
        buffer->tail = (buffer->tail + 1) % SERIAL_RX_BUFFER_SIZE;
        buffer->count--;
    }

    buffer->data[buffer->head] = value;
    buffer->head = (buffer->head + 1) % SERIAL_RX_BUFFER_SIZE;
    buffer->count++;
}

static int serial_rx_buffer_pop(serial_rx_buffer_t *buffer, char *out_char) {
    if (!buffer || buffer->count == 0 || !out_char) {
        return 0;
    }

    *out_char = buffer->data[buffer->tail];
    buffer->tail = (buffer->tail + 1) % SERIAL_RX_BUFFER_SIZE;
    buffer->count--;
    return 1;
}

/*
 * Read byte from I/O port
 */
static inline uint8_t inb(uint16_t port) {
    uint8_t value;
    __asm__ volatile ("inb %1, %0" : "=a" (value) : "Nd" (port));
    return value;
}

/*
 * Write byte to I/O port
 */
static inline void outb(uint16_t port, uint8_t value) {
    __asm__ volatile ("outb %0, %1" : : "a" (value), "Nd" (port));
}

uint8_t serial_read_register(uint16_t port, uint8_t reg_offset) {
    return inb(port + reg_offset);
}

void serial_write_register(uint16_t port, uint8_t reg_offset, uint8_t value) {
    outb(port + reg_offset, value);
}

/* ========================================================================
 * BAUD RATE CALCULATION
 * ======================================================================== */

uint16_t serial_calculate_divisor(uint32_t baud_rate) {
    const uint32_t base_frequency = 115200;

    if (baud_rate == 0 || baud_rate > base_frequency) {
        return 0;  /* Invalid baud rate */
    }

    return (uint16_t)(base_frequency / baud_rate);
}

/* ========================================================================
 * SERIAL PORT INITIALIZATION
 * ======================================================================== */

int serial_init(uint16_t port, uint32_t baud_rate, uint8_t data_bits,
                uint8_t stop_bits, uint8_t parity) {
    int port_index = get_port_index(port);
    if (port_index < 0) {
        return SERIAL_ERROR_INVALID_PORT;
    }

    /* Validate parameters */
    if (data_bits < 5 || data_bits > 8) {
        set_port_error(port, SERIAL_ERROR_INVALID_CONFIG);
        return SERIAL_ERROR_INVALID_CONFIG;
    }

    if (stop_bits < 1 || stop_bits > 2) {
        set_port_error(port, SERIAL_ERROR_INVALID_CONFIG);
        return SERIAL_ERROR_INVALID_CONFIG;
    }

    if (parity > SERIAL_PARITY_EVEN) {
        set_port_error(port, SERIAL_ERROR_INVALID_CONFIG);
        return SERIAL_ERROR_INVALID_CONFIG;
    }

    uint16_t divisor = serial_calculate_divisor(baud_rate);
    if (divisor == 0) {
        set_port_error(port, SERIAL_ERROR_INVALID_BAUD);
        return SERIAL_ERROR_INVALID_BAUD;
    }

    /* Disable interrupts */
    outb(port + SERIAL_INT_ENABLE_REG, 0x00);

    /* Set DLAB to access baud rate divisor */
    outb(port + SERIAL_LINE_CTRL_REG, SERIAL_LCR_DLAB);

    /* Set baud rate divisor */
    outb(port + SERIAL_DATA_REG, divisor & 0xFF);         /* Low byte */
    outb(port + SERIAL_INT_ENABLE_REG, (divisor >> 8) & 0xFF);  /* High byte */

    /* Configure line control register */
    uint8_t lcr = 0;

    /* Set data bits */
    lcr |= (data_bits - 5);  /* 5 bits = 0, 6 bits = 1, 7 bits = 2, 8 bits = 3 */

    /* Set stop bits */
    if (stop_bits == 2) {
        lcr |= 0x04;  /* Stop bit (bit 2) */
    }

    /* Set parity */
    if (parity != SERIAL_PARITY_NONE) {
        lcr |= 0x08;  /* Parity enable (bit 3) */
        if (parity == SERIAL_PARITY_EVEN) {
            lcr |= 0x10;  /* Even parity (bit 4) */
        }
    }

    /* Clear DLAB and set line parameters */
    outb(port + SERIAL_LINE_CTRL_REG, lcr);

    /* Enable FIFO and clear it */
    outb(port + SERIAL_FIFO_CTRL_REG, 0xC7);

    /* Enable DTR, RTS, and set OUT2 */
    outb(port + SERIAL_MODEM_CTRL_REG, 0x0B);

    /* Test serial chip by enabling loopback mode */
    outb(port + SERIAL_MODEM_CTRL_REG, 0x1E);

    /* Send test byte */
    outb(port + SERIAL_DATA_REG, 0xAE);

    /* Check if we receive the same byte */
    if (inb(port + SERIAL_DATA_REG) != 0xAE) {
        set_port_error(port, SERIAL_ERROR_HARDWARE);
        return SERIAL_ERROR_HARDWARE;
    }

    /* Disable loopback mode and enable normal operation */
    outb(port + SERIAL_MODEM_CTRL_REG, 0x0F);

    /* Store configuration */
    port_configs[port_index].base_port = port;
    port_configs[port_index].baud_rate = baud_rate;
    port_configs[port_index].data_bits = data_bits;
    port_configs[port_index].stop_bits = stop_bits;
    port_configs[port_index].parity = parity;
    port_configs[port_index].initialized = 1;

    serial_rx_buffer_reset(&serial_rx_buffers[port_index]);

    set_port_error(port, SERIAL_ERROR_NONE);
    return SERIAL_ERROR_NONE;
}

int serial_init_com1(void) {
    return serial_init(COM1_BASE, SERIAL_BAUD_115200, SERIAL_DATA_BITS_8,
                      SERIAL_STOP_BITS_1, SERIAL_PARITY_NONE);
}

/* ========================================================================
 * SERIAL PORT STATUS CHECKING
 * ======================================================================== */

int serial_transmitter_ready(uint16_t port) {
    uint8_t status = inb(port + SERIAL_LINE_STATUS_REG);
    return (status & SERIAL_LSR_THR_EMPTY) != 0;
}

int serial_data_available(uint16_t port) {
    uint8_t status = inb(port + SERIAL_LINE_STATUS_REG);
    return (status & SERIAL_LSR_DATA_READY) != 0;
}

uint8_t serial_get_line_status(uint16_t port) {
    return inb(port + SERIAL_LINE_STATUS_REG);
}

uint8_t serial_get_modem_status(uint16_t port) {
    return inb(port + SERIAL_MODEM_STATUS_REG);
}

int serial_buffer_pending(uint16_t port) {
    serial_rx_buffer_t *buffer = serial_get_rx_buffer(port);
    if (!buffer) {
        return 0;
    }
    return buffer->count > 0;
}

int serial_buffer_read(uint16_t port, char *out_char) {
    serial_rx_buffer_t *buffer = serial_get_rx_buffer(port);
    if (!buffer || !out_char) {
        return 0;
    }

    return serial_rx_buffer_pop(buffer, out_char);
}

/* ========================================================================
 * SERIAL INTERRUPT HANDLING
 * ======================================================================== */

static void serial_drain_receive_fifo(uint16_t port, int from_interrupt) {
    serial_rx_buffer_t *buffer = serial_get_rx_buffer(port);
    if (!buffer) {
        return;
    }

    int received = 0;

    while (inb(port + SERIAL_LINE_STATUS_REG) & SERIAL_LSR_DATA_READY) {
        char c = inb(port + SERIAL_DATA_REG);
        serial_rx_buffer_push(buffer, c);
        received = 1;
    }

    if (received) {
        tty_notify_input_ready();
        if (from_interrupt) {
            scheduler_request_reschedule_from_interrupt();
            return;
        }
    }
}

void serial_poll_receive(uint16_t port) {
    serial_drain_receive_fifo(port, 0);
}

static void serial_process_interrupts(uint16_t port) {
    while (1) {
        uint8_t ident = inb(port + SERIAL_INT_IDENT_REG);
        if (ident & SERIAL_IIR_NO_PENDING) {
            break;
        }

        uint8_t reason = ident & SERIAL_IIR_REASON_MASK;
        switch (reason) {
            case SERIAL_IIR_REASON_RX_AVAIL:
            case SERIAL_IIR_REASON_TIMEOUT:
                serial_drain_receive_fifo(port, 1);
                break;
            case SERIAL_IIR_REASON_LINE_STATUS:
                (void)inb(port + SERIAL_LINE_STATUS_REG);
                break;
            case SERIAL_IIR_REASON_TX_EMPTY:
            default:
                /* Nothing to do for transmit-empty or unknown reasons */
                break;
        }
    }
}

static void serial_irq_handler(uint8_t irq, struct interrupt_frame *frame, void *context) {
    (void)irq;
    (void)frame;

    uint16_t port = (uint16_t)(uintptr_t)context;
    serial_process_interrupts(port);
}

int serial_enable_interrupts(uint16_t port, uint8_t irq_line) {
    int index = get_port_index(port);
    if (index < 0) {
        return SERIAL_ERROR_INVALID_PORT;
    }

    if (!port_configs[index].initialized) {
        return SERIAL_ERROR_NOT_INITIALIZED;
    }

    if (serial_irq_registered[index]) {
        return 0;
    }

    if (irq_register_handler(irq_line, serial_irq_handler,
                             (void *)(uintptr_t)port, "serial") != 0) {
        boot_log_info("Serial: Failed to register IRQ handler");
        return -1;
    }

    serial_irq_registered[index] = 1;

    uint8_t ier = inb(port + SERIAL_INT_ENABLE_REG);
    ier |= (SERIAL_IER_RECEIVED_DATA | SERIAL_IER_LINE_STATUS);
    outb(port + SERIAL_INT_ENABLE_REG, ier);

    /* Clear any pending status */
    (void)inb(port + SERIAL_LINE_STATUS_REG);
    (void)inb(port + SERIAL_DATA_REG);
    (void)inb(port + SERIAL_INT_IDENT_REG);

    /* Drain any bytes that arrived before interrupts were armed */
    serial_poll_receive(port);

    boot_log_info("Serial: COM port interrupts armed");

    return 0;
}

/* ========================================================================
 * SERIAL PORT TRANSMIT FUNCTIONS
 * ======================================================================== */

void serial_putc(uint16_t port, char c) {
    /* Wait for transmitter to be ready */
    while (!serial_transmitter_ready(port)) {
        /* Busy wait */
    }

    /* Send character */
    outb(port + SERIAL_DATA_REG, c);
}

void serial_puts(uint16_t port, const char *str) {
    if (!str) return;

    while (*str) {
        serial_putc(port, *str++);
    }
}

void serial_puts_line(uint16_t port, const char *str) {
    serial_puts(port, str);
    serial_putc(port, '\r');
    serial_putc(port, '\n');
}

void serial_write(uint16_t port, const void *data, size_t length) {
    const uint8_t *bytes = (const uint8_t *)data;

    for (size_t i = 0; i < length; i++) {
        serial_putc(port, bytes[i]);
    }
}

void serial_flush(uint16_t port) {
    /* Wait for both THR empty and transmitter empty */
    while (!(inb(port + SERIAL_LINE_STATUS_REG) & SERIAL_LSR_TRANSMITTER_EMPTY)) {
        /* Busy wait */
    }
}

/* ========================================================================
 * SERIAL PORT RECEIVE FUNCTIONS
 * ======================================================================== */

char serial_getc(uint16_t port) {
    /* Wait for data to be available */
    while (!serial_data_available(port)) {
        /* Busy wait */
    }

    return inb(port + SERIAL_DATA_REG);
}

/* ========================================================================
 * COM1 CONVENIENCE FUNCTIONS
 * ======================================================================== */

void serial_putc_com1(char c) {
    serial_putc(COM1_BASE, c);
}

void serial_puts_com1(const char *str) {
    serial_puts(COM1_BASE, str);
}

void serial_puts_line_com1(const char *str) {
    serial_puts_line(COM1_BASE, str);
}

void serial_put_hex_com1(uint64_t value) {
    const char hex_chars[] = "0123456789ABCDEF";
    char buffer[19];  /* "0x" + 16 hex digits + null terminator */
    int i;

    buffer[0] = '0';
    buffer[1] = 'x';

    /* Convert to hex string (16 digits for 64-bit) */
    for (i = 15; i >= 0; i--) {
        buffer[2 + (15 - i)] = hex_chars[(value >> (i * 4)) & 0xF];
    }

    buffer[18] = '\0';
    serial_puts_com1(buffer);
}

void serial_put_decimal_com1(uint64_t value) {
    char buffer[21];  /* Maximum 20 digits for 64-bit + null terminator */
    int i = 19;

    buffer[20] = '\0';

    if (value == 0) {
        buffer[19] = '0';
        serial_puts_com1(&buffer[19]);
        return;
    }

    while (value > 0 && i >= 0) {
        buffer[i--] = '0' + (value % 10);
        value /= 10;
    }

    serial_puts_com1(&buffer[i + 1]);
}

/* ========================================================================
 * KERNEL OUTPUT INTERFACE
 * ======================================================================== */

void serial_set_kernel_output(uint16_t port) {
    kernel_output_port = port;
}

uint16_t serial_get_kernel_output(void) {
    return kernel_output_port;
}

void kprint(const char *str) {
    serial_puts(kernel_output_port, str);
}

void kprintln(const char *str) {
    serial_puts_line(kernel_output_port, str);
}

void kprint_hex(uint64_t value) {
    if (kernel_output_port == COM1_BASE) {
        serial_put_hex_com1(value);
    } else {
        /* Implement for other ports if needed */
        serial_put_hex_com1(value);  /* Fallback to COM1 */
    }
}

void kprint_decimal(uint64_t value) {
    if (kernel_output_port == COM1_BASE) {
        serial_put_decimal_com1(value);
    } else {
        /* Implement for other ports if needed */
        serial_put_decimal_com1(value);  /* Fallback to COM1 */
    }
}

/* ========================================================================
 * CONFIGURATION AND DIAGNOSTICS
 * ======================================================================== */

int serial_get_config(uint16_t port, serial_config_t *config) {
    int port_index = get_port_index(port);
    if (port_index < 0 || !config) {
        return SERIAL_ERROR_INVALID_PORT;
    }

    if (!port_configs[port_index].initialized) {
        return SERIAL_ERROR_NOT_INITIALIZED;
    }

    *config = port_configs[port_index];
    return SERIAL_ERROR_NONE;
}

int serial_get_last_error(uint16_t port) {
    int port_index = get_port_index(port);
    if (port_index < 0) {
        return SERIAL_ERROR_INVALID_PORT;
    }

    return port_errors[port_index];
}

const char *serial_get_error_string(int error_code) {
    switch (error_code) {
        case SERIAL_ERROR_NONE:             return "No error";
        case SERIAL_ERROR_INVALID_PORT:     return "Invalid port number";
        case SERIAL_ERROR_INVALID_BAUD:     return "Invalid baud rate";
        case SERIAL_ERROR_INVALID_CONFIG:   return "Invalid configuration";
        case SERIAL_ERROR_TIMEOUT:          return "Operation timeout";
        case SERIAL_ERROR_HARDWARE:         return "Hardware error";
        case SERIAL_ERROR_NOT_INITIALIZED:  return "Port not initialized";
        default:                            return "Unknown error";
    }
}

/* ========================================================================
 * EMERGENCY OUTPUT FUNCTIONS
 * ======================================================================== */

void serial_emergency_putc(char c) {
    /* Bypass all checks and output directly to COM1 */
    /* Wait a short time for transmitter ready */
    for (int i = 0; i < 1000; i++) {
        if (inb(COM1_BASE + SERIAL_LINE_STATUS_REG) & SERIAL_LSR_THR_EMPTY) {
            break;
        }
    }

    outb(COM1_BASE + SERIAL_DATA_REG, c);
}

void serial_emergency_puts(const char *str) {
    if (!str) return;

    while (*str) {
        serial_emergency_putc(*str++);
    }
}

void serial_emergency_put_hex(uint64_t value) {
    const char hex_chars[] = "0123456789ABCDEF";

    serial_emergency_putc('0');
    serial_emergency_putc('x');

    for (int i = 15; i >= 0; i--) {
        serial_emergency_putc(hex_chars[(value >> (i * 4)) & 0xF]);
    }
}

/* ========================================================================
 * ADVANCED FUNCTIONS
 * ======================================================================== */

void serial_set_flow_control(uint16_t port, int enable) {
    uint8_t mcr = inb(port + SERIAL_MODEM_CTRL_REG);

    if (enable) {
        mcr |= 0x02;  /* Set RTS */
    } else {
        mcr &= ~0x02; /* Clear RTS */
    }

    outb(port + SERIAL_MODEM_CTRL_REG, mcr);
}

void serial_set_break(uint16_t port, int enable) {
    uint8_t lcr = inb(port + SERIAL_LINE_CTRL_REG);

    if (enable) {
        lcr |= 0x40;  /* Set break enable bit */
    } else {
        lcr &= ~0x40; /* Clear break enable bit */
    }

    outb(port + SERIAL_LINE_CTRL_REG, lcr);
}

int serial_self_test(uint16_t port) {
    /* Perform loopback test */
    uint8_t original_mcr = inb(port + SERIAL_MODEM_CTRL_REG);

    /* Enable loopback mode */
    outb(port + SERIAL_MODEM_CTRL_REG, 0x1E);

    /* Test pattern */
    const uint8_t test_patterns[] = {0x55, 0xAA, 0xFF, 0x00};

    for (int i = 0; i < 4; i++) {
        outb(port + SERIAL_DATA_REG, test_patterns[i]);

        /* Wait for data */
        int timeout = 1000;
        while (!serial_data_available(port) && timeout-- > 0) {
            /* Wait */
        }

        if (timeout <= 0) {
            /* Restore original MCR */
            outb(port + SERIAL_MODEM_CTRL_REG, original_mcr);
            return SERIAL_ERROR_TIMEOUT;
        }

        uint8_t received = inb(port + SERIAL_DATA_REG);
        if (received != test_patterns[i]) {
            /* Restore original MCR */
            outb(port + SERIAL_MODEM_CTRL_REG, original_mcr);
            return SERIAL_ERROR_HARDWARE;
        }
    }

    /* Restore original MCR */
    outb(port + SERIAL_MODEM_CTRL_REG, original_mcr);
    return SERIAL_ERROR_NONE;
}

/*
 * Print hex byte (2 digits)
 */
void kprint_hex_byte(uint8_t value) {
    static const char hex_chars[] = "0123456789ABCDEF";
    serial_putc(SERIAL_COM1_PORT, hex_chars[(value >> 4) & 0xF]);
    serial_putc(SERIAL_COM1_PORT, hex_chars[value & 0xF]);
}
