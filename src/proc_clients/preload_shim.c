/*
 * pgmcp unprivileged file-write tracer — LD_PRELOAD shim (ADR-022 Phase 2D).
 *
 * Interposes the file-MODIFYING libc calls (write-mode opens, creat, fopen(w/a/+),
 * rename, truncate, mkdir, unlink, link, symlink) and sends one datagram per event
 * to the pgmcp daemon's SOCK_DGRAM Unix socket. LD_PRELOAD is inherited across
 * fork/exec, so injecting this into an agent (via crucible/scripts/agent-scope.sh)
 * covers the agent AND its whole subprocess subtree (cargo->rustc->rg...) with ZERO
 * privilege — no caps, no root, no cgroup. Attribution rests on PGMCP_AGENT_ID
 * (set by the wrapper); the cgroup id is read opportunistically so the eBPF
 * cgroup-join also works when an agent additionally runs under a scope.
 *
 * Wire format (parsed by src/proc_clients/preload.rs::parse_preload_line):
 *   P\t<pid>\t<ppid>\t<cgroupid>\t<agent_id>\t<op>\t<flags>\t<abs_path>
 *   op in { 'w' write/create, 'e' edit/rename/unlink/..., 'r' read (opt-in) }
 *
 * Safety contract (a bug here must degrade to "no telemetry", never break the
 * traced program):
 *   - Thread-local re-entrancy guard (`in_hook`) so our own getcwd/readlink/open
 *     of /proc never re-enter an interposer.
 *   - The REAL libc function is resolved and called BEFORE any emit logic; emit
 *     never alters the call's return value, and errno is saved/restored around it.
 *   - Socket is SOCK_DGRAM | SOCK_CLOEXEC | SOCK_NONBLOCK; sendto uses
 *     MSG_DONTWAIT | MSG_NOSIGNAL and DROPS on any error. Never blocks, never
 *     spins, never writes stderr, never aborts.
 *   - secure_getenv() so an AT_SECURE (setuid/setgid) exec ignores our env.
 *   - No heap allocation on the hot path (fixed stack buffers).
 *
 * Built by build.rs (best-effort) -> $OUT_DIR/libpgmcp_fstrace.so:
 *   cc -shared -fPIC -O2 -fvisibility=hidden -o ... preload_shim.c -ldl
 */
#define _GNU_SOURCE
#include <dlfcn.h>
#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <sched.h>
#include <stdarg.h>
#include <stddef.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <sys/un.h>
#include <unistd.h>

#define PUBLIC __attribute__((visibility("default")))

#ifndef O_TMPFILE
#define O_TMPFILE 0 /* if the platform lacks it, the test below is a no-op */
#endif

/* ── shim state ──────────────────────────────────────────────────────────── */
static __thread int in_hook = 0;
static volatile int g_init_state = 0; /* 0=uninit, 1=initializing, 2=done */
static int g_enabled = 0;
static int g_emit_reads = 0;
static int g_sock = -1;
static struct sockaddr_un g_addr;
static int g_addr_len = 0;
static unsigned long long g_cgroup = 0;
static char g_agent[256] = "";

/* Transport selection (ADR-022 Phase 2E). FILE precedence: `PGMCP_FSTRACE_FILE`
 * wins when set — Codex's seccomp sandbox EPERMs socket I/O (`sendto`/`connect`)
 * but ALLOWS `write()` to a file in a `writable_roots` dir. Else `PGMCP_FSTRACE_SOCK`
 * (Claude — no sandbox blocks it). The wrapper sets exactly one per agent. */
enum { T_NONE = 0, T_SOCK = 1, T_FILE = 2 };
static volatile int g_transport = T_NONE;
static char g_file_path[PATH_MAX] = "";
static volatile int g_trace_fd = -1;
static volatile int g_file_open_failed = 0;

/* Resolve the next libc symbol named `name`, caching the pointer in `*slot`.
 * A `(void*)-1` sentinel records "resolved-but-absent" so we never re-dlsym. */
