/* j740 */
/* Read the linker's actual GNU build ID from this static image. There is no
 * dynamic-loader lookup, filesystem identity or invented fallback ID. — SAKER */
#ifndef __kumo__
#error Static KUMO image only
#endif
#include <assert.h>
#include <elf.h>
#include <stdint.h>
#include <stddef.h>
#include <string.h>
#include "util/build_id.h"

extern const Elf64_Ehdr __ehdr_start;
_Static_assert(sizeof(Elf64_Word) == 4 && sizeof(Elf64_Xword) == 8, "ELF64 scalar ABI");
_Static_assert(sizeof(Elf64_Ehdr) == 64 && offsetof(Elf64_Ehdr, e_phoff) == 32, "ELF64 header ABI");
_Static_assert(sizeof(Elf64_Phdr) == 56 && sizeof(Elf64_Nhdr) == 12, "ELF64 program/note ABI");
struct build_id_note {
    Elf64_Nhdr header;
    char name[4];
    uint8_t data[];
};

static int mapped(const Elf64_Phdr *programs, unsigned count, uintptr_t start, size_t size)
{
    for (unsigned i = 0; i < count; ++i) {
        const Elf64_Phdr *p = &programs[i];
        if (p->p_type == PT_LOAD && start >= p->p_vaddr &&
            start - p->p_vaddr <= p->p_memsz && size <= p->p_memsz - (start - p->p_vaddr))
            return 1;
    }
    return 0;
}

const struct build_id_note *build_id_find_nhdr_for_addr(const void *address)
{
    const Elf64_Ehdr *elf = &__ehdr_start;
    if (memcmp(elf->e_ident, ELFMAG, SELFMAG) || elf->e_type != ET_EXEC ||
        elf->e_ident[EI_CLASS] != ELFCLASS64 || elf->e_phentsize != sizeof(Elf64_Phdr) ||
        elf->e_phoff > 4096 || elf->e_phnum > 64)
        return NULL;
    const Elf64_Phdr *programs = (const void *)((const char *)elf + elf->e_phoff);
    if (!mapped(programs, elf->e_phnum, (uintptr_t)address, 1))
        return NULL;
    for (unsigned i = 0; i < elf->e_phnum; ++i) {
        const Elf64_Phdr *p = &programs[i];
        if (p->p_type != PT_NOTE || !mapped(programs, elf->e_phnum, p->p_vaddr, p->p_filesz))
            continue;
        const uint8_t *cursor = (const void *)(uintptr_t)p->p_vaddr;
        size_t left = p->p_filesz;
        while (left >= sizeof(Elf64_Nhdr)) {
            const Elf64_Nhdr *header = (const void *)cursor;
            uint64_t name_size = ((uint64_t)header->n_namesz + 3) & ~UINT64_C(3);
            uint64_t data_size = ((uint64_t)header->n_descsz + 3) & ~UINT64_C(3);
            uint64_t total = sizeof(*header) + name_size + data_size;
            if (total > left) break;
            if (header->n_type == NT_GNU_BUILD_ID && header->n_namesz == 4 &&
                header->n_descsz > 0 && header->n_descsz <= BLAKE3_KEY_LEN &&
                !memcmp(cursor + sizeof(*header), "GNU\0", 4))
                return (const void *)cursor;
            cursor += total;
            left -= total;
        }
    }
    return NULL;
}

unsigned build_id_length(const struct build_id_note *note)
{
    assert(note);
    return note->header.n_descsz;
}

const uint8_t *build_id_data(const struct build_id_note *note)
{
    assert(note);
    return note->data;
}

void copy_build_id_to_sha1(uint8_t sha1[BLAKE3_KEY_LEN], const struct build_id_note *note)
{
    unsigned size = build_id_length(note);
    assert(size <= BLAKE3_KEY_LEN);
    memcpy(sha1, build_id_data(note), size);
    memset(sha1 + size, 0, BLAKE3_KEY_LEN - size);
}
