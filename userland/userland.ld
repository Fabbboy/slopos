OUTPUT_FORMAT(elf64-x86-64)
OUTPUT_ARCH(i386:x86-64)
ENTRY(_start)

/* User programs start at PROCESS_CODE_START_VA */
USER_CODE_BASE = 0x0000000000400000;

SECTIONS
{
  . = USER_CODE_BASE;

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

