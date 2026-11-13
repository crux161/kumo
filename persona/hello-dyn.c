/* SH7-c (PLAN/022 gate I-e): the first dynamically linked Linux program on KUMO. Built against
 * musl by scripts/persona-payloads.sh; persona-linux-shim loads it plus musl's own dynamic
 * loader (PT_INTERP) and ld-musl does the rest. — TEAL */
#include <stdio.h>

int main(int argc, char **argv)
{
    printf("SH7-c dynamic musl hello: argc=%d argv0=%s\n", argc, argv[0]);
    return 0;
}
