//j736
// No-exceptions allocation and failure hooks over KUMO's real relibc. — SAKER
#include <__config>
#include <__system_error/throw_system_error.h>
#include <__verbose_abort>
#include <new>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>

static void *allocate(size_t size)
{
    void *result = malloc(size ? size : 1);
    if (!result)
        std::__libcpp_verbose_abort("KUMO C++ allocation failed (%zu bytes)", size);
    return result;
}

void *operator new(size_t size) { return allocate(size); }
void *operator new[](size_t size) { return allocate(size); }
void operator delete(void *pointer) noexcept { free(pointer); }
void operator delete[](void *pointer) noexcept { free(pointer); }
void operator delete(void *pointer, size_t) noexcept { free(pointer); }
void operator delete[](void *pointer, size_t) noexcept { free(pointer); }

_LIBCPP_BEGIN_NAMESPACE_STD
[[noreturn]] void __libcpp_verbose_abort(const char *format, ...) noexcept
{
    va_list args;
    va_start(args, format);
    vfprintf(stderr, format, args);
    va_end(args);
    fputc('\n', stderr);
    abort();
}

[[noreturn]] void __throw_system_error(int error, const char *message)
{
    __libcpp_verbose_abort("KUMO C++: %s (errno %d)", message, error);
}
_LIBCPP_END_NAMESPACE_STD

extern "C" {
// relibc owns __cxa_atexit and its registry. This static image supplies the
// crtbegin-style fini hook that drains that registry at exit. — SAKER
void *__dso_handle = &__dso_handle;
void __cxa_finalize(void *dso);

[[noreturn]] void __abort_message(const char *format, ...)
{
    va_list args;
    va_start(args, format);
    vfprintf(stderr, format, args);
    va_end(args);
    fputc('\n', stderr);
    abort();
}
}

__attribute__((destructor)) static void finalize_static_image()
{
    __cxa_finalize(__dso_handle);
}