static void *resolve(void **slot, const char *name) {
    void *p = __atomic_load_n(slot, __ATOMIC_ACQUIRE);
    if (!p) {
        p = dlsym(RTLD_NEXT, name);
        if (!p) {
            p = (void *) -1;
        }
        __atomic_store_n(slot, p, __ATOMIC_RELEASE);
    }
    return (p == (void *) -1) ? NULL : p;
}

/* cgroup-v2 id == inode of the process's cgroup directory (matches the kernel
 * bpf_get_current_cgroup_id()). Read once at init. `open` here is our own
 * interposer but `in_hook` is set during init, so it passes straight through;
 * read/close/stat are not interposed. Returns 0 off cgroup-v2 / on any failure. */
static unsigned long long read_cgroup_id(void) {
    int fd = open("/proc/self/cgroup", O_RDONLY | O_CLOEXEC);
    if (fd < 0) {
        return 0;
    }
    char buf[4096];
    ssize_t n = read(fd, buf, sizeof(buf) - 1);
    close(fd);
    if (n <= 0) {
        return 0;
    }
    buf[n] = '\0';
    char *p = buf;
    while (p && *p) {
        if (p[0] == '0' && p[1] == ':' && p[2] == ':') {
            char *rel = p + 3;
            char *eol = strchr(rel, '\n');
            if (eol) {
                *eol = '\0';
            }
            char path[PATH_MAX];
            int pn = snprintf(path, sizeof(path), "/sys/fs/cgroup%s", rel);
            if (pn <= 0 || (size_t) pn >= sizeof(path)) {
                return 0;
            }
            struct stat st;
            if (stat(path, &st) == 0) {
                return (unsigned long long) st.st_ino;
            }
            return 0;
        }
        char *nl = strchr(p, '\n');
        if (!nl) {
            break;
        }
        p = nl + 1;
    }
    return 0;
}

/* One-time init: read env (secure_getenv), open the datagram socket, cache the
 * cgroup id + sanitized agent id. Called under `in_hook == 1`. */
static void init_once(void) {
    int expected = 0;
    if (!__atomic_compare_exchange_n(&g_init_state, &expected, 1, 0, __ATOMIC_ACQ_REL,
                                     __ATOMIC_ACQUIRE)) {
        /* Another thread is initializing (or has) — wait until it is done. */
        while (__atomic_load_n(&g_init_state, __ATOMIC_ACQUIRE) != 2) {
            sched_yield();
        }
        return;
    }

    g_enabled = 0;

    /* Agent id + reads flag are needed by either transport — parse once. */
    const char *aid = secure_getenv("PGMCP_AGENT_ID");
    if (aid) {
        size_t i = 0;
        for (; aid[i] && i < sizeof(g_agent) - 1; i++) {
            char c = aid[i];
            g_agent[i] = (c == '\t' || c == '\n') ? '_' : c;
        }
        g_agent[i] = '\0';
    }
    const char *rd = secure_getenv("PGMCP_FSTRACE_READS");
    g_emit_reads = (rd && rd[0] == '1') ? 1 : 0;

    /* FILE transport takes precedence (sandbox-safe). The fd is opened lazily on
     * first event (so we open it in the child after fork/exec, not inherit one). */
    const char *file = secure_getenv("PGMCP_FSTRACE_FILE");
    if (file && *file && strlen(file) < sizeof(g_file_path)) {
        memcpy(g_file_path, file, strlen(file) + 1);
        g_transport = T_FILE;
        g_cgroup = read_cgroup_id();
        g_enabled = 1;
    } else {
        const char *sock = secure_getenv("PGMCP_FSTRACE_SOCK");
        if (sock && *sock) {
            size_t sl = strlen(sock);
            if (sl < sizeof(g_addr.sun_path)) {
                memset(&g_addr, 0, sizeof(g_addr));
                g_addr.sun_family = AF_UNIX;
                memcpy(g_addr.sun_path, sock, sl);
                g_addr_len = (int) (offsetof(struct sockaddr_un, sun_path) + sl + 1);
                g_sock = socket(AF_UNIX, SOCK_DGRAM | SOCK_CLOEXEC | SOCK_NONBLOCK, 0);
                if (g_sock >= 0) {
                    g_transport = T_SOCK;
                    g_cgroup = read_cgroup_id();
                    g_enabled = 1;
                }
            }
        }
    }
    __atomic_store_n(&g_init_state, 2, __ATOMIC_RELEASE);
}

