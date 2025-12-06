#include "pci.h"
#include "serial.h"
#include "../mm/phys_virt.h"
#include "../lib/klog.h"

#define PCI_CONFIG_ADDRESS 0xCF8
#define PCI_CONFIG_DATA    0xCFC

#define PCI_VENDOR_ID_OFFSET        0x00
#define PCI_DEVICE_ID_OFFSET        0x02
#define PCI_COMMAND_OFFSET          0x04
#define PCI_STATUS_OFFSET           0x06
#define PCI_REVISION_ID_OFFSET      0x08
#define PCI_PROG_IF_OFFSET          0x09
#define PCI_SUBCLASS_OFFSET         0x0A
#define PCI_CLASS_CODE_OFFSET       0x0B
#define PCI_HEADER_TYPE_OFFSET      0x0E
#define PCI_INTERRUPT_LINE_OFFSET   0x3C
#define PCI_INTERRUPT_PIN_OFFSET    0x3D
#define PCI_BAR0_OFFSET             0x10

#define PCI_HEADER_TYPE_MASK            0x7F
#define PCI_HEADER_TYPE_MULTI_FUNCTION  0x80
#define PCI_HEADER_TYPE_DEVICE          0x00
#define PCI_HEADER_TYPE_BRIDGE          0x01

#define PCI_BAR_IO_SPACE            0x1
#define PCI_BAR_IO_ADDRESS_MASK     0xFFFFFFFC
#define PCI_BAR_MEM_TYPE_MASK       0x6
#define PCI_BAR_MEM_TYPE_64         0x4
#define PCI_BAR_MEM_PREFETCHABLE    0x8
#define PCI_BAR_MEM_ADDRESS_MASK    0xFFFFFFF0

#define PCI_CLASS_DISPLAY           0x03

#define PCI_MAX_BUSES               256
#define PCI_MAX_DEVICES             256

static uint8_t bus_visited[PCI_MAX_BUSES];
static pci_device_info_t devices[PCI_MAX_DEVICES];
static size_t device_count;
static int pci_initialized;
static pci_gpu_info_t primary_gpu;

static inline void outl(uint16_t port, uint32_t value) {
    __asm__ volatile ("outl %0, %1" : : "a"(value), "Nd"(port));
}

static inline uint32_t inl(uint16_t port) {
    uint32_t value;
    __asm__ volatile ("inl %1, %0" : "=a"(value) : "Nd"(port));
    return value;
}

static uint32_t pci_config_read32(uint8_t bus, uint8_t device,
                                  uint8_t function, uint8_t offset) {
    uint32_t address = (uint32_t)0x80000000 |
                       ((uint32_t)bus << 16) |
                       ((uint32_t)device << 11) |
                       ((uint32_t)function << 8) |
                       (offset & 0xFC);

    outl(PCI_CONFIG_ADDRESS, address);
    return inl(PCI_CONFIG_DATA);
}

static uint16_t pci_config_read16(uint8_t bus, uint8_t device,
                                  uint8_t function, uint8_t offset) {
    uint32_t value = pci_config_read32(bus, device, function, offset);
    uint32_t shift = (offset & 0x2) * 8;
    return (uint16_t)((value >> shift) & 0xFFFF);
}

static uint8_t pci_config_read8(uint8_t bus, uint8_t device,
                                uint8_t function, uint8_t offset) {
    uint32_t value = pci_config_read32(bus, device, function, offset);
    uint32_t shift = (offset & 0x3) * 8;
    return (uint8_t)((value >> shift) & 0xFF);
}

static void pci_config_write32(uint8_t bus, uint8_t device,
                               uint8_t function, uint8_t offset,
                               uint32_t value) {
    uint32_t address = (uint32_t)0x80000000 |
                       ((uint32_t)bus << 16) |
                       ((uint32_t)device << 11) |
                       ((uint32_t)function << 8) |
                       (offset & 0xFC);

    outl(PCI_CONFIG_ADDRESS, address);
    outl(PCI_CONFIG_DATA, value);
}

