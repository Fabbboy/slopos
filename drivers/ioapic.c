/*
 * SlopOS IOAPIC Driver
 * Discovers IOAPIC controllers via ACPI MADT and exposes redirection helpers
 */

#include "ioapic.h"
#include "apic.h"
#include "serial.h"
#include "pic.h"
#include "../boot/log.h"
#include "../boot/limine_protocol.h"
#include "../lib/memory.h"

#include <stdint.h>
#include <stddef.h>
#include <stdbool.h>

#define IOAPIC_MAX_CONTROLLERS   8
#define IOAPIC_MAX_ISO_ENTRIES   32

#define IOAPIC_REG_ID            0x00
#define IOAPIC_REG_VER           0x01
#define IOAPIC_REG_REDIR_BASE    0x10

#define IOAPIC_REDIR_WRITABLE_MASK  ((7u << 8) | (1u << 11) | (1u << 13) | (1u << 15) | (1u << 16))

#define MADT_ENTRY_LOCAL_APIC           0
#define MADT_ENTRY_IOAPIC               1
#define MADT_ENTRY_INTERRUPT_OVERRIDE   2

#define ACPI_MADT_POLARITY_MASK   0x3
#define ACPI_MADT_TRIGGER_MASK    0xC
#define ACPI_MADT_TRIGGER_SHIFT   2

struct acpi_rsdp {
    char signature[8];
    uint8_t checksum;
    char oem_id[6];
    uint8_t revision;
    uint32_t rsdt_address;
    uint32_t length;
    uint64_t xsdt_address;
    uint8_t extended_checksum;
    uint8_t reserved[3];
} __attribute__((packed));

struct acpi_sdt_header {
    char signature[4];
    uint32_t length;
    uint8_t revision;
    uint8_t checksum;
    char oem_id[6];
    char oem_table_id[8];
    uint32_t oem_revision;
    uint32_t creator_id;
    uint32_t creator_revision;
} __attribute__((packed));

struct acpi_madt {
    struct acpi_sdt_header header;
    uint32_t lapic_address;
    uint32_t flags;
    uint8_t entries[];
} __attribute__((packed));

struct acpi_madt_entry_header {
    uint8_t type;
    uint8_t length;
} __attribute__((packed));

struct acpi_madt_ioapic_entry {
    struct acpi_madt_entry_header header;
    uint8_t ioapic_id;
    uint8_t reserved;
    uint32_t ioapic_address;
    uint32_t gsi_base;
} __attribute__((packed));

struct acpi_madt_iso_entry {
    struct acpi_madt_entry_header header;
    uint8_t bus_source;
    uint8_t irq_source;
    uint32_t gsi;
    uint16_t flags;
} __attribute__((packed));

struct ioapic_controller {
    uint8_t id;
    uint32_t gsi_base;
    uint32_t gsi_count;
    uint32_t version;
    uint64_t phys_addr;
    volatile uint32_t *reg_select;
    volatile uint32_t *reg_window;
};

struct ioapic_iso {
    uint8_t bus_source;
    uint8_t irq_source;
    uint32_t gsi;
    uint16_t flags;
};

static struct ioapic_controller ioapic_table[IOAPIC_MAX_CONTROLLERS];
static struct ioapic_iso iso_table[IOAPIC_MAX_ISO_ENTRIES];
static size_t ioapic_count = 0;
static size_t iso_count = 0;
static int ioapic_ready = 0;

static inline void *phys_to_virt(uint64_t phys) {
    if (phys == 0) {
        return NULL;
    }
    if (!is_hhdm_available()) {
        return (void *)(uintptr_t)phys;
    }
    return (void *)(uintptr_t)(phys + get_hhdm_offset());
}

static uint8_t acpi_checksum(const void *table, size_t length) {
    const uint8_t *bytes = (const uint8_t *)table;
    uint8_t sum = 0;
    for (size_t i = 0; i < length; i++) {
        sum = (uint8_t)(sum + bytes[i]);
    }
    return sum;
}

static bool acpi_validate_rsdp(const struct acpi_rsdp *rsdp) {
    if (!rsdp) {
        return false;
    }
    if (acpi_checksum(rsdp, 20) != 0) {
        return false;
    }
    if (rsdp->revision >= 2 && rsdp->length >= sizeof(struct acpi_rsdp)) {
        if (acpi_checksum(rsdp, rsdp->length) != 0) {
            return false;
        }
    }
    return true;
}

