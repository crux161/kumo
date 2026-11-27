//J947_r13_001
// SH9-c8: std::random_device and the C entropy calls over the kernel CSPRNG (ChaCha20 keyed from
// the firmware's EFI_RNG_PROTOCOL). One binary, two runs: `relibc sh9-c8 seeded` on a boot whose
// firmware gave entropy (QEMU with virtio-rng, the Orange Pi's own firmware), and
// `relibc sh9-c8 unseeded` on a boot that had none, where everything that promises real entropy
// must refuse and only GRND_INSECURE may answer. — TEAL
#include <cerrno>
#include <cstring>
#include <iostream>
#include <random>
#include <set>
#include <string>
#include <system_error>
#include <sys/random.h>
#include <unistd.h>

static int fail(const char* why) {
    std::cout << "SH9-c8: " << why << "\nSH9-c8 entropy FAILED" << std::endl;
    return 1;
}

static int seeded() {
    std::random_device device;
    std::set<unsigned> seen;
    for (int draw = 0; draw < 64; ++draw) seen.insert(device());
    // 64 independent 32-bit draws collide with probability ~5e-7; a stuck or clock-paced
    // source would collapse to a handful.
    if (seen.size() < 60) return fail("random_device draws repeat");
    std::mt19937 engine(device());
    std::uniform_int_distribution<int> die(1, 6);
    for (int roll = 0; roll < 100; ++roll) {
        const int face = die(engine);
        if (face < 1 || face > 6) return fail("seeded engine range");
    }
    std::cout << "SH9-c8: random_device draws from the kernel CSPRNG -> OK" << std::endl;

    unsigned char first[32], second[32], big[1000];
    if (getentropy(first, sizeof first) != 0 || getentropy(second, sizeof second) != 0 ||
        std::memcmp(first, second, sizeof first) == 0)
        return fail("getentropy");
    errno = 0;
    unsigned char too_much[257];
    if (getentropy(too_much, sizeof too_much) != -1 || errno != EINVAL) return fail("getentropy limit");
    // 1000 bytes: relibc assembles them from 256-byte kernel reads.
    std::memset(big, 0, sizeof big);
    if (getrandom(big, sizeof big, 0) != static_cast<ssize_t>(sizeof big)) return fail("getrandom 1000");
    int zeros = 0;
    for (unsigned char byte : big) zeros += byte == 0;
    if (zeros > 20) return fail("getrandom output looks empty");
    std::cout << "SH9-c8: getentropy, its limit, and a multi-read getrandom -> OK" << std::endl;
    return 0;
}

static int unseeded() {
    bool threw = false;
    try {
        std::random_device device;
        (void)device();
    } catch (const std::system_error& error) {
        threw = error.code().value() == EIO;
    }
    if (!threw) return fail("random_device did not refuse");
    unsigned char bytes[32];
    errno = 0;
    if (getentropy(bytes, sizeof bytes) != -1 || errno != EIO) return fail("getentropy did not refuse");
    errno = 0;
    if (getrandom(bytes, sizeof bytes, 0) != -1 || errno != EAGAIN) return fail("blocking getrandom did not refuse");
    std::cout << "SH9-c8: an unseeded kernel refuses random_device, getentropy and getrandom -> OK" << std::endl;
    if (getrandom(bytes, sizeof bytes, GRND_INSECURE) != static_cast<ssize_t>(sizeof bytes))
        return fail("GRND_INSECURE");
    std::cout << "SH9-c8: GRND_INSECURE still answers unseeded -> OK" << std::endl;
    return 0;
}

extern "C" int main(int argc, char** argv) {
    std::cout << "SH9-c8 entropy begin" << std::endl;
    const std::string mode = argc > 1 ? argv[argc - 1] : "";
    int status;
    if (mode == "seeded") {
        status = seeded();
    } else if (mode == "unseeded") {
        status = unseeded();
    } else {
        return fail("argv names no mode");
    }
    if (status != 0) return status;
    std::cout << "SH9-c8 entropy green (" << mode << ")" << std::endl;
    return 0;
}
