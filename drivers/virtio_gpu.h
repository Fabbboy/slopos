#ifndef DRIVERS_VIRTIO_GPU_H
#define DRIVERS_VIRTIO_GPU_H

#include <stddef.h>
#include "pci.h"

#define VIRTIO_GPU_VENDOR_ID          0x1AF4
#define VIRTIO_GPU_DEVICE_ID_PRIMARY  0x1050
#define VIRTIO_GPU_DEVICE_ID_TRANS     0x1010

typedef struct virtio_gpu_device {
    int present;
    pci_device_info_t device;
    void *mmio_base;
    size_t mmio_size;
} virtio_gpu_device_t;

void virtio_gpu_register_driver(void);
const virtio_gpu_device_t *virtio_gpu_get_device(void);

#endif /* DRIVERS_VIRTIO_GPU_H */
