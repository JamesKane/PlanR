/* User programs (ADR 0002's start-up): static, text and read-only data
 * from TEXT_BASE, then data and bss on pages of their own. `end` is the
 * end of the bss, where `brk` starts. */

ENTRY(_start)

PHDRS
{
    text PT_LOAD FLAGS(5);  /* R X */
    data PT_LOAD FLAGS(6);  /* R W */
}

SECTIONS
{
    . = 0x400000;
    .text : { *(.text._start) *(.text .text.*) } :text
    .rodata : { *(.rodata .rodata.*) } :text
    . = ALIGN(4096);
    .data : { *(.data .data.*) *(.got .got.*) } :data
    .bss : { *(.bss .bss.*) *(COMMON) } :data
    end = .;
    /DISCARD/ : { *(.eh_frame*) *(.note*) *(.comment) }
}
