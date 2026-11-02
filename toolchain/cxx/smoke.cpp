//j736
// Exercise the measured C++ runtime in a separate KUMO process. — SAKER
#include <algorithm>
#include <mutex>
#include <unordered_map>
#include <vector>
#include <stdio.h>
#include <stdlib.h>

static unsigned startup_count;
static unsigned guard_count;
static unsigned nested_count;
static bool main_passed;
static thread_local unsigned tls_word = 0x736;

static void check(bool condition, const char *operation)
{
    if (!condition) {
        printf("G7l C++ runtime FAILED: %s\n", operation);
        abort();
    }
}

struct Startup {
    Startup()
    {
        std::vector<int> values = {7, 3, 6};
        std::sort(values.begin(), values.end());
        check(values[0] == 3 && values[2] == 7, "allocation in static constructor");
        ++startup_count;
    }
    ~Startup()
    {
        check(main_passed && startup_count == 1, "exit destructor order");
        puts("G7l C++ runtime green: static init, virtual dispatch, vector, sort, guarded statics, mutex, hash, TLS, exit");
    }
};
static Startup startup;

struct Nested {
    Nested() { ++nested_count; }
};
static void nested()
{
    static Nested value;
    (void)value;
}
struct Guarded {
    Guarded()
    {
        nested();
        ++guard_count;
    }
};
static void guarded()
{
    static Guarded value;
    (void)value;
}

struct Base {
    virtual ~Base() = default;
    virtual unsigned value() const = 0;
};
struct Derived : Base {
    unsigned value() const override { return 736; }
};
__attribute__((noinline)) static unsigned dispatch(const Base *base)
{
    return base->value();
}

extern "C" int main(int argc, char **)
{
    puts("G7l C++ runtime begin");
    check(startup_count == 1, "static constructor");
    Base *base = new Derived;
    check(dispatch(base) == 736, "virtual dispatch");
    delete base;
    for (unsigned i = 0; i < 4; ++i)
        guarded();
    check(guard_count == 1 && nested_count == 1, "nested guarded initialization exactly once");
    {
        std::vector<unsigned> values;
        // argc keeps the input dependent on runtime startup data.
        for (unsigned i = 0; i < 257; ++i)
            values.push_back((i * 73 + static_cast<unsigned>(argc)) % 257);
        std::sort(values.begin(), values.end());
        for (unsigned i = 0; i < 257; ++i)
            check(values[i] == i, "vector growth and sort");
        std::unordered_map<unsigned, unsigned> map;
        for (unsigned i = 0; i < 64; ++i)
            map.emplace(i, i * i);
        for (unsigned i = 0; i < 64; ++i)
            check(map.at(i) == i * i, "hash growth / next_prime");
        std::mutex mutex;
        check(mutex.try_lock(), "mutex try_lock");
        mutex.unlock();
        mutex.lock();
        ++tls_word;
        mutex.unlock();
    }
    check(tls_word == 0x737, "TLS");
    main_passed = true;
    puts("G7l C++ main passed; waiting for static destructor");
    return 0;
}
