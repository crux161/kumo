/* SPDX-License-Identifier: MIT */
/*j643*/
/*j644*/
/*j645*/
/*j646*/
/*j647*/
/*j650*/
#include <stdio.h>

extern unsigned kumo_mesa_bridge_proof(void);

int main(void)
{
    puts("G7g native Mesa properties begin: modeled service");
    unsigned failed = kumo_mesa_bridge_proof();
    if (failed) {
        printf("G7g native Mesa properties FAILED at case %u\n", failed);
        return 1;
    }
    puts("G7g native Mesa properties green: upstream variant resolution, model match, refusal, loss; modeled service");
    return 0;
}
