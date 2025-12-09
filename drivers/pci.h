#ifndef DRIVERS_PCI_H
#define DRIVERS_PCI_H

#include <stdint.h>
#include <stddef.h>

#define PCI_COMMAND_OFFSET 0x04

#define PCI_MAX_BARS 6

struct pci_driver;

typedef struct {
    uint64_t base;
    uint64_t size;
    uint8_t is_io;
    uint8_t is_64bit;
    uint8_t prefetchable;
} pci_bar_info_t;

typedef struct {
    uint8_t bus;
    uint8_t device;
    uint8_t function;
    uint16_t vendor_id;
    uint16_t device_id;
    uint8_t class_code;
    uint8_t subclass;
    uint8_t prog_if;
    uint8_t revision;
    uint8_t header_type;
    uint8_t irq_line;
    uint8_t irq_pin;
    uint8_t bar_count;
    pci_bar_info_t bars[PCI_MAX_BARS];
} pci_device_info_t;

typedef struct {
    int present;
    pci_device_info_t device;
    uint64_t mmio_phys_base;
    void *mmio_virt_base;
    uint64_t mmio_size;
} pci_gpu_info_t;

int pci_init(void);
size_t pci_get_device_count(void);
const pci_device_info_t *pci_get_devices(void);
const pci_gpu_info_t *pci_get_primary_gpu(void);

uint32_t pci_config_read32(uint8_t bus, uint8_t device, uint8_t function, uint8_t offset);
uint16_t pci_config_read16(uint8_t bus, uint8_t device, uint8_t function, uint8_t offset);
uint8_t pci_config_read8(uint8_t bus, uint8_t device, uint8_t function, uint8_t offset);
void pci_config_write32(uint8_t bus, uint8_t device, uint8_t function, uint8_t offset, uint32_t value);
void pci_config_write16(uint8_t bus, uint8_t device, uint8_t function, uint8_t offset, uint16_t value);
void pci_config_write8(uint8_t bus, uint8_t device, uint8_t function, uint8_t offset, uint8_t value);
size_t pci_get_registered_driver_count(void);
const struct pci_driver *pci_get_registered_driver(size_t index);

#endif /* DRIVERS_PCI_H */
