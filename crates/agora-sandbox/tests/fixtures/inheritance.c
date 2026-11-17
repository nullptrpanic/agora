#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <pthread.h>
#include <spawn.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/resource.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <unistd.h>

extern char **environ;
extern int posix_spawn_file_actions_addchdir(posix_spawn_file_actions_t *, const char *) __attribute__((weak_import));
extern int posix_spawn_file_actions_addfchdir(posix_spawn_file_actions_t *, int) __attribute__((weak_import));
extern int native_addchdir_np(posix_spawn_file_actions_t *, const char *) __asm("_posix_spawn_file_actions_addchdir_np");
extern int native_addfchdir_np(posix_spawn_file_actions_t *, int) __asm("_posix_spawn_file_actions_addfchdir_np");
#define CHECK(test) do { if (!(test)) { fprintf(stderr, "%s:%d: %s (errno=%d)\n", __FILE__, __LINE__, #test, errno); return 1; } } while (0)
static const char output[] = "diagnostic-output\n";
static const char *late_path;
static int late_fd = -1;

static void prepare(void) { late_fd = open(late_path, O_CREAT | O_TRUNC | O_RDWR, 0600); }
static int wait_child(pid_t child) {
    int status;
    CHECK(waitpid(child, &status, 0) == child);
    CHECK(WIFEXITED(status) && WEXITSTATUS(status) == 0);
    return 0;
}
static int verify(const char *path) {
    char bytes[sizeof(output)] = {0};
    int fd = open(path, O_RDONLY);
    CHECK(fd >= 0);
    CHECK(read(fd, bytes, sizeof(bytes)) == sizeof(output) - 1);
    CHECK(memcmp(bytes, output, sizeof(output) - 1) == 0);
    CHECK(close(fd) == 0);
    return 0;
}

