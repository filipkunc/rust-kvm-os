ENTRY(_start)

SECTIONS
{
    . = 0x200000;

    .text : {
        *(.text._start)
        *(.text .text.*)
    }

    .rodata : ALIGN(4K) {
        *(.rodata .rodata.*)
    }

    .data : ALIGN(4K) {
        *(.data .data.*)
    }

    .bss (NOLOAD) : ALIGN(4K) {
        *(.bss .bss.*)
        *(COMMON)
    }

    /DISCARD/ : {
        *(.eh_frame .eh_frame_hdr)
        *(.comment)
    }
}
