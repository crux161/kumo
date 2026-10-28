/* SPDX-License-Identifier: MIT */
/*j646*/
/*j647*/
/*j650*/
/*j651*/
/*j652*/
#include <stdio.h>

extern unsigned kumo_mesa_bridge_proof(void);

int main(void)
{
    puts("G7i texture array begin: modeled service");
    unsigned failed = kumo_mesa_bridge_proof();
    if (failed) {
        printf("G7i texture array FAILED at case %u\n", failed);
        return 1;
    }
    puts("G7i texture array green: four-word TEXTURE_FEATURES, absence gate, version guard; modeled service");
    return 0;
}
