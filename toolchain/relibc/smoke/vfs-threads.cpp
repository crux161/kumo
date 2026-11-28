//J954_r20_001
// PROPOSAL/027 P2a: one process, many threads, one filesystem session. relibc's vfs_call writes a
// request and reads the next message on the session channel; without a lock, two threads can each
// read the other's reply. Every thread here owns a file of a distinct size and byte, and checks
// each answer it gets: a stat of the wrong size, a read of the wrong bytes or length, or a listing
// of the wrong count is a crossed reply. Runs on the RAM FAT volume (nothing persistent). — TEAL
#include <atomic>
#include <cerrno>
#include <cstdio>
#include <cstring>
#include <string>
#include <thread>
#include <vector>

#include <dirent.h>
#include <fcntl.h>
#include <sys/stat.h>
#include <unistd.h>

namespace {

constexpr int THREADS = 8;
constexpr int ROUNDS = 200;
constexpr const char* DIR_PATH = "/P2A";

std::atomic<int> wrong_stat{0}, wrong_read{0}, wrong_list{0}, failed_call{0};

std::string file_path(int index) { return std::string(DIR_PATH) + "/T" + std::to_string(index) + ".TXT"; }
size_t file_size(int index) { return 64 + 37 * static_cast<size_t>(index); }
char file_byte(int index) { return static_cast<char>('A' + index); }

bool write_file(int index) {
    const std::string path = file_path(index);
    const int fd = open(path.c_str(), O_WRONLY | O_CREAT | O_TRUNC, 0644);
    if (fd < 0) return false;
    const std::string bytes(file_size(index), file_byte(index));
    const bool ok = write(fd, bytes.data(), bytes.size()) == static_cast<ssize_t>(bytes.size());
    return close(fd) == 0 && ok;
}

void worker(int index) {
    const std::string path = file_path(index);
    const size_t size = file_size(index);
    std::vector<char> buffer(size + 64);
    for (int round = 0; round < ROUNDS; ++round) {
        struct stat st {};
        if (stat(path.c_str(), &st) != 0) {
            ++failed_call;
        } else if (static_cast<size_t>(st.st_size) != size || !S_ISREG(st.st_mode)) {
            ++wrong_stat;
        }
        const int fd = open(path.c_str(), O_RDONLY);
        if (fd < 0) {
            ++failed_call;
        } else {
            size_t total = 0;
            for (;;) {
                const ssize_t n = read(fd, buffer.data() + total, buffer.size() - total);
                if (n < 0) {
                    ++failed_call;
                    break;
                }
                if (n == 0 || total + static_cast<size_t>(n) >= buffer.size()) {
                    total += static_cast<size_t>(n);
                    break;
                }
                total += static_cast<size_t>(n);
            }
            bool same = total == size;
            for (size_t at = 0; same && at < total; ++at) same = buffer[at] == file_byte(index);
            if (!same) ++wrong_read;
            if (close(fd) != 0) ++failed_call;
        }
        if (round % 10 == index % 10) {
            DIR* dir = opendir(DIR_PATH);
            if (!dir) {
                ++failed_call;
            } else {
                int files = 0;
                while (dirent* entry = readdir(dir))
                    if (entry->d_name[0] != '.') ++files;
                if (files != THREADS) ++wrong_list;
                closedir(dir);
            }
        }
    }
}

int fail(const char* why) {
    std::printf("P2a: %s (errno %d)\nP2a VFS threads FAILED\n", why, errno);
    std::fflush(stdout);
    return 1;
}

}  // namespace

extern "C" int main() {
    std::printf("P2a VFS threads begin\n");
    std::fflush(stdout);
    struct stat st {};
    if (stat(DIR_PATH, &st) == 0) return fail("/P2A left over");
    if (mkdir(DIR_PATH, 0755) != 0) return fail("mkdir /P2A");
    for (int index = 0; index < THREADS; ++index)
        if (!write_file(index)) return fail("writing a thread's file");
    std::printf("P2a: %d files of distinct sizes and bytes on the volume -> OK\n", THREADS);
    std::fflush(stdout);

    std::vector<std::thread> threads;
    for (int index = 0; index < THREADS; ++index) threads.emplace_back(worker, index);
    for (std::thread& thread : threads) thread.join();
    std::printf("P2a: %d threads x %d rounds: wrong stat %d, wrong read %d, wrong listing %d, failed calls %d\n",
                THREADS, ROUNDS, wrong_stat.load(), wrong_read.load(), wrong_list.load(), failed_call.load());
    std::fflush(stdout);
    if (wrong_stat || wrong_read || wrong_list || failed_call) return fail("a thread got another thread's reply");
    std::printf("P2a: concurrent stat/open/read/readdir on one session, no crossed replies -> OK\n");

    for (int index = 0; index < THREADS; ++index)
        if (unlink(file_path(index).c_str()) != 0) return fail("unlink");
    if (rmdir(DIR_PATH) != 0 || stat(DIR_PATH, &st) == 0) return fail("rmdir /P2A");
    std::printf("P2a: files removed -> OK\nP2a VFS threads green\n");
    std::fflush(stdout);
    return 0;
}