/* True while the shim might still emit: uninitialized (must run init) or enabled.
 * Once init settles to "disabled", this is false and interposers add ~nothing. */
static inline int active(void) {
    return __atomic_load_n(&g_init_state, __ATOMIC_ACQUIRE) != 2 || g_enabled == 1;
}

/* Resolve `path` (possibly relative / dirfd-based) to an absolute path in `out`.
 * Absolute fast-path; AT_FDCWD/plain -> getcwd(); real dirfd -> readlink of
 * /proc/self/fd/<dirfd>. Lexical join only (no realpath: never blocks, and the
 * downstream workspace prefix-match tolerates un-normalized paths). 0 on failure. */
static int resolve_abs(int dirfd, const char *path, char *out, size_t outsz) {
    if (!path || !*path) {
        return 0;
    }
    if (path[0] == '/') {
        size_t l = strlen(path);
        if (l >= outsz) {
            return 0;
        }
        memcpy(out, path, l + 1);
        return 1;
    }
    char base[PATH_MAX];
    if (dirfd == AT_FDCWD) {
        if (!getcwd(base, sizeof(base))) {
            return 0;
        }
    } else {
        char procp[64];
        int pn = snprintf(procp, sizeof(procp), "/proc/self/fd/%d", dirfd);
        if (pn <= 0 || (size_t) pn >= sizeof(procp)) {
            return 0;
        }
        ssize_t r = readlink(procp, base, sizeof(base) - 1);
        if (r < 0) {
            return 0;
        }
        base[r] = '\0';
    }
    int n = snprintf(out, outsz, "%s/%s", base, path);
    if (n <= 0 || (size_t) n >= outsz) {
        return 0;
    }
    return 1;
}

/* File transport (ADR-022 Phase 2E): append one newline-terminated record.
 * `O_APPEND` writes are atomic across concurrent writers only for len <= PIPE_BUF
 * (4096), so we HARD-CAP the record there — a >4 KiB line (pathological path) is
 * dropped rather than torn/interleaved. Open-once, cache the fd (`O_CLOEXEC` ⇒ each
 * exec'd child re-opens its own fd to the shared inode, preserving atomic appends).
 * Reached only from `do_emit_*` (so `in_hook==1` — our own `open`/`write` pass
 * through the interposers; errno is already saved by the caller). */
static void send_event_file(char *buf, size_t n) {
    if (g_file_open_failed) {
        return; /* sticky: don't hammer an unwritable path */
    }
    if (n >= (size_t) PIPE_BUF) {
        return; /* keep records atomic; drop the rare oversize line uncorrupted */
    }
    buf[n++] = '\n';

    int fd = __atomic_load_n(&g_trace_fd, __ATOMIC_ACQUIRE);
    if (fd < 0) {
        fd = open(g_file_path, O_WRONLY | O_APPEND | O_CREAT | O_CLOEXEC, 0600);
        if (fd < 0) {
            g_file_open_failed = 1; /* e.g. dir not writable in the sandbox */
            return;
        }
        int expected = -1;
        if (!__atomic_compare_exchange_n(&g_trace_fd, &expected, fd, 0, __ATOMIC_ACQ_REL,
                                         __ATOMIC_ACQUIRE)) {
            close(fd); /* lost the race — use the winner's fd */
            fd = expected;
        }
    }
    (void) write(fd, buf, n); /* best-effort single atomic append */
}