static uint64_t pci_probe_bar_size(uint8_t bus, uint8_t device,
                                   uint8_t function, uint8_t offset,
                                   uint32_t original_value) {
    if (original_value == 0) {
        return 0;
    }

    if (original_value & PCI_BAR_IO_SPACE) {
        pci_config_write32(bus, device, function, offset, 0xFFFFFFFF);
        uint32_t size_mask = pci_config_read32(bus, device, function, offset);
        pci_config_write32(bus, device, function, offset, original_value);

        uint32_t masked = size_mask & PCI_BAR_IO_ADDRESS_MASK;
        if (masked == 0) {
            return 0;
        }
        uint32_t size = (~masked + 1) & 0xFFFFFFFF;
        return size;
    }

    uint32_t type = original_value & PCI_BAR_MEM_TYPE_MASK;
    int is_64bit = (type == PCI_BAR_MEM_TYPE_64);

    pci_config_write32(bus, device, function, offset, 0xFFFFFFFF);
    uint32_t size_low = pci_config_read32(bus, device, function, offset);
    pci_config_write32(bus, device, function, offset, original_value);

    uint64_t mask = size_low & PCI_BAR_MEM_ADDRESS_MASK;
    uint64_t size_value = (~mask + 1);

    if (is_64bit) {
        uint32_t original_high = pci_config_read32(bus, device, function, offset + 4);
        pci_config_write32(bus, device, function, offset + 4, 0xFFFFFFFF);
        uint32_t size_high = pci_config_read32(bus, device, function, offset + 4);
        pci_config_write32(bus, device, function, offset + 4, original_high);

        mask |= ((uint64_t)size_high << 32);
        size_value = (~mask + 1);
    }

    return size_value;
}

static void pci_log_device_header(const pci_device_info_t *info) {
    klog_raw(KLOG_INFO, "PCI: [Bus ");
    klog_decimal(KLOG_INFO, info->bus);
    klog_raw(KLOG_INFO, " Dev ");
    klog_decimal(KLOG_INFO, info->device);
    klog_raw(KLOG_INFO, " Func ");
    klog_decimal(KLOG_INFO, info->function);
    klog_raw(KLOG_INFO, "] VID=");
    klog_hex(KLOG_INFO, info->vendor_id);
    klog_raw(KLOG_INFO, " DID=");
    klog_hex(KLOG_INFO, info->device_id);
    klog_raw(KLOG_INFO, " Class=");
    klog_hex(KLOG_INFO, info->class_code);
    klog_raw(KLOG_INFO, ":");
    klog_hex(KLOG_INFO, info->subclass);
    klog_raw(KLOG_INFO, " ProgIF=");
    klog_hex(KLOG_INFO, info->prog_if);
    klog_raw(KLOG_INFO, " Rev=");
    klog_hex(KLOG_INFO, info->revision);
    klog(KLOG_INFO, "");
}

static void pci_log_bar(const pci_bar_info_t *bar, uint8_t index) {
    klog_raw(KLOG_INFO, "    BAR");
    klog_decimal(KLOG_INFO, index);
    klog_raw(KLOG_INFO, ": ");
    if (bar->is_io) {
        klog_raw(KLOG_INFO, "IO base=0x");
        klog_hex(KLOG_INFO, bar->base);
        if (bar->size) {
            klog_raw(KLOG_INFO, " size=");
            klog_decimal(KLOG_INFO, bar->size);
        }
    } else {
        klog_raw(KLOG_INFO, "MMIO base=0x");
        klog_hex(KLOG_INFO, bar->base);
        if (bar->size) {
            klog_raw(KLOG_INFO, " size=0x");
            klog_hex(KLOG_INFO, bar->size);
        }
        klog_raw(KLOG_INFO, bar->prefetchable ? " prefetch" : " non-prefetch");
        if (bar->is_64bit) {
            klog_raw(KLOG_INFO, " 64bit");
        }
    }
    klog(KLOG_INFO, "");
}

