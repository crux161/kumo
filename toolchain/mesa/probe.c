/* SPDX-License-Identifier: MIT */
/*j645*/
/*j646*/
/*j647*/
/*j650*/
/*j651*/
#include <stdio.h>

extern unsigned kumo_mesa_bridge_proof(void);

int main(void)
{
    puts("G7h native Mesa properties begin: modeled service");
    unsigned failed = kumo_mesa_bridge_proof();
    if (failed) {
        printf("G7h native Mesa properties FAILED at case %u\n", failed);
        return 1;
    }
    puts("G7h native Mesa properties green: upstream thread derivation without the nonzero assert, absence, contradiction, variant resolution, refusal, loss; modeled service");
    return 0;
}
