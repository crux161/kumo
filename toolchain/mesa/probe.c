/* SPDX-License-Identifier: MIT */
/*j647*/
/*j650*/
/*j651*/
/*j652*/
/*j653*/
#include <stdio.h>

extern unsigned kumo_mesa_bridge_proof(void);

int main(void)
{
    puts("G7j native Mesa properties begin: modeled service");
    unsigned failed = kumo_mesa_bridge_proof();
    if (failed) {
        printf("G7j native Mesa properties FAILED at case %u\n", failed);
        return 1;
    }
    puts("G7j native Mesa properties green: upstream variant resolution, model match, refusal, loss; modeled service");
    return 0;
}
