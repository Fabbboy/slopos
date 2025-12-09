#ifndef DRIVERS_PCI_DRIVER_H
#define DRIVERS_PCI_DRIVER_H

#include <stdbool.h>
#include "pci.h"

typedef struct pci_driver {
    const char *name;
    bool (*match)(const pci_device_info_t *device, void *context);
    int (*probe)(const pci_device_info_t *device, void *context);
    void *context;
} pci_driver_t;

int pci_register_driver(const pci_driver_t *driver);

#endif /* DRIVERS_PCI_DRIVER_H */