static void send_event(char op, long flags, const char *abspath) {
    char buf[PATH_MAX + 256];
    int n = snprintf(buf, sizeof(buf), "P\t%ld\t%ld\t%llu\t%s\t%c\t%ld\t%s", (long) getpid(),
                     (long) getppid(), g_cgroup, g_agent, op, flags, abspath);
    if (n <= 0 || (size_t) n >= sizeof(buf)) {
        return; /* drop oversize / format error */
    }
    if (__atomic_load_n(&g_transport, __ATOMIC_ACQUIRE) == T_FILE) {
        send_event_file(buf, (size_t) n);
        return;
    }
    (void) sendto(g_sock, buf, (size_t) n, MSG_DONTWAIT | MSG_NOSIGNAL,
                  (struct sockaddr *) &g_addr, (socklen_t) g_addr_len);
    /* best-effort: ignore EAGAIN/ENOBUFS/ECONNREFUSED — never block, never spin */
}

static void emit_path(char op, long flags, int dirfd, const char *path) {
    char abs[PATH_MAX];
    if (resolve_abs(dirfd, path, abs, sizeof(abs))) {
        send_event(op, flags, abs);
    }
}

/* ── emit helpers (each sets in_hook, runs init once, restores errno) ─────── */
static void do_emit_open(int dirfd, const char *path, int flags) {
    in_hook = 1;
    int saved = errno;
    if (__atomic_load_n(&g_init_state, __ATOMIC_ACQUIRE) != 2) {
        init_once();
    }
    if (g_enabled == 1 && (flags & O_TMPFILE) != O_TMPFILE) {
        char op = 0;
        if ((flags & O_ACCMODE) != O_RDONLY || (flags & O_CREAT)) {
            op = 'w';
        } else if (g_emit_reads) {
            op = 'r';
        }
        if (op) {
            emit_path(op, (long) flags, dirfd, path);
        }
    }
    errno = saved;
    in_hook = 0;
}

static void do_emit_simple(char op, int dirfd, const char *path) {
    in_hook = 1;
    int saved = errno;
    if (__atomic_load_n(&g_init_state, __ATOMIC_ACQUIRE) != 2) {
        init_once();
    }
    if (g_enabled == 1) {
        emit_path(op, 0, dirfd, path);
    }
    errno = saved;
    in_hook = 0;
}

static void do_emit_fopen(const char *path, const char *mode) {
    in_hook = 1;
    int saved = errno;
    if (__atomic_load_n(&g_init_state, __ATOMIC_ACQUIRE) != 2) {
        init_once();
    }
    if (g_enabled == 1) {
        char op = 0;
        if (mode && strpbrk(mode, "wa+")) {
            op = 'w';
        } else if (g_emit_reads) {
            op = 'r';
        }
        if (op) {
            emit_path(op, 0, AT_FDCWD, path);
        }
    }
    errno = saved;
    in_hook = 0;
}

/* ── interposers: open family (varargs mode) ─────────────────────────────── */
#define OPEN_BODY(realname, dirfd, pathexpr)                                                        \
    do {                                                                                            \
        if (ret >= 0 && !in_hook && active()) {                                                     \
            do_emit_open((dirfd), (pathexpr), flags);                                               \
        }                                                                                           \
        return ret;                                                                                 \
    } while (0)

PUBLIC int open(const char *path, int flags, ...) {
    static void *slot;
    int (*real)(const char *, int, ...) = (int (*)(const char *, int, ...)) resolve(&slot, "open");
    mode_t mode = 0;
    if (flags & (O_CREAT | O_TMPFILE)) {
        va_list ap;
        va_start(ap, flags);
        mode = (mode_t) va_arg(ap, int);
        va_end(ap);
    }
    if (!real) {
        errno = ENOSYS;
        return -1;
    }
    int ret = real(path, flags, (int) mode);
    OPEN_BODY("open", AT_FDCWD, path);
}

