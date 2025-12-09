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

  /* User-executable text (mapped U/S=1 for ring3 tasks) */
  .user_text ALIGN(4096) : {
    _user_text_start = .;
    *(.user_text .user_text.*)
    _user_text_end = .;
  } :text

  /* Kernel start symbol (alias for start of text) */
  _kernel_start = _text_start;

  .rodata ALIGN(4096) : {
    _rodata_start = .;
    *(.rodata .rodata.*)
    _rodata_end = .;
  } :rodata

  /* User read-only data */
  .user_rodata ALIGN(4096) : {
    _user_rodata_start = .;
    *(.user_rodata .user_rodata.*)
    _user_rodata_end = .;
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

  .data ALIGN(4096) : {
    _data_start = .;
    *(.data .data.*)
    _data_end = .;
  } :data

  /* User writable globals */
  .user_data ALIGN(4096) : {
    _user_data_start = .;
    *(.user_data .user_data.*)
    _user_data_end = .;
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
    *(.eh_frame*)
    *(.eh_frame_hdr)
    *(.interp)
    *(.dynamic)
    *(.dynsym)
    *(.dynstr)
    *(.gnu*)
  }
}
