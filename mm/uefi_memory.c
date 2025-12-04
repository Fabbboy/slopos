/*
 * SlopOS Memory Management - UEFI Memory Map Parser
 * Parses EFI memory descriptors from Multiboot2 structure
 * Provides memory layout information for physical memory allocation
 */

#include <stdint.h>
#include <stddef.h>
#include "../boot/constants.h"
#include "../boot/kernel_panic.h"
#include "../drivers/serial.h"

/* Forward declarations */
int add_page_alloc_region(uint64_t start_addr, uint64_t size, uint8_t type);

/* ========================================================================
 * EFI MEMORY TYPE CONSTANTS
 * ======================================================================== */

/* EFI Memory Types - as defined by UEFI specification */
#define EFI_RESERVED_MEMORY_TYPE          0
#define EFI_LOADER_CODE                   1
#define EFI_LOADER_DATA                   2
#define EFI_BOOT_SERVICES_CODE            3
#define EFI_BOOT_SERVICES_DATA            4
#define EFI_RUNTIME_SERVICES_CODE         5
#define EFI_RUNTIME_SERVICES_DATA         6
#define EFI_CONVENTIONAL_MEMORY           7  /* Available for allocation */
#define EFI_UNUSABLE_MEMORY               8
#define EFI_ACPI_RECLAIM_MEMORY           9
#define EFI_ACPI_MEMORY_NVS               10
#define EFI_MEMORY_MAPPED_IO              11
#define EFI_MEMORY_MAPPED_IO_PORT_SPACE   12
#define EFI_PAL_CODE                      13
#define EFI_PERSISTENT_MEMORY             14
#define EFI_MAX_MEMORY_TYPE               15

/* EFI Memory Descriptor Attributes */
#define EFI_MEMORY_UC                     0x0000000000000001ULL  /* Uncacheable */
#define EFI_MEMORY_WC                     0x0000000000000002ULL  /* Write-combine */
#define EFI_MEMORY_WT                     0x0000000000000004ULL  /* Write-through */
#define EFI_MEMORY_WB                     0x0000000000000008ULL  /* Write-back */
#define EFI_MEMORY_UCE                    0x0000000000000010ULL  /* Uncacheable, exported */
#define EFI_MEMORY_WP                     0x0000000000001000ULL  /* Write-protect */
#define EFI_MEMORY_RP                     0x0000000000002000ULL  /* Read-protect */
#define EFI_MEMORY_XP                     0x0000000000004000ULL  /* Execute-protect */
#define EFI_MEMORY_NV                     0x0000000000008000ULL  /* Non-volatile */
#define EFI_MEMORY_MORE_RELIABLE          0x0000000000010000ULL  /* More reliable */
#define EFI_MEMORY_RO                     0x0000000000020000ULL  /* Read-only */
#define EFI_MEMORY_RUNTIME                0x8000000000000000ULL  /* Runtime services */

/* ========================================================================
 * EFI MEMORY STRUCTURES
 * ======================================================================== */

/* EFI Memory Descriptor structure */
typedef struct {
    uint32_t type;                /* Memory type */
    uint32_t pad;                 /* Padding for alignment */
    uint64_t physical_start;      /* Physical start address */
    uint64_t virtual_start;       /* Virtual start address */
    uint64_t number_of_pages;     /* Number of 4KB pages */
    uint64_t attribute;           /* Memory attributes */
} __attribute__((packed)) efi_memory_descriptor_t;

/* Multiboot2 EFI Memory Map Tag structure */
typedef struct {
    uint32_t type;                /* Tag type (MULTIBOOT_TAG_TYPE_EFI_MMAP) */
    uint32_t size;                /* Tag size */
    uint32_t descriptor_size;     /* Size of each memory descriptor */
    uint32_t descriptor_version;  /* Descriptor version */
    efi_memory_descriptor_t descriptors[];  /* Array of descriptors */
} __attribute__((packed)) multiboot_tag_efi_mmap_t;