PUBLIC int open64(const char *path, int flags, ...) {
    static void *slot;
    int (*real)(const char *, int, ...) = (int (*)(const char *, int, ...)) resolve(&slot, "open64");
    mode_t mode = 0;
    if (flags & (O_CREAT | O_TMPFILE)) {
        va_list ap;
        va_start(ap, flags);
        mode = (mode_t) va_arg(ap, int);
        va_end(ap);
    }
    if (!real) {
        errno = ENOSYS;
        return -1;
    }
    int ret = real(path, flags, (int) mode);
    OPEN_BODY("open64", AT_FDCWD, path);
}

PUBLIC int openat(int dirfd, const char *path, int flags, ...) {
    static void *slot;
    int (*real)(int, const char *, int, ...) =
        (int (*)(int, const char *, int, ...)) resolve(&slot, "openat");
    mode_t mode = 0;
    if (flags & (O_CREAT | O_TMPFILE)) {
        va_list ap;
        va_start(ap, flags);
        mode = (mode_t) va_arg(ap, int);
        va_end(ap);
    }
    if (!real) {
        errno = ENOSYS;
        return -1;
    }
    int ret = real(dirfd, path, flags, (int) mode);
    OPEN_BODY("openat", dirfd, path);
}

PUBLIC int openat64(int dirfd, const char *path, int flags, ...) {
    static void *slot;
    int (*real)(int, const char *, int, ...) =
        (int (*)(int, const char *, int, ...)) resolve(&slot, "openat64");
    mode_t mode = 0;
    if (flags & (O_CREAT | O_TMPFILE)) {
        va_list ap;
        va_start(ap, flags);
        mode = (mode_t) va_arg(ap, int);
        va_end(ap);
    }
    if (!real) {
        errno = ENOSYS;
        return -1;
    }
    int ret = real(dirfd, path, flags, (int) mode);
    OPEN_BODY("openat64", dirfd, path);
}

/* Fortified variants (_FORTIFY_SOURCE) — no varargs mode. */
PUBLIC int __open_2(const char *path, int flags) {
    static void *slot;
    int (*real)(const char *, int) = (int (*)(const char *, int)) resolve(&slot, "__open_2");
    if (!real) {
        errno = ENOSYS;
        return -1;
    }
    int ret = real(path, flags);
    OPEN_BODY("__open_2", AT_FDCWD, path);
}

PUBLIC int __open64_2(const char *path, int flags) {
    static void *slot;
    int (*real)(const char *, int) = (int (*)(const char *, int)) resolve(&slot, "__open64_2");
    if (!real) {
        errno = ENOSYS;
        return -1;
    }
    int ret = real(path, flags);
    OPEN_BODY("__open64_2", AT_FDCWD, path);
}

PUBLIC int __openat_2(int dirfd, const char *path, int flags) {
    static void *slot;
    int (*real)(int, const char *, int) =
        (int (*)(int, const char *, int)) resolve(&slot, "__openat_2");
    if (!real) {
        errno = ENOSYS;
        return -1;
    }
    int ret = real(dirfd, path, flags);
    OPEN_BODY("__openat_2", dirfd, path);
}

PUBLIC int __openat64_2(int dirfd, const char *path, int flags) {
    static void *slot;
    int (*real)(int, const char *, int) =
        (int (*)(int, const char *, int)) resolve(&slot, "__openat64_2");
    if (!real) {
        errno = ENOSYS;
        return -1;
    }
    int ret = real(dirfd, path, flags);
    OPEN_BODY("__openat64_2", dirfd, path);
}

/* creat == open(O_WRONLY|O_CREAT|O_TRUNC): always a write. */
PUBLIC int creat(const char *path, mode_t mode) {
    static void *slot;
    int (*real)(const char *, mode_t) = (int (*)(const char *, mode_t)) resolve(&slot, "creat");
    if (!real) {
        errno = ENOSYS;
        return -1;
    }
    int ret = real(path, mode);
    if (ret >= 0 && !in_hook && active()) {
        do_emit_simple('w', AT_FDCWD, path);
    }
    return ret;
}

