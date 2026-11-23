/* Higher-half kernel for the Limine boot protocol. */
OUTPUT_FORMAT(elf64-x86-64)
ENTRY(kmain)

PHDRS
{
    requests PT_LOAD FLAGS(6);   /* RW: Limine writes response pointers */
    text     PT_LOAD FLAGS(5);   /* RX */
    rodata   PT_LOAD FLAGS(4);   /* R  */
    data     PT_LOAD FLAGS(6);   /* RW, NX */
}

SECTIONS
{
    . = 0xffffffff80000000;

    .limine_requests : {
        KEEP(*(.limine_requests_start))
        KEEP(*(.limine_requests))
        KEEP(*(.limine_requests_end))
    } :requests

    . = ALIGN(CONSTANT(MAXPAGESIZE));
    .text : { *(.text .text.*) } :text

    . = ALIGN(CONSTANT(MAXPAGESIZE));
    .rodata : { *(.rodata .rodata.*) } :rodata

    . = ALIGN(CONSTANT(MAXPAGESIZE));
    .data : { *(.data .data.*) *(.data.rel.ro .data.rel.ro.*) *(.got .got.*) } :data
    .bss : { *(.bss .bss.*) *(COMMON) } :data

    /DISCARD/ : { *(.eh_frame*) *(.note .note.*) *(.comment) }
}
