#include <fcntl.h>
#include <errno.h>
#include <stddef.h>
#include <stdarg.h>
#include <stdint.h>
#include <sys/types.h>
#include <sys/uio.h>

typedef uint64_t guardid_t;

extern int agora_sandbox_open_with_mode(const char *path, int flags, mode_t mode);
extern int agora_sandbox_openat_with_mode(int directory, const char *path, int flags, mode_t mode);
extern int agora_sandbox_guarded_open_with_mode(
    const char *path,
    const guardid_t *guard,
    unsigned int guardflags,
    int flags,
    mode_t mode
);
extern int agora_sandbox_guarded_open_dprotected_with_mode(
    const char *path,
    const guardid_t *guard,
    unsigned int guardflags,
    int flags,
    int dpclass,
    int dpflags,
    mode_t mode
);
extern const void *agora_sandbox_original_fcntl(void);
extern void agora_sandbox_track_fcntl_duplicate(int source, int destination);
extern int agora_sandbox_commit_synced_descriptor(int descriptor);
extern int agora_sandbox_fcntl_setfd_argument(int descriptor, int flags);
extern void agora_sandbox_fcntl_commit_setfd(int descriptor);
extern int agora_sandbox_fcntl_getfl(int descriptor, int native_flags);
extern int agora_sandbox_fcntl_setfl_argument(int descriptor, int flags);
extern int agora_sandbox_fcntl_commit_setfl(int descriptor, int flags);
extern int agora_sandbox_validate_content_fcntl(int descriptor);
extern int agora_sandbox_lock_descriptor(int descriptor);
extern int agora_sandbox_data_descriptor_requires_hook(int descriptor);
extern const void *agora_sandbox_original_read(void);
extern const void *agora_sandbox_original_pread(void);
extern const void *agora_sandbox_original_readv(void);
extern const void *agora_sandbox_original_preadv(void);
extern const void *agora_sandbox_original_write(void);
extern const void *agora_sandbox_original_pwrite(void);
extern const void *agora_sandbox_original_writev(void);
extern const void *agora_sandbox_original_pwritev(void);
extern const void *agora_sandbox_original_read_nocancel(void);
extern const void *agora_sandbox_original_pread_nocancel(void);
extern const void *agora_sandbox_original_readv_nocancel(void);
extern const void *agora_sandbox_original_preadv_nocancel(void);
extern const void *agora_sandbox_original_write_nocancel(void);
extern const void *agora_sandbox_original_pwrite_nocancel(void);
extern const void *agora_sandbox_original_writev_nocancel(void);
extern const void *agora_sandbox_original_pwritev_nocancel(void);
extern ssize_t agora_sandbox_read(int descriptor, void *buffer, size_t length);
extern ssize_t agora_sandbox_pread(
    int descriptor,
    void *buffer,
    size_t length,
    off_t offset
);
extern ssize_t agora_sandbox_readv(int descriptor, const struct iovec *vectors, int count);
extern ssize_t agora_sandbox_preadv(
    int descriptor,
    const struct iovec *vectors,
    int count,
    off_t offset
);
extern ssize_t agora_sandbox_write(int descriptor, const void *buffer, size_t length);
extern ssize_t agora_sandbox_pwrite(
    int descriptor,
    const void *buffer,
    size_t length,
    off_t offset
);
extern ssize_t agora_sandbox_writev(int descriptor, const struct iovec *vectors, int count);
extern ssize_t agora_sandbox_pwritev(
    int descriptor,
    const struct iovec *vectors,
    int count,
    off_t offset
);
extern ssize_t agora_sandbox_read_nocancel(int descriptor, void *buffer, size_t length);
extern ssize_t agora_sandbox_pread_nocancel(
    int descriptor,
    void *buffer,
    size_t length,
    off_t offset
);
extern ssize_t agora_sandbox_readv_nocancel(
    int descriptor,
    const struct iovec *vectors,
    int count
);
extern ssize_t agora_sandbox_preadv_nocancel(
    int descriptor,
    const struct iovec *vectors,
    int count,
    off_t offset
);
extern ssize_t agora_sandbox_write_nocancel(
    int descriptor,
    const void *buffer,
    size_t length
);
extern ssize_t agora_sandbox_pwrite_nocancel(
    int descriptor,
    const void *buffer,
    size_t length,
    off_t offset
);
extern ssize_t agora_sandbox_writev_nocancel(
    int descriptor,
    const struct iovec *vectors,
    int count
);
extern ssize_t agora_sandbox_pwritev_nocancel(
    int descriptor,
    const struct iovec *vectors,
    int count,
    off_t offset
);

