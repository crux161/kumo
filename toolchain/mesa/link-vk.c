/* j735 */
/* G7k link root only; this ELF is never executed. */
extern void vk_icdGetInstanceProcAddr(void);
static void (*volatile root)(void) = vk_icdGetInstanceProcAddr;
int main(void)
{
    return root == 0;
}
