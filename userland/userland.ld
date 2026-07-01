OUTPUT_FORMAT(elf64-x86-64)
OUTPUT_ARCH(i386:x86-64)
ENTRY(_start)

/* User programs start at PROCESS_CODE_START_VA */
USER_CODE_BASE = 0x0000000000400000;

SECTIONS
{
  /* Reserve room for the ELF header + program headers at the start of the
   * first PT_LOAD so the loader can map them and expose a valid AT_PHDR
   * (libc walks AT_PHDR -> PT_TLS to set up thread-local storage). Without
   * this, the phdrs fall outside every loadable segment and AT_PHDR is 0. */
  . = USER_CODE_BASE + SIZEOF_HEADERS;

  .text : {
    *(.text .text.*)
  }

  .rodata : {
    *(.rodata .rodata.*)
  }

  .data : {
    *(.data .data.*)
  }

  .bss : {
    *(.bss .bss.*)
    *(COMMON)
  }

  /* Preserve relocation sections for runtime relocation */
  .rela.text : {
    *(.rela.text .rela.text.*)
  }

  .rela.rodata : {
    *(.rela.rodata .rela.rodata.*)
  }

  .rela.data : {
    *(.rela.data .rela.data.*)
  }

  /DISCARD/ : {
    *(.comment*)
    *(.note*)
    *(.eh_frame*)
    *(.eh_frame_hdr)
    *(.interp)
  }
}