/* EFI memory analysis results */
typedef struct {
    uint64_t total_memory;        /* Total physical memory */
    uint64_t usable_memory;       /* Usable conventional memory */
    uint64_t reserved_memory;     /* Reserved memory */
    uint32_t num_descriptors;     /* Number of descriptors processed */
    uint32_t num_usable_regions;  /* Number of usable regions */
    uint64_t largest_region_size; /* Size of largest usable region */
    uint64_t largest_region_addr; /* Address of largest usable region */
} efi_memory_analysis_t;

/* Global EFI memory analysis */
static efi_memory_analysis_t efi_analysis = {0};

/* ========================================================================
 * UTILITY FUNCTIONS
 * ======================================================================== */

/*
 * Get human-readable name for EFI memory type
 */
static const char *get_efi_memory_type_name(uint32_t type) {
    switch (type) {
        case EFI_RESERVED_MEMORY_TYPE:      return "Reserved";
        case EFI_LOADER_CODE:               return "LoaderCode";
        case EFI_LOADER_DATA:               return "LoaderData";
        case EFI_BOOT_SERVICES_CODE:        return "BootServicesCode";
        case EFI_BOOT_SERVICES_DATA:        return "BootServicesData";
        case EFI_RUNTIME_SERVICES_CODE:     return "RuntimeServicesCode";
        case EFI_RUNTIME_SERVICES_DATA:     return "RuntimeServicesData";
        case EFI_CONVENTIONAL_MEMORY:       return "Conventional";
        case EFI_UNUSABLE_MEMORY:           return "Unusable";
        case EFI_ACPI_RECLAIM_MEMORY:       return "ACPIReclaim";
        case EFI_ACPI_MEMORY_NVS:           return "ACPINVS";
        case EFI_MEMORY_MAPPED_IO:          return "MMIO";
        case EFI_MEMORY_MAPPED_IO_PORT_SPACE: return "MMIOPortSpace";
        case EFI_PAL_CODE:                  return "PALCode";
        case EFI_PERSISTENT_MEMORY:         return "Persistent";
        default:                            return "Unknown";
    }
}

/*
 * Check if memory type is available for allocation
 */
static int is_memory_available(uint32_t type) {
    return (type == EFI_CONVENTIONAL_MEMORY) ||
           (type == EFI_BOOT_SERVICES_CODE) ||
           (type == EFI_BOOT_SERVICES_DATA);
}

/*
 * Convert EFI memory type to internal memory type
 */
static uint8_t convert_efi_type(uint32_t efi_type) {
    switch (efi_type) {
        case EFI_CONVENTIONAL_MEMORY:
        case EFI_BOOT_SERVICES_CODE:
        case EFI_BOOT_SERVICES_DATA:
            return EFI_CONVENTIONAL_MEMORY;  /* Mark as available */
        default:
            return (uint8_t)efi_type;  /* Keep original type */
    }
}

/*
 * Validate EFI memory descriptor
 */
static int validate_efi_descriptor(const efi_memory_descriptor_t *desc) {
    if (!desc) {
        return 0;
    }

    /* Check for valid memory type */
    if (desc->type >= EFI_MAX_MEMORY_TYPE) {
        return 0;
    }

    /* Check for non-zero page count */
    if (desc->number_of_pages == 0) {
        return 0;
    }

    /* Check for reasonable physical address */
    if (desc->physical_start >= 0x100000000000ULL) {  /* 16TB limit */
        return 0;
    }

    return 1;
}

/* ========================================================================
 * EFI MEMORY MAP PROCESSING
 * ======================================================================== */

/*
 * Process a single EFI memory descriptor
 */
