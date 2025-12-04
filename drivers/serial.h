/*
 * SlopOS Serial Port Driver Header
 * Interface for COM port communication and kernel output
 */

#ifndef DRIVERS_SERIAL_H
#define DRIVERS_SERIAL_H

#include <stdint.h>
#include <stddef.h>

/* Include boot constants for serial port definitions */
#include "../boot/constants.h"

/* ========================================================================
 * SERIAL PORT CONFIGURATION
 * ======================================================================== */

/* Serial port configuration structure */
typedef struct {
    uint16_t base_port;     /* Base I/O port address */
    uint32_t baud_rate;     /* Baud rate (bits per second) */
    uint8_t data_bits;      /* Number of data bits (5-8) */
    uint8_t stop_bits;      /* Number of stop bits (1-2) */
    uint8_t parity;         /* Parity setting */
    uint8_t initialized;    /* Initialization status */
} serial_config_t;

/* Parity settings */
#define SERIAL_PARITY_NONE    0   /* No parity */
#define SERIAL_PARITY_ODD     1   /* Odd parity */
#define SERIAL_PARITY_EVEN    2   /* Even parity */

/* Data bits settings */
#define SERIAL_DATA_BITS_5    5   /* 5 data bits */
#define SERIAL_DATA_BITS_6    6   /* 6 data bits */
#define SERIAL_DATA_BITS_7    7   /* 7 data bits */
#define SERIAL_DATA_BITS_8    8   /* 8 data bits */

/* Stop bits settings */
#define SERIAL_STOP_BITS_1    1   /* 1 stop bit */
#define SERIAL_STOP_BITS_2    2   /* 2 stop bits */

/* Standard COM port addresses - use canonical definitions from constants.h */
#define SERIAL_COM1_PORT      COM1_BASE   /* COM1 port */
#define SERIAL_COM2_PORT      COM2_BASE   /* COM2 port */
#define SERIAL_COM3_PORT      COM3_BASE   /* COM3 port */
#define SERIAL_COM4_PORT      COM4_BASE   /* COM4 port */

#define SERIAL_COM1_IRQ       4       /* Legacy PIC line for COM1 */

/* Common baud rates */
#define SERIAL_BAUD_9600      9600
#define SERIAL_BAUD_19200     19200
#define SERIAL_BAUD_38400     38400
#define SERIAL_BAUD_57600     57600
#define SERIAL_BAUD_115200    115200

/* ========================================================================
 * SERIAL PORT INTERFACE FUNCTIONS
 * ======================================================================== */

/*
 * Initialize a serial port with specified configuration
 * Returns 0 on success, non-zero on failure
 */
int serial_init(uint16_t port, uint32_t baud_rate, uint8_t data_bits,
                uint8_t stop_bits, uint8_t parity);

/*
 * Initialize COM1 with default settings (115200 8N1)
 * Returns 0 on success, non-zero on failure
 */
int serial_init_com1(void);

/*
 * Send a single character to the specified serial port
 * Blocks until character is transmitted
 */
void serial_putc(uint16_t port, char c);

/*
 * Send a string to the specified serial port
 * Null-terminated string, blocks until complete
 */
void serial_puts(uint16_t port, const char *str);

/*
 * Send a string to the specified serial port with newline
 */
void serial_puts_line(uint16_t port, const char *str);

/*
 * Send binary data to the specified serial port
 * Sends exactly 'length' bytes
 */
void serial_write(uint16_t port, const void *data, size_t length);

/*
 * Check if data is available to read from the serial port
 * Returns non-zero if data is available, zero otherwise
 */
int serial_data_available(uint16_t port);

/*
 * Check if buffered data (captured via interrupts) is available.
 * Returns non-zero if pending characters exist.
 */
int serial_buffer_pending(uint16_t port);

/*
 * Pop a buffered character if available.
 * Returns 1 on success and stores the byte in out_char, 0 if buffer empty.
 */
int serial_buffer_read(uint16_t port, char *out_char);

/*
 * Drain any pending RX bytes into the buffered queue without requiring an IRQ.
 */
void serial_poll_receive(uint16_t port);

