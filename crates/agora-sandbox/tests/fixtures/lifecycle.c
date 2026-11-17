#include <assert.h>
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <unistd.h>

static void wait_child(pid_t child) {
    int status;
    assert(child > 0 && waitpid(child, &status, 0) == child);
    assert(WIFEXITED(status) && WEXITSTATUS(status) == 0);
}

static int fifo;
static atomic_int reading;
static void *read_fifo(void *unused) {
    (void)unused;
    char byte;
    atomic_store(&reading, 1);
    assert(read(fifo, &byte, 1) == 1 && byte == 'x');
    return NULL;
}

int main(int argc, char **argv) {
    assert(argc == 3);
    const char *mode = argv[1], *path = argv[2];
    size_t page = (size_t)sysconf(_SC_PAGESIZE);
    if (!strcmp(mode, "stdio")) {
        pid_t child = fork();
        if (child == 0) {
            FILE *stream = fopen(path, "w");
            assert(stream);
            assert(fwrite("abc", 1, 3, stream) == 3 && fflush(stream) == 0);
            assert(fwrite("XYZ", 1, 3, stream) == 3);
            return 0; /* libc exit must flush the outstanding buffer. */
        }
        wait_child(child);
        int fd = open(path, O_RDONLY);
        char bytes[16] = {0};
        assert(fd >= 0 && read(fd, bytes, sizeof(bytes)) == 6);
        assert(!memcmp(bytes, "abcXYZ", 6) && close(fd) == 0);
    } else if (!strcmp(mode, "fifo")) {
        fifo = open(path, O_RDONLY);
        assert(fifo >= 0);
        pthread_t thread;
        assert(pthread_create(&thread, NULL, read_fifo, NULL) == 0);
        while (!atomic_load(&reading)) usleep(1000);
        usleep(200000);
        pid_t child = fork();
        if (child == 0) _exit(write(STDIN_FILENO, "x", 1) == 1 ? 0 : 1);
        wait_child(child);
        assert(pthread_join(thread, NULL) == 0 && close(fifo) == 0);
    } else if (!strcmp(mode, "cwd") || !strcmp(mode, "cwd_lower")) {
        char renamed[4096];
        assert(snprintf(renamed, sizeof(renamed), "%s-moved", path) > 0);
        assert((mkdir(path, 0700) == 0 || errno == EEXIST) && chdir(path) == 0);
        assert(rename(path, renamed) == 0);
        int fd = open("child", O_CREAT | O_RDWR, 0600);
        assert(fd >= 0 && close(fd) == 0);
        pid_t child = fork();
        if (child == 0) {
            execl("/bin/sh", "sh", "-c", "test -f child", (char *)NULL);
            _exit(2);
        }
        wait_child(child);
    } else {
        int fd = open(path, O_CREAT | O_TRUNC | O_RDWR, 0600);
        assert(fd >= 0);
        if (!strcmp(mode, "fchmod")) {
            assert(unlink(path) == 0);
            int replacement = open(path, O_CREAT | O_RDWR, 0600);
            assert(replacement >= 0 && fchmod(fd, 0400) == 0);
            struct stat status;
            assert(stat(path, &status) == 0 && (status.st_mode & 0777) == 0600);
            assert(fstat(fd, &status) == 0 && (status.st_mode & 0777) == 0400);
            assert(close(replacement) == 0);
        } else if (!strcmp(mode, "locks")) {
            assert(ftruncate(fd, 100) == 0 && lseek(fd, 50, SEEK_SET) == 50);
            struct flock lock = {.l_type = F_WRLCK, .l_whence = SEEK_CUR, .l_len = 10};
            assert(fcntl(fd, F_SETLK, &lock) == 0);
            pid_t child = fork();
            if (child == 0) {
                int other = open(path, O_RDWR);
                assert(other >= 0);
                for (int offset = 0; offset <= 50; offset += 50) {
                    struct flock query = {.l_type = F_WRLCK, .l_whence = SEEK_SET,
                                          .l_start = offset, .l_len = 10};
                    assert(fcntl(other, F_GETLK, &query) == 0);
                    assert((query.l_type == F_UNLCK) == (offset == 0));
                }
                assert(close(other) == 0);
                return 0;
            }
            wait_child(child);
            lock.l_whence = SEEK_END; lock.l_start = -10;
            assert(fcntl(fd, F_SETLK, &lock) == 0);
            child = fork();
            if (child == 0) {
                struct flock query = {.l_type = F_WRLCK, .l_whence = SEEK_SET, .l_start = 90, .l_len = 1};
                assert(fcntl(fd, F_GETLK, &query) == 0 && query.l_type == F_WRLCK && query.l_start == 90);
                return 0;
            }
            wait_child(child);
            int readonly = open(path, O_RDONLY);
            assert(readonly >= 0);
            lock.l_whence = SEEK_SET; lock.l_start = 0;
            assert(fcntl(readonly, F_SETLK, &lock) == -1 && errno == EBADF);
            assert(fcntl(readonly, F_GETLK, (void *)1) == -1 && errno == EFAULT);
            assert(close(readonly) == 0);
        } else if (!strcmp(mode, "munmap")) {
            assert(ftruncate(fd, (off_t)(2 * page)) == 0);
            char *p = mmap(NULL, 2 * page, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
            assert(p != MAP_FAILED);
            p[0] = 'a'; p[page] = 'b';
            assert(munmap(p, 1) == 0);
            assert(fsync(fd) == 0);
            assert(munmap(p + page, page) == 0);
        } else if (!strcmp(mode, "readonly")) {
            assert(ftruncate(fd, (off_t)page) == 0 && close(fd) == 0);
            fd = open(path, O_RDONLY);
            assert(fd >= 0);
            void *p = mmap(NULL, page, PROT_READ, MAP_SHARED, fd, 0);
            assert(p != MAP_FAILED);
            assert(mprotect(p, page, PROT_READ | PROT_WRITE) == -1 && errno == EACCES);
            assert(munmap(p, page) == 0);
            p = mmap(NULL, page, PROT_READ, MAP_PRIVATE, fd, 0);
            assert(p != MAP_FAILED && mprotect(p, page, PROT_READ | PROT_WRITE) == 0);
            assert(munmap(p, page) == 0);
        } else {
            return 2;
        }
        assert(close(fd) == 0);
    }
    return 0;
}