static bool acpi_validate_table(const struct acpi_sdt_header *header) {
    if (!header || header->length < sizeof(struct acpi_sdt_header)) {
        return false;
    }
    return acpi_checksum(header, header->length) == 0;
}

static const struct acpi_sdt_header *acpi_map_table(uint64_t phys_addr) {
    if (phys_addr == 0) {
        return NULL;
    }
    return (const struct acpi_sdt_header *)phys_to_virt(phys_addr);
}

static const struct acpi_sdt_header *acpi_scan_table(const struct acpi_sdt_header *sdt,
                                                     size_t entry_size,
                                                     const char signature[4]) {
    if (!sdt || sdt->length < sizeof(struct acpi_sdt_header)) {
        return NULL;
    }

    size_t payload_bytes = sdt->length - sizeof(struct acpi_sdt_header);
    size_t entry_count = payload_bytes / entry_size;
    const uint8_t *entries = ((const uint8_t *)sdt) + sizeof(struct acpi_sdt_header);

    for (size_t i = 0; i < entry_count; i++) {
        uint64_t phys = 0;
        const uint8_t *entry_ptr = entries + (i * entry_size);
        if (entry_size == 8) {
            uint64_t value = 0;
            memcpy(&value, entry_ptr, sizeof(uint64_t));
            phys = value;
        } else {
            uint32_t value = 0;
            memcpy(&value, entry_ptr, sizeof(uint32_t));
            phys = value;
        }

        const struct acpi_sdt_header *candidate = acpi_map_table(phys);
        if (!candidate) {
            continue;
        }
        if (memcmp(candidate->signature, signature, 4) != 0) {
            continue;
        }
        if (!acpi_validate_table(candidate)) {
            boot_log_info("ACPI: Found table with invalid checksum, skipping");
            continue;
        }
        return candidate;
    }
    return NULL;
}

static const struct acpi_sdt_header *acpi_find_table(const struct acpi_rsdp *rsdp,
                                                     const char signature[4]) {
    if (!rsdp) {
        return NULL;
    }

    if (rsdp->revision >= 2 && rsdp->xsdt_address != 0) {
        const struct acpi_sdt_header *xsdt =
            acpi_map_table(rsdp->xsdt_address);
        if (xsdt && acpi_validate_table(xsdt)) {
            const struct acpi_sdt_header *hit =
                acpi_scan_table(xsdt, sizeof(uint64_t), signature);
            if (hit) {
                return hit;
            }
        }
    }

    if (rsdp->rsdt_address != 0) {
        const struct acpi_sdt_header *rsdt =
            acpi_map_table(rsdp->rsdt_address);
        if (rsdt && acpi_validate_table(rsdt)) {
            const struct acpi_sdt_header *hit =
                acpi_scan_table(rsdt, sizeof(uint32_t), signature);
            if (hit) {
                return hit;
            }
        }
    }

    return NULL;
}

static struct ioapic_controller *ioapic_find_controller(uint32_t gsi) {
    for (size_t i = 0; i < ioapic_count; i++) {
        struct ioapic_controller *ctrl = &ioapic_table[i];
        uint32_t start = ctrl->gsi_base;
        uint32_t end = ctrl->gsi_base + ctrl->gsi_count - 1;
        if (gsi >= start && gsi <= end) {
            return ctrl;
        }
    }
    return NULL;
}

static uint32_t ioapic_read(const struct ioapic_controller *ctrl, uint8_t reg) {
    if (!ctrl || !ctrl->reg_select || !ctrl->reg_window) {
        return 0;
    }
    *ctrl->reg_select = reg;
    return *ctrl->reg_window;
}

static void ioapic_write(const struct ioapic_controller *ctrl, uint8_t reg, uint32_t value) {
    if (!ctrl || !ctrl->reg_select || !ctrl->reg_window) {
        return;
    }
    *ctrl->reg_select = reg;
    *ctrl->reg_window = value;
}

static uint32_t ioapic_entry_low_index(uint32_t pin) {
    return (uint32_t)(IOAPIC_REG_REDIR_BASE + (pin * 2));
}

