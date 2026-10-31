/* j740 */
/* Execute the pinned Mesa utilities against the real KUMO runtime. — SAKER */
#include <dlfcn.h>
#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <xf86drm.h>
#include "util/build_id.h"
#include "util/hash_table.h"
#include "util/ralloc.h"
#include "util/simple_mtx.h"
#include "util/sparse_array.h"
#include "util/u_call_once.h"
#include "util/u_dynarray.h"
#include "util/u_qsort.h"
#include "panfrost/lib/kmod/pan_kmod.h"

static void check(int valid, const char *operation)
{
    if (!valid) {
        printf("G7m Mesa utilities FAILED: %s (errno=%d)\n", operation, errno);
        abort();
    }
}

static unsigned destroyed;
static void destructor(void *data) { (void)data; ++destroyed; }
static uint32_t collision_hash(const void *key) { (void)key; return 7; }
static void allocations(void)
{
    void *a = ralloc_context(NULL), *b = ralloc_context(NULL);
    check(a && b, "ralloc contexts");
    unsigned *child = rzalloc(a, unsigned);
    check(child && *child == 0 && ralloc_parent(child) == a, "ralloc zero child");
    *child = 42;
    ralloc_set_destructor(child, destructor);
    ralloc_steal(b, child);
    ralloc_free(a);
    check(destroyed == 0 && *child == 42 && ralloc_parent(child) == b, "ralloc steal survives parent");
    char *label = ralloc_asprintf(b, "shader-%u", *child);
    check(label && !strcmp(label, "shader-42"), "ralloc formatting");
    struct hash_table *hash = _mesa_hash_table_create(b, collision_hash, _mesa_key_string_equal);
    check(hash != NULL, "hash create");
    for (unsigned i = 0; i < 128; ++i) {
        char *key = ralloc_asprintf(b, "key-%u", i);
        check(key && _mesa_hash_table_insert(hash, key, (void *)(uintptr_t)(i + 1)), "hash collision insert/grow");
    }
    for (unsigned i = 0; i < 128; ++i) {
        char key[32];
        snprintf(key, sizeof(key), "key-%u", i);
        struct hash_entry *entry = _mesa_hash_table_search(hash, key);
        check(entry && (uintptr_t)entry->data == i + 1, "hash retrieval");
        if (i % 2) _mesa_hash_table_remove(hash, entry);
    }
    check(_mesa_hash_table_num_entries(hash) == 64 && !_mesa_hash_table_search(hash, "absent"), "hash removal/miss");
    _mesa_hash_table_destroy(hash, NULL);
    struct util_dynarray array;
    util_dynarray_init(&array, b);
    for (unsigned i = 0; i < 1024; ++i) {
        unsigned *slot = util_dynarray_grow(&array, unsigned, 1);
        check(slot != NULL, "dynarray growth");
        *slot = i ^ 0x5a5a;
    }
    check(array.size == 1024 * sizeof(unsigned), "dynarray size");
    for (unsigned i = 0; i < 1024; ++i)
        check(((unsigned *)array.data)[i] == (i ^ 0x5a5a), "dynarray contents");
    util_dynarray_fini(&array);
    ralloc_free(b);
    check(destroyed == 1, "ralloc destructor exactly once");

    struct util_sparse_array sparse;
    const uint64_t indices[] = { 0, 63, 64, 65537, (UINT64_C(1) << 40) + 3 };
    uint64_t *saved[5];
    util_sparse_array_init(&sparse, sizeof(uint64_t), 64);
    for (unsigned i = 0; i < 5; ++i) {
        saved[i] = util_sparse_array_get(&sparse, indices[i]);
        check(saved[i] && *saved[i] == 0, "sparse zero allocation");
        *saved[i] = indices[i] + 17;
    }
    for (unsigned i = 0; i < 5; ++i)
        check(util_sparse_array_get(&sparse, indices[i]) == saved[i] && *saved[i] == indices[i] + 17,
              "sparse stable address/value");
    util_sparse_array_validate(&sparse);
    util_sparse_array_finish(&sparse);
}

