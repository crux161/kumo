/* SH7-d2 (PLAN/022): a shared library musl must find in /lib on its own (NEEDED libgreet.so). */
const char *greet(void)
{
    return "hello from libgreet.so";
}
