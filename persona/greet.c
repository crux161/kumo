/* SH7-d2 (PLAN/022): a program with a NEEDED library besides libc. musl's loader opens
 * /lib/libgreet.so through openat + read + fstat + mmap on KUMO's file surface. — TEAL */
#include <stdio.h>

const char *greet(void);

int main(void)
{
    printf("greet: %s\n", greet());
    return 0;
}
