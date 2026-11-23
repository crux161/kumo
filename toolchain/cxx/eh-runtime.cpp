//J936_r02_001
// relibc owns process-exit registration; this static image drains its registry. — KESTREL
extern "C" {
void *__dso_handle = &__dso_handle;
void __cxa_finalize(void *);
}
__attribute__((destructor)) static void finalize_static_image()
{
    __cxa_finalize(__dso_handle);
}
