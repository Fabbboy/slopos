OUTPUT_FORMAT(elf64-x86-64)
OUTPUT_ARCH(i386:x86-64)
ENTRY(_start)

/* Limine loads kernel at higher half virtual address */
/* Physical memory is identity-mapped by Limine */
KERNEL_VIRT_BASE = 0xFFFFFFFF80000000;

PHDRS
{
  text PT_LOAD FLAGS(5);  /* R-X (read + execute) */
  rodata PT_LOAD FLAGS(4);  /* R-- (read only) */
  data PT_LOAD FLAGS(6);  /* RW- (read + write) */
}

SECTIONS
{
  /* Kernel starts at higher half virtual address */
  . = KERNEL_VIRT_BASE;
  __executable_start = .;

  /* Limine requests section - must be in first LOAD segment */
  .limine_requests ALIGN(4096) : {
    KEEP(*(.limine_requests_start_marker))
    KEEP(*(.limine_requests))
    KEEP(*(.limine_requests_end_marker))
  } :text

  /* Code section */
  .text ALIGN(4096) : {
    _text_start = .;
    *(.text .text.*)
    _text_end = .;
  } :text
  __etext = _text_end;

  /* Kernel start symbol (alias for start of text) */
  _kernel_start = _text_start;

  .rodata ALIGN(4096) : {
    _rodata_start = .;
    *(.rodata .rodata.*)
    _rodata_end = .;
  } :rodata

  /* Binary-search index over .eh_frame, consumed by the unwinder's
     fde-gnu-eh-frame-hdr finder. Emitted only when the target spec sets
     "eh-frame-header": true; the ASSERT turns a missing index into a link
     failure rather than a silent fall back to a linear .eh_frame scan. */
  .eh_frame_hdr ALIGN(8) : {
    __GNU_EH_FRAME_HDR = .;
    KEEP(*(.eh_frame_hdr))
  } :rodata
  ASSERT(SIZEOF(.eh_frame_hdr) >= 12,
    "link.ld: .eh_frame_hdr is empty - set eh-frame-header true in targets/x86_64-slos.json")

  .eh_frame ALIGN(8) : {
    __eh_frame = .;
    KEEP(*(.eh_frame))
    KEEP(*(.eh_frame.*))
  } :rodata

  .gcc_except_table ALIGN(8) : {
    KEEP(*(.gcc_except_table))
    KEEP(*(.gcc_except_table.*))
  } :rodata

  .boot_init_early_hw ALIGN(8) : {
    __start_boot_init_early_hw = .;
    KEEP(*(.boot_init_early_hw))
    __stop_boot_init_early_hw = .;
  } :rodata

  .boot_init_memory ALIGN(8) : {
    __start_boot_init_memory = .;
    KEEP(*(.boot_init_memory))
    __stop_boot_init_memory = .;
  } :rodata

  .boot_init_drivers ALIGN(8) : {
    __start_boot_init_drivers = .;
    KEEP(*(.boot_init_drivers))
    __stop_boot_init_drivers = .;
  } :rodata

  .boot_init_services ALIGN(8) : {
    __start_boot_init_services = .;
    KEEP(*(.boot_init_services))
    __stop_boot_init_services = .;
  } :rodata

  .boot_init_optional ALIGN(8) : {
    __start_boot_init_optional = .;
    KEEP(*(.boot_init_optional))
    __stop_boot_init_optional = .;
  } :rodata

  .driver_registry ALIGN(8) : {
    __start_driver_registry = .;
    KEEP(*(.driver_registry))
    __stop_driver_registry = .;
  } :rodata

  .platform_driver_registry ALIGN(8) : {
    __start_platform_driver_registry = .;
    KEEP(*(.platform_driver_registry))
    __stop_platform_driver_registry = .;
  } :rodata

  .test_registry ALIGN(8) : {
    __start_test_registry = .;
    KEEP(*(.test_registry))
    __stop_test_registry = .;
  } :rodata

  .hermetic_state_registry ALIGN(8) : {
    __start_hermetic_state_registry = .;
    KEEP(*(.hermetic_state_registry))
    __stop_hermetic_state_registry = .;
  } :rodata

  .data ALIGN(4096) : {
    _data_start = .;
    *(.data .data.*)
    _data_end = .;
  } :data

  .bss (NOLOAD) : ALIGN(4096) {
    _bss_start = .;
    *(.bss .bss.*)
    *(COMMON)
    _bss_end = .;
  } :data

  /* Early page tables at fixed addresses */
  . = ALIGN(0x1000);
  early_pml4 = .;
  . = . + 0x1000;  /* 4KB for PML4 */
  early_pdpt = .;
  . = . + 0x1000;  /* 4KB for PDPT */
  early_pd_identity = .;
  . = . + 0x1000;  /* 4KB for identity mapping PD */
  early_pd_kernel = .;
  . = . + 0x1000;  /* 4KB for higher-half kernel PD */

  /* Kernel end symbol */
  _kernel_end = .;

  /DISCARD/ : {
    *(.comment*)
    *(.note*)
    *(.interp)
    *(.dynamic)
    *(.dynsym)
    *(.dynstr)
    *(.gnu*)
  }
}
