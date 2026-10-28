/* SPDX-License-Identifier: MIT */
/*j643*/
/*j644*/
/*j645*/
/*j646*/
/*j647*/
#include <stdio.h>

extern unsigned kumo_mesa_bridge_proof(void);

int main(void)
{
    puts("G7f native Mesa properties begin: modeled service");
    unsigned failed = kumo_mesa_bridge_proof();
    if (failed) {
        printf("G7f native Mesa properties FAILED at case %u\n", failed);
        return 1;
    }
    puts("G7f native Mesa properties green: upstream topology, VA-range, tiler and texture derivation, refusal, loss; modeled service");
    return 0;
}