static uint32_t ioapic_entry_high_index(uint32_t pin) {
    return ioapic_entry_low_index(pin) + 1;
}

static void ioapic_log_controller(const struct ioapic_controller *ctrl) {
    if (!ctrl) return;
    BOOT_LOG_BLOCK(BOOT_LOG_LEVEL_INFO, {
        kprint("IOAPIC: ID ");
        kprint_hex(ctrl->id);
        kprint(" @ phys ");
        kprint_hex(ctrl->phys_addr);
        kprint(", GSIs ");
        kprint_dec(ctrl->gsi_base);
        kprint("-");
        kprint_dec(ctrl->gsi_base + ctrl->gsi_count - 1);
        kprint(", version 0x");
        kprint_hex(ctrl->version & 0xFF);
        kprintln("");
    });
}

static void ioapic_log_iso(const struct ioapic_iso *iso) {
    if (!iso) return;
    BOOT_LOG_BLOCK(BOOT_LOG_LEVEL_DEBUG, {
        kprint("IOAPIC: ISO bus ");
        kprint_dec(iso->bus_source);
        kprint(", IRQ ");
        kprint_dec(iso->irq_source);
        kprint(" -> GSI ");
        kprint_dec(iso->gsi);
        kprint(", flags 0x");
        kprint_hex(iso->flags);
        kprintln("");
    });
}

static uint32_t ioapic_flags_from_acpi(uint8_t bus_source, uint16_t flags) {
    uint32_t result = 0;

    uint16_t polarity = flags & ACPI_MADT_POLARITY_MASK;
    switch (polarity) {
        case 0: /* Conform */
        case 1: /* Active high */
            result |= IOAPIC_FLAG_POLARITY_HIGH;
            break;
        case 3: /* Active low */
            result |= IOAPIC_FLAG_POLARITY_LOW;
            break;
        default:
            result |= IOAPIC_FLAG_POLARITY_HIGH;
            break;
    }

    uint16_t trigger = (flags & ACPI_MADT_TRIGGER_MASK) >> ACPI_MADT_TRIGGER_SHIFT;
    switch (trigger) {
        case 0: /* Conform */
        case 1: /* Edge */
            result |= IOAPIC_FLAG_TRIGGER_EDGE;
            break;
        case 3: /* Level */
            result |= IOAPIC_FLAG_TRIGGER_LEVEL;
            break;
        default:
            result |= IOAPIC_FLAG_TRIGGER_EDGE;
            break;
    }

    (void)bus_source; /* Future: differentiate buses */
    return result;
}

static const struct ioapic_iso *ioapic_find_iso(uint8_t irq) {
    for (size_t i = 0; i < iso_count; i++) {
        if (iso_table[i].irq_source == irq) {
            return &iso_table[i];
        }
    }
    return NULL;
}

static int ioapic_update_mask(uint32_t gsi, bool mask) {
    struct ioapic_controller *ctrl = ioapic_find_controller(gsi);
    if (!ctrl) {
        boot_log_info("IOAPIC: No controller for requested GSI");
        return -1;
    }

    uint32_t pin = gsi - ctrl->gsi_base;
    if (pin >= ctrl->gsi_count) {
        boot_log_info("IOAPIC: Pin out of range for mask request");
        return -1;
    }

    uint32_t reg = ioapic_entry_low_index(pin);
    uint32_t value = ioapic_read(ctrl, reg);

    if (mask) {
        value |= IOAPIC_FLAG_MASK;
    } else {
        value &= ~IOAPIC_FLAG_MASK;
    }

    ioapic_write(ctrl, reg, value);

    BOOT_LOG_BLOCK(BOOT_LOG_LEVEL_DEBUG, {
        kprint("IOAPIC: ");
        kprint(mask ? "Masked" : "Unmasked");
        kprint(" GSI ");
        kprint_dec(gsi);
        kprint(" (pin ");
        kprint_dec(pin);
        kprint(") -> low=0x");
        kprint_hex(value);
        kprintln("");
    });

    return 0;
}

