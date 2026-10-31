/* j740 */
/* The native backend must use capabilities. These retained DRM entry points
 * refuse every operation; an integer FD never becomes a KUMO handle. — SAKER */
#ifndef __kumo__
#error This refusal boundary is only for KUMO static Mesa targets
#endif
#include <errno.h>
#include <stdlib.h>
#include <xf86drm.h>

static int unsupported(void)
{
    errno = ENOTSUP;
    return -ENOTSUP;
}

int drmIoctl(int fd, unsigned long request, void *arg)
{
    (void)fd; (void)request; (void)arg;
    unsupported();
    return -1;
}

drmVersionPtr drmGetVersion(int fd)
{
    (void)fd;
    unsupported();
    return NULL;
}

/* No object can originate here. NULL cleanup is valid; a foreign live object
 * is a programming error, rather than permission to guess its ownership. — SAKER */
void drmFreeVersion(drmVersionPtr version)
{
    if (version) abort();
}
void drmFreeDevice(drmDevicePtr *device)
{
    if (device && *device) abort();
}
void drmFreeDevices(drmDevicePtr devices[], int count)
{
    for (int i = 0; i < count; ++i)
        drmFreeDevice(&devices[i]);
}

#define REFUSE(name, params, unused) int name params { unused; return unsupported(); }
REFUSE(drmGetDevice2, (int fd, uint32_t flags, drmDevicePtr *device),
       (void)fd; (void)flags; (void)device)
REFUSE(drmGetDevices2, (uint32_t flags, drmDevicePtr devices[], int max_devices),
       (void)flags; (void)devices; (void)max_devices)
REFUSE(drmGetCap, (int fd, uint64_t capability, uint64_t *value),
       (void)fd; (void)capability; (void)value)
REFUSE(drmCloseBufferHandle, (int fd, uint32_t handle), (void)fd; (void)handle)
REFUSE(drmPrimeHandleToFD, (int fd, uint32_t handle, uint32_t flags, int *prime_fd),
       (void)fd; (void)handle; (void)flags; (void)prime_fd)
REFUSE(drmPrimeFDToHandle, (int fd, int prime_fd, uint32_t *handle),
       (void)fd; (void)prime_fd; (void)handle)
REFUSE(drmSyncobjCreate, (int fd, uint32_t flags, uint32_t *handle),
       (void)fd; (void)flags; (void)handle)
REFUSE(drmSyncobjDestroy, (int fd, uint32_t handle), (void)fd; (void)handle)
REFUSE(drmSyncobjHandleToFD, (int fd, uint32_t handle, int *obj_fd),
       (void)fd; (void)handle; (void)obj_fd)
REFUSE(drmSyncobjFDToHandle, (int fd, int obj_fd, uint32_t *handle),
       (void)fd; (void)obj_fd; (void)handle)
REFUSE(drmSyncobjImportSyncFile, (int fd, uint32_t handle, int sync_file_fd),
       (void)fd; (void)handle; (void)sync_file_fd)
REFUSE(drmSyncobjExportSyncFile, (int fd, uint32_t handle, int *sync_file_fd),
       (void)fd; (void)handle; (void)sync_file_fd)
REFUSE(drmSyncobjWait, (int fd, uint32_t *handles, unsigned count, int64_t timeout,
                       unsigned flags, uint32_t *first),
       (void)fd; (void)handles; (void)count; (void)timeout; (void)flags; (void)first)
REFUSE(drmSyncobjTimelineWait, (int fd, uint32_t *handles, uint64_t *points,
                               unsigned count, int64_t timeout, unsigned flags, uint32_t *first),
       (void)fd; (void)handles; (void)points; (void)count; (void)timeout; (void)flags; (void)first)
REFUSE(drmSyncobjReset, (int fd, const uint32_t *handles, uint32_t count),
       (void)fd; (void)handles; (void)count)
REFUSE(drmSyncobjSignal, (int fd, const uint32_t *handles, uint32_t count),
       (void)fd; (void)handles; (void)count)
REFUSE(drmSyncobjTimelineSignal, (int fd, const uint32_t *handles, uint64_t *points, uint32_t count),
       (void)fd; (void)handles; (void)points; (void)count)
REFUSE(drmSyncobjQuery2, (int fd, uint32_t *handles, uint64_t *points, uint32_t count, uint32_t flags),
       (void)fd; (void)handles; (void)points; (void)count; (void)flags)
REFUSE(drmSyncobjTransfer, (int fd, uint32_t dst, uint64_t dst_point,
                           uint32_t src, uint64_t src_point, uint32_t flags),
       (void)fd; (void)dst; (void)dst_point; (void)src; (void)src_point; (void)flags)
