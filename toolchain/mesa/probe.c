/* SPDX-License-Identifier: MIT */
/*j650*/
/*j651*/
/*j652*/
/*j653*/
/*j742*/
#include <stdio.h>

extern unsigned kumo_mesa_bridge_proof(void);

int main(void)
{
    puts("G7o-a native Mesa properties begin: modeled service");
    unsigned failed = kumo_mesa_bridge_proof();
    if (failed) {
        printf("G7o-a native Mesa properties FAILED at case %u\n", failed);
        return 1;
    }
    puts("G7o-a native Mesa properties green: complete props, upstream L2 and bus derivations, refusal, loss; modeled service");
    return 0;
}
