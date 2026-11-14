/*
 * Manual baseline diagnostic only. Interpose immediately before blocking unit
 * EX attempts (including the initial try), without changing flags or results.
 * This makes the old
 * ABBA interleaving reproducible without modifying the baseline Cargo binary.
 */
#define _GNU_SOURCE
#include <dlfcn.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/file.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <unistd.h>

int flock(int fd, int operation) {
    int (*original)(int, int) = dlsym(RTLD_NEXT, "flock");
    const char *socket_path = getenv("__CARGO_TEST_UNIT_LOCK_SOCKET");
    if ((operation & LOCK_EX) && socket_path) {
        char descriptor[64], path[4096];
        snprintf(descriptor, sizeof(descriptor), "/proc/self/fd/%d", fd);
        ssize_t length = readlink(descriptor, path, sizeof(path) - 1);
        if (length >= 6) {
            path[length] = 0;
            const char *package = strstr(path, "/build/a/") ? "a" :
                                  strstr(path, "/build/b/") ? "b" : NULL;
            if (package && !strcmp(path + length - 6, "/.lock")) {
                int connection = socket(AF_UNIX, SOCK_STREAM, 0);
                struct sockaddr_un address = { .sun_family = AF_UNIX };
                if (strlen(socket_path) >= sizeof(address.sun_path)) _exit(120);
                strcpy(address.sun_path, socket_path);
                if (connect(connection, (void *)&address, sizeof(address))) _exit(121);
                dprintf(connection,
                    "{\"pid\":%d,\"phase\":\"BaselineEX\",\"package\":\"%s\","
                    "\"mode\":\"compile\",\"target\":\"%s\",\"lock\":\"%s\"}\n",
                    getpid(), package, package, path);
                char reply;
                if (read(connection, &reply, 1) != 1) _exit(122);
                close(connection);
            }
        }
    }
    return original(fd, operation);
}