typedef int (*open_fn)(const char *, int, ...);
typedef int (*openat_fn)(int, const char *, int, ...);
typedef int (*guarded_open_fn)(const char *, const guardid_t *, unsigned int, int, ...);
typedef int (*guarded_open_dprotected_fn)(
    const char *,
    const guardid_t *,
    unsigned int,
    int,
    int,
    int,
    ...
);
typedef int (*fcntl_fn)(int, int, ...);
typedef ssize_t (*read_fn)(int, void *, size_t);
typedef ssize_t (*pread_fn)(int, void *, size_t, off_t);
typedef ssize_t (*readv_fn)(int, const struct iovec *, int);
typedef ssize_t (*preadv_fn)(int, const struct iovec *, int, off_t);
typedef ssize_t (*write_fn)(int, const void *, size_t);
typedef ssize_t (*pwrite_fn)(int, const void *, size_t, off_t);
typedef ssize_t (*writev_fn)(int, const struct iovec *, int);
typedef ssize_t (*pwritev_fn)(int, const struct iovec *, int, off_t);

static ssize_t agora_sandbox_call_native_read(
    const void *function,
    int descriptor,
    void *buffer,
    size_t length
) {
    if (function == NULL) {
        errno = ENOSYS;
        return -1;
    }
    return ((read_fn)function)(descriptor, buffer, length);
}

static ssize_t agora_sandbox_call_native_readv(
    const void *function,
    int descriptor,
    const struct iovec *vectors,
    int count
) {
    if (function == NULL) {
        errno = ENOSYS;
        return -1;
    }
    return ((readv_fn)function)(descriptor, vectors, count);
}

static ssize_t agora_sandbox_call_native_pread(
    const void *function,
    int descriptor,
    void *buffer,
    size_t length,
    off_t offset
) {
    if (function == NULL) {
        errno = ENOSYS;
        return -1;
    }
    return ((pread_fn)function)(descriptor, buffer, length, offset);
}

static ssize_t agora_sandbox_call_native_preadv(
    const void *function,
    int descriptor,
    const struct iovec *vectors,
    int count,
    off_t offset
) {
    if (function == NULL) {
        errno = ENOSYS;
        return -1;
    }
    return ((preadv_fn)function)(descriptor, vectors, count, offset);
}

static ssize_t agora_sandbox_call_native_write(
    const void *function,
    int descriptor,
    const void *buffer,
    size_t length
) {
    if (function == NULL) {
        errno = ENOSYS;
        return -1;
    }
    return ((write_fn)function)(descriptor, buffer, length);
}

static ssize_t agora_sandbox_call_native_pwrite(
    const void *function,
    int descriptor,
    const void *buffer,
    size_t length,
    off_t offset
) {
    if (function == NULL) {
        errno = ENOSYS;
        return -1;
    }
    return ((pwrite_fn)function)(descriptor, buffer, length, offset);
}

static ssize_t agora_sandbox_call_native_writev(
    const void *function,
    int descriptor,
    const struct iovec *vectors,
    int count
) {
    if (function == NULL) {
        errno = ENOSYS;
        return -1;
    }
    return ((writev_fn)function)(descriptor, vectors, count);
}

static ssize_t agora_sandbox_call_native_pwritev(
    const void *function,
    int descriptor,
    const struct iovec *vectors,
    int count,
    off_t offset
) {
    if (function == NULL) {
        errno = ENOSYS;
        return -1;
    }
    return ((pwritev_fn)function)(descriptor, vectors, count, offset);
}

ssize_t agora_sandbox_read_shim(int descriptor, void *buffer, size_t length) {
    if (agora_sandbox_data_descriptor_requires_hook(descriptor)) {
        return agora_sandbox_read(descriptor, buffer, length);
    }
    return agora_sandbox_call_native_read(
        agora_sandbox_original_read(),
        descriptor,
        buffer,
        length
    );
}

ssize_t agora_sandbox_readv_shim(
    int descriptor,
    const struct iovec *vectors,
    int count
) {
    if (agora_sandbox_data_descriptor_requires_hook(descriptor)) {
        return agora_sandbox_readv(descriptor, vectors, count);
    }
    return agora_sandbox_call_native_readv(
        agora_sandbox_original_readv(),
        descriptor,
        vectors,
        count
    );
}

ssize_t agora_sandbox_pread_shim(
    int descriptor,
    void *buffer,
    size_t length,
    off_t offset
) {
    if (agora_sandbox_data_descriptor_requires_hook(descriptor)) {
        return agora_sandbox_pread(descriptor, buffer, length, offset);
    }
    return agora_sandbox_call_native_pread(
        agora_sandbox_original_pread(),
        descriptor,
        buffer,
        length,
        offset
    );
}

