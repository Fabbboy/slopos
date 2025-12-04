/*
 * SlopOS Multiboot2 Parser with Strict Validation
 * CRITICAL: Panics on invalid Multiboot2 data - cannot proceed without valid EFI memory maps
 * Implements rigorous validation as required for kernel stability
 */

#include <stdint.h>
#include <stddef.h>
#include "../boot/constants.h"
#include "../boot/kernel_panic.h"
#include "../drivers/serial.h"

/* ========================================================================
 * MULTIBOOT2 STRUCTURES WITH STRICT VALIDATION
 * ======================================================================== */

/* Basic Multiboot2 tag structure */
struct multiboot_tag {
    uint32_t type;
    uint32_t size;
} __attribute__((packed));

/* Multiboot2 string tag */
struct multiboot_tag_string {
    uint32_t type;
    uint32_t size;
    char string[];
} __attribute__((packed));

/* Basic memory info tag */
struct multiboot_tag_basic_meminfo {
    uint32_t type;
    uint32_t size;
    uint32_t mem_lower;
    uint32_t mem_upper;
} __attribute__((packed));

/* Memory map entry */
struct multiboot_mmap_entry {
    uint64_t addr;
    uint64_t len;
    uint32_t type;
    uint32_t zero;
} __attribute__((packed));

/* Memory map tag */
struct multiboot_tag_mmap {
    uint32_t type;
    uint32_t size;
    uint32_t entry_size;
    uint32_t entry_version;
    struct multiboot_mmap_entry entries[];
} __attribute__((packed));

/* Framebuffer tag */
struct multiboot_tag_framebuffer_common {
    uint32_t type;
    uint32_t size;
    uint64_t framebuffer_addr;
    uint32_t framebuffer_pitch;
    uint32_t framebuffer_width;
    uint32_t framebuffer_height;
    uint8_t framebuffer_bpp;
    uint8_t framebuffer_type;
    uint16_t reserved;
} __attribute__((packed));

/* EFI64 system table tag */
struct multiboot_tag_efi64 {
    uint32_t type;
    uint32_t size;
    uint64_t pointer;
} __attribute__((packed));

/* EFI memory map tag */
struct multiboot_tag_efi_mmap {
    uint32_t type;
    uint32_t size;
    uint32_t descr_size;
    uint32_t descr_vers;
    uint8_t efi_mmap[];
} __attribute__((packed));

/* EFI Memory Descriptor */
typedef struct {
    uint32_t type;
    uint64_t phys_start;
    uint64_t virt_start;
    uint64_t num_pages;
    uint64_t attribute;
} efi_memory_descriptor_t;

/* ========================================================================
 * VALIDATED SYSTEM INFORMATION STORAGE
 * ======================================================================== */

/* System information with validation flags */
typedef struct {
    /* Memory information */
    uint64_t total_memory;
    uint64_t available_memory;
    uint64_t lower_memory;
    uint64_t upper_memory;

    /* EFI information */
    uint64_t efi_system_table;
    uint32_t efi_map_descriptor_size;
    uint32_t efi_map_descriptor_version;
    uint8_t *efi_memory_map;
    uint32_t efi_memory_map_size;

    /* Framebuffer information */
    uint64_t framebuffer_addr;
    uint32_t framebuffer_width;
    uint32_t framebuffer_height;
    uint32_t framebuffer_pitch;
    uint8_t framebuffer_bpp;
    uint8_t framebuffer_type;

    /* Validation flags - CRITICAL for operation */
    uint8_t has_valid_memory_map;
    uint8_t has_valid_efi_map;
    uint8_t has_valid_efi_system_table;
    uint8_t has_valid_framebuffer;
    uint8_t basic_info_validated;
} validated_system_info_t;

/* Global validated system information */
static validated_system_info_t system_info = {0};

/* ========================================================================
 * CRITICAL VALIDATION FUNCTIONS
 * ======================================================================== */

/*
 * Validate Multiboot2 header structure
 * PANICS if basic structure is invalid
 */
