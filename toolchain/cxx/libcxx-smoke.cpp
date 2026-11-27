//J945_r11_001
// SH9-c6: the complete narrow-character libc++, used the way LLVM's own sources use it. Built
// against the installed include/c++/v1 + lib layout only, so the install is proven
// self-contained. No filesystem access beyond the standard streams. — TEAL
#include <atomic>
#include <barrier>
#include <charconv>
#include <condition_variable>
#include <cstdint>
#include <format>
#include <future>
#include <iostream>
#include <map>
#include <memory_resource>
#include <mutex>
#include <random>
#include <regex>
#include <shared_mutex>
#include <sstream>
#include <string>
#include <system_error>
#include <thread>
#include <unordered_map>
#include <vector>

static int fail(const char* why) {
    std::cout << "SH9-c6: " << why << "\nSH9-c6 complete libc++ FAILED" << std::endl;
    return 1;
}

extern "C" int main() {
    // 1. iostream: the standard streams are libc++'s own objects over relibc's descriptors.
    std::cout << "SH9-c6 complete libc++ begin" << std::endl;
    std::ostringstream text;
    text << 42 << ' ' << std::hex << 255 << std::dec << ' ' << 2.5;
    if (text.str() != "42 ff 2.5") return fail("ostringstream");
    std::cout << "SH9-c6: iostream cout and stream formatting -> OK" << std::endl;

    // 2. Threads: four workers under a mutex and condition variable, a barrier, an async future,
    //    and a shared_mutex reader/writer pair.
    {
        std::mutex lock;
        std::condition_variable ready;
        int arrived = 0;
        std::atomic<int> phases{0};
        std::barrier sync(4, [&]() noexcept { phases.fetch_add(1); });
        std::vector<std::thread> workers;
        for (int index = 0; index < 4; ++index) {
            workers.emplace_back([&] {
                {
                    std::lock_guard<std::mutex> guard(lock);
                    ++arrived;
                }
                ready.notify_one();
                sync.arrive_and_wait();
                sync.arrive_and_wait();
            });
        }
        {
            std::unique_lock<std::mutex> guard(lock);
            ready.wait(guard, [&] { return arrived == 4; });
        }
        for (std::thread& worker : workers) worker.join();
        if (phases.load() != 2) return fail("barrier phases");
        std::future<int> answer = std::async(std::launch::async, [] { return 6 * 7; });
        if (answer.get() != 42) return fail("async future");
        std::shared_mutex table_lock;
        std::map<int, int> table;
        std::thread writer([&] {
            std::unique_lock<std::shared_mutex> guard(table_lock);
            table[1] = 10;
        });
        writer.join();
        std::shared_lock<std::shared_mutex> reader(table_lock);
        if (table.at(1) != 10) return fail("shared_mutex");
    }
    std::cout << "SH9-c6: threads, barrier, async and shared_mutex -> OK" << std::endl;

    // 3. <random> engines are deterministic: mt19937's 10000th output from the default seed is
    //    the value the C++ standard fixes (26.6.6), and a seeded distribution stays in range.
    {
        std::mt19937 engine;
        engine.discard(9999);
        if (engine() != 4123659995u) return fail("mt19937 10000th value");
        std::mt19937_64 wide(5489u);
        wide.discard(9999);
        if (wide() != 9981545732273789042ull) return fail("mt19937_64 10000th value");
        std::uniform_int_distribution<int> die(1, 6);
        std::mt19937 seeded(2026);
        for (int roll = 0; roll < 1000; ++roll) {
            const int face = die(seeded);
            if (face < 1 || face > 6) return fail("distribution range");
        }
    }
    std::cout << "SH9-c6: random engines and distributions -> OK" << std::endl;

    // 4. <regex>: ECMAScript match with captures, search, and replace.
    {
        const std::regex name(R"(SH9-c(\d)-(\d+))");
        std::smatch parts;
        const std::string slice = "SH9-c6-2026";
        if (!std::regex_match(slice, parts, name) || parts[1] != "6" || parts[2] != "2026")
            return fail("regex_match");
        if (std::regex_replace(std::string("a1b22c333"), std::regex(R"(\d+)"), "#") != "a#b#c#")
            return fail("regex_replace");
        if (!std::regex_search(std::string("libc++ on KUMO"), std::regex("K[A-Z]+O"))) return fail("regex_search");
    }
    std::cout << "SH9-c6: regex match, search and replace -> OK" << std::endl;

    // 5. <charconv> (LLVM-libc's shared float parser) and <format>.
    {
        char buffer[64];
        auto [end, error] = std::to_chars(buffer, buffer + sizeof buffer, 0.1);
        if (error != std::errc() || std::string(buffer, end) != "0.1") return fail("to_chars double");
        double parsed = 0;
        const char* digits = "3.25e2";
        auto [stop, parse_error] = std::from_chars(digits, digits + 6, parsed);
        if (parse_error != std::errc() || stop != digits + 6 || parsed != 325.0) return fail("from_chars double");
        std::uint64_t big = 0;
        const char* hex = "ffffffffffffffff";
        if (std::from_chars(hex, hex + 16, big, 16).ec != std::errc() || big != UINT64_MAX)
            return fail("from_chars hex");
        if (std::format("{:>5}|{:.2f}|{:#x}", 42, 3.14159, 255) != "   42|3.14|0xff") return fail("format");
    }
    std::cout << "SH9-c6: charconv and format -> OK" << std::endl;

    // 6. Polymorphic allocators over a fixed buffer, and the containers LLVM leans on.
    {
        alignas(16) unsigned char arena[4096];
        std::pmr::monotonic_buffer_resource pool(arena, sizeof arena, std::pmr::null_memory_resource());
        std::pmr::vector<int> numbers(&pool);
        for (int index = 0; index < 100; ++index) numbers.push_back(index);
        std::pmr::unordered_map<int, std::pmr::string> names(&pool);
        names.emplace(1, "one");
        if (numbers.size() != 100 || numbers.back() != 99 || names.at(1) != "one") return fail("pmr containers");
        bool exhausted = false;
        try {
            std::pmr::vector<char> huge(8192, 'x', &pool);
        } catch (const std::bad_alloc&) {
            exhausted = true;
        }
        if (!exhausted) return fail("null_memory_resource upstream");
    }
    std::cout << "SH9-c6: pmr resources and containers -> OK" << std::endl;

    std::cout << "SH9-c6 complete libc++ green" << std::endl;
    return 0;
}