static void ioapic_parse_madt(const struct acpi_madt *madt) {
    if (!madt) {
        return;
    }

    ioapic_count = 0;
    iso_count = 0;

    const uint8_t *cursor = madt->entries;
    const uint8_t *end = ((const uint8_t *)madt) + madt->header.length;

    while (cursor + sizeof(struct acpi_madt_entry_header) <= end) {
        const struct acpi_madt_entry_header *hdr =
            (const struct acpi_madt_entry_header *)cursor;

        if (hdr->length == 0 || cursor + hdr->length > end) {
            break;
        }

        switch (hdr->type) {
            case MADT_ENTRY_IOAPIC: {
                if (hdr->length < sizeof(struct acpi_madt_ioapic_entry)) {
                    break;
                }
                if (ioapic_count >= IOAPIC_MAX_CONTROLLERS) {
                    boot_log_info("IOAPIC: Too many controllers, ignoring extra entries");
                    break;
                }
                const struct acpi_madt_ioapic_entry *entry =
                    (const struct acpi_madt_ioapic_entry *)cursor;
                struct ioapic_controller *ctrl = &ioapic_table[ioapic_count++];
                ctrl->id = entry->ioapic_id;
                ctrl->gsi_base = entry->gsi_base;
                ctrl->phys_addr = entry->ioapic_address;
                ctrl->reg_select = (volatile uint32_t *)phys_to_virt(ctrl->phys_addr);
                ctrl->reg_window = (volatile uint32_t *)phys_to_virt(ctrl->phys_addr + 0x10);
                ctrl->version = ioapic_read(ctrl, IOAPIC_REG_VER);
                ctrl->gsi_count = ((ctrl->version >> 16) & 0xFF) + 1;
                ioapic_log_controller(ctrl);
                break;
            }
            case MADT_ENTRY_INTERRUPT_OVERRIDE: {
                if (hdr->length < sizeof(struct acpi_madt_iso_entry)) {
                    break;
                }
                if (iso_count >= IOAPIC_MAX_ISO_ENTRIES) {
                    boot_log_info("IOAPIC: Too many source overrides, ignoring extras");
                    break;
                }
                const struct acpi_madt_iso_entry *entry =
                    (const struct acpi_madt_iso_entry *)cursor;
                struct ioapic_iso *iso = &iso_table[iso_count++];
                iso->bus_source = entry->bus_source;
                iso->irq_source = entry->irq_source;
                iso->gsi = entry->gsi;
                iso->flags = entry->flags;
                ioapic_log_iso(iso);
                break;
            }
            default:
                break;
        }

        cursor += hdr->length;
    }
}

int ioapic_init(void) {
    if (ioapic_ready) {
        return 0;
    }

    if (!is_hhdm_available()) {
        boot_log_info("IOAPIC: HHDM unavailable, cannot map MMIO registers");
        return -1;
    }

    if (!is_rsdp_available()) {
        boot_log_info("IOAPIC: ACPI RSDP unavailable, skipping IOAPIC init");
        return -1;
    }

    const struct acpi_rsdp *rsdp =
        (const struct acpi_rsdp *)get_rsdp_address();
    if (!acpi_validate_rsdp(rsdp)) {
        boot_log_info("IOAPIC: ACPI RSDP checksum failed");
        return -1;
    }

    const struct acpi_sdt_header *madt_header =
        acpi_find_table(rsdp, "APIC");
    if (!madt_header) {
        boot_log_info("IOAPIC: MADT not found in ACPI tables");
        return -1;
    }

    if (!acpi_validate_table(madt_header)) {
        boot_log_info("IOAPIC: MADT checksum invalid");
        return -1;
    }

    const struct acpi_madt *madt = (const struct acpi_madt *)madt_header;
    ioapic_parse_madt(madt);

    if (ioapic_count == 0) {
        boot_log_info("IOAPIC: No controllers discovered");
        return -1;
    }

    boot_log_info("IOAPIC: Discovery complete");
    ioapic_ready = 1;
    return 0;
}