static void validate_multiboot2_header(uint64_t multiboot_info_addr) {
    if (multiboot_info_addr == 0) {
        kernel_panic("CRITICAL: Multiboot2 info address is NULL - cannot proceed");
    }

    /* Check alignment */
    if (multiboot_info_addr & 0x7) {
        kernel_panic("CRITICAL: Multiboot2 info not 8-byte aligned");
    }

    uint32_t *info = (uint32_t*)multiboot_info_addr;
    uint32_t total_size = info[0];
    uint32_t reserved = info[1];

    /* Validate basic structure */
    if (total_size < 8) {
        kernel_panic("CRITICAL: Multiboot2 info structure too small");
    }

    if (total_size > 64 * 1024) {  /* Sanity check - 64KB max */
        kernel_panic("CRITICAL: Multiboot2 info structure suspiciously large");
    }

    if (reserved != 0) {
        kprint("WARNING: Multiboot2 reserved field not zero: ");
        kprint_hex(reserved);
        kprintln("");
    }

    kprint("Multiboot2 header validated, size: ");
    kprint_decimal(total_size);
    kprintln(" bytes");
}

/*
 * Validate memory map entry
 * Returns 0 if invalid, 1 if valid
 */
static int validate_memory_map_entry(const struct multiboot_mmap_entry *entry) {
    /* Check for null entry */
    if (!entry) {
        return 0;
    }

    /* Check for zero-length regions */
    if (entry->len == 0) {
        return 0;
    }

    /* Check for overflow */
    if (entry->addr + entry->len < entry->addr) {
        return 0;
    }

    /* Validate memory type */
    if (entry->type > MULTIBOOT_MEMORY_BADRAM) {
        return 0;
    }

    /* Check alignment (should be page-aligned) */
    if (entry->addr & (PAGE_SIZE_4KB - 1)) {
        kprint("WARNING: Memory region not page-aligned: ");
        kprint_hex(entry->addr);
        kprintln("");
    }

    return 1;
}

/*
 * Validate EFI memory descriptor
 */
static int validate_efi_memory_descriptor(const efi_memory_descriptor_t *desc) {
    if (!desc) {
        return 0;
    }

    /* Check for zero pages */
    if (desc->num_pages == 0) {
        return 0;
    }

    /* Check for overflow */
    uint64_t end_addr = desc->phys_start + (desc->num_pages * EFI_PAGE_SIZE);
    if (end_addr < desc->phys_start) {
        return 0;
    }

    /* EFI memory types should be reasonable */
    if (desc->type > 15) {  /* EFI spec defines types 0-15 */
        return 0;
    }

    return 1;
}

/* ========================================================================
 * TAG PARSING WITH VALIDATION
 * ======================================================================== */

/*
 * Parse basic memory info with validation
 */
static void parse_and_validate_basic_meminfo(const struct multiboot_tag_basic_meminfo *tag) {
    if (!tag) {
        kernel_panic("CRITICAL: Basic memory info tag is NULL");
    }

    if (tag->size < sizeof(*tag)) {
        kernel_panic("CRITICAL: Basic memory info tag too small");
    }

    /* Validate memory amounts are reasonable */
    if (tag->mem_lower > 1024) {  /* Should be <= 1MB */
        kprint("WARNING: Lower memory suspiciously large: ");
        kprint_decimal(tag->mem_lower);
        kprintln(" KB");
    }

    if (tag->mem_upper == 0) {
        kernel_panic("CRITICAL: No upper memory reported - system unusable");
    }

    if (tag->mem_upper < 1024) {  /* Should have at least 1MB upper memory */
        kernel_panic("CRITICAL: Insufficient upper memory for kernel operation");
    }

    /* Store validated information */
    system_info.lower_memory = (uint64_t)tag->mem_lower * 1024;
    system_info.upper_memory = (uint64_t)tag->mem_upper * 1024;
    system_info.total_memory = system_info.lower_memory + system_info.upper_memory + (1024 * 1024);
    system_info.basic_info_validated = 1;

    kprint("Basic memory validated - Lower: ");
    kprint_decimal(tag->mem_lower);
    kprint(" KB, Upper: ");
    kprint_decimal(tag->mem_upper);
    kprintln(" KB");
}

/*
 * Parse memory map with strict validation
 */
