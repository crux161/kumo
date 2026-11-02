/* j735 */
/* G7k link roots only. Addresses are retained; no Mesa entry point is called.
 * These generic function types carry no runtime ABI claim. */
extern void panfrost_create_screen(void);
extern void st_api_create_context(void);
extern void _mesa_glapi_get_dispatch(void);
extern void glClear(void);
static void (*volatile roots[])(void) = {
    panfrost_create_screen, st_api_create_context, _mesa_glapi_get_dispatch, glClear,
};
int main(void)
{
    int missing = 0;
    for (unsigned i = 0; i < sizeof(roots) / sizeof(roots[0]); ++i)
        missing |= roots[i] == 0;
    return missing;
}