static void pci_consider_gpu_candidate(const pci_device_info_t *info) {
    if (primary_gpu.present) {
        return;
    }

    if (info->class_code != PCI_CLASS_DISPLAY) {
        return;
    }

    for (uint8_t i = 0; i < info->bar_count; ++i) {
        const pci_bar_info_t *bar = &info->bars[i];
        if (bar->is_io || bar->base == 0) {
            continue;
        }

        primary_gpu.present = 1;
        primary_gpu.device = *info;
        primary_gpu.mmio_phys_base = bar->base;
        primary_gpu.mmio_size = bar->size ? bar->size : 0x1000; /* map first page when size unknown */
        primary_gpu.mmio_virt_base = mm_map_mmio_region(primary_gpu.mmio_phys_base,
                                                        (size_t)primary_gpu.mmio_size);

        klog_raw(KLOG_INFO, "PCI: Selected GPU candidate at MMIO phys=0x");
        klog_hex(KLOG_INFO, primary_gpu.mmio_phys_base);
        klog_raw(KLOG_INFO, " size=0x");
        klog_hex(KLOG_INFO, primary_gpu.mmio_size);
        if (primary_gpu.mmio_virt_base) {
            klog_raw(KLOG_INFO, " virt=0x");
            klog_hex(KLOG_INFO, (uint64_t)(uintptr_t)primary_gpu.mmio_virt_base);
            klog(KLOG_INFO, "");
        } else {
            klog(KLOG_INFO, " (mapping failed)");
        }

        klog(KLOG_INFO, "PCI: GPU acceleration groundwork ready (MMIO mapped)");
        if (!primary_gpu.mmio_virt_base) {
            klog(KLOG_INFO, "PCI: WARNING GPU MMIO not accessible; check paging support");
        }
        return;
    }
}

static void pci_collect_bars(pci_device_info_t *info) {
    info->bar_count = 0;

    uint8_t header_type = info->header_type & PCI_HEADER_TYPE_MASK;
    int max_bars = (header_type == PCI_HEADER_TYPE_DEVICE) ? 6 :
                   (header_type == PCI_HEADER_TYPE_BRIDGE) ? 2 : 0;

    for (int bar_index = 0; bar_index < max_bars && info->bar_count < PCI_MAX_BARS; ++bar_index) {
        uint8_t offset = PCI_BAR0_OFFSET + (uint8_t)(bar_index * 4);
        uint32_t raw = pci_config_read32(info->bus, info->device, info->function, offset);

        if (raw == 0) {
            continue;
        }

        pci_bar_info_t *bar = &info->bars[info->bar_count];
        bar->size = pci_probe_bar_size(info->bus, info->device, info->function, offset, raw);

        if (raw & PCI_BAR_IO_SPACE) {
            bar->is_io = 1;
            bar->is_64bit = 0;
            bar->prefetchable = 0;
            bar->base = raw & PCI_BAR_IO_ADDRESS_MASK;
        } else {
            uint8_t type = (raw & PCI_BAR_MEM_TYPE_MASK) >> 1;
            bar->is_io = 0;
            bar->prefetchable = (raw & PCI_BAR_MEM_PREFETCHABLE) ? 1 : 0;
            bar->is_64bit = (type == 0x2) ? 1 : 0;
            uint64_t base = raw & PCI_BAR_MEM_ADDRESS_MASK;

            if (bar->is_64bit && bar_index + 1 < max_bars) {
                uint32_t upper = pci_config_read32(info->bus, info->device, info->function, offset + 4);
                base |= ((uint64_t)upper << 32);
                bar_index++; /* Skip the high DWORD in iteration */
            }

            bar->base = base;
        }

        pci_log_bar(bar, info->bar_count);
        info->bar_count++;
    }
}