static void parse_and_validate_memory_map(const struct multiboot_tag_mmap *tag) {
    if (!tag) {
        kernel_panic("CRITICAL: Memory map tag is NULL");
    }

    if (tag->size < sizeof(*tag)) {
        kernel_panic("CRITICAL: Memory map tag too small");
    }

    if (tag->entry_size == 0 || tag->entry_size > 64) {
        kernel_panic("CRITICAL: Invalid memory map entry size");
    }

    const struct multiboot_mmap_entry *entry;
    uint64_t available_memory = 0;
    int valid_entries = 0;
    int available_entries = 0;

    for (entry = tag->entries;
         (uint8_t*)entry < (uint8_t*)tag + tag->size;
         entry = (struct multiboot_mmap_entry*)((uint64_t)entry + tag->entry_size)) {

        if (!validate_memory_map_entry(entry)) {
            kprint("WARNING: Invalid memory map entry at ");
            kprint_hex((uint64_t)entry);
            kprintln("");
            continue;
        }

        valid_entries++;

        if (entry->type == MULTIBOOT_MEMORY_AVAILABLE) {
            available_memory += entry->len;
            available_entries++;
        }
    }

    /* CRITICAL: Must have at least some available memory */
    if (available_entries == 0) {
        kernel_panic("CRITICAL: No available memory regions found - system unusable");
    }

    if (available_memory < 16 * 1024 * 1024) {  /* Need at least 16MB */
        kernel_panic("CRITICAL: Insufficient available memory for kernel operation");
    }

    system_info.available_memory = available_memory;
    system_info.has_valid_memory_map = 1;

    kprint("Memory map validated - ");
    kprint_decimal(valid_entries);
    kprint(" valid entries, ");
    kprint_decimal(available_entries);
    kprint(" available regions, ");
    kprint_decimal(available_memory / (1024 * 1024));
    kprintln(" MB available");
}

/*
 * Parse EFI memory map with validation
 */
static void parse_and_validate_efi_memory_map(const struct multiboot_tag_efi_mmap *tag) {
    if (!tag) {
        kprintln("WARNING: EFI memory map tag is NULL");
        return;
    }

    if (tag->size < sizeof(*tag)) {
        kprintln("WARNING: EFI memory map tag too small");
        return;
    }

    if (tag->descr_size == 0 || tag->descr_size > 256) {
        kprintln("WARNING: Invalid EFI memory descriptor size");
        return;
    }

    uint32_t desc_count = (tag->size - sizeof(*tag)) / tag->descr_size;
    if (desc_count == 0) {
        kprintln("WARNING: No EFI memory descriptors found");
        return;
    }

    uint64_t available_memory = 0;
    int valid_descriptors = 0;

    for (uint32_t i = 0; i < desc_count; i++) {
        const efi_memory_descriptor_t *desc =
            (const efi_memory_descriptor_t*)((uint8_t*)tag->efi_mmap + i * tag->descr_size);

        if (!validate_efi_memory_descriptor(desc)) {
            continue;
        }

        valid_descriptors++;

        /* EFI conventional memory type */
        if (desc->type == EFI_CONVENTIONAL_MEMORY) {
            available_memory += desc->num_pages * EFI_PAGE_SIZE;
        }
    }

    /* Update available memory if EFI provides more accurate information */
    if (available_memory > system_info.available_memory) {
        system_info.available_memory = available_memory;
    }

    system_info.efi_memory_map = (uint8_t*)tag->efi_mmap;
    system_info.efi_memory_map_size = tag->size - sizeof(*tag);
    system_info.efi_map_descriptor_size = tag->descr_size;
    system_info.efi_map_descriptor_version = tag->descr_vers;
    system_info.has_valid_efi_map = 1;

    kprint("EFI memory map validated - ");
    kprint_decimal(valid_descriptors);
    kprint(" valid descriptors");
    kprintln("");
}

/*
 * Parse framebuffer info with validation
 */