static void process_efi_descriptor(const efi_memory_descriptor_t *desc) {
    if (!validate_efi_descriptor(desc)) {
        kprint("Invalid EFI descriptor, skipping\n");
        return;
    }

    uint64_t start_addr = desc->physical_start;
    uint64_t size = desc->number_of_pages * EFI_PAGE_SIZE;
    uint64_t end_addr = start_addr + size;

    kprint("EFI Region: ");
    kprint_hex(start_addr);
    kprint(" - ");
    kprint_hex(end_addr);
    kprint(" (");
    kprint_decimal(size >> 20);  /* Size in MB */
    kprint("MB) ");
    kprint(get_efi_memory_type_name(desc->type));

    /* Show attributes if present */
    if (desc->attribute & EFI_MEMORY_RUNTIME) {
        kprint(" [RUNTIME]");
    }
    if (desc->attribute & EFI_MEMORY_UC) {
        kprint(" [UC]");
    }
    if (desc->attribute & EFI_MEMORY_WC) {
        kprint(" [WC]");
    }

    kprint("\n");

    /* Update analysis statistics */
    efi_analysis.total_memory += size;
    efi_analysis.num_descriptors++;

    if (is_memory_available(desc->type)) {
        efi_analysis.usable_memory += size;
        efi_analysis.num_usable_regions++;

        /* Track largest usable region */
        if (size > efi_analysis.largest_region_size) {
            efi_analysis.largest_region_size = size;
            efi_analysis.largest_region_addr = start_addr;
        }

        /* Add to memory allocator */
        uint8_t internal_type = convert_efi_type(desc->type);
        if (add_page_alloc_region(start_addr, size, internal_type) != 0) {
            kprint("WARNING: Failed to add memory region to allocator\n");
        }
    } else {
        efi_analysis.reserved_memory += size;
    }
}

/*
 * Parse EFI memory map from Multiboot2 tag
 */
static int parse_efi_memory_map(const multiboot_tag_efi_mmap_t *mmap_tag) {
    if (!mmap_tag) {
        kprint("parse_efi_memory_map: NULL tag pointer\n");
        return -1;
    }

    kprint("EFI Memory Map Tag:\n");
    kprint("  Size: ");
    kprint_decimal(mmap_tag->size);
    kprint(" bytes\n");
    kprint("  Descriptor size: ");
    kprint_decimal(mmap_tag->descriptor_size);
    kprint(" bytes\n");
    kprint("  Descriptor version: ");
    kprint_decimal(mmap_tag->descriptor_version);
    kprint("\n");

    /* Validate descriptor size */
    if (mmap_tag->descriptor_size < sizeof(efi_memory_descriptor_t)) {
        kprint("ERROR: EFI descriptor size too small\n");
        return -1;
    }

    /* Calculate number of descriptors */
    uint32_t descriptors_size = mmap_tag->size - sizeof(multiboot_tag_efi_mmap_t);
    uint32_t num_descriptors = descriptors_size / mmap_tag->descriptor_size;

    kprint("Processing ");
    kprint_decimal(num_descriptors);
    kprint(" EFI memory descriptors:\n");

    /* Process each descriptor */
    const uint8_t *desc_ptr = (const uint8_t*)mmap_tag->descriptors;
    for (uint32_t i = 0; i < num_descriptors; i++) {
        const efi_memory_descriptor_t *desc = (const efi_memory_descriptor_t*)desc_ptr;
        process_efi_descriptor(desc);
        desc_ptr += mmap_tag->descriptor_size;
    }

    return 0;
}

/* ========================================================================
 * MEMORY LAYOUT ANALYSIS
 * ======================================================================== */

/*
 * Print EFI memory analysis summary
 */
static void print_memory_analysis(void) {
    kprint("\n=== EFI Memory Analysis ===\n");
    kprint("Total Memory: ");
    kprint_decimal(efi_analysis.total_memory >> 20);
    kprint(" MB\n");
    kprint("Usable Memory: ");
    kprint_decimal(efi_analysis.usable_memory >> 20);
    kprint(" MB\n");
    kprint("Reserved Memory: ");
    kprint_decimal(efi_analysis.reserved_memory >> 20);
    kprint(" MB\n");
    kprint("Descriptors: ");
    kprint_decimal(efi_analysis.num_descriptors);
    kprint("\n");
    kprint("Usable Regions: ");
    kprint_decimal(efi_analysis.num_usable_regions);
    kprint("\n");
    kprint("Largest Region: ");
    kprint_hex(efi_analysis.largest_region_addr);
    kprint(" (");
    kprint_decimal(efi_analysis.largest_region_size >> 20);
    kprint(" MB)\n");
    kprint("==========================\n");
}

/*
 * Validate memory layout for kernel requirements
 */