int main(int argc, char **argv) {
    CHECK(argc == 3);
    if (!strcmp(argv[1], "setexec-hop")) {
        posix_spawnattr_t attributes;
        CHECK(posix_spawnattr_init(&attributes) == 0);
        CHECK(posix_spawnattr_setflags(&attributes, POSIX_SPAWN_SETEXEC) == 0);
        char *arguments[] = {argv[0], "copy", "unused", NULL};
        int result = posix_spawn(NULL, argv[0], NULL, &attributes, arguments, environ);
        fprintf(stderr, "SETEXEC unexpectedly returned %d\n", result);
        return 1;
    }
    if (!strcmp(argv[1], "copy")) {
        char bytes[sizeof(output)];
        CHECK(read(0, bytes, sizeof(bytes)) == sizeof(output) - 1);
        CHECK(write(1, bytes, sizeof(output) - 1) == sizeof(output) - 1);
        return 0;
    }
    if (!strcmp(argv[1], "writer")) {
        char signal;
        CHECK(read(atoi(argv[2]), &signal, 1) == 1);
        CHECK(write(1, output, sizeof(output) - 1) == sizeof(output) - 1);
        return 0;
    }
    if (!strncmp(argv[1], "reader", 6)) {
        char byte;
        CHECK(read(atoi(argv[2]), &byte, 1) == 1);
        CHECK(byte == 'B');
        if (!strcmp(argv[1], "reader-dir")) {
            char cwd[PATH_MAX];
            CHECK(getcwd(cwd, sizeof(cwd)) != NULL && !strcmp(cwd, "/"));
        }
        return 0;
    }
    if (!strncmp(argv[1], "exec", 4)) {
        char destination[PATH_MAX];
        CHECK(snprintf(destination, sizeof(destination), "%s.out", argv[2]) < (int)sizeof(destination));
        pid_t child = fork();
        CHECK(child >= 0);
        if (child == 0) {
            int input = open(argv[2], O_CREAT | O_TRUNC | O_RDWR, 0600);
            CHECK(input >= 0);
            CHECK(write(input, output, sizeof(output) - 1) == sizeof(output) - 1);
            CHECK(lseek(input, 0, SEEK_SET) == 0);
            CHECK(dup2(input, 0) == 0);
            CHECK(close(input) == 0);
            int file = open(destination, O_CREAT | O_TRUNC | O_WRONLY, 0600);
            CHECK(file >= 0 && dup2(file, 1) == 1);
            CHECK(close(file) == 0);
            execl("/usr/bin/env", "env", "/usr/bin/env", argv[0],
                !strcmp(argv[1], "exec-setexec") ? "setexec-hop" : "copy", "unused", NULL);
            CHECK(0);
        }
        CHECK(wait_child(child) == 0);
        return verify(destination);
    }
    if (!strcmp(argv[1], "fork")) {
        int gate[2];
        CHECK(pipe(gate) == 0);
        late_path = argv[2];
        CHECK(pthread_atfork(prepare, NULL, NULL) == 0);
        pid_t child = fork();
        CHECK(child >= 0);
        if (child == 0) {
            CHECK(close(gate[1]) == 0);
            char signal;
            CHECK(read(gate[0], &signal, 1) == 1);
            CHECK(pwrite(late_fd, output, sizeof(output) - 1, 0) == sizeof(output) - 1);
            CHECK(close(late_fd) == 0);
            return 0;
        }
        CHECK(close(gate[0]) == 0);
        CHECK(close(late_fd) == 0);
        CHECK(write(gate[1], "x", 1) == 1);
        CHECK(close(gate[1]) == 0);
        CHECK(wait_child(child) == 0);
        return verify(argv[2]);
    }
    posix_spawn_file_actions_t actions;
    CHECK(posix_spawn_file_actions_init(&actions) == 0);
    int file = open(argv[2], O_CREAT | O_TRUNC | O_RDWR, 0600);
    CHECK(file >= 0);
    pid_t child;
    if (!strcmp(argv[1], "stale")) {
        CHECK(posix_spawn_file_actions_destroy(&actions) == 0);
        CHECK(fcntl(file, F_SETFD, 0) == 0);
        CHECK(posix_spawn_file_actions_init(&actions) == 0);
        CHECK(close(file) == 0);
        char *arguments[] = {"/usr/bin/true", NULL};
        int result = posix_spawn(&child, arguments[0], &actions, NULL, arguments, environ);
        if (result) fprintf(stderr, "spawn returned %d\n", result);
        CHECK(result == 0);
        CHECK(posix_spawn_file_actions_destroy(&actions) == 0);
        return wait_child(child);
    }
    if (!strncmp(argv[1], "remap", 5)) {
        CHECK(write(file, "ABCD", 4) == 4);
        int second = open(argv[2], O_RDONLY);
        CHECK(second >= 0);
        CHECK(fcntl(file, F_SETFD, 0) == 0 && fcntl(second, F_SETFD, 0) == 0);
        CHECK(lseek(file, 1, SEEK_SET) == 1 && lseek(second, 3, SEEK_SET) == 3);
        if (!strcmp(argv[1], "remap-chain")) {
            CHECK(posix_spawn_file_actions_adddup2(&actions, file, 240) == 0);
            CHECK(posix_spawn_file_actions_addclose(&actions, file) == 0);
            CHECK(posix_spawn_file_actions_adddup2(&actions, 240, second) == 0);
            CHECK(posix_spawn_file_actions_addclose(&actions, 240) == 0);
        } else {
            CHECK(posix_spawn_file_actions_adddup2(&actions, file, second) == 0);
        }
        if (!strcmp(argv[1], "remap-open")) {
            CHECK(posix_spawn_file_actions_addopen(&actions, file, "/dev/null", O_RDONLY, 0) == 0);
        }
        if (!strcmp(argv[1], "remap-high-close")) {
            struct rlimit limit;
            CHECK(getrlimit(RLIMIT_NOFILE, &limit) == 0 && limit.rlim_cur >= 256);
            limit.rlim_cur = 256;
            CHECK(setrlimit(RLIMIT_NOFILE, &limit) == 0);
            CHECK(fcntl(file, F_DUPFD, 255) == 255);
            CHECK(posix_spawn_file_actions_addclose(&actions, 255) == 0);
        }
        int directory = -1;
        if (strstr(argv[1], "fchdir")) {
            directory = open("/", O_RDONLY | O_DIRECTORY);
            CHECK(directory >= 0);
            if (!strcmp(argv[1], "remap-fchdir")) {
                if (__builtin_available(macOS 26.0, *)) {
                    CHECK(posix_spawn_file_actions_addfchdir(&actions, directory) == 0);
                } else {
                    CHECK(native_addfchdir_np(&actions, directory) == 0);
                }
            } else {
                CHECK(native_addfchdir_np(&actions, directory) == 0);
            }
            CHECK(posix_spawn_file_actions_addclose(&actions, directory) == 0);
        } else if (strstr(argv[1], "chdir")) {
            if (!strcmp(argv[1], "remap-chdir")) {
                if (__builtin_available(macOS 26.0, *)) {
                    CHECK(posix_spawn_file_actions_addchdir(&actions, "/") == 0);
                } else {
                    CHECK(native_addchdir_np(&actions, "/") == 0);
                }
            } else {
                CHECK(native_addchdir_np(&actions, "/") == 0);
            }
        }
        char descriptor[32];
        snprintf(descriptor, sizeof(descriptor), "%d", second);
        char *arguments[] = {argv[0], strstr(argv[1], "dir") ? "reader-dir" : "reader", descriptor, NULL};
        // Reusing an action list must not accumulate hidden actions or retain state.
        for (int i = 0; i < 2; ++i) {
            CHECK(lseek(file, 1, SEEK_SET) == 1);
            CHECK(posix_spawnp(&child, argv[0], &actions, NULL, arguments, environ) == 0);
            CHECK(wait_child(child) == 0);
            CHECK(lseek(file, 0, SEEK_CUR) == 2);
            CHECK(lseek(second, 0, SEEK_CUR) == 3);
        }
        CHECK(close(second) == 0 && close(file) == 0);
        if (!strcmp(argv[1], "remap-high-close")) CHECK(close(255) == 0);
        if (directory >= 0) CHECK(close(directory) == 0);
    } else if (!strcmp(argv[1], "spawn-failure")) {
        CHECK(fcntl(file, F_SETFD, 0) == 0);
        CHECK(posix_spawn_file_actions_adddup2(&actions, file, 1) == 0);
        CHECK(posix_spawn_file_actions_adddup2(&actions, 240, 2) == 0);
        CHECK(fcntl(240, F_GETFD) < 0 && errno == EBADF);
        char *arguments[] = {"/usr/bin/true", NULL};
        int count = -1;
        for (int attempt = 0; attempt < 4; ++attempt) {
            CHECK(posix_spawn(&child, arguments[0], &actions, NULL, arguments, environ) == EBADF);
            int current = 0;
            for (int fd = 0; fd < 512; ++fd) if (fcntl(fd, F_GETFD) >= 0) ++current;
            if (count >= 0) CHECK(current == count);
            count = current;
        }
        CHECK(write(file, output, sizeof(output) - 1) == sizeof(output) - 1);
        CHECK(close(file) == 0 && verify(argv[2]) == 0);
    } else {
        int gate[2];
        CHECK(pipe(gate) == 0);
        CHECK(posix_spawn_file_actions_adddup2(&actions, file, 1) == 0);
        CHECK(posix_spawn_file_actions_addclose(&actions, file) == 0);
        CHECK(posix_spawn_file_actions_addinherit_np(&actions, gate[0]) == 0);
        posix_spawnattr_t attributes;
        CHECK(posix_spawnattr_init(&attributes) == 0);
        if (!strcmp(argv[1], "redirect-cloexec")) {
            CHECK(posix_spawnattr_setflags(&attributes, POSIX_SPAWN_CLOEXEC_DEFAULT) == 0);
        }
        char descriptor[32];
        snprintf(descriptor, sizeof(descriptor), "%d", gate[0]);
        char *arguments[] = {argv[0], "writer", descriptor, NULL};
        int result = posix_spawn(&child, argv[0], &actions, &attributes, arguments, environ);
        if (result != 0) fprintf(stderr, "redirect spawn returned %d\n", result);
        CHECK(result == 0);
        CHECK(posix_spawnattr_destroy(&attributes) == 0);
        CHECK(close(gate[0]) == 0 && close(file) == 0);
        CHECK(write(gate[1], "x", 1) == 1);
        CHECK(close(gate[1]) == 0);
        CHECK(wait_child(child) == 0);
        CHECK(verify(argv[2]) == 0);
    }
    CHECK(posix_spawn_file_actions_destroy(&actions) == 0);
    return 0;
}
