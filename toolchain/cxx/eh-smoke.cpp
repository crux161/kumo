//J936_r02_001
// Native C++ exception/RTTI proof; every verdict reaches the kernel log. — KESTREL
#include <atomic>
#include <exception>
#include <new>
#include <stdexcept>
#include <typeinfo>
#include <pthread.h>
#include <sched.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static void record(const char *message)
{
    register unsigned long x0 asm("x0") = 5; // NOTICE
    register const char *x1 asm("x1") = message;
    register unsigned long x2 asm("x2") = strlen(message);
    register unsigned long x8 asm("x8") = 47; // kumo-abi LogWrite
    asm volatile("svc #0" : "+r"(x0) : "r"(x1), "r"(x2), "r"(x8) : "memory");
}

static void check(bool condition, const char *why)
{
    if (!condition) {
        record("SH9-b1 native C++ EH FAILED\n");
        record(why);
        _Exit(1);
    }
}

struct Guard {
    unsigned &order;
    unsigned value;
    ~Guard() {
        check(std::uncaught_exceptions() == 1, "cleanup must see one active unwind\n");
        order = order * 10 + value;
    }
};

struct Left { virtual ~Left() = default; unsigned left = 17; };
struct Right { virtual ~Right() = default; unsigned right = 29; };
struct Payload : Left, Right { explicit Payload(unsigned v) : value(v) {} unsigned value; };

__attribute__((noinline)) static void leaf(unsigned &order, unsigned id)
{
    Guard guard{order, 3};
    throw Payload{id};
}

__attribute__((noinline)) static void middle(unsigned &order, unsigned id)
{
    Guard guard{order, 2};
    leaf(order, id);
}

static void catch_once(unsigned id)
{
    unsigned order = 0;
    bool caught = false;
    try {
        Guard guard{order, 1};
        middle(order, id);
    } catch (const Right &base) {
        const auto *payload = dynamic_cast<const Payload *>(&base);
        check(payload && payload->value == id && base.right == 29, "adjusted base catch/RTTI\n");
        check(std::uncaught_exceptions() == 0, "caught exception count\n");
        caught = true;
    }
    check(caught && order == 321, "exact nested cleanup order\n");
}

static std::atomic<unsigned> arrivals{0};
static thread_local unsigned tls_word = 0x936;

static void *worker(void *arg)
{
    const unsigned id = static_cast<unsigned>(reinterpret_cast<unsigned long>(arg));
    check(tls_word == 0x936, "fresh initialized TLS\n");
    tls_word = id;
    for (unsigned i = 0; i < 16; ++i) {
        catch_once(id);
        std::exception_ptr saved;
        try {
            throw Payload{id};
        } catch (...) {
            // All four threads retain a caught exception simultaneously. The
            // exception stack must be TLS, even on a single CPU. — KESTREL
            arrivals.fetch_add(1);
            while (arrivals.load() < 4 * (i + 1))
                sched_yield();
            saved = std::current_exception();
        }
        bool caught = false;
        try { std::rethrow_exception(saved); }
        catch (const Payload &payload) { caught = payload.value == id; }
        check(caught && tls_word == id && std::uncaught_exceptions() == 0, "thread exception/TLS isolation\n");
    }
    return arg;
}

static unsigned guard_attempts;
static unsigned guard_completions;
struct RetryStatic {
    RetryStatic() {
        if (++guard_attempts == 1) throw 19;
        ++guard_completions;
    }
};
static void guarded() { static RetryStatic value; (void)value; }

static bool main_passed;
struct ExitProof {
    ~ExitProof() {
        check(main_passed, "normal process exit after catches\n");
        record("SH9-b1 native C++ EH green\n");
    }
} exit_proof;

__attribute__((noinline)) static void throw_uncaught() { throw Payload{73}; }
__attribute__((noinline)) static void break_noexcept() noexcept { throw_uncaught(); }
static const char *termination_marker;

extern "C" int main(int argc, char **argv)
{
    if (argc == 2 && (strcmp(argv[1], "sh9-b1-noexcept") == 0 || strcmp(argv[1], "sh9-b1-uncaught") == 0)) {
        const bool is_noexcept = strcmp(argv[1], "sh9-b1-noexcept") == 0;
        record(is_noexcept ? "SH9-b1 noexcept begin\n" : "SH9-b1 uncaught begin\n");
        termination_marker = is_noexcept ? "SH9-b1 noexcept green\n" : "SH9-b1 uncaught green\n";
        // Reaching this handler is the expected verdict. A nonzero ProcessExit
        // intentionally produces an ABEND, so success uses status zero. — KESTREL
        std::set_terminate([] { record(termination_marker); _Exit(0); });
        if (is_noexcept) {
            // Keep an outer landing pad: the indirect call has a throwing type,
            // so the compiler cannot erase the catch based on noexcept. — KESTREL
            void (*volatile invoke)() = break_noexcept;
            try { invoke(); }
            catch (...) { check(false, "exception escaped noexcept\n"); }
        } else throw_uncaught();
        check(false, "terminal exception returned\n");
    }
    record("SH9-b1 native C++ EH begin\n");
    for (unsigned i = 0; i < 64; ++i) catch_once(i);
    record("SH9-b1: typed catches, RTTI and LIFO cleanup x64 -> OK\n");
    unsigned order = 0;
    bool caught = false;
    try {
        Guard outer{order, 2};
        try { Guard inner{order, 1}; throw std::runtime_error("native rethrow"); }
        catch (const std::exception &error) {
            check(strcmp(error.what(), "native rethrow") == 0, "std::exception message\n");
            throw;
        }
    } catch (const std::runtime_error &error) { caught = strcmp(error.what(), "native rethrow") == 0; }
    check(caught && order == 12, "rethrow cleanup\n");
    record("SH9-b1: standard exception message and rethrow -> OK\n");

    try { guarded(); check(false, "throwing initializer unexpectedly completed\n"); }
    catch (int value) { check(value == 19, "initializer payload\n"); }
    guarded(); guarded();
    check(guard_attempts == 2 && guard_completions == 1, "guard abort/retry/release\n");
    record("SH9-b1: throwing static initializer retries exactly once -> OK\n");

    volatile size_t impossible = static_cast<size_t>(-1);
    caught = false;
    try { void *p = ::operator new(impossible); ::operator delete(p); }
    catch (const std::bad_alloc &) { caught = true; }
    check(caught, "allocation failure must throw bad_alloc\n");
    record("SH9-b1: allocation failure throws bad_alloc -> OK\n");

    pthread_t threads[4];
    for (unsigned long i = 0; i < 4; ++i)
        check(pthread_create(&threads[i], nullptr, worker, reinterpret_cast<void *>(i + 1)) == 0, "pthread_create\n");
    for (unsigned long i = 0; i < 4; ++i) {
        void *result = nullptr;
        check(pthread_join(threads[i], &result) == 0 && result == reinterpret_cast<void *>(i + 1), "pthread_join\n");
    }
    check(arrivals.load() == 64 && tls_word == 0x936, "main TLS and completed work\n");
    record("SH9-b1: thread-local catches and exception_ptr x64 -> OK\n");
    main_passed = true;
    return 0;
}