ssize_t agora_sandbox_preadv_shim(
    int descriptor,
    const struct iovec *vectors,
    int count,
    off_t offset
) {
    if (agora_sandbox_data_descriptor_requires_hook(descriptor)) {
        return agora_sandbox_preadv(descriptor, vectors, count, offset);
    }
    return agora_sandbox_call_native_preadv(
        agora_sandbox_original_preadv(),
        descriptor,
        vectors,
        count,
        offset
    );
}

ssize_t agora_sandbox_write_shim(int descriptor, const void *buffer, size_t length) {
    if (agora_sandbox_data_descriptor_requires_hook(descriptor)) {
        return agora_sandbox_write(descriptor, buffer, length);
    }
    return agora_sandbox_call_native_write(
        agora_sandbox_original_write(),
        descriptor,
        buffer,
        length
    );
}

ssize_t agora_sandbox_pwrite_shim(
    int descriptor,
    const void *buffer,
    size_t length,
    off_t offset
) {
    if (agora_sandbox_data_descriptor_requires_hook(descriptor)) {
        return agora_sandbox_pwrite(descriptor, buffer, length, offset);
    }
    return agora_sandbox_call_native_pwrite(
        agora_sandbox_original_pwrite(),
        descriptor,
        buffer,
        length,
        offset
    );
}

ssize_t agora_sandbox_writev_shim(
    int descriptor,
    const struct iovec *vectors,
    int count
) {
    if (agora_sandbox_data_descriptor_requires_hook(descriptor)) {
        return agora_sandbox_writev(descriptor, vectors, count);
    }
    return agora_sandbox_call_native_writev(
        agora_sandbox_original_writev(),
        descriptor,
        vectors,
        count
    );
}

ssize_t agora_sandbox_pwritev_shim(
    int descriptor,
    const struct iovec *vectors,
    int count,
    off_t offset
) {
    if (agora_sandbox_data_descriptor_requires_hook(descriptor)) {
        return agora_sandbox_pwritev(descriptor, vectors, count, offset);
    }
    return agora_sandbox_call_native_pwritev(
        agora_sandbox_original_pwritev(),
        descriptor,
        vectors,
        count,
        offset
    );
}

ssize_t agora_sandbox_read_nocancel_shim(int descriptor, void *buffer, size_t length) {
    if (agora_sandbox_data_descriptor_requires_hook(descriptor)) {
        return agora_sandbox_read_nocancel(descriptor, buffer, length);
    }
    return agora_sandbox_call_native_read(
        agora_sandbox_original_read_nocancel(),
        descriptor,
        buffer,
        length
    );
}

ssize_t agora_sandbox_readv_nocancel_shim(
    int descriptor,
    const struct iovec *vectors,
    int count
) {
    if (agora_sandbox_data_descriptor_requires_hook(descriptor)) {
        return agora_sandbox_readv_nocancel(descriptor, vectors, count);
    }
    return agora_sandbox_call_native_readv(
        agora_sandbox_original_readv_nocancel(),
        descriptor,
        vectors,
        count
    );
}

ssize_t agora_sandbox_pread_nocancel_shim(
    int descriptor,
    void *buffer,
    size_t length,
    off_t offset
) {
    if (agora_sandbox_data_descriptor_requires_hook(descriptor)) {
        return agora_sandbox_pread_nocancel(descriptor, buffer, length, offset);
    }
    return agora_sandbox_call_native_pread(
        agora_sandbox_original_pread_nocancel(),
        descriptor,
        buffer,
        length,
        offset
    );
}

ssize_t agora_sandbox_preadv_nocancel_shim(
    int descriptor,
    const struct iovec *vectors,
    int count,
    off_t offset
) {
    if (agora_sandbox_data_descriptor_requires_hook(descriptor)) {
        return agora_sandbox_preadv_nocancel(descriptor, vectors, count, offset);
    }
    return agora_sandbox_call_native_preadv(
        agora_sandbox_original_preadv_nocancel(),
        descriptor,
        vectors,
        count,
        offset
    );
}

ssize_t agora_sandbox_write_nocancel_shim(
    int descriptor,
    const void *buffer,
    size_t length
) {
    if (agora_sandbox_data_descriptor_requires_hook(descriptor)) {
        return agora_sandbox_write_nocancel(descriptor, buffer, length);
    }
    return agora_sandbox_call_native_write(
        agora_sandbox_original_write_nocancel(),
        descriptor,
        buffer,
        length
    );
}

