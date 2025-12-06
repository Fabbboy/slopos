/*
 * SlopOS Serial Hardware Constants
 * Low-level COM port addresses and register definitions.
 */

#ifndef DRIVERS_SERIAL_HW_H
#define DRIVERS_SERIAL_HW_H

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

#endif /* DRIVERS_SERIAL_HW_H */