PUBLIC int creat64(const char *path, mode_t mode) {
    static void *slot;
    int (*real)(const char *, mode_t) = (int (*)(const char *, mode_t)) resolve(&slot, "creat64");
    if (!real) {
        errno = ENOSYS;
        return -1;
    }
    int ret = real(path, mode);
    if (ret >= 0 && !in_hook && active()) {
        do_emit_simple('w', AT_FDCWD, path);
    }
    return ret;
}

/* ── interposers: stdio ──────────────────────────────────────────────────── */
PUBLIC FILE *fopen(const char *path, const char *mode) {
    static void *slot;
    FILE *(*real)(const char *, const char *) =
        (FILE * (*) (const char *, const char *)) resolve(&slot, "fopen");
    if (!real) {
        errno = ENOSYS;
        return NULL;
    }
    FILE *ret = real(path, mode);
    if (ret && !in_hook && active()) {
        do_emit_fopen(path, mode);
    }
    return ret;
}

PUBLIC FILE *fopen64(const char *path, const char *mode) {
    static void *slot;
    FILE *(*real)(const char *, const char *) =
        (FILE * (*) (const char *, const char *)) resolve(&slot, "fopen64");
    if (!real) {
        errno = ENOSYS;
        return NULL;
    }
    FILE *ret = real(path, mode);
    if (ret && !in_hook && active()) {
        do_emit_fopen(path, mode);
    }
    return ret;
}

PUBLIC FILE *freopen(const char *path, const char *mode, FILE *stream) {
    static void *slot;
    FILE *(*real)(const char *, const char *, FILE *) =
        (FILE * (*) (const char *, const char *, FILE *)) resolve(&slot, "freopen");
    if (!real) {
        errno = ENOSYS;
        return NULL;
    }
    FILE *ret = real(path, mode, stream);
    if (ret && path && !in_hook && active()) {
        do_emit_fopen(path, mode);
    }
    return ret;
}

/* ── interposers: rename / link / unlink / truncate / mkdir (op='e') ─────── */
PUBLIC int rename(const char *oldp, const char *newp) {
    static void *slot;
    int (*real)(const char *, const char *) =
        (int (*)(const char *, const char *)) resolve(&slot, "rename");
    if (!real) {
        errno = ENOSYS;
        return -1;
    }
    int ret = real(oldp, newp);
    if (ret == 0 && !in_hook && active()) {
        do_emit_simple('e', AT_FDCWD, newp);
    }
    return ret;
}

PUBLIC int renameat(int oldfd, const char *oldp, int newfd, const char *newp) {
    static void *slot;
    int (*real)(int, const char *, int, const char *) =
        (int (*)(int, const char *, int, const char *)) resolve(&slot, "renameat");
    if (!real) {
        errno = ENOSYS;
        return -1;
    }
    int ret = real(oldfd, oldp, newfd, newp);
    if (ret == 0 && !in_hook && active()) {
        do_emit_simple('e', newfd, newp);
    }
    return ret;
}

PUBLIC int renameat2(int oldfd, const char *oldp, int newfd, const char *newp, unsigned int flags) {
    static void *slot;
    int (*real)(int, const char *, int, const char *, unsigned int) =
        (int (*)(int, const char *, int, const char *, unsigned int)) resolve(&slot, "renameat2");
    if (!real) {
        errno = ENOSYS;
        return -1;
    }
    int ret = real(oldfd, oldp, newfd, newp, flags);
    if (ret == 0 && !in_hook && active()) {
        do_emit_simple('e', newfd, newp);
    }
    return ret;
}