ssize_t agora_sandbox_pwrite_nocancel_shim(
    int descriptor,
    const void *buffer,
    size_t length,
    off_t offset
) {
    if (agora_sandbox_data_descriptor_requires_hook(descriptor)) {
        return agora_sandbox_pwrite_nocancel(descriptor, buffer, length, offset);
    }
    return agora_sandbox_call_native_pwrite(
        agora_sandbox_original_pwrite_nocancel(),
        descriptor,
        buffer,
        length,
        offset
    );
}

ssize_t agora_sandbox_writev_nocancel_shim(
    int descriptor,
    const struct iovec *vectors,
    int count
) {
    if (agora_sandbox_data_descriptor_requires_hook(descriptor)) {
        return agora_sandbox_writev_nocancel(descriptor, vectors, count);
    }
    return agora_sandbox_call_native_writev(
        agora_sandbox_original_writev_nocancel(),
        descriptor,
        vectors,
        count
    );
}

ssize_t agora_sandbox_pwritev_nocancel_shim(
    int descriptor,
    const struct iovec *vectors,
    int count,
    off_t offset
) {
    if (agora_sandbox_data_descriptor_requires_hook(descriptor)) {
        return agora_sandbox_pwritev_nocancel(descriptor, vectors, count, offset);
    }
    return agora_sandbox_call_native_pwritev(
        agora_sandbox_original_pwritev_nocancel(),
        descriptor,
        vectors,
        count,
        offset
    );
}

int agora_sandbox_call_open(const void *function, const char *path, int flags, mode_t mode) {
    open_fn original = (open_fn)function;
    return (flags & O_CREAT) != 0 ? original(path, flags, (int)mode) : original(path, flags);
}

int agora_sandbox_call_openat(
    const void *function,
    int directory,
    const char *path,
    int flags,
    mode_t mode
) {
    openat_fn original = (openat_fn)function;
    return (flags & O_CREAT) != 0 ? original(directory, path, flags, (int)mode)
                                  : original(directory, path, flags);
}

int agora_sandbox_call_guarded_open(
    const void *function,
    const char *path,
    const guardid_t *guard,
    unsigned int guardflags,
    int flags,
    mode_t mode
) {
    guarded_open_fn original = (guarded_open_fn)function;
    return (flags & O_CREAT) != 0
        ? original(path, guard, guardflags, flags, (int)mode)
        : original(path, guard, guardflags, flags);
}

int agora_sandbox_call_guarded_open_dprotected(
    const void *function,
    const char *path,
    const guardid_t *guard,
    unsigned int guardflags,
    int flags,
    int dpclass,
    int dpflags,
    mode_t mode
) {
    guarded_open_dprotected_fn original = (guarded_open_dprotected_fn)function;
    return (flags & O_CREAT) != 0
        ? original(path, guard, guardflags, flags, dpclass, dpflags, (int)mode)
        : original(path, guard, guardflags, flags, dpclass, dpflags);
}

int agora_sandbox_open_shim(const char *path, int flags, ...) {
    mode_t mode = 0;
    if ((flags & O_CREAT) != 0) {
        va_list arguments;
        va_start(arguments, flags);
        mode = (mode_t)va_arg(arguments, int);
        va_end(arguments);
    }
    return agora_sandbox_open_with_mode(path, flags, mode);
}

int agora_sandbox_openat_shim(int directory, const char *path, int flags, ...) {
    mode_t mode = 0;
    if ((flags & O_CREAT) != 0) {
        va_list arguments;
        va_start(arguments, flags);
        mode = (mode_t)va_arg(arguments, int);
        va_end(arguments);
    }
    return agora_sandbox_openat_with_mode(directory, path, flags, mode);
}

int agora_sandbox_guarded_open_shim(
    const char *path,
    const guardid_t *guard,
    unsigned int guardflags,
    int flags,
    ...
) {
    mode_t mode = 0;
    if ((flags & O_CREAT) != 0) {
        va_list arguments;
        va_start(arguments, flags);
        mode = (mode_t)va_arg(arguments, int);
        va_end(arguments);
    }
    return agora_sandbox_guarded_open_with_mode(path, guard, guardflags, flags, mode);
}