static void parse_and_validate_framebuffer(const struct multiboot_tag_framebuffer_common *tag) {
    if (!tag) {
        kprintln("WARNING: Framebuffer tag is NULL");
        return;
    }

    if (tag->size < sizeof(*tag)) {
        kprintln("WARNING: Framebuffer tag too small");
        return;
    }

    /* Validate framebuffer parameters */
    if (tag->framebuffer_addr == 0) {
        kprintln("WARNING: Framebuffer address is zero");
        return;
    }

    if (tag->framebuffer_width == 0 || tag->framebuffer_height == 0) {
        kprintln("WARNING: Invalid framebuffer dimensions");
        return;
    }

    if (tag->framebuffer_bpp == 0 || tag->framebuffer_bpp > 32) {
        kprintln("WARNING: Invalid framebuffer bit depth");
        return;
    }

    /* Store validated framebuffer info */
    system_info.framebuffer_addr = tag->framebuffer_addr;
    system_info.framebuffer_width = tag->framebuffer_width;
    system_info.framebuffer_height = tag->framebuffer_height;
    system_info.framebuffer_pitch = tag->framebuffer_pitch;
    system_info.framebuffer_bpp = tag->framebuffer_bpp;
    system_info.framebuffer_type = tag->framebuffer_type;
    system_info.has_valid_framebuffer = 1;

    kprint("Framebuffer validated - ");
    kprint_decimal(tag->framebuffer_width);
    kprint("x");
    kprint_decimal(tag->framebuffer_height);
    kprint("x");
    kprint_decimal(tag->framebuffer_bpp);
    kprintln("");
}

/*
 * Parse EFI64 system table with validation
 */
static void parse_and_validate_efi64(const struct multiboot_tag_efi64 *tag) {
    if (!tag) {
        kprintln("WARNING: EFI64 tag is NULL");
        return;
    }

    if (tag->size < sizeof(*tag)) {
        kprintln("WARNING: EFI64 tag too small");
        return;
    }

    if (tag->pointer == 0) {
        kprintln("WARNING: EFI system table pointer is NULL");
        return;
    }

    system_info.efi_system_table = tag->pointer;
    system_info.has_valid_efi_system_table = 1;

    kprint("EFI system table validated at ");
    kprint_hex(tag->pointer);
    kprintln("");
}

/* ========================================================================
 * MAIN PARSING FUNCTION WITH CRITICAL VALIDATION
 * ======================================================================== */

/*
 * Parse Multiboot2 information with strict validation
 * PANICS on critical failures that prevent kernel operation
 */