PUBLIC int truncate(const char *path, off_t length) {
    static void *slot;
    int (*real)(const char *, off_t) = (int (*)(const char *, off_t)) resolve(&slot, "truncate");
    if (!real) {
        errno = ENOSYS;
        return -1;
    }
    int ret = real(path, length);
    if (ret == 0 && !in_hook && active()) {
        do_emit_simple('e', AT_FDCWD, path);
    }
    return ret;
}

PUBLIC int truncate64(const char *path, off_t length) {
    static void *slot;
    int (*real)(const char *, off_t) = (int (*)(const char *, off_t)) resolve(&slot, "truncate64");
    if (!real) {
        errno = ENOSYS;
        return -1;
    }
    int ret = real(path, length);
    if (ret == 0 && !in_hook && active()) {
        do_emit_simple('e', AT_FDCWD, path);
    }
    return ret;
}

PUBLIC int mkdir(const char *path, mode_t mode) {
    static void *slot;
    int (*real)(const char *, mode_t) = (int (*)(const char *, mode_t)) resolve(&slot, "mkdir");
    if (!real) {
        errno = ENOSYS;
        return -1;
    }
    int ret = real(path, mode);
    if (ret == 0 && !in_hook && active()) {
        do_emit_simple('e', AT_FDCWD, path);
    }
    return ret;
}

PUBLIC int unlink(const char *path) {
    static void *slot;
    int (*real)(const char *) = (int (*)(const char *)) resolve(&slot, "unlink");
    if (!real) {
        errno = ENOSYS;
        return -1;
    }
    int ret = real(path);
    if (ret == 0 && !in_hook && active()) {
        do_emit_simple('e', AT_FDCWD, path);
    }
    return ret;
}

PUBLIC int unlinkat(int dirfd, const char *path, int flags) {
    static void *slot;
    int (*real)(int, const char *, int) =
        (int (*)(int, const char *, int)) resolve(&slot, "unlinkat");
    if (!real) {
        errno = ENOSYS;
        return -1;
    }
    int ret = real(dirfd, path, flags);
    if (ret == 0 && !in_hook && active()) {
        do_emit_simple('e', dirfd, path);
    }
    return ret;
}

PUBLIC int link(const char *oldp, const char *newp) {
    static void *slot;
    int (*real)(const char *, const char *) =
        (int (*)(const char *, const char *)) resolve(&slot, "link");
    if (!real) {
        errno = ENOSYS;
        return -1;
    }
    int ret = real(oldp, newp);
    if (ret == 0 && !in_hook && active()) {
        do_emit_simple('e', AT_FDCWD, newp);
    }
    return ret;
}

PUBLIC int linkat(int oldfd, const char *oldp, int newfd, const char *newp, int flags) {
    static void *slot;
    int (*real)(int, const char *, int, const char *, int) =
        (int (*)(int, const char *, int, const char *, int)) resolve(&slot, "linkat");
    if (!real) {
        errno = ENOSYS;
        return -1;
    }
    int ret = real(oldfd, oldp, newfd, newp, flags);
    if (ret == 0 && !in_hook && active()) {
        do_emit_simple('e', newfd, newp);
    }
    return ret;
}

PUBLIC int symlink(const char *target, const char *linkpath) {
    static void *slot;
    int (*real)(const char *, const char *) =
        (int (*)(const char *, const char *)) resolve(&slot, "symlink");
    if (!real) {
        errno = ENOSYS;
        return -1;
    }
    int ret = real(target, linkpath);
    if (ret == 0 && !in_hook && active()) {
        do_emit_simple('e', AT_FDCWD, linkpath);
    }
    return ret;
}

PUBLIC int symlinkat(const char *target, int newdirfd, const char *linkpath) {
    static void *slot;
    int (*real)(const char *, int, const char *) =
        (int (*)(const char *, int, const char *)) resolve(&slot, "symlinkat");
    if (!real) {
        errno = ENOSYS;
        return -1;
    }
    int ret = real(target, newdirfd, linkpath);
    if (ret == 0 && !in_hook && active()) {
        do_emit_simple('e', newdirfd, linkpath);
    }
    return ret;
}