int ioapic_config_irq(uint32_t gsi, uint8_t vector, uint8_t lapic_id, uint32_t flags) {
    if (!ioapic_ready) {
        boot_log_info("IOAPIC: Driver not initialized");
        return -1;
    }

    struct ioapic_controller *ctrl = ioapic_find_controller(gsi);
    if (!ctrl) {
        boot_log_info("IOAPIC: No IOAPIC handles requested GSI");
        return -1;
    }

    uint32_t pin = gsi - ctrl->gsi_base;
    if (pin >= ctrl->gsi_count) {
        boot_log_info("IOAPIC: Calculated pin outside controller range");
        return -1;
    }

    uint32_t writable_flags = flags & IOAPIC_REDIR_WRITABLE_MASK;
    uint32_t low = (uint32_t)vector | writable_flags;
    uint32_t high = ((uint32_t)lapic_id) << 24;

    ioapic_write(ctrl, ioapic_entry_high_index(pin), high);
    ioapic_write(ctrl, ioapic_entry_low_index(pin), low);

    BOOT_LOG_BLOCK(BOOT_LOG_LEVEL_INFO, {
        kprint("IOAPIC: Configured GSI ");
        kprint_dec(gsi);
        kprint(" (pin ");
        kprint_dec(pin);
        kprint(") -> vector ");
        kprint_hex(vector);
        kprint(", LAPIC ");
        kprint_hex(lapic_id);
        kprint(", low=0x");
        kprint_hex(low);
        kprint(", high=0x");
        kprint_hex(high);
        kprintln("");
    });

    return 0;
}

int ioapic_mask_gsi(uint32_t gsi) {
    return ioapic_update_mask(gsi, true);
}

int ioapic_unmask_gsi(uint32_t gsi) {
    return ioapic_update_mask(gsi, false);
}

int ioapic_is_ready(void) {
    return ioapic_ready;
}

int ioapic_legacy_irq_info(uint8_t legacy_irq, uint32_t *out_gsi, uint32_t *out_flags) {
    if (!out_gsi || !out_flags) {
        return -1;
    }

    if (!ioapic_ready) {
        boot_log_info("IOAPIC: Legacy route query before initialization");
        return -1;
    }

    uint32_t gsi = legacy_irq;
    uint32_t flags = IOAPIC_FLAG_POLARITY_HIGH | IOAPIC_FLAG_TRIGGER_EDGE;

    const struct ioapic_iso *iso = ioapic_find_iso(legacy_irq);
    if (iso) {
        gsi = iso->gsi;
        flags = ioapic_flags_from_acpi(iso->bus_source, iso->flags);
        ioapic_log_iso(iso);
    }

    *out_gsi = gsi;
    *out_flags = flags;
    return 0;
}

int ioapic_route_legacy_irq1(uint8_t vector) {
    if (!ioapic_ready) {
        boot_log_info("IOAPIC: Driver not initialized, cannot route IRQ1");
        return -1;
    }

    if (!apic_is_available()) {
        boot_log_info("IOAPIC: Local APIC unavailable, cannot route IRQ1");
        return -1;
    }

    uint32_t gsi = PIC_IRQ_KEYBOARD;
    uint32_t redir_flags = IOAPIC_FLAG_DELIVERY_FIXED |
                           IOAPIC_FLAG_DEST_PHYSICAL |
                           IOAPIC_FLAG_UNMASKED;

    const struct ioapic_iso *iso = ioapic_find_iso(PIC_IRQ_KEYBOARD);
    if (iso) {
        gsi = iso->gsi;
        redir_flags |= ioapic_flags_from_acpi(iso->bus_source, iso->flags);
        BOOT_LOG_BLOCK(BOOT_LOG_LEVEL_DEBUG, {
            kprint("IOAPIC: Using ISO for IRQ1 -> GSI ");
            kprint_dec(gsi);
            kprintln("");
        });
    } else {
        gsi = PIC_IRQ_KEYBOARD;
        redir_flags |= IOAPIC_FLAG_POLARITY_HIGH | IOAPIC_FLAG_TRIGGER_EDGE;
    }

    uint8_t lapic_id = (uint8_t)apic_get_id();
    int rc = ioapic_config_irq(gsi, vector, lapic_id, redir_flags);

    if (rc == 0) {
        BOOT_LOG_BLOCK(BOOT_LOG_LEVEL_INFO, {
            kprint("IOAPIC: Routed legacy IRQ1 through IOAPIC (GSI ");
            kprint_dec(gsi);
            kprint(", vector ");
            kprint_hex(vector);
            kprintln(")");
        });
    }

    return rc;
}
