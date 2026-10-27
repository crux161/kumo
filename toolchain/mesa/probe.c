/* SPDX-License-Identifier: MIT */
/*j643*/
#include <stdio.h>

extern unsigned kumo_mesa_bridge_proof(void);

int main(void)
{
    puts("G7b native Mesa bridge begin: modeled service");
    unsigned failed = kumo_mesa_bridge_proof();
    if (failed) {
        printf("G7b native Mesa bridge FAILED at case %u\n", failed);
        return 1;
    }
    puts("G7b native Mesa bridge green: pinned headers, C/Rust query, IDs, refusal, loss; modeled service");
    return 0;
}
