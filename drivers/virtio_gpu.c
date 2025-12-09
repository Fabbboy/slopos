#include "virtio_gpu.h"
#include "pci_driver.h"
#include "wl_currency.h"
#include "../lib/klog.h"
#include "../mm/phys_virt.h"

#include <stdint.h>
#include <stddef.h>
#include <stdbool.h>

#define VIRTIO_PCI_STATUS_OFFSET      0x12
#define VIRTIO_STATUS_ACKNOWLEDGE     0x01
#define VIRTIO_STATUS_DRIVER          0x02

#define VIRTIO_MMIO_DEFAULT_SIZE      0x1000
#define PCI_COMMAND_MEMORY_SPACE      0x0002
#define PCI_COMMAND_BUS_MASTER         0x0004

static virtio_gpu_device_t virtio_gpu_device = {0};

static void virtio_gpu_enable_master(const pci_device_info_t *info) {
    uint16_t command = pci_config_read16(info->bus, info->device, info->function, PCI_COMMAND_OFFSET);
    uint16_t desired = command | PCI_COMMAND_MEMORY_SPACE | PCI_COMMAND_BUS_MASTER;
    if (command != desired) {
        pci_config_write16(info->bus, info->device, info->function, PCI_COMMAND_OFFSET, desired);
    }
}

static bool virtio_gpu_match(const pci_device_info_t *info, void *context) {
    (void)context;
    if (info->vendor_id != VIRTIO_GPU_VENDOR_ID) {
        return false;
    }

    return info->device_id == VIRTIO_GPU_DEVICE_ID_PRIMARY ||
           info->device_id == VIRTIO_GPU_DEVICE_ID_TRANS;
}

static int virtio_gpu_probe(const pci_device_info_t *info, void *context) {
    (void)context;

    if (virtio_gpu_device.present) {
        klog_printf(KLOG_DEBUG, "PCI: virtio-gpu driver already claimed a device\n");
        return -1;
    }

    const pci_bar_info_t *bar = NULL;
    for (uint8_t i = 0; i < info->bar_count; ++i) {
        if (!info->bars[i].is_io && info->bars[i].base != 0) {
            bar = &info->bars[i];
            break;
        }
    }

    if (!bar) {
        klog_printf(KLOG_INFO, "PCI: virtio-gpu missing MMIO BAR\n");
        wl_award_loss();
        return -1;
    }

    size_t mmio_size = bar->size ? (size_t)bar->size : VIRTIO_MMIO_DEFAULT_SIZE;
    void *mmio_base = mm_map_mmio_region(bar->base, mmio_size);
    if (!mmio_base) {
        klog_printf(KLOG_INFO, "PCI: virtio-gpu MMIO mapping failed for phys=0x%llx\n",
                    (unsigned long long)bar->base);
        wl_award_loss();
        return -1;
    }

    virtio_gpu_enable_master(info);

    uint8_t status_before = pci_config_read8(info->bus, info->device, info->function, VIRTIO_PCI_STATUS_OFFSET);
    klog_printf(KLOG_DEBUG, "PCI: virtio-gpu status read=0x%02x\n", status_before);

    pci_config_write8(info->bus, info->device, info->function, VIRTIO_PCI_STATUS_OFFSET, 0x00);
    uint8_t status_zeroed = pci_config_read8(info->bus, info->device, info->function, VIRTIO_PCI_STATUS_OFFSET);
    klog_printf(KLOG_DEBUG, "PCI: virtio-gpu status after clear=0x%02x\n", status_zeroed);

    uint8_t handshake = VIRTIO_STATUS_ACKNOWLEDGE | VIRTIO_STATUS_DRIVER;
    pci_config_write8(info->bus, info->device, info->function, VIRTIO_PCI_STATUS_OFFSET, handshake);
    uint8_t status_handshake = pci_config_read8(info->bus, info->device, info->function, VIRTIO_PCI_STATUS_OFFSET);
    if ((status_handshake & handshake) != handshake) {
        klog_printf(KLOG_INFO, "PCI: virtio-gpu handshake incomplete (status=0x%02x)\n", status_handshake);
        mm_unmap_mmio_region(mmio_base, mmio_size);
        wl_award_loss();
        return -1;
    }

    volatile uint32_t *sample = (volatile uint32_t *)mmio_base;
    uint32_t sample_value = *sample;
    klog_printf(KLOG_DEBUG, "PCI: virtio-gpu MMIO sample value=0x%08x\n", sample_value);

    virtio_gpu_device.present = 1;
    virtio_gpu_device.device = *info;
    virtio_gpu_device.mmio_base = mmio_base;
    virtio_gpu_device.mmio_size = mmio_size;

    klog_printf(KLOG_INFO, "PCI: virtio-gpu driver probe succeeded (wheel gave a W)\n");
    wl_award_win();
    return 0;
}

static const pci_driver_t virtio_gpu_pci_driver = {
    .name = "virtio-gpu",
    .match = virtio_gpu_match,
    .probe = virtio_gpu_probe,
    .context = NULL
};

void virtio_gpu_register_driver(void) {
    static int registered = 0;
    if (registered) {
        return;
    }

    if (pci_register_driver(&virtio_gpu_pci_driver) != 0) {
        klog_printf(KLOG_INFO, "PCI: virtio-gpu driver registration failed\n");
    }

    registered = 1;
}

const virtio_gpu_device_t *virtio_gpu_get_device(void) {
    return virtio_gpu_device.present ? &virtio_gpu_device : NULL;
}