static unsigned once_count, data_count;
static void initialize(void) { ++once_count; }
static void initialize_data(const void *data) { data_count += *(const unsigned *)data; }
static int compare(const void *a, const void *b, void *data)
{
    return (*(const int *)a - *(const int *)b) * *(int *)data;
}
static void synchronization(void)
{
    simple_mtx_t mutex = SIMPLE_MTX_INITIALIZER;
    util_once_flag once = UTIL_ONCE_FLAG_INIT, with_data = UTIL_ONCE_FLAG_INIT;
    unsigned value = 7;
    for (unsigned i = 0; i < 32; ++i) {
        simple_mtx_lock(&mutex);
        util_call_once(&once, initialize);
        util_call_once_data(&with_data, initialize_data, &value);
        simple_mtx_unlock(&mutex);
    }
    check(once_count == 1 && data_count == 7, "Mesa mutex / once and once-data");
    simple_mtx_destroy(&mutex);
    int values[] = { 3, -7, 4, 3, 0 }, direction = -1;
    util_tls_qsort_r(values, 5, sizeof(int), compare, &direction);
    const int expected[] = { 4, 3, 3, 0, -7 };
    check(!memcmp(values, expected, sizeof(values)), "Mesa C++ sort TLS callback");
}

static void refusals(void)
{
    uint32_t handle = 0xa5a5, first = 0xb6b6;
    uint64_t point = 0xc7c7, capability = 0xd8d8;
    int descriptor = 123;
    drmDevicePtr device = NULL;
#define REFUSED(call) do { errno = 0; check((call) == -ENOTSUP && errno == ENOTSUP, #call); } while (0)
    REFUSED(drmGetDevice2(0, 0, &device));
    REFUSED(drmGetDevices2(0, &device, 1));
    REFUSED(drmGetCap(0, 0, &capability));
    REFUSED(drmCloseBufferHandle(0, handle));
    REFUSED(drmPrimeHandleToFD(0, handle, 0, &descriptor));
    REFUSED(drmPrimeFDToHandle(0, descriptor, &handle));
    REFUSED(drmSyncobjCreate(0, 0, &handle));
    REFUSED(drmSyncobjDestroy(0, handle));
    REFUSED(drmSyncobjHandleToFD(0, handle, &descriptor));
    REFUSED(drmSyncobjFDToHandle(0, descriptor, &handle));
    REFUSED(drmSyncobjImportSyncFile(0, handle, descriptor));
    REFUSED(drmSyncobjExportSyncFile(0, handle, &descriptor));
    REFUSED(drmSyncobjWait(0, &handle, 1, 0, 0, &first));
    REFUSED(drmSyncobjTimelineWait(0, &handle, &point, 1, 0, 0, &first));
    REFUSED(drmSyncobjReset(0, &handle, 1));
    REFUSED(drmSyncobjSignal(0, &handle, 1));
    REFUSED(drmSyncobjTimelineSignal(0, &handle, &point, 1));
    REFUSED(drmSyncobjQuery2(0, &handle, &point, 1, 0));
    REFUSED(drmSyncobjTransfer(0, handle, point, handle, point, 0));
    check(drmIoctl(0, 0, &handle) == -1 && errno == ENOTSUP, "ioctl refuses");
    check(!drmGetVersion(0) && errno == ENOTSUP, "version refuses");
    check(!pan_kmod_dev_create(0, 0, NULL), "actual Mesa DRM factory refuses");
    check(handle == 0xa5a5 && first == 0xb6b6 && point == 0xc7c7 && capability == 0xd8d8 &&
          descriptor == 123 && !device, "refusals publish nothing");
    drmFreeVersion(NULL);
    drmFreeDevice(&device);
    drmFreeDevices(&device, 1);
    check(dlerror() == NULL, "initial loader error clear");
    check(dlopen("/not-a-capability/mesa.so", RTLD_NOW) == NULL, "dynamic loader refuses");
    check(dlerror() != NULL && dlerror() == NULL, "loader error reported and cleared");
}

int main(void)
{
    puts("G7m Mesa utilities begin");
    allocations();
    synchronization();
    refusals();
    const struct build_id_note *note = build_id_find_nhdr_for_addr(main);
    check(note && build_id_length(note) == 20, "static ELF build ID");
    check(!build_id_find_nhdr_for_addr(NULL) && !build_id_find_nhdr_for_addr((void *)1), "foreign address has no build ID");
    uint8_t hash[BLAKE3_KEY_LEN];
    copy_build_id_to_sha1(hash, note);
    check(!memcmp(hash, build_id_data(note), 20), "build ID bytes");
    for (unsigned i = 20; i < sizeof(hash); ++i) check(hash[i] == 0, "build ID padding");
    printf("G7m static build ID: ");
    for (unsigned i = 0; i < 20; ++i) printf("%02x", hash[i]);
    putchar('\n');
    puts("G7m Mesa utilities green: ralloc, hash, sparse, dynarray, mutex, once, sort, DRM refusal, dlopen refusal, static build ID");
    return 0;
}