static int validate_memory_layout(void) {
    kprint("Validating memory layout for kernel requirements\n");

    /* Check minimum memory requirements */
    if (efi_analysis.usable_memory < (64 * 1024 * 1024)) {  /* 64MB minimum */
        kprint("ERROR: Insufficient usable memory (");
        kprint_decimal(efi_analysis.usable_memory >> 20);
        kprint(" MB < 64 MB)\n");
        return -1;
    }

    /* Check for reasonable number of regions */
    if (efi_analysis.num_usable_regions == 0) {
        kprint("ERROR: No usable memory regions found\n");
        return -1;
    }

    if (efi_analysis.num_descriptors > MAX_EFI_DESCRIPTORS) {
        kprint("WARNING: Too many EFI descriptors (");
        kprint_decimal(efi_analysis.num_descriptors);
        kprint(" > ");
        kprint_decimal(MAX_EFI_DESCRIPTORS);
        kprint(")\n");
    }

    /* Check largest region size */
    if (efi_analysis.largest_region_size < (16 * 1024 * 1024)) {  /* 16MB minimum */
        kprint("WARNING: Largest region is small (");
        kprint_decimal(efi_analysis.largest_region_size >> 20);
        kprint(" MB)\n");
    }

    kprint("Memory layout validation passed\n");
    return 0;
}

/* ========================================================================
 * MAIN INTERFACE FUNCTIONS
 * ======================================================================== */

/*
 * Parse EFI memory map from Multiboot2 information
 * Returns 0 on success, -1 on failure
 */
int parse_efi_memory_map_mb2(const void *mboot_info, uint32_t mboot_size) {
    if (!mboot_info || mboot_size < 8) {
        kernel_panic("Invalid Multiboot2 information for EFI parsing");
    }

    kprint("Parsing EFI memory map from Multiboot2\n");

    /* Initialize analysis structure */
    efi_analysis.total_memory = 0;
    efi_analysis.usable_memory = 0;
    efi_analysis.reserved_memory = 0;
    efi_analysis.num_descriptors = 0;
    efi_analysis.num_usable_regions = 0;
    efi_analysis.largest_region_size = 0;
    efi_analysis.largest_region_addr = 0;

    /* Search for EFI memory map tag in Multiboot2 structure */
    const uint8_t *ptr = (const uint8_t*)mboot_info + 8;  /* Skip total size and reserved */
    const uint8_t *end = (const uint8_t*)mboot_info + mboot_size;

    while (ptr < end) {
        const uint32_t *tag_header = (const uint32_t*)ptr;
        uint32_t tag_type = tag_header[0];
        uint32_t tag_size = tag_header[1];

        if (tag_type == MULTIBOOT_TAG_TYPE_END) {
            break;
        }

        if (tag_type == MULTIBOOT_TAG_TYPE_EFI_MMAP) {
            const multiboot_tag_efi_mmap_t *mmap_tag = (const multiboot_tag_efi_mmap_t*)ptr;

            if (parse_efi_memory_map(mmap_tag) != 0) {
                kernel_panic("Failed to parse EFI memory map");
            }

            /* Print analysis and validate */
            print_memory_analysis();

            if (validate_memory_layout() != 0) {
                kernel_panic("Memory layout validation failed");
            }

            return 0;
        }

        /* Move to next tag */
        ptr += (tag_size + 7) & ~7;  /* Round up to 8-byte boundary */
    }

    kprint("ERROR: EFI memory map not found in Multiboot2 structure\n");
    return -1;
}

/*
 * Get EFI memory analysis results
 */
void get_efi_memory_analysis(efi_memory_analysis_t *analysis) {
    if (analysis) {
        *analysis = efi_analysis;
    }
}

/*
 * Get available memory statistics
 */
void get_available_memory_stats(uint64_t *total_memory, uint64_t *usable_memory,
                                uint32_t *num_regions) {
    if (total_memory) {
        *total_memory = efi_analysis.total_memory;
    }
    if (usable_memory) {
        *usable_memory = efi_analysis.usable_memory;
    }
    if (num_regions) {
        *num_regions = efi_analysis.num_usable_regions;
    }
}