void parse_multiboot2_info_strict(uint64_t multiboot_info_addr) {
    kprintln("Starting strict Multiboot2 validation...");

    /* CRITICAL: Validate basic structure first */
    validate_multiboot2_header(multiboot_info_addr);

    uint32_t *info = (uint32_t*)multiboot_info_addr;
    uint32_t total_size = info[0];

    /* Parse all tags with validation */
    struct multiboot_tag *tag = (struct multiboot_tag*)(info + 2);
    uint64_t addr = (uint64_t)tag;
    uint64_t end_addr = multiboot_info_addr + total_size;

    int tags_processed = 0;
    int basic_meminfo_found = 0;
    int memory_map_found = 0;

    while (addr < end_addr) {
        tag = (struct multiboot_tag*)addr;

        /* Validate tag structure */
        if (addr + sizeof(*tag) > end_addr) {
            kernel_panic("CRITICAL: Malformed Multiboot2 tag extends beyond structure");
        }

        if (tag->size < sizeof(*tag)) {
            kernel_panic("CRITICAL: Multiboot2 tag size too small");
        }

        if (addr + tag->size > end_addr) {
            kernel_panic("CRITICAL: Multiboot2 tag extends beyond structure");
        }

        if (tag->type == MULTIBOOT_TAG_TYPE_END) {
            break;
        }

        switch (tag->type) {
            case MULTIBOOT_TAG_TYPE_BASIC_MEMINFO:
                parse_and_validate_basic_meminfo((const struct multiboot_tag_basic_meminfo*)tag);
                basic_meminfo_found = 1;
                break;

            case MULTIBOOT_TAG_TYPE_MMAP:
                parse_and_validate_memory_map((const struct multiboot_tag_mmap*)tag);
                memory_map_found = 1;
                break;

            case MULTIBOOT_TAG_TYPE_FRAMEBUFFER:
                parse_and_validate_framebuffer((const struct multiboot_tag_framebuffer_common*)tag);
                break;

            case MULTIBOOT_TAG_TYPE_EFI64:
                parse_and_validate_efi64((const struct multiboot_tag_efi64*)tag);
                break;

            case MULTIBOOT_TAG_TYPE_EFI_MMAP:
                parse_and_validate_efi_memory_map((const struct multiboot_tag_efi_mmap*)tag);
                break;

            default:
                /* Unknown tag - log but continue */
                kprint("Unknown Multiboot2 tag type: ");
                kprint_decimal(tag->type);
                kprintln("");
                break;
        }

        tags_processed++;

        /* Move to next tag (aligned to 8-byte boundary) */
        addr = (addr + tag->size + 7) & ~7ULL;
    }

    /* CRITICAL VALIDATION: Must have essential information */
    if (!basic_meminfo_found && !memory_map_found) {
        kernel_panic("CRITICAL: No memory information found - cannot proceed");
    }

    if (!system_info.has_valid_memory_map && !system_info.basic_info_validated) {
        kernel_panic("CRITICAL: No valid memory information - cannot proceed");
    }

    if (system_info.available_memory < 8 * 1024 * 1024) {  /* Need at least 8MB */
        kernel_panic("CRITICAL: Insufficient memory for kernel operation");
    }

    kprint("Multiboot2 validation complete - ");
    kprint_decimal(tags_processed);
    kprintln(" tags processed");

    /* Report validation status */
    kprint("Validation status: Memory=");
    kprint(system_info.has_valid_memory_map ? "OK" : "BASIC");
    kprint(", EFI=");
    kprint(system_info.has_valid_efi_map ? "OK" : "NO");
    kprint(", FB=");
    kprint(system_info.has_valid_framebuffer ? "OK" : "NO");
    kprintln("");
}

/* ========================================================================
 * SYSTEM INFORMATION ACCESS FUNCTIONS
 * ======================================================================== */

/*
 * Get total available memory (validated)
 */
uint64_t get_validated_available_memory(void) {
    if (!system_info.has_valid_memory_map && !system_info.basic_info_validated) {
        kernel_panic("CRITICAL: Attempted to access unvalidated memory information");
    }
    return system_info.available_memory;
}

/*
 * Get framebuffer information (validated)
 */
int get_validated_framebuffer_info(uint64_t *addr, uint32_t *width, uint32_t *height,
                                  uint32_t *pitch, uint8_t *bpp) {
    if (!system_info.has_valid_framebuffer) {
        return 0;
    }

    if (addr) *addr = system_info.framebuffer_addr;
    if (width) *width = system_info.framebuffer_width;
    if (height) *height = system_info.framebuffer_height;
    if (pitch) *pitch = system_info.framebuffer_pitch;
    if (bpp) *bpp = system_info.framebuffer_bpp;

    return 1;
}

/*
 * Get EFI system table (validated)
 */
uint64_t get_validated_efi_system_table(void) {
    if (!system_info.has_valid_efi_system_table) {
        return 0;
    }
    return system_info.efi_system_table;
}

/*
 * Check if we have valid EFI memory map
 */
int has_valid_efi_memory_map(void) {
    return system_info.has_valid_efi_map;
}

/*
 * Get validation status summary
 */
void print_validation_status(void) {
    kprintln("=== Multiboot2 Validation Status ===");
    kprint("Memory Map:      ");
    kprintln(system_info.has_valid_memory_map ? "VALID" : "INVALID");
    kprint("EFI Memory Map:  ");
    kprintln(system_info.has_valid_efi_map ? "VALID" : "INVALID");
    kprint("EFI System Table:");
    kprintln(system_info.has_valid_efi_system_table ? "VALID" : "INVALID");
    kprint("Framebuffer:     ");
    kprintln(system_info.has_valid_framebuffer ? "VALID" : "INVALID");
    kprint("Available Memory:");
    kprint_decimal(system_info.available_memory / (1024 * 1024));
    kprintln(" MB");
}