/*
 * Check if the transmitter is ready to send data
 * Returns non-zero if ready, zero if busy
 */
int serial_transmitter_ready(uint16_t port);

/*
 * Flush the transmitter buffer (wait for all data to be sent)
 */
void serial_flush(uint16_t port);

/*
 * Enable hardware receive interrupts for the specified port/IRQ line.
 * Returns 0 on success, non-zero on failure.
 */
int serial_enable_interrupts(uint16_t port, uint8_t irq_line);

/* ========================================================================
 * CONVENIENCE FUNCTIONS FOR DEFAULT COM1 OUTPUT
 * ======================================================================== */

/*
 * Send character to COM1 (default kernel output)
 */
void serial_putc_com1(char c);

/*
 * Send formatted hex value to COM1
 * Outputs "0x" followed by hex digits
 */
void serial_put_hex_com1(uint64_t value);

/*
 * Send formatted decimal value to COM1
 */
void serial_put_decimal_com1(uint64_t value);

/* ========================================================================
 * KERNEL OUTPUT INTERFACE
 * ======================================================================== */

/*
 * Set the default serial port for kernel output
 * All kernel print functions will use this port
 */
void serial_set_kernel_output(uint16_t port);

/*
 * Get the current kernel output port
 */
uint16_t serial_get_kernel_output(void);

/*
 * Kernel print function - outputs to default kernel serial port
 * Simple alternative to printf for kernel debugging
 */
void kprint(const char *str);

/*
 * Kernel print with newline
 */
void kprintln(const char *str);

/*
 * Kernel print hex value
 */
void kprint_hex(uint64_t value);

/*
 * Kernel print decimal value
 */
void kprint_decimal(uint64_t value);

/*
 * Kernel print hex byte
 */
void kprint_hex_byte(uint8_t value);

int serial_get_config(uint16_t port, serial_config_t *config);
uint8_t serial_get_line_status(uint16_t port);
uint8_t serial_get_modem_status(uint16_t port);

/* ========================================================================
 * ERROR HANDLING AND DIAGNOSTICS
 * ======================================================================== */

/* Serial error codes */
#define SERIAL_ERROR_NONE             0   /* No error */
#define SERIAL_ERROR_INVALID_PORT     1   /* Invalid port number */
#define SERIAL_ERROR_INVALID_BAUD     2   /* Invalid baud rate */
#define SERIAL_ERROR_INVALID_CONFIG   3   /* Invalid configuration */
#define SERIAL_ERROR_TIMEOUT          4   /* Operation timeout */
#define SERIAL_ERROR_HARDWARE         5   /* Hardware error */
#define SERIAL_ERROR_NOT_INITIALIZED  6   /* Port not initialized */

/*
 * Get the last error code for a serial port
 */
int serial_get_last_error(uint16_t port);

/*
 * Get error description string
 */
const char *serial_get_error_string(int error_code);

/* ========================================================================
 * EMERGENCY OUTPUT FUNCTIONS
 * ======================================================================== */

/*
 * Emergency output functions for panic situations
 * These functions bypass normal initialization checks
 * and attempt to output directly to hardware
 */

/*
 * Emergency character output to COM1
 * Used during kernel panic when normal systems may be down
 */
void serial_emergency_putc(char c);

/*
 * Emergency string output to COM1
 */
void serial_emergency_puts(const char *str);

/*
 * Emergency hex output to COM1
 */
void serial_emergency_put_hex(uint64_t value);

/* ========================================================================
 * INTERNAL HARDWARE ACCESS FUNCTIONS
 * ======================================================================== */

/*
 * Low-level port I/O functions
 * These are primarily for internal use but may be needed
 * for advanced serial port manipulation
 */

/*
 * Read byte from serial port register
 */
uint8_t serial_read_register(uint16_t port, uint8_t reg_offset);

/*
 * Write byte to serial port register
 */
void serial_write_register(uint16_t port, uint8_t reg_offset, uint8_t value);

/*
 * Calculate baud rate divisor for given baud rate
 * Returns divisor value, 0 if baud rate not supported
 */
uint16_t serial_calculate_divisor(uint32_t baud_rate);

#endif /* DRIVERS_SERIAL_H */