int agora_sandbox_guarded_open_dprotected_shim(
    const char *path,
    const guardid_t *guard,
    unsigned int guardflags,
    int flags,
    int dpclass,
    int dpflags,
    ...
) {
    mode_t mode = 0;
    if ((flags & O_CREAT) != 0) {
        va_list arguments;
        va_start(arguments, dpflags);
        mode = (mode_t)va_arg(arguments, int);
        va_end(arguments);
    }
    return agora_sandbox_guarded_open_dprotected_with_mode(
        path,
        guard,
        guardflags,
        flags,
        dpclass,
        dpflags,
        mode
    );
}

int agora_sandbox_fcntl_shim(int descriptor, int command, ...) {
    fcntl_fn original_fcntl = (fcntl_fn)agora_sandbox_original_fcntl();
    if (original_fcntl == NULL) {
        errno = ENOSYS;
        return -1;
    }

    int lock_command = command == F_GETLK || command == F_SETLK || command == F_SETLKW;
#ifdef F_OFD_GETLK
    lock_command = lock_command || command == F_OFD_GETLK || command == F_OFD_SETLK
        || command == F_OFD_SETLKW;
#endif
    int operation_descriptor =
        lock_command ? agora_sandbox_lock_descriptor(descriptor) : descriptor;
    int result;
    switch (command) {
        case F_GETFD:
        case F_GETOWN:
        case F_FLUSH_DATA:
        case F_CHKCLEAN:
        case F_FULLFSYNC:
        case F_FREEZE_FS:
        case F_THAW_FS:
        case F_GETPROTECTIONCLASS:
        case F_GETNOSIGPIPE:
        case F_GETPROTECTIONLEVEL:
        case F_BARRIERFSYNC:
        case F_GETLEASE:
            result = original_fcntl(descriptor, command);
            break;
        case F_GETFL:
            result = original_fcntl(descriptor, command);
            if (result >= 0) {
                result = agora_sandbox_fcntl_getfl(descriptor, result);
            }
            break;
        case F_DUPFD:
        case F_DUPFD_CLOEXEC:
        case F_SETOWN:
        case F_RDAHEAD:
        case F_NOCACHE:
        case F_GLOBAL_NOCACHE:
        case F_NODIRECT:
        case F_SETPROTECTIONCLASS:
        case F_SETNOSIGPIPE:
        case F_SINGLE_WRITER:
        case F_SETBACKINGSTORE:
        case F_SETLEASE:
        case F_NOCACHE_EXT: {
            va_list arguments;
            va_start(arguments, command);
            int argument = va_arg(arguments, int);
            va_end(arguments);
            result = original_fcntl(operation_descriptor, command, argument);
            break;
        }
        case F_SETFL: {
            va_list arguments;
            va_start(arguments, command);
            int argument = va_arg(arguments, int);
            va_end(arguments);
            int native_argument = agora_sandbox_fcntl_setfl_argument(descriptor, argument);
            result = original_fcntl(descriptor, command, native_argument);
            if (result >= 0) {
                result = agora_sandbox_fcntl_commit_setfl(descriptor, argument);
            }
            break;
        }
        case F_SETFD: {
            va_list arguments;
            va_start(arguments, command);
            int argument = va_arg(arguments, int);
            va_end(arguments);
            result = original_fcntl(
                descriptor,
                command,
                agora_sandbox_fcntl_setfd_argument(descriptor, argument)
            );
            if (result >= 0) {
                agora_sandbox_fcntl_commit_setfd(descriptor);
            }
            break;
        }
        case F_SETSIZE: {
            if (agora_sandbox_validate_content_fcntl(descriptor) < 0) {
                return -1;
            }
            va_list arguments;
            va_start(arguments, command);
            off_t argument = va_arg(arguments, off_t);
            va_end(arguments);
            result = original_fcntl(operation_descriptor, command, argument);
            break;
        }
#ifdef F_PUNCHHOLE
        case F_PUNCHHOLE: {
            if (agora_sandbox_validate_content_fcntl(descriptor) < 0) {
                return -1;
            }
            va_list arguments;
            va_start(arguments, command);
            void *argument = va_arg(arguments, void *);
            va_end(arguments);
            result = original_fcntl(descriptor, command, argument);
            break;
        }
#endif
        default: {
            va_list arguments;
            va_start(arguments, command);
            void *argument = va_arg(arguments, void *);
            va_end(arguments);
            result = original_fcntl(operation_descriptor, command, argument);
            break;
        }
    }
    if (result >= 0 && (command == F_DUPFD || command == F_DUPFD_CLOEXEC)) {
        agora_sandbox_track_fcntl_duplicate(descriptor, result);
    }
    if (result >= 0 && (command == F_FULLFSYNC || command == F_BARRIERFSYNC)) {
        return agora_sandbox_commit_synced_descriptor(descriptor);
    }
    return result;
}