static void pci_scan_function(uint8_t bus, uint8_t device, uint8_t function) {
    uint16_t vendor_id = pci_config_read16(bus, device, function, PCI_VENDOR_ID_OFFSET);
    if (vendor_id == 0xFFFF) {
        return;
    }

    uint16_t device_id = pci_config_read16(bus, device, function, PCI_DEVICE_ID_OFFSET);
    uint8_t class_code = pci_config_read8(bus, device, function, PCI_CLASS_CODE_OFFSET);
    uint8_t subclass = pci_config_read8(bus, device, function, PCI_SUBCLASS_OFFSET);
    uint8_t prog_if = pci_config_read8(bus, device, function, PCI_PROG_IF_OFFSET);
    uint8_t revision = pci_config_read8(bus, device, function, PCI_REVISION_ID_OFFSET);
    uint8_t header_type = pci_config_read8(bus, device, function, PCI_HEADER_TYPE_OFFSET);
    uint8_t irq_line = pci_config_read8(bus, device, function, PCI_INTERRUPT_LINE_OFFSET);
    uint8_t irq_pin = pci_config_read8(bus, device, function, PCI_INTERRUPT_PIN_OFFSET);

    if (device_count >= PCI_MAX_DEVICES) {
        if (device_count == PCI_MAX_DEVICES) {
            klog(KLOG_INFO, "PCI: Device buffer full, additional devices will not be tracked");
        }
        return;
    }

    pci_device_info_t *info = &devices[device_count++];

    info->bus = bus;
    info->device = device;
    info->function = function;
    info->vendor_id = vendor_id;
    info->device_id = device_id;
    info->class_code = class_code;
    info->subclass = subclass;
    info->prog_if = prog_if;
    info->revision = revision;
    info->header_type = header_type;
    info->irq_line = irq_line;
    info->irq_pin = irq_pin;
    info->bar_count = 0;

    pci_log_device_header(info);
    pci_collect_bars(info);
    pci_consider_gpu_candidate(info);

    if ((header_type & PCI_HEADER_TYPE_MASK) == PCI_HEADER_TYPE_BRIDGE) {
        uint8_t secondary_bus = pci_config_read8(bus, device, function, 0x19);
        if (secondary_bus != 0 && !bus_visited[secondary_bus]) {
            klog_raw(KLOG_INFO, "PCI: Traversing to secondary bus ");
            klog_decimal(KLOG_INFO, secondary_bus);
            klog(KLOG_INFO, "");
            bus_visited[secondary_bus] = 1;
            // Recursive scan of downstream bus
            for (uint8_t dev = 0; dev < 32; ++dev) {
                uint16_t sec_vendor = pci_config_read16(secondary_bus, dev, 0, PCI_VENDOR_ID_OFFSET);
                if (sec_vendor == 0xFFFF) {
                    continue;
                }
                pci_scan_function(secondary_bus, dev, 0);
                uint8_t sec_header = pci_config_read8(secondary_bus, dev, 0, PCI_HEADER_TYPE_OFFSET);
                if (sec_header & PCI_HEADER_TYPE_MULTI_FUNCTION) {
                    for (uint8_t func = 1; func < 8; ++func) {
                        sec_vendor = pci_config_read16(secondary_bus, dev, func, PCI_VENDOR_ID_OFFSET);
                        if (sec_vendor == 0xFFFF) {
                            continue;
                        }
                        pci_scan_function(secondary_bus, dev, func);
                    }
                }
            }
        }
    }
}

static void pci_scan_device(uint8_t bus, uint8_t device) {
    uint16_t vendor_id = pci_config_read16(bus, device, 0, PCI_VENDOR_ID_OFFSET);
    if (vendor_id == 0xFFFF) {
        return;
    }

    pci_scan_function(bus, device, 0);

    uint8_t header_type = pci_config_read8(bus, device, 0, PCI_HEADER_TYPE_OFFSET);
    if (header_type & PCI_HEADER_TYPE_MULTI_FUNCTION) {
        for (uint8_t function = 1; function < 8; ++function) {
            if (pci_config_read16(bus, device, function, PCI_VENDOR_ID_OFFSET) != 0xFFFF) {
                pci_scan_function(bus, device, function);
            }
        }
    }
}

static void pci_enumerate_bus(uint8_t bus) {
    if (bus_visited[bus]) {
        return;
    }

    bus_visited[bus] = 1;

    for (uint8_t device = 0; device < 32; ++device) {
        pci_scan_device(bus, device);
    }
}

int pci_init(void) {
    if (pci_initialized) {
        return 0;
    }

    klog(KLOG_INFO, "PCI: Initializing PCI subsystem");
    device_count = 0;
    primary_gpu.present = 0;
    primary_gpu.mmio_phys_base = 0;
    primary_gpu.mmio_size = 0;
    primary_gpu.mmio_virt_base = NULL;

    for (size_t i = 0; i < PCI_MAX_BUSES; ++i) {
        bus_visited[i] = 0;
    }

    pci_enumerate_bus(0);

    if (!primary_gpu.present) {
        klog(KLOG_INFO, "PCI: No GPU-class device detected on primary bus");
    }

    klog_raw(KLOG_INFO, "PCI: Enumeration complete. Devices discovered: ");
    klog_decimal(KLOG_INFO, device_count);
    klog(KLOG_INFO, "");

    pci_initialized = 1;
    return 0;
}

size_t pci_get_device_count(void) {
    return device_count;
}

const pci_device_info_t *pci_get_devices(void) {
    return devices;
}

const pci_gpu_info_t *pci_get_primary_gpu(void) {
    return primary_gpu.present ? &primary_gpu : NULL;
}
