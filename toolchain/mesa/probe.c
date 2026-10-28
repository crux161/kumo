/* SPDX-License-Identifier: MIT */
/*j643*/
/*j644*/
#include <stdio.h>

extern unsigned kumo_mesa_bridge_proof(void);

int main(void)
{
    puts("G7c native Mesa topology begin: modeled service");
    unsigned failed = kumo_mesa_bridge_proof();
    if (failed) {
        printf("G7c native Mesa topology FAILED at case %u\n", failed);
        return 1;
    }
    puts("G7c native Mesa topology green: upstream core count/range, sparse64, IDs, refusal, loss; modeled service");
    return 0;
}
