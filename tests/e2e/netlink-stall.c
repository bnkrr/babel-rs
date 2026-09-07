#define _GNU_SOURCE
#include <dlfcn.h>
#include <errno.h>
#include <fcntl.h>
#include <linux/netlink.h>
#include <linux/rtnetlink.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <unistd.h>

/* Test-only preload: acknowledge sending route dumps without sending them to
 * the kernel. The real daemon awaits replies asynchronously forever, keeping
 * its export lock busy while its shutdown timer and protocol tasks can run. */
static int active(void) {
    const char *marker = getenv("BABEL_TEST_STALL_MARKER");
    return marker && access(marker, F_OK) == 0;
}

static void record_hit(const char *variable) {
    const char *path = getenv(variable);
    if (!path) return;
    int fd = open(path, O_WRONLY | O_CREAT | O_TRUNC, 0600);
    if (fd >= 0) {
        (void)write(fd, "hit", 3);
        (void)close(fd);
    }
}

static int drop_dump(int fd, const void *buffer, size_t length) {
    if (length < sizeof(struct nlmsghdr) || !active()) return 0;
    int domain, protocol;
    socklen_t size = sizeof(int);
    if (getsockopt(fd, SOL_SOCKET, SO_DOMAIN, &domain, &size) != 0 || domain != AF_NETLINK)
        return 0;
    size = sizeof(int);
    if (getsockopt(fd, SOL_SOCKET, SO_PROTOCOL, &protocol, &size) != 0 || protocol != NETLINK_ROUTE)
        return 0;
    struct nlmsghdr header;
    memcpy(&header, buffer, sizeof(header));
    if (header.nlmsg_type != RTM_GETROUTE) return 0;
    record_hit("BABEL_TEST_NETLINK_HIT");
    return 1;
}

ssize_t sendto(int fd, const void *buffer, size_t length, int flags,
               const struct sockaddr *address, socklen_t address_length) {
    if (drop_dump(fd, buffer, length)) return (ssize_t)length;
    ssize_t (*original)(int, const void *, size_t, int, const struct sockaddr *, socklen_t)
        = dlsym(RTLD_NEXT, "sendto");
    if (!original) { errno = ENOSYS; return -1; }
    return original(fd, buffer, length, flags, address, address_length);
}

ssize_t send(int fd, const void *buffer, size_t length, int flags) {
    if (drop_dump(fd, buffer, length)) return (ssize_t)length;
    ssize_t (*original)(int, const void *, size_t, int) = dlsym(RTLD_NEXT, "send");
    if (!original) { errno = ENOSYS; return -1; }
    return original(fd, buffer, length, flags);
}

int fsync(int fd) {
    if (getenv("BABEL_TEST_STALL_FSYNC") && active()) {
        record_hit("BABEL_TEST_FSYNC_HIT");
        for (;;) pause();
    }
    int (*original)(int) = dlsym(RTLD_NEXT, "fsync");
    if (!original) { errno = ENOSYS; return -1; }
    return original(fd);
}